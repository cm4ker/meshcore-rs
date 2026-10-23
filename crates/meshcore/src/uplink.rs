//! MQTT observer ("uplink") messages, compatible with the MeshCoreTel `pkio` firmware: every
//! received (and optionally transmitted) packet and a periodic status go to
//! `meshcore/<IATA>/<PUBKEY>/{packets,status}` on meshcoretel.ru / LetsMesh / ommesh.
//!
//! This module holds the transport-independent parts: topic and JSON formats (byte-exact with
//! the C++ implementation), the Ed25519 JWT used as the MQTT password, and a minimal MQTT 3.1.1
//! codec. The firmware provides Wi-Fi, TCP and TLS.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::identity::LocalIdentity;
use crate::packet::Packet;
use crate::util::{epoch_to_datetime, to_hex};

/// Tokens are only minted once the clock is plausible (2025-01-01).
pub const MIN_TOKEN_TIME: u32 = 1_735_689_600;
pub const TOKEN_LIFETIME_SECS: u32 = 3600;
pub const STATUS_INTERVAL_SECS: u32 = 300;

/// Uppercase, `[A-Za-z0-9_-]` only, at most 7 characters.
pub fn sanitize_iata(s: &str) -> String {
    s.chars()
        .take(7)
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c.to_ascii_uppercase() } else { '_' })
        .collect()
}

pub fn topic(iata: &str, device_id: &str, leaf: &str) -> String {
    format!("meshcore/{iata}/{device_id}/{leaf}")
}

/// `origin`: JSON-escaped node name, control characters replaced, at most 79 bytes.
fn origin(name: &str, device_id: &str) -> String {
    let src = if name.is_empty() { device_id } else { name };
    let mut out = String::new();
    for c in src.chars() {
        let piece: &str = &match c {
            '"' => String::from("\\\""),
            '\\' => String::from("\\\\"),
            '\n' => String::from("\\n"),
            '\r' => String::from("\\r"),
            '\t' => String::from("\\t"),
            c if (c as u32) < 0x20 => String::from("_"),
            c => String::from(c),
        };
        if out.len() + piece.len() > 79 {
            break;
        }
        out.push_str(piece);
    }
    out
}

/// `2026-10-22T12:34:56.000000` (UTC, no `Z`, like the C++ firmware).
fn iso_timestamp(t: u32) -> String {
    let (y, mo, d, h, mi, s) = epoch_to_datetime(t);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000000")
}

/// Identity of the observer as it appears in every message.
pub struct Observer<'a> {
    pub name: &'a str,
    /// 64 uppercase hex chars of the node public key.
    pub device_id: &'a str,
    pub model: &'a str,
    pub firmware_version: &'a str,
    pub client_version: &'a str,
    /// `"%.6f,%.1f,%u,%u"` of freq, bw, sf, cr.
    pub radio: &'a str,
}

pub fn radio_string(freq_mhz: f32, bw_khz: f32, sf: u8, cr: u8) -> String {
    format!("{:.6},{:.1},{},{}", freq_mhz, bw_khz, sf, cr)
}

/// A packet reception (`rx`) or transmission (`tx`); `score_duration` only for RX.
pub fn packet_json(
    obs: &Observer<'_>, now: u32, tx: bool, pkt: &Packet, raw: &[u8], snr: f32, rssi: f32,
    score_duration: Option<(i32, u32)>,
) -> String {
    let (y, mo, d, h, mi, s) = epoch_to_datetime(now);
    let mut j = format!(
        "{{\"origin\":\"{}\",\"origin_id\":\"{}\",\"timestamp\":\"{}\",\"type\":\"PACKET\",\"direction\":\"{}\",\
\"time\":\"{h:02}:{mi:02}:{s:02}\",\"date\":\"{d:02}/{mo:02}/{y:04}\",\"len\":\"{}\",\"packet_type\":\"{}\",\
\"route\":\"{}\",\"payload_len\":\"{}\",\"raw\":\"{}\",\"SNR\":\"{:.1}\",\"RSSI\":\"{}\"",
        origin(obs.name, obs.device_id),
        obs.device_id,
        iso_timestamp(now),
        if tx { "tx" } else { "rx" },
        raw.len(),
        pkt.payload_type(),
        if pkt.is_route_direct() { "D" } else { "F" },
        pkt.payload_len,
        to_hex(raw),
        snr,
        rssi as i32,
    );
    if let Some((score, duration)) = score_duration {
        let _ = write!(j, ",\"score\":\"{score}\",\"duration\":\"{duration}\"");
    }
    let _ = write!(j, ",\"hash\":\"{}\"", to_hex(&pkt.calculate_hash()));
    if pkt.is_route_direct() && pkt.path_len > 0 {
        let (count, size) = (pkt.path_hash_count(), pkt.path_hash_size());
        let _ = write!(j, ",\"path\":\"path_{count}x{size}_{}b\"", count * size);
    }
    j.push('}');
    j
}

