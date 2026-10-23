//! The room server role: a port of upstream `examples/simple_room_server`.
//!
//! A room server is the same node as the repeater (CLI, ACL, regions, adverts) with forwarding
//! off and a bulletin board on top: logged-in clients post plain-text messages, and the server
//! pushes every post to each other client in turn, one at a time, waiting for its ACK. A client's
//! `sync_since` (the newest post it has acknowledged) survives in the ACL record, so a client that
//! logs in again only gets what it missed. Posts live in RAM, like upstream.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::*;

pub const FIRMWARE_ROLE_ROOM: &str = "room_server";
pub(super) const FIRMWARE_VER_LEVEL_ROOM: u8 = 1;

const MAX_UNSYNCED_POSTS: usize = 32;
/// Upstream `MAX_POST_TEXT_LEN` (160 - 9).
pub const MAX_POST_TEXT_LEN: usize = 151;

const REQ_TYPE_KEEP_ALIVE: u8 = 0x02;
const TXT_TYPE_SIGNED_PLAIN: u8 = 2;

const REPLY_DELAY_MILLIS: u32 = 1500;
const PUSH_NOTIFY_DELAY_MILLIS: u64 = 2000;
const SYNC_PUSH_INTERVAL: u64 = 1200;
const PUSH_ACK_TIMEOUT_FLOOD: u64 = 12_000;
const PUSH_TIMEOUT_BASE: u64 = 4000;
const PUSH_ACK_TIMEOUT_FACTOR: u64 = 2000;
const POST_SYNC_DELAY_SECS: u32 = 6;
const MAX_PUSH_FAILURES: u8 = 3;

/// Per-client sync state (upstream `ClientInfo::extra.room`).
#[derive(Clone, Copy, Debug, Default)]
pub struct RoomClient {
    /// Newest post (by our clock) this client has acknowledged; persisted in the ACL record.
    pub sync_since: u32,
    /// Expected ACK of the post being pushed (0 = none outstanding).
    pub pending_ack: u32,
    pub push_post_timestamp: u32,
    pub ack_timeout: u64,
    pub push_failures: u8,
}

#[derive(Clone, Debug, Default)]
struct Post {
    author: Identity,
    /// By OUR clock; 0 = empty slot.
    timestamp: u32,
    text: String,
}

/// Room server state (`MyMesh` fields of `simple_room_server`).
pub(super) struct Room {
    /// Cyclic queue; `next_post_idx` is the oldest slot.
    posts: Vec<Post>,
    next_post_idx: usize,
    next_client_idx: usize,
    next_push: u64,
    pub(super) num_posted: u16,
    pub(super) num_post_pushes: u16,
}

impl Room {
    pub(super) fn new() -> Box<Self> {
        Box::new(Self {
            posts: alloc::vec![Post::default(); MAX_UNSYNCED_POSTS],
            next_post_idx: 0,
            next_client_idx: 0,
            next_push: 0,
            num_posted: 0,
            num_post_pushes: 0,
        })
    }

    pub(super) fn next_timer(&self) -> Option<u64> {
        (self.next_push != 0).then_some(self.next_push)
    }
}

impl NodePrefs {
    /// Defaults of upstream `simple_room_server`: same radio, forwarding off.
    pub fn room_server_defaults() -> Self {
        Self { node_name: "Rust Room".into(), disable_fwd: true, direct_tx_delay_factor: 0.2, ..Self::default() }
    }
}

/// Text up to [`MAX_POST_TEXT_LEN`] bytes, cut at a character boundary.
fn post_text(text: &[u8]) -> String {
    let s = String::from_utf8_lossy(text);
    let mut n = s.len().min(MAX_POST_TEXT_LEN);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    String::from(&s[..n])
}

impl Repeater {
    /// A room server instead of a repeater (`simple_room_server`).
    pub fn new_room_server(prefs: NodePrefs, board: BoardInfo) -> Self {
        let mut node = Self::new(prefs, board);
        node.room = Some(Room::new());
        node
    }

    pub fn is_room_server(&self) -> bool {
        self.room.is_some()
    }