/// `RAW` message (`mqtt.raw on`): the packet bytes only.
pub fn raw_json(obs: &Observer<'_>, now: u32, raw: &[u8]) -> String {
    format!(
        "{{\"origin\":\"{}\",\"origin_id\":\"{}\",\"timestamp\":\"{}\",\"type\":\"RAW\",\"data\":\"{}\"}}",
        origin(obs.name, obs.device_id),
        obs.device_id,
        iso_timestamp(now),
        to_hex(raw)
    )
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StatusStats {
    pub battery_mv: u16,
    pub uptime_secs: u32,
    pub errors: u16,
    pub queue_len: u16,
    pub noise_floor: i16,
    pub tx_air_secs: u32,
    pub rx_air_secs: u32,
    pub recv_errors: u32,
}

/// `online` status (retained) with stats, or the `offline` last will (no stats).
pub fn status_json(obs: &Observer<'_>, now: u32, online: bool, stats: Option<&StatusStats>) -> String {
    let mut j = format!(
        "{{\"status\":\"{}\",\"timestamp\":\"{}\",\"origin\":\"{}\",\"origin_id\":\"{}\",\"model\":\"{}\",\
\"firmware_version\":\"{}\",\"radio\":\"{}\",\"client_version\":\"{}\"",
        if online { "online" } else { "offline" },
        iso_timestamp(now),
        origin(obs.name, obs.device_id),
        obs.device_id,
        obs.model,
        obs.firmware_version,
        obs.radio,
        obs.client_version,
    );
    if let Some(s) = stats {
        let _ = write!(
            j,
            ",\"stats\":{{\"battery_mv\":{},\"uptime_secs\":{},\"errors\":{},\"queue_len\":{},\"noise_floor\":{},\
\"tx_air_secs\":{},\"rx_air_secs\":{},\"recv_errors\":{}}}",
            s.battery_mv, s.uptime_secs, s.errors, s.queue_len, s.noise_floor, s.tx_air_secs, s.rx_air_secs, s.recv_errors
        );
    }
    j.push('}');
    j
}

fn base64url(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// JWT for LetsMesh / ommesh: Ed25519 over `b64(header).b64(payload)` with the node key,
/// signature appended as 128 uppercase hex chars (not base64url).
pub fn jwt(id: &LocalIdentity, aud: &str, iat: u32, owner: Option<&str>, email: Option<&str>) -> String {
    let mut payload = format!(
        "{{\"publicKey\":\"{}\",\"aud\":\"{aud}\",\"iat\":{iat},\"exp\":{}",
        to_hex(id.pub_key()),
        iat + TOKEN_LIFETIME_SECS
    );
    if let Some(o) = owner {
        let _ = write!(payload, ",\"owner\":\"{o}\"");
    }
    if let Some(e) = email {
        let _ = write!(payload, ",\"email\":\"{e}\"");
    }
    payload.push('}');
    let signed = format!("{}.{}", base64url(br#"{"alg":"Ed25519","typ":"JWT"}"#), base64url(payload.as_bytes()));
    let sig = id.sign(signed.as_bytes());
    format!("{signed}.{}", to_hex(&sig))
}

/// Minimal MQTT 3.1.1 encoder/decoder (what an observer needs: connect, publish, ping).
pub mod mqtt {
    use super::*;

    pub struct Will<'a> {
        pub topic: &'a str,
        pub payload: &'a [u8],
        pub qos: u8,
        pub retain: bool,
    }

    fn remaining_len(out: &mut Vec<u8>, mut n: usize) {
        loop {
            let mut b = (n % 128) as u8;
            n /= 128;
            if n > 0 {
                b |= 0x80;
            }
            out.push(b);
            if n == 0 {
                break;
            }
        }
    }

    fn string(out: &mut Vec<u8>, s: &[u8]) {
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s);
    }

    fn packet(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 5);
        out.push(kind);
        remaining_len(&mut out, body.len());
        out.extend_from_slice(body);
        out
    }

    pub fn connect(client_id: &str, user: &str, password: &str, keepalive: u16, will: Option<&Will<'_>>) -> Vec<u8> {
        let mut flags = 0x02 | 0x80 | 0x40; // clean session, username, password
        if let Some(w) = will {
            flags |= 0x04 | (w.qos & 3) << 3 | if w.retain { 0x20 } else { 0 };
        }
        let mut body = Vec::new();
        string(&mut body, b"MQTT");
        body.push(4); // protocol level 3.1.1
        body.push(flags);
        body.extend_from_slice(&keepalive.to_be_bytes());
        string(&mut body, client_id.as_bytes());
        if let Some(w) = will {
            string(&mut body, w.topic.as_bytes());
            string(&mut body, w.payload);
        }
        string(&mut body, user.as_bytes());
        string(&mut body, password.as_bytes());
        packet(0x10, &body)
    }

    /// QoS 0 has no packet id; QoS 1 needs a non-zero `id`.
    pub fn publish(topic: &str, payload: &[u8], qos: u8, retain: bool, id: u16) -> Vec<u8> {
        let mut body = Vec::with_capacity(topic.len() + payload.len() + 4);
        string(&mut body, topic.as_bytes());
        if qos > 0 {
            body.extend_from_slice(&id.to_be_bytes());
        }
        body.extend_from_slice(payload);
        packet(0x30 | (qos & 3) << 1 | retain as u8, &body)
    }

    pub fn pingreq() -> Vec<u8> {
        alloc::vec![0xC0, 0]
    }

    pub fn disconnect() -> Vec<u8> {
        alloc::vec![0xE0, 0]
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum Incoming {
        ConnAck { code: u8 },
        PubAck { id: u16 },
        PingResp,
        Other(u8),
    }

    /// Parse one control packet from the start of `buf`: `(packet, bytes consumed)`, or `None`
    /// if more bytes are needed.
    pub fn parse(buf: &[u8]) -> Option<(Incoming, usize)> {
        let kind = *buf.first()?;
        let (mut len, mut mult, mut i) = (0usize, 1usize, 1usize);
        loop {
            let b = *buf.get(i)?;
            len += (b & 0x7F) as usize * mult;
            mult *= 128;
            i += 1;
            if b & 0x80 == 0 {
                break;
            }
            if i > 4 {
                return Some((Incoming::Other(kind), buf.len())); // malformed: drop everything
            }
        }
        let body = buf.get(i..i + len)?;
        let p = match kind >> 4 {
            2 => Incoming::ConnAck { code: *body.get(1).unwrap_or(&0xFF) },
            4 => Incoming::PubAck { id: u16::from_be_bytes([*body.first().unwrap_or(&0), *body.get(1).unwrap_or(&0)]) },
            13 => Incoming::PingResp,
            k => Incoming::Other(k),
        };
        Some((p, i + len))
    }
}

/// Minimal RFC 6455 client framing for MQTT over WebSocket (`wss://host/mqtt`, subprotocol `mqtt`).
pub mod ws {
    use super::*;

    pub const OP_CONT: u8 = 0;
    pub const OP_TEXT: u8 = 1;
    pub const OP_BINARY: u8 = 2;
    pub const OP_CLOSE: u8 = 8;
    pub const OP_PING: u8 = 9;
    pub const OP_PONG: u8 = 10;

    fn base64(data: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for c in data.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
            }
        }
        out
    }

    /// HTTP upgrade request; `nonce` becomes the `Sec-WebSocket-Key`.
    pub fn upgrade_request(host: &str, path: &str, nonce: &[u8; 16]) -> String {
        format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: mqtt\r\n\r\n",
            base64(nonce)
        )
    }

    /// The server accepted the upgrade (status line `HTTP/1.1 101`). The accept hash is not
    /// checked: TLS already authenticates the server.
    pub fn upgrade_accepted(response_head: &[u8]) -> bool {
        response_head.starts_with(b"HTTP/1.1 101")
    }

    /// One client frame (FIN set, masked as clients must).
    pub fn frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 14);
        out.push(0x80 | opcode);
        match payload.len() {
            n if n < 126 => out.push(0x80 | n as u8),
            n if n <= 0xFFFF => {
                out.push(0x80 | 126);
                out.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                out.push(0x80 | 127);
                out.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        out
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Header {
        pub opcode: u8,
        pub len: u64,
        pub mask: Option<[u8; 4]>,
    }

    impl Header {
        /// Bytes still needed after the first two to complete the header.
        pub fn extra_len(b1: u8) -> usize {
            (match b1 & 0x7F {
                126 => 2,
                127 => 8,
                _ => 0,
            }) + if b1 & 0x80 != 0 { 4 } else { 0 }
        }

        /// Decode a complete header (`2 + extra_len` bytes).
        pub fn parse(h: &[u8]) -> Option<Self> {
            let (b0, b1) = (*h.first()?, *h.get(1)?);
            let (len, mut i) = match b1 & 0x7F {
                126 => (u16::from_be_bytes(h.get(2..4)?.try_into().ok()?) as u64, 4),
                127 => (u64::from_be_bytes(h.get(2..10)?.try_into().ok()?), 10),
                n => (n as u64, 2),
            };
            let mask = if b1 & 0x80 != 0 {
                let m = h.get(i..i + 4)?.try_into().ok()?;
                i += 4;
                Some(m)
            } else {
                None
            };
            (h.len() >= i).then_some(Self { opcode: b0 & 0x0F, len, mask })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::*;

    fn observer<'a>(id: &'a str) -> Observer<'a> {
        Observer {
            name: "PKIO \"x\"",
            device_id: id,
            model: "Heltec V4 OLED",
            firmware_version: "v1.18.0-rs",
            client_version: "rs-1",
            radio: "868.731018,62.5,7,7",
        }
    }

    #[test]
    fn packet_message_format() {
        let mut p = Packet::with_type(PAYLOAD_TYPE_TXT_MSG);
        p.set_route_type(ROUTE_TYPE_DIRECT);
        p.set_path(3, &[0xAA, 0xBB, 0xCC]);
        p.set_payload(&[1, 2, 3, 4]);
        let mut raw = [0u8; 255];
        let n = p.write_to(&mut raw).unwrap();
        let id = "AB".repeat(32);
        let j = packet_json(&observer(&id), 1_792_670_096, false, &p, &raw[..n], 7.5, -92.4, Some((812, 330)));
        let hash = to_hex(&p.calculate_hash());
        assert_eq!(
            j,
            format!(
                "{{\"origin\":\"PKIO \\\"x\\\"\",\"origin_id\":\"{id}\",\"timestamp\":\"2026-10-22T11:54:56.000000\",\
\"type\":\"PACKET\",\"direction\":\"rx\",\"time\":\"11:54:56\",\"date\":\"22/10/2026\",\"len\":\"9\",\"packet_type\":\"2\",\
\"route\":\"D\",\"payload_len\":\"4\",\"raw\":\"0A03AABBCC01020304\",\"SNR\":\"7.5\",\"RSSI\":\"-92\",\"score\":\"812\",\
\"duration\":\"330\",\"hash\":\"{hash}\",\"path\":\"path_3x1_3b\"}}"
            )
        );
    }

    #[test]
    fn status_message_format() {
        let id = "CD".repeat(32);
        let j = status_json(&observer(&id), 1_792_670_096, false, None);
        assert!(j.starts_with("{\"status\":\"offline\",\"timestamp\":\"2026-10-22T11:54:56.000000\""));
        assert!(j.ends_with("\"client_version\":\"rs-1\"}"));
        let s = StatusStats { battery_mv: 4100, noise_floor: -112, ..Default::default() };
        let j = status_json(&observer(&id), 0, true, Some(&s));
        assert!(j.contains(",\"stats\":{\"battery_mv\":4100,\"uptime_secs\":0,\"errors\":0,\"queue_len\":0,\"noise_floor\":-112,"));
        assert_eq!(radio_string(868.731, 62.5, 7, 7), "868.731018,62.5,7,7");
        assert_eq!(sanitize_iata("oms!long"), "OMS_LON");
    }

    #[test]
    fn jwt_verifies_with_the_node_key() {
        let id = LocalIdentity::from_seed(&[9; 32]);
        let t = jwt(&id, "agent.ommesh.ru", 1_792_670_000, None, None);
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts[0], "eyJhbGciOiJFZDI1NTE5IiwidHlwIjoiSldUIn0");
        assert_eq!(parts[2].len(), 128);
        let mut sig = [0u8; 64];
        assert!(crate::util::from_hex(&mut sig, parts[2]));
        assert!(id.id.verify(&sig, format!("{}.{}", parts[0], parts[1]).as_bytes()));
        // payload decodes back to the expected JSON
        let b64 = parts[1];
        assert_eq!(base64url(format!("{{\"publicKey\":\"{}\",\"aud\":\"agent.ommesh.ru\",\"iat\":1792670000,\"exp\":1792673600}}", to_hex(id.pub_key())).as_bytes()), b64);
    }

    #[test]
    fn mqtt_codec() {
        let c = mqtt::connect("mqtt_x", "meshcore", "meshcore", 30, Some(&mqtt::Will { topic: "t", payload: b"bye", qos: 1, retain: true }));
        assert_eq!(&c[..2], &[0x10, (c.len() - 2) as u8]);
        assert_eq!(c[8], 4);
        assert_eq!(c[9], 0x02 | 0x80 | 0x40 | 0x04 | 0x08 | 0x20);
        let p = mqtt::publish("a/b", b"hi", 1, true, 7);
        assert_eq!(p, [0x33, 9, 0, 3, b'a', b'/', b'b', 0, 7, b'h', b'i']);
        let big = mqtt::publish("t", &[0u8; 200], 0, false, 0);
        assert_eq!(&big[..3], &[0x30, 0xCB, 0x01]); // 203 = 0x4B | 0x80, 1
        assert_eq!(mqtt::parse(&[0x20, 2, 0, 0, 0xD0]), Some((mqtt::Incoming::ConnAck { code: 0 }, 4)));
        assert_eq!(mqtt::parse(&[0x40, 2, 0, 7]), Some((mqtt::Incoming::PubAck { id: 7 }, 4)));
        assert_eq!(mqtt::parse(&[0xD0, 0]), Some((mqtt::Incoming::PingResp, 2)));
        assert_eq!(mqtt::parse(&[0x40, 2, 0]), None);
    }

    #[test]
    fn raw_message_format() {
        let id = "AB".repeat(32);
        let j = raw_json(&observer(&id), 1_792_670_096, &[0x0A, 0x00, 0xFF]);
        assert_eq!(
            j,
            format!(
                "{{\"origin\":\"PKIO \\\"x\\\"\",\"origin_id\":\"{id}\",\"timestamp\":\"{}\",\"type\":\"RAW\",\"data\":\"0A00FF\"}}",
                iso_timestamp(1_792_670_096)
            )
        );
    }

    #[test]
    fn websocket_framing() {
        // RFC 6455 4.1 example nonce
        let nonce: [u8; 16] = *b"the sample nonce";
        let req = ws::upgrade_request("agent.ommesh.ru", "/mqtt", &nonce);
        assert!(req.starts_with("GET /mqtt HTTP/1.1\r\nHost: agent.ommesh.ru\r\n"));
        assert!(req.contains("Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"));
        assert!(req.ends_with("Sec-WebSocket-Protocol: mqtt\r\n\r\n"));
        assert!(ws::upgrade_accepted(b"HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(!ws::upgrade_accepted(b"HTTP/1.1 400 Bad Request\r\n"));

        // RFC 6455 5.7: masked "Hello"
        let f = ws::frame(ws::OP_TEXT, b"Hello", [0x37, 0xfa, 0x21, 0x3d]);
        assert_eq!(f, [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58]);
        let h = ws::Header::parse(&f[..2 + ws::Header::extra_len(f[1])]).unwrap();
        assert_eq!(h, ws::Header { opcode: ws::OP_TEXT, len: 5, mask: Some([0x37, 0xfa, 0x21, 0x3d]) });

        let big = ws::frame(ws::OP_BINARY, &[0u8; 300], [0; 4]);
        assert_eq!(&big[..4], &[0x82, 0xFE, 0x01, 0x2C]);
        assert_eq!(ws::Header::extra_len(big[1]), 6);
        // unmasked server frame with a 16-bit length
        assert_eq!(ws::Header::parse(&[0x82, 0x7E, 0x01, 0x00]), Some(ws::Header { opcode: 2, len: 256, mask: None }));
        assert_eq!(ws::Header::parse(&[0x82, 0x7E, 0x01]), None);
    }
}