    pub fn role(&self) -> &'static str {
        if self.room.is_some() { FIRMWARE_ROLE_ROOM } else { FIRMWARE_ROLE }
    }

    /// Posts in the room and pushes sent, since boot.
    pub fn room_stats(&self) -> Option<(u16, u16)> {
        self.room.as_ref().map(|r| (r.num_posted, r.num_post_pushes))
    }

    /// `addSystemPost`: a post by the server itself (`room.post`).
    pub fn add_system_post(&mut self, ctx: &mut MeshCtx, text: &str) {
        if !text.is_empty() {
            let id = ctx.self_id.id;
            self.store_post(ctx, id, text.as_bytes());
        }
    }

    fn store_post(&mut self, ctx: &mut MeshCtx, author: Identity, text: &[u8]) {
        let timestamp = ctx.current_time_unique();
        let Some(room) = self.room.as_mut() else {
            return;
        };
        let idx = room.next_post_idx;
        room.posts[idx] = Post { author, timestamp, text: post_text(text) };
        room.next_post_idx = (idx + 1) % MAX_UNSYNCED_POSTS;
        room.next_push = ctx.now + PUSH_NOTIFY_DELAY_MILLIS;
        room.num_posted = room.num_posted.wrapping_add(1);
    }

    /// Posts this client has not acknowledged yet (not counting its own).
    fn unsynced_count(&self, client: &ClientInfo) -> u8 {
        let Some(room) = self.room.as_ref() else {
            return 0;
        };
        room.posts.iter().filter(|p| p.timestamp > client.room.sync_since && p.author != client.id).count() as u8
    }

    fn push_post_to_client(&mut self, ctx: &mut MeshCtx, client_idx: usize, post_idx: usize) {
        let Some(room) = self.room.as_ref() else {
            return;
        };
        let post = &room.posts[post_idx];
        let mut data = Vec::with_capacity(9 + post.text.len());
        data.extend_from_slice(&post.timestamp.to_le_bytes()); // a PAST timestamp, accepted by clients
        let mut attempt = [0u8; 1];
        ctx.rng.fill(&mut attempt); // retries get a different packet hash (and ACK)
        data.push((TXT_TYPE_SIGNED_PLAIN << 2) | (attempt[0] & 3));
        data.extend_from_slice(&post.author.pub_key[..4]);
        data.extend_from_slice(post.text.as_bytes());
        let post_ts = post.timestamp;

        let client = &self.acl[client_idx];
        let mut ack = [0u8; 4];
        crypto::sha256(&mut ack, &[&data, &client.id.pub_key]);
        let id = client.id;
        let secret = client.shared_secret;
        let out = client.out_path().map(|(l, p)| (l, p.to_vec()));

        let Some(pkt) = ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &id, &secret, &data) else {
            self.acl[client_idx].room.pending_ack = 0;
            return;
        };
        let timeout = match out {
            None => {
                let scope = self.default_scope();
                self.send_flood_scoped(ctx, pkt, scope, 0, self.prefs.path_hash_mode as usize + 1);
                PUSH_ACK_TIMEOUT_FLOOD
            }
            Some((len, path)) => {
                ctx.send_direct(pkt, len, &path, 0);
                PUSH_TIMEOUT_BASE + PUSH_ACK_TIMEOUT_FACTOR * ((len & 63) as u64 + 1)
            }
        };
        let c = &mut self.acl[client_idx].room;
        c.pending_ack = u32::from_le_bytes(ack);
        c.push_post_timestamp = post_ts;
        c.ack_timeout = ctx.now + timeout;
        if let Some(room) = self.room.as_mut() {
            room.num_post_pushes = room.num_post_pushes.wrapping_add(1);
        }
    }

    /// An ACK from a client for the post pushed to it: advance its `sync_since`.
    pub(super) fn room_process_ack(&mut self, ack: &[u8]) -> bool {
        let Some(ack) = ack.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())) else {
            return false;
        };
        let Some(c) = self.acl.iter_mut().find(|c| c.room.pending_ack != 0 && c.room.pending_ack == ack) else {
            return false;
        };
        c.room.pending_ack = 0;
        c.room.push_failures = 0;
        c.room.sync_since = c.room.push_post_timestamp;
        true
    }

    /// Round-robin push of the oldest unsynced post to the next client.
    pub(super) fn room_loop(&mut self, ctx: &mut MeshCtx) {
        let Some(room) = self.room.as_ref() else {
            return;
        };
        if room.next_push == 0 || ctx.now < room.next_push || self.acl.is_empty() {
            return;
        }
        for c in self.acl.iter_mut() {
            if c.room.pending_ack != 0 && ctx.now >= c.room.ack_timeout {
                c.room.push_failures += 1;
                c.room.pending_ack = 0;
            }
        }
        let client_idx = room.next_client_idx.min(self.acl.len() - 1);
        let client = &self.acl[client_idx];
        let mut pushed = false;
        if client.room.pending_ack == 0 && client.last_activity != 0 && client.room.push_failures < MAX_PUSH_FAILURES {
            let now = ctx.current_time();
            let found = (0..MAX_UNSYNCED_POSTS).map(|k| (room.next_post_idx + k) % MAX_UNSYNCED_POSTS).find(|&i| {
                let p = &room.posts[i];
                now >= p.timestamp.saturating_add(POST_SYNC_DELAY_SECS)
                    && p.timestamp > client.room.sync_since
                    && p.author != client.id
            });
            if let Some(post_idx) = found {
                self.push_post_to_client(ctx, client_idx, post_idx);
                pushed = true;
            }
        }
        let n = self.acl.len();
        if let Some(room) = self.room.as_mut() {
            room.next_client_idx = (client_idx + 1) % n;
            // no unsynced posts for this client: check the next one much sooner
            room.next_push = ctx.now + if pushed { SYNC_PUSH_INTERVAL } else { SYNC_PUSH_INTERVAL / 8 };
        }
    }

    /// Room login (`onAnonDataRecv`): `[timestamp][sync_since][password]`.
    pub(super) fn room_login(&mut self, ctx: &mut MeshCtx, pkt: &Packet, secret: &Secret, sender: &Identity, data: &[u8]) {
        if data.len() < 8 {
            return;
        }
        let sender_timestamp = read_u32(data, 0);
        let sender_sync_since = read_u32(data, 4);
        let password = c_str(&data[8..]);

        let mut client = if password.is_empty() { self.get_client(&sender.pub_key) } else { None };
        if client.is_none() {
            let perm = if password == self.prefs.password.as_bytes() {
                PERM_ACL_ADMIN
            } else if password == self.prefs.guest_password.as_bytes() {
                PERM_ACL_READ_WRITE // the room password
            } else if self.prefs.allow_read_only {
                PERM_ACL_GUEST
            } else {
                return; // wrong room password: no response, the client times out
            };
            let i = self.put_client(*sender, 0);
            if sender_timestamp <= self.acl[i].last_timestamp {
                return; // possible replay attack
            }
            let now = ctx.current_time();
            let c = &mut self.acl[i];
            c.last_timestamp = sender_timestamp;
            c.room.sync_since = sender_sync_since;
            c.room.pending_ack = 0;
            c.room.push_failures = 0;
            c.last_activity = now;
            c.permissions = (c.permissions & !PERM_ACL_ROLE_MASK) | perm;
            c.shared_secret = *secret;
            self.dirty_contacts_expiry = ctx.now + LAZY_CONTACTS_WRITE_DELAY;
            client = Some(i);
        }
        let i = client.unwrap();
        if pkt.is_route_flood() {
            self.acl[i].out_path_len = OUT_PATH_UNKNOWN; // rediscover out_path
        }

        let c = &self.acl[i];
        let mut reply = Vec::with_capacity(13);
        reply.extend_from_slice(&ctx.current_time_unique().to_le_bytes());
        reply.push(RESP_SERVER_LOGIN_OK);
        reply.push(0); // legacy keep-alive interval
        reply.push(if c.is_admin() { 1 } else if c.permissions == 0 { 2 } else { 0 });
        reply.push(c.permissions);
        let mut blob = [0u8; 4];
        ctx.rng.fill(&mut blob); // packet-hash uniqueness
        reply.extend_from_slice(&blob);
        reply.push(FIRMWARE_VER_LEVEL_ROOM);
        let secret = c.shared_secret;
        let out = c.out_path().map(|(l, p)| (l, p.to_vec()));
        if let Some(room) = self.room.as_mut() {
            room.next_push = ctx.now + PUSH_NOTIFY_DELAY_MILLIS; // let the response arrive first
        }

        let phs = pkt.path_hash_size();
        if pkt.is_route_flood() {
            // tell the sender the path to here, with the response attached
            if let Some(path) =
                ctx.create_path_return(sender.hash(), &secret, pkt.path_len, &pkt.path, PAYLOAD_TYPE_RESPONSE, &reply)
            {
                self.send_flood_reply(ctx, path, SERVER_RESPONSE_DELAY, phs);
            }
        } else if let Some(resp) = ctx.create_datagram(PAYLOAD_TYPE_RESPONSE, sender, &secret, &reply) {
            match out {
                Some((len, path)) => ctx.send_direct(resp, len, &path, SERVER_RESPONSE_DELAY),
                None => self.send_flood_reply(ctx, resp, SERVER_RESPONSE_DELAY, phs),
            }
        }
    }

    /// Posts, CLI and requests from a logged-in client (`onPeerDataRecv`).
    pub(super) fn room_peer_data(
        &mut self, ctx: &mut MeshCtx, pkt: &Packet, payload_type: u8, i: usize, secret: &Secret, data: &[u8],
    ) {
        let phs = pkt.path_hash_size();
        let timestamp = read_u32(data, 0);
        if payload_type == PAYLOAD_TYPE_TXT_MSG && data.len() > 5 {
            let flags = data[4] >> 2;
            if !matches!(flags, TXT_TYPE_PLAIN | TXT_TYPE_CLI_DATA | TXT_TYPE_CLI_COMMAND) {
                return;
            }
            if timestamp < self.acl[i].last_timestamp {
                return; // replay; retries (same timestamp) still get their ACK
            }
            let is_retry = timestamp == self.acl[i].last_timestamp;
            let now = ctx.current_time_unique();
            let c = &mut self.acl[i];
            c.last_timestamp = timestamp;
            c.last_activity = now;
            c.room.push_failures = 0; // so pushes resume
            let id = c.id;
            let is_admin = c.is_admin();
            let is_guest = c.permissions & PERM_ACL_ROLE_MASK == PERM_ACL_GUEST;
            let out = c.out_path().map(|(l, p)| (l, p.to_vec()));
            let text = c_str(&data[5..]);

            let mut ack = [0u8; 4];
            crypto::sha256(&mut ack, &[&data[..5 + text.len()], &id.pub_key]);

            let mut reply = String::new();
            let mut send_ack = false;
            if flags == TXT_TYPE_CLI_DATA || flags == TXT_TYPE_CLI_COMMAND {
                if is_admin && !is_retry {
                    let cmd = String::from_utf8_lossy(text).into_owned();
                    reply = self.handle_command(ctx, timestamp, &cmd);
                }
            } else if !is_guest {
                if !is_retry {
                    self.store_post(ctx, id, text);
                }
                send_ack = true; // no reply, the ACK is enough
            }

            let mut delay = 0;
            if send_ack {
                match &out {
                    None => {
                        let a = ctx.create_ack(&ack);
                        self.send_flood_reply(ctx, a, TXT_ACK_DELAY, phs);
                        delay = TXT_ACK_DELAY + REPLY_DELAY_MILLIS;
                    }
                    Some((len, path)) => {
                        let mut d = TXT_ACK_DELAY;
                        if self.prefs.multi_acks > 0 {
                            let a1 = ctx.create_multi_ack(&ack, 1);
                            ctx.send_direct(a1, *len, path, d);
                            d += 300;
                        }
                        let a2 = ctx.create_ack(&ack);
                        ctx.send_direct(a2, *len, path, d);
                        delay = d + REPLY_DELAY_MILLIS;
                    }
                }
            }
            if !reply.is_empty() {
                let mut ts = now;
                if ts == timestamp {
                    ts += 1; // the two timestamps must differ in the CLI view
                }
                let mut temp = Vec::with_capacity(5 + reply.len());
                temp.extend_from_slice(&ts.to_le_bytes());
                temp.push(TXT_TYPE_CLI_DATA << 2);
                let max_text = MAX_PACKET_PAYLOAD - 2 - 2 - 16 - 5;
                temp.extend_from_slice(&reply.as_bytes()[..reply.len().min(max_text)]);
                if let Some(msg) = ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &id, secret, &temp) {
                    match out {
                        Some((len, path)) => ctx.send_direct(msg, len, &path, delay + SERVER_RESPONSE_DELAY),
                        None => self.send_flood_reply(ctx, msg, delay + SERVER_RESPONSE_DELAY, phs),
                    }
                }
            }
        } else if payload_type == PAYLOAD_TYPE_REQ && data.len() >= 5 {
            if timestamp < self.acl[i].last_timestamp {
                return; // replay
            }
            let now = ctx.current_time();
            let c = &mut self.acl[i];
            c.last_timestamp = timestamp;
            c.last_activity = now; // keeps the client's session alive
            c.room.push_failures = 0;

            if data[4] == REQ_TYPE_KEEP_ALIVE && pkt.is_route_direct() {
                // optional: the newest post the client has (may be 0 from the decryption padding)
                let force_since = read_u32(data, 5);
                if force_since > 0 {
                    c.room.sync_since = force_since;
                }
                c.room.pending_ack = 0;
                // keep-alive answers go DIRECT only
                let c = &self.acl[i];
                if let Some((len, path)) = c.out_path().map(|(l, p)| (l, p.to_vec())) {
                    let mut buf = [0u8; 9];
                    buf[..data.len().min(9)].copy_from_slice(&data[..data.len().min(9)]);
                    let mut ack = [0u8; 4];
                    crypto::sha256(&mut ack, &[&buf, &c.id.pub_key]);
                    let unsynced = self.unsynced_count(c);
                    let mut a = ctx.create_ack(&ack);
                    a.payload[a.payload_len] = unsynced; // unsynced counter appended to the ACK
                    a.payload_len += 1;
                    ctx.send_direct(a, len, &path, SERVER_RESPONSE_DELAY);
                }
                return;
            }
            let reply = self.handle_request(ctx, i, timestamp, &data[4..]);
            if reply.is_empty() {
                return;
            }
            let id = self.acl[i].id;
            let out = self.acl[i].out_path().map(|(l, p)| (l, p.to_vec()));
            if pkt.is_route_flood() {
                if let Some(path) =
                    ctx.create_path_return(id.hash(), secret, pkt.path_len, &pkt.path, PAYLOAD_TYPE_RESPONSE, &reply)
                {
                    self.send_flood_reply(ctx, path, SERVER_RESPONSE_DELAY, phs);
                }
            } else if let Some(resp) = ctx.create_datagram(PAYLOAD_TYPE_RESPONSE, &id, secret, &reply) {
                match out {
                    Some((len, path)) => ctx.send_direct(resp, len, &path, SERVER_RESPONSE_DELAY),
                    None => self.send_flood_reply(ctx, resp, SERVER_RESPONSE_DELAY, phs),
                }
            }
        }
    }

    /// Room-only CLI commands; `None` = not one of them.
    pub(super) fn room_command(&mut self, ctx: &mut MeshCtx, cmd: &str) -> Option<String> {
        self.room.as_ref()?;
        if let Some(msg) = cmd.strip_prefix("room.post") {
            let msg = msg.trim_start_matches(' ');
            return Some(if msg.is_empty() {
                "ERR empty message".into()
            } else {
                self.add_system_post(ctx, msg);
                "OK".into()
            });
        }
        if cmd.starts_with("set repeat on") {
            return Some("ERR - repeat now deprecated".into());
        }
        if cmd.starts_with("neighbors") {
            return Some("not supported".into());
        }
        if cmd.starts_with("discover.neighbors") {
            return Some("Unknown command".into());
        }
        if cmd == "get role" || cmd.starts_with("get role ") {
            return Some(format!("> {FIRMWARE_ROLE_ROOM}"));
        }
        None
    }
}
