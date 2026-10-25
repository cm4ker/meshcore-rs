//! Companion command frames (`MyMesh::handleCmdFrame`) and the text CLI (`handleCommand`).

use alloc::format;

use super::*;
use crate::util::{atof, atoi, atoi_signed, ftoa};
use crate::ota::{OtaEndpoint, OtaOutcome};
use crate::{FIRMWARE_VERSION, MAX_GROUP_DATA_LENGTH, SIGNATURE_SIZE};

const MAX_SIGN_DATA_LEN: usize = 8 * 1024;

/// `StrHelper::ftoa3`: three decimals, trailing zeros trimmed.
fn ftoa3(v: f32) -> String {
    ftoa(v as f64, 3)
}

impl Companion {
    /// Handle one command frame from the app; replies are queued for [`Self::take_frame`].
    pub fn handle_frame(&mut self, ctx: &mut MeshCtx, frame: &[u8]) {
        let Some(&code) = frame.first() else {
            return;
        };
        let len = frame.len();
        let at = |i: usize| frame.get(i).copied().unwrap_or(0);
        let u32_at = |i: usize| read_u32(frame, i);
        let key_at = |i: usize| frame.get(i..i + PUB_KEY_SIZE);

        match code {
            // firmware update (this firmware's extension), same requests as over LoRa
            code if OtaEndpoint::is_request(code) => {
                let (reply, outcome) = self.ota.handle(frame, FIRMWARE_VERSION);
                if let Some(body) = reply {
                    let mut f = alloc::vec![code];
                    f.extend_from_slice(&body);
                    self.write_frame(&f);
                }
                if outcome == OtaOutcome::Committed {
                    self.effects.reboot = true;
                }
            }
            CMD_DEVICE_QUERY if len >= 2 => {
                self.app_target_ver = frame[1];
                let mut f = alloc::vec![RESP_CODE_DEVICE_INFO, FIRMWARE_VER_CODE, (MAX_CONTACTS / 2) as u8, MAX_GROUP_CHANNELS as u8];
                f.extend_from_slice(&self.prefs.ble_pin.to_le_bytes());
                push_padded(&mut f, self.board.build_date.as_bytes(), 12);
                push_padded(&mut f, self.board.manufacturer.as_bytes(), 40);
                push_padded(&mut f, FIRMWARE_VERSION.as_bytes(), 20);
                f.push(self.prefs.is_repeat_enabled() as u8);
                f.push(self.prefs.path_hash_mode);
                self.write_frame(&f);
            }
            CMD_APP_START if len >= 8 => {
                self.iter = None; // stop any left-over contacts iterator
                let p = &self.prefs;
                let mut f = alloc::vec![RESP_CODE_SELF_INFO, ADV_TYPE_CHAT, p.tx_power_dbm as u8, self.board.max_tx_power_dbm as u8];
                f.extend_from_slice(ctx.self_id.pub_key());
                f.extend_from_slice(&((p.node_lat * 1e6) as i32).to_le_bytes());
                f.extend_from_slice(&((p.node_lon * 1e6) as i32).to_le_bytes());
                f.push(p.multi_acks);
                f.push(p.advert_loc_policy);
                f.push((p.telemetry_mode_env << 4) | (p.telemetry_mode_loc << 2) | p.telemetry_mode_base);
                f.push(p.manual_add_contacts);
                f.extend_from_slice(&((p.radio.freq_mhz * 1000.0) as u32).to_le_bytes());
                f.extend_from_slice(&((p.radio.bw_khz * 1000.0) as u32).to_le_bytes());
                f.push(p.radio.sf);
                f.push(p.radio.cr);
                f.extend_from_slice(p.node_name.as_bytes());
                self.write_frame(&f);
            }
            CMD_RUN_CLI_COMMAND if len >= 3 => {
                let text = String::from_utf8_lossy(c_str(&frame[1..])).into_owned();
                let reply = self.handle_command(ctx, &text);
                let mut f = alloc::vec![RESP_CODE_CLI_REPLY];
                f.extend_from_slice(reply.as_bytes());
                f.truncate(MAX_FRAME_SIZE);
                self.write_frame(&f);
            }
            CMD_SEND_TXT_MSG if len >= 14 => {
                let txt_type = frame[1];
                let attempt = frame[2];
                let mut msg_timestamp = u32_at(3);
                let recipient = self.lookup_contact(&frame[7..13]);
                match recipient {
                    Some(idx) if matches!(txt_type, TXT_TYPE_PLAIN | TXT_TYPE_CLI_DATA | TXT_TYPE_CLI_COMMAND) => {
                        let text = c_str(&frame[13..]);
                        let result = if txt_type == TXT_TYPE_PLAIN {
                            self.send_message(ctx, idx, msg_timestamp, attempt, text)
                        } else {
                            // the node's clock, so replay protection on the other side holds
                            msg_timestamp = ctx.current_time_unique();
                            self.send_command_data(ctx, idx, msg_timestamp, attempt, txt_type, text)
                                .map(|(sent, est)| (sent, 0, est))
                        };
                        match result {
                            None => self.write_err(ERR_CODE_TABLE_FULL),
                            Some((sent, expected_ack, est_timeout)) => {
                                if expected_ack != 0 {
                                    self.expected_acks[self.next_ack_idx] = Some(AckEntry {
                                        msg_sent: ctx.now,
                                        ack: expected_ack,
                                        contact: self.contacts[idx].id.pub_key,
                                    });
                                    self.next_ack_idx = (self.next_ack_idx + 1) % EXPECTED_ACK_TABLE_SIZE;
                                }
                                self.write_sent(sent, expected_ack, est_timeout);
                            }
                        }
                    }
                    Some(_) => self.write_err(ERR_CODE_UNSUPPORTED_CMD),
                    None => self.write_err(ERR_CODE_NOT_FOUND),
                }
            }
            CMD_SEND_CHANNEL_TXT_MSG if len >= 7 => {
                let txt_type = frame[1];
                let idx = frame[2] as usize;
                let ts = u32_at(3);
                if txt_type != TXT_TYPE_PLAIN {
                    self.write_err(ERR_CODE_UNSUPPORTED_CMD);
                } else if idx < MAX_GROUP_CHANNELS && self.send_group_message(ctx, ts, idx, &frame[7..]) {
                    self.write_ok();
                } else {
                    self.write_err(ERR_CODE_NOT_FOUND);
                }
            }
            CMD_SEND_CHANNEL_DATA => {
                if len < 4 {
                    return self.write_err(ERR_CODE_ILLEGAL_ARG);
                }
                let idx = frame[1] as usize;
                let path_len = frame[2];
                if !Packet::is_valid_path_len(path_len) && path_len != OUT_PATH_UNKNOWN {
                    return self.write_err(ERR_CODE_ILLEGAL_ARG);
                }
                let mut i = 3;
                let mut path = [0u8; MAX_PATH_SIZE];
                if path_len != OUT_PATH_UNKNOWN {
                    let n = Packet::path_bytes_for(path_len);
                    for (k, b) in path.iter_mut().enumerate().take(n) {
                        *b = at(i + k);
                    }
                    i += n;
                }
                let data_type = at(i) as u16 | (at(i + 1) as u16) << 8;
                i += 2;
                let payload = frame.get(i..).unwrap_or(&[]);
                if idx >= MAX_GROUP_CHANNELS {
                    self.write_err(ERR_CODE_NOT_FOUND);
                } else if data_type == 0 || payload.len() > MAX_GROUP_DATA_LENGTH {
                    self.write_err(ERR_CODE_ILLEGAL_ARG); // type 0 is reserved
                } else if self.send_group_data(ctx, idx, path_len, &path, data_type, payload) {
                    self.write_ok();
                } else {
                    self.write_err(ERR_CODE_TABLE_FULL);
                }
            }
            CMD_GET_CONTACTS => {
                if self.iter.is_some() {
                    return self.write_err(ERR_CODE_BAD_STATE);
                }
                let since = if len >= 5 { u32_at(1) } else { 0 };
                let mut f = alloc::vec![RESP_CODE_CONTACTS_START];
                f.extend_from_slice(&(self.num_contacts() as u32).to_le_bytes()); // total, not filtered
                self.write_frame(&f);
                self.iter = Some((MAX_ANON_CONTACTS, since, 0));
            }
            CMD_SET_ADVERT_NAME if len >= 2 => {
                let name = String::from_utf8_lossy(c_str(&frame[1..])).into_owned();
                self.prefs.node_name = crate::util::truncate_str(&name, crate::prefs::NAME_MAX);
                self.effects.save_prefs = true;
                self.write_ok();
            }
            CMD_SET_ADVERT_LATLON if len >= 9 => {
                let lat = u32_at(1) as i32;
                let lon = u32_at(5) as i32;
                if (-90_000_000..=90_000_000).contains(&lat) && (-180_000_000..=180_000_000).contains(&lon) {
                    self.prefs.node_lat = lat as f64 / 1e6;
                    self.prefs.node_lon = lon as f64 / 1e6;
                    self.effects.save_prefs = true;
                    self.write_ok();
                } else {
                    self.write_err(ERR_CODE_ILLEGAL_ARG);
                }
            }
            CMD_GET_DEVICE_TIME => {
                let mut f = alloc::vec![RESP_CODE_CURR_TIME];
                f.extend_from_slice(&ctx.current_time().to_le_bytes());
                self.write_frame(&f);
            }
            CMD_SET_DEVICE_TIME if len >= 5 => {
                let secs = u32_at(1);
                if secs >= ctx.current_time() {
                    ctx.set_current_time(secs);
                    self.write_ok();
                } else {
                    self.write_err(ERR_CODE_ILLEGAL_ARG);
                }
            }
            CMD_SEND_SELF_ADVERT => match self.self_advert(ctx) {
                Some(pkt) => {
                    if len >= 2 && frame[1] == 1 {
                        let key = TransportKey(self.prefs.default_scope_key);
                        self.send_flood_with_key(ctx, key, pkt, 0);
                    } else {
                        ctx.send_zero_hop(pkt, 0);
                    }
                    self.write_ok();
                }
                None => self.write_err(ERR_CODE_TABLE_FULL),
            },
            CMD_RESET_PATH if len > PUB_KEY_SIZE => match self.lookup_contact(&frame[1..33]) {
                Some(idx) => {
                    self.contacts[idx].out_path_len = OUT_PATH_UNKNOWN;
                    self.contacts[idx].rt_pending = false;
                    self.mark_contacts_dirty(ctx);
                    self.write_ok();
                }
                None => self.write_err(ERR_CODE_NOT_FOUND),
            },
            CMD_ADD_UPDATE_CONTACT if len > 1 + 32 + 2 => {
                let mut last_mod = ctx.current_time(); // if not in the frame
                match self.lookup_contact(&frame[1..33]) {
                    Some(idx) => {
                        let before = (self.contacts[idx].out_path_len, self.contacts[idx].out_path);
                        update_contact_from_frame(&mut self.contacts[idx], &mut last_mod, frame);
                        let c = &mut self.contacts[idx];
                        c.lastmod = last_mod;
                        // a path the app set (or changed): the contact gets it reversed
                        if (c.out_path_len, c.out_path) != before && c.has_out_path() {
                            c.rt_pending = true;
                        }
                        self.mark_contacts_dirty(ctx);
                        self.write_ok();
                    }
                    None => {
                        let mut c = ContactInfo::new(Identity::from_slice(&frame[1..33]).unwrap());
                        update_contact_from_frame(&mut c, &mut last_mod, frame);
                        c.lastmod = last_mod;
                        c.sync_since = 0;
                        c.rt_pending = c.has_out_path();
                        if self.add_contact(c) {
                            self.mark_contacts_dirty(ctx);
                            self.write_ok();
                        } else {
                            self.write_err(ERR_CODE_TABLE_FULL);
                        }
                    }
                }
            }
            CMD_REMOVE_CONTACT => match key_at(1).and_then(|k| self.lookup_contact(k)) {
                Some(idx) => {
                    let key = self.contacts[idx].id.pub_key;
                    self.remove_contact(idx);
                    self.blobs.delete(&key);
                    self.mark_contacts_dirty(ctx);
                    self.write_ok();
                }
                None => self.write_err(ERR_CODE_NOT_FOUND),
            },
            CMD_SHARE_CONTACT => match key_at(1).and_then(|k| self.lookup_contact(k)) {
                Some(idx) => {
                    if self.share_contact_zero_hop(ctx, idx) {
                        self.write_ok();
                    } else {
                        self.write_err(ERR_CODE_TABLE_FULL);
                    }
                }
                None => self.write_err(ERR_CODE_NOT_FOUND),
            },
            CMD_GET_CONTACT_BY_KEY => match key_at(1).and_then(|k| self.lookup_contact(k)) {
                Some(idx) => {
                    let f = Self::contact_frame(RESP_CODE_CONTACT, &self.contacts[idx]);
                    self.write_frame(&f);
                }
                None => self.write_err(ERR_CODE_NOT_FOUND),
            },
            CMD_EXPORT_CONTACT => {
                if len < 1 + PUB_KEY_SIZE {
                    // export SELF, as it would be flooded
                    match self.self_advert(ctx) {
                        Some(mut pkt) => {
                            pkt.set_route_type(ROUTE_TYPE_FLOOD);
                            let mut raw = [0u8; MAX_TRANS_UNIT];
                            let n = pkt.write_to(&mut raw).unwrap_or(0);
                            let mut f = alloc::vec![RESP_CODE_EXPORT_CONTACT];
                            f.extend_from_slice(&raw[..n]);
                            self.write_frame(&f);
                        }
                        None => self.write_err(ERR_CODE_TABLE_FULL),
                    }
                } else {
                    let blob = self.lookup_contact(&frame[1..33]).and_then(|i| self.blobs.get(&self.contacts[i].id.pub_key));
                    match blob {
                        Some(raw) if !raw.is_empty() => {
                            let mut f = alloc::vec![RESP_CODE_EXPORT_CONTACT];
                            f.extend_from_slice(&raw);
                            self.write_frame(&f);
                        }
                        _ => self.write_err(ERR_CODE_NOT_FOUND),
                    }
                }
            }
            CMD_IMPORT_CONTACT if len > 2 + 32 + 64 => {
                if self.import_contact(ctx, &frame[1..]) {
                    self.write_ok();
                } else {
                    self.write_err(ERR_CODE_ILLEGAL_ARG);
                }
            }
            CMD_SYNC_NEXT_MESSAGE => match self.offline_queue.pop_front() {
                Some(f) => self.write_frame(&f),
                None => self.write_frame(&[RESP_CODE_NO_MORE_MESSAGES]),
            },
            CMD_SET_RADIO_PARAMS => {
                let freq = u32_at(1);
                let bw = u32_at(5);
                let sf = at(9);
                let cr = at(10);
                let repeat = len > 11 && frame[11] != 0;
                let valid_repeat = REPEAT_FREQ_RANGES.iter().any(|&(lo, hi)| (lo..=hi).contains(&freq));
                if repeat && !valid_repeat {
                    self.write_err(ERR_CODE_ILLEGAL_ARG);
                } else if (150_000..=2_500_000).contains(&freq)
                    && (5..=12).contains(&sf)
                    && (5..=8).contains(&cr)
                    && (7000..=500_000).contains(&bw)
                {
                    let p = &mut self.prefs;
                    p.radio = RadioParams { freq_mhz: freq as f32 / 1000.0, bw_khz: bw as f32 / 1000.0, sf, cr };
                    p.disable_fwd = !repeat;
                    self.effects.save_prefs = true;
                    self.effects.apply_radio = Some(p.radio);
                    self.write_ok();
                } else {
                    self.write_err(ERR_CODE_ILLEGAL_ARG);
                }
            }
            CMD_SET_RADIO_TX_POWER if len >= 2 => {
                let power = frame[1] as i8;
                if power < -9 || power > self.board.max_tx_power_dbm {
                    self.write_err(ERR_CODE_ILLEGAL_ARG);
                } else {
                    self.prefs.tx_power_dbm = power;
                    self.effects.save_prefs = true;
                    self.effects.tx_power = Some(power);
                    self.write_ok();
                }
            }
            CMD_SET_TUNING_PARAMS if len >= 9 => {
                self.prefs.rx_delay_base = u32_at(1) as f32 / 1000.0;
                self.prefs.airtime_factor = u32_at(5) as f32 / 1000.0;
                self.effects.save_prefs = true;
                self.write_ok();
            }
            CMD_GET_TUNING_PARAMS => {
                let mut f = alloc::vec![RESP_CODE_TUNING_PARAMS];
                f.extend_from_slice(&((self.prefs.rx_delay_base * 1000.0) as u32).to_le_bytes());
                f.extend_from_slice(&((self.prefs.airtime_factor * 1000.0) as u32).to_le_bytes());
                self.write_frame(&f);
            }
            CMD_SET_OTHER_PARAMS if len >= 2 => {
                let p = &mut self.prefs;
                p.manual_add_contacts = frame[1];
                if len >= 3 {
                    p.telemetry_mode_base = frame[2] & 0x03;
                    p.telemetry_mode_loc = (frame[2] >> 2) & 0x03;
                    p.telemetry_mode_env = (frame[2] >> 4) & 0x03;
                    if len >= 4 {
                        p.advert_loc_policy = frame[3];
                        if len >= 5 {
                            p.multi_acks = frame[4];
                        }
                    }
                }
                self.effects.save_prefs = true;
                self.write_ok();
            }
            CMD_SET_PATH_HASH_MODE if len >= 3 && frame[1] == 0 => {
                if frame[2] >= 3 {
                    self.write_err(ERR_CODE_ILLEGAL_ARG);
                } else {
                    self.prefs.path_hash_mode = frame[2];
                    self.effects.save_prefs = true;
                    self.write_ok();
                }
            }
            CMD_REBOOT if frame[1..].starts_with(b"reboot") => {
                if self.dirty_contacts_expiry != 0 {
                    self.effects.save_contacts = true;
                    self.dirty_contacts_expiry = 0;
                }
                self.effects.reboot = true;
            }
            CMD_GET_BATT_AND_STORAGE => {
                let (used, total) = self.blobs.usage_kb();
                let mut f = alloc::vec![RESP_CODE_BATT_AND_STORAGE];
                f.extend_from_slice(&self.board.batt_mv.to_le_bytes());
                f.extend_from_slice(&used.to_le_bytes());
                f.extend_from_slice(&total.to_le_bytes());
                self.write_frame(&f);
            }
            CMD_EXPORT_PRIVATE_KEY => {
                let mut f = alloc::vec![RESP_CODE_PRIVATE_KEY];
                f.extend_from_slice(ctx.self_id.private_key());
                self.write_frame(&f);
            }
            CMD_IMPORT_PRIVATE_KEY if len > PRV_KEY_SIZE => {
                let prv: [u8; PRV_KEY_SIZE] = frame[1..65].try_into().unwrap();
                if !crate::identity::LocalIdentity::validate_private_key(&prv) {
                    self.write_err(ERR_CODE_ILLEGAL_ARG);
                } else {
                    ctx.self_id = crate::identity::LocalIdentity::from_private_key(&prv);
                    self.effects.save_identity = Some(prv);
                    for c in self.contacts.iter_mut() {
                        c.secret = None; // ECDH secrets depend on our key
                    }
                    self.write_ok();
                }
            }
            CMD_SEND_RAW_DATA if len >= 6 => {
                let path_len = frame[1];
                if !Packet::is_valid_path_len(path_len) {
                    return self.write_err(ERR_CODE_UNSUPPORTED_CMD); // flood not supported (yet)
                }
                let i = 2 + Packet::path_bytes_for(path_len);
                if i + 4 > len {
                    return self.write_err(ERR_CODE_ILLEGAL_ARG); // min payload 4 bytes
                }
                let path = frame[2..i].to_vec();
                match ctx.create_raw_data(&frame[i..]) {
                    Some(pkt) => {
                        ctx.send_direct(pkt, path_len, &path, 0);
                        self.write_ok();
                    }
                    None => self.write_err(ERR_CODE_TABLE_FULL),
                }
            }
            CMD_SEND_LOGIN if len > PUB_KEY_SIZE => match self.lookup_contact(&frame[1..33]) {
                Some(idx) => {
                    let password = c_str(&frame[33..]);
                    match self.send_login(ctx, idx, password) {
                        Some((sent, est)) => {
                            self.clear_pending_reqs();
                            self.pending_login = read_u32(&self.contacts[idx].id.pub_key, 0);
                            self.write_sent(sent, self.pending_login, est);
                        }
                        None => self.write_err(ERR_CODE_TABLE_FULL),
                    }
                }
                None => self.write_err(ERR_CODE_NOT_FOUND),
            },
            CMD_SEND_ANON_REQ if len > 1 + PUB_KEY_SIZE => {
                let mut recipient = self.lookup_contact(&frame[1..33]);
                if recipient.is_none() {
                    // requests to non-contacts go through a transient entry, zero-hop by default
                    let mut anon = ContactInfo::new(Identity::from_slice(&frame[1..33]).unwrap());
                    anon.out_path_len = 0;
                    anon.lastmod = ctx.current_time();
                    if self.add_contact(anon) {
                        recipient = self.lookup_contact(&frame[1..33]);
                    }
                }
                match recipient {
                    Some(idx) => match self.send_anon_req(ctx, idx, &frame[33..]) {
                        Some((sent, tag, est)) => {
                            self.clear_pending_reqs();
                            self.pending_req = tag;
                            self.write_sent(sent, tag, est);
                        }
                        None => self.write_err(ERR_CODE_TABLE_FULL),
                    },
                    None => self.write_err(ERR_CODE_TABLE_FULL), // contacts full
                }
            }
            CMD_SEND_STATUS_REQ if len > PUB_KEY_SIZE => match self.lookup_contact(&frame[1..33]) {
                Some(idx) => match self.send_request_type(ctx, idx, REQ_TYPE_GET_STATUS) {
                    Some((sent, tag, est)) => {
                        self.clear_pending_reqs();
                        self.pending_status = read_u32(&self.contacts[idx].id.pub_key, 0); // legacy matching
                        self.write_sent(sent, tag, est);
                    }
                    None => self.write_err(ERR_CODE_TABLE_FULL),
                },
                None => self.write_err(ERR_CODE_NOT_FOUND),
            },
            CMD_SEND_PATH_DISCOVERY_REQ if len >= 2 + PUB_KEY_SIZE && frame[1] == 0 => {
                match self.lookup_contact(&frame[2..34]) {
                    Some(idx) => {
                        // a flooded telemetry request for BASE data only
                        let mut req = [REQ_TYPE_GET_TELEMETRY_DATA, !TELEM_PERM_BASE, 0, 0, 0, 0, 0, 0, 0];
                        ctx.rng.fill(&mut req[5..]);
                        let saved = self.contacts[idx].out_path_len;
                        self.contacts[idx].out_path_len = OUT_PATH_UNKNOWN;
                        let result = self.send_request(ctx, idx, &req);
                        self.contacts[idx].out_path_len = saved;
                        match result {
                            Some((sent, tag, est)) => {
                                self.clear_pending_reqs();
                                self.pending_discovery = tag;
                                self.write_sent(sent, tag, est);
                            }
                            None => self.write_err(ERR_CODE_TABLE_FULL),
                        }
                    }
                    None => self.write_err(ERR_CODE_NOT_FOUND),
                }
            }
            CMD_SEND_TELEMETRY_REQ if len >= 4 + PUB_KEY_SIZE => match self.lookup_contact(&frame[4..36]) {
                Some(idx) => match self.send_request_type(ctx, idx, REQ_TYPE_GET_TELEMETRY_DATA) {
                    Some((sent, tag, est)) => {
                        self.clear_pending_reqs();
                        self.pending_telemetry = tag;
                        self.write_sent(sent, tag, est);
                    }
                    None => self.write_err(ERR_CODE_TABLE_FULL),
                },
                None => self.write_err(ERR_CODE_NOT_FOUND),
            },
            CMD_SEND_TELEMETRY_REQ if len == 4 => {
                // our own telemetry
                let mut f = alloc::vec![PUSH_CODE_TELEMETRY_RESPONSE, 0];
                f.extend_from_slice(&ctx.self_id.pub_key()[..6]);
                self.lpp_voltage(&mut f);
                self.lpp_temperature(&mut f);
                self.lpp_sensors(&mut f, 0xFF);
                self.write_frame(&f);
            }
            CMD_SEND_BINARY_REQ if len >= 2 + PUB_KEY_SIZE => match self.lookup_contact(&frame[1..33]) {
                Some(idx) => match self.send_request(ctx, idx, &frame[33..]) {
                    Some((sent, tag, est)) => {
                        self.clear_pending_reqs();
                        self.pending_req = tag;
                        self.write_sent(sent, tag, est);
                    }
                    None => self.write_err(ERR_CODE_TABLE_FULL),
                },
                None => self.write_err(ERR_CODE_NOT_FOUND),
            },
            CMD_HAS_CONNECTION if len > PUB_KEY_SIZE => {
                if self.has_connection_to(&frame[1..33]) {
                    self.write_ok();
                } else {
                    self.write_err(ERR_CODE_NOT_FOUND);
                }
            }
            CMD_LOGOUT if len > PUB_KEY_SIZE => {
                self.stop_connection(&frame[1..33]);
                self.write_ok();
            }
            CMD_GET_CHANNEL if len >= 2 => {
                let idx = frame[1] as usize;
                match self.channels.get(idx) {
                    Some(ch) => {
                        let mut f = alloc::vec![RESP_CODE_CHANNEL_INFO, idx as u8];
                        push_padded(&mut f, ch.name.as_bytes(), 32);
                        f.extend_from_slice(&ch.secret[..16]); // only 128-bit keys
                        self.write_frame(&f);
                    }
                    None => self.write_err(ERR_CODE_NOT_FOUND),
                }
            }
            CMD_SET_CHANNEL if len >= 2 + 32 + 32 => self.write_err(ERR_CODE_UNSUPPORTED_CMD), // 256-bit keys
            CMD_SET_CHANNEL if len >= 2 + 32 + 16 => {
                let idx = frame[1] as usize;
                if idx < MAX_GROUP_CHANNELS {
                    let name = String::from_utf8_lossy(c_str(&frame[2..34])).into_owned();
                    self.channels[idx] = Channel::new(&name, &frame[34..50]);
                    self.effects.save_channels = true;
                    self.write_ok();
                } else {
                    self.write_err(ERR_CODE_NOT_FOUND);
                }
            }
            CMD_SIGN_START => {
                let mut f = alloc::vec![RESP_CODE_SIGN_START, 0];
                f.extend_from_slice(&(MAX_SIGN_DATA_LEN as u32).to_le_bytes());
                self.write_frame(&f);
                self.sign_data = Some(Vec::new());
            }
            CMD_SIGN_DATA if len > 1 => match self.sign_data.as_mut() {
                None => self.write_err(ERR_CODE_BAD_STATE),
                Some(d) if d.len() + len - 1 > MAX_SIGN_DATA_LEN => self.write_err(ERR_CODE_TABLE_FULL),
                Some(d) => {
                    d.extend_from_slice(&frame[1..]);
                    self.write_ok();
                }
            },
            CMD_SIGN_FINISH => match self.sign_data.take() {
                Some(d) => {
                    let mut f = alloc::vec![RESP_CODE_SIGNATURE];
                    f.extend_from_slice(&ctx.self_id.sign(&d));
                    debug_assert_eq!(f.len(), 1 + SIGNATURE_SIZE);
                    self.write_frame(&f);
                }
                None => self.write_err(ERR_CODE_BAD_STATE),
            },
            CMD_SEND_TRACE_PATH if len > 10 && len - 10 < MAX_PACKET_PAYLOAD - 5 => {
                let path_len = len - 10;
                let flags = frame[9];
                let path_sz = flags & 0x03;
                if (path_len >> path_sz) > MAX_PATH_SIZE || !path_len.is_multiple_of(1 << path_sz) {
                    return self.write_err(ERR_CODE_ILLEGAL_ARG);
                }
                let tag = u32_at(1);
                let pkt = ctx.create_trace(tag, u32_at(5), flags);
                let t = ctx.radio.est_airtime_ms(pkt.payload_len + path_len + 2);
                ctx.send_direct(pkt, path_len as u8, &frame[10..], 0);
                let est = Self::calc_direct_timeout(t, (path_len >> path_sz) as u8);
                self.write_sent(Sent::Direct, tag, est);
            }
            CMD_SET_DEVICE_PIN if len >= 5 => {
                let pin = u32_at(1);
                if pin == 0 || (100_000..=999_999).contains(&pin) {
                    self.prefs.ble_pin = pin;
                    self.effects.save_prefs = true;
                    self.write_ok();
                } else {
                    self.write_err(ERR_CODE_ILLEGAL_ARG);
                }
            }
            CMD_GET_CUSTOM_VARS => {
                // the GPS's settings, and the root keys whose signed releases are installed
                // (fingerprints separated by spaces: the list itself is comma-separated)
                let mut vars = alloc::vec::Vec::new();
                if self.board.has_gps {
                    vars.push(String::from(if self.prefs.gps_enabled { "gps:1" } else { "gps:0" }));
                }
                vars.push(match self.prefs.ota_roots.is_empty() {
                    true => String::from("ota_root:off"),
                    false => alloc::format!("ota_root:{}", release::join_fingerprints(&self.prefs.ota_roots, " ")),
                });
                let mut f = alloc::vec![RESP_CODE_CUSTOM_VARS];
                f.extend_from_slice(vars.join(",").as_bytes());
                self.write_frame(&f);
            }
            CMD_SET_CUSTOM_VAR if len >= 4 => {
                let var = &frame[1..];
                let (name, value) = match var.iter().position(|&c| c == b':') {
                    Some(sep) => (&var[..sep], &var[sep + 1..]),
                    None => (var, &[][..]),
                };
                match name {
                    b"gps" if self.board.has_gps => {
                        self.prefs.gps_enabled = value.first() == Some(&b'1');
                        self.effects.gps = true;
                        self.effects.save_prefs = true;
                        self.write_ok();
                    }
                    b"gps_interval" if self.board.has_gps => {
                        let secs = core::str::from_utf8(value).map_or(0, crate::util::atoi);
                        self.prefs.gps_interval = secs.min(86_400);
                        self.effects.save_prefs = true;
                        self.write_ok();
                    }
                    b"ota_root" => {
                        let text = core::str::from_utf8(value).unwrap_or("?").trim();
                        let roots = if text == "off" { Some(alloc::vec::Vec::new()) } else { release::parse_fingerprints(text) };
                        match roots {
                            Some(roots) => {
                                self.ota.roots = roots.clone();
                                self.prefs.ota_roots = roots;
                                self.effects.save_prefs = true;
                                self.write_ok();
                            }
                            None => self.write_err(ERR_CODE_ILLEGAL_ARG),
                        }
                    }
                    _ => self.write_err(ERR_CODE_ILLEGAL_ARG),
                }
            }
            CMD_GET_ADVERT_PATH if len >= PUB_KEY_SIZE + 2 => {
                let key = &frame[2..];
                match self.advert_paths.iter().find(|p| key.starts_with(&p.pubkey_prefix)) {
                    Some(p) => {
                        let mut f = alloc::vec![RESP_CODE_ADVERT_PATH];
                        f.extend_from_slice(&p.recv_timestamp.to_le_bytes());
                        f.push(p.path_len);
                        f.extend_from_slice(&p.path);
                        self.write_frame(&f);
                    }
                    None => self.write_err(ERR_CODE_NOT_FOUND),
                }
            }
            CMD_GET_STATS if len >= 2 => {
                let s = &ctx.stats;
                let mut f = alloc::vec![RESP_CODE_STATS, frame[1]];
                match frame[1] {
                    STATS_TYPE_CORE => {
                        f.extend_from_slice(&self.board.batt_mv.to_le_bytes());
                        f.extend_from_slice(&((ctx.now / 1000) as u32).to_le_bytes());
                        f.extend_from_slice(&s.err_flags.to_le_bytes());
                        f.push(ctx.outbound_len().min(255) as u8);
                    }
                    STATS_TYPE_RADIO => {
                        f.extend_from_slice(&s.noise_floor.to_le_bytes());
                        f.push(s.last_rssi as i8 as u8);
                        f.push((s.last_snr * 4.0) as i8 as u8);
                        f.extend_from_slice(&((s.total_air_time_ms / 1000) as u32).to_le_bytes());
                        f.extend_from_slice(&((s.rx_air_time_ms / 1000) as u32).to_le_bytes());
                    }
                    STATS_TYPE_PACKETS => {
                        for v in [
                            s.n_packets_recv,
                            s.n_packets_sent,
                            s.n_sent_flood,
                            s.n_sent_direct,
                            s.n_recv_flood,
                            s.n_recv_direct,
                            s.n_recv_errors,
                        ] {
                            f.extend_from_slice(&v.to_le_bytes());
                        }
                    }
                    _ => return self.write_err(ERR_CODE_ILLEGAL_ARG),
                }
                self.write_frame(&f);
            }
            CMD_FACTORY_RESET if frame[1..].starts_with(b"reset") => {
                self.write_ok();
                self.effects.factory_reset = true;
                self.effects.reboot = true;
            }
            CMD_SET_FLOOD_SCOPE_KEY if len >= 2 && frame[1] == 0 => {
                self.send_scope = if len >= 18 { TransportKey(frame[2..18].try_into().unwrap()) } else { TransportKey::default() };
                self.send_unscoped = false;
                self.write_ok();
            }
            CMD_SET_FLOOD_SCOPE_KEY if len >= 2 && frame[1] == 1 => {
                self.send_unscoped = true;
                self.write_ok();
            }
            CMD_SET_DEFAULT_FLOOD_SCOPE => {
                if len >= 1 + 31 + 16 {
                    let name = c_str(&frame[1..32]);
                    if name.is_empty() || name.len() >= 31 {
                        return self.write_err(ERR_CODE_ILLEGAL_ARG);
                    }
                    self.prefs.default_scope_name = String::from_utf8_lossy(name).into_owned();
                    self.prefs.default_scope_key = frame[32..48].try_into().unwrap();
                } else {
                    self.prefs.default_scope_name.clear();
                    self.prefs.default_scope_key = [0; 16];
                }
                self.effects.save_prefs = true;
                self.write_ok();
            }
            CMD_GET_DEFAULT_FLOOD_SCOPE => {
                let mut f = alloc::vec![RESP_CODE_DEFAULT_FLOOD_SCOPE];
                if !self.prefs.default_scope_name.is_empty() {
                    push_padded(&mut f, self.prefs.default_scope_name.as_bytes(), 31);
                    f.extend_from_slice(&self.prefs.default_scope_key);
                }
                self.write_frame(&f);
            }
            CMD_SEND_CONTROL_DATA if len >= 2 && frame[1] & 0x80 != 0 => match ctx.create_control_data(&frame[1..]) {
                Some(pkt) => {
                    ctx.send_zero_hop(pkt, 0);
                    self.write_ok();
                }
                None => self.write_err(ERR_CODE_TABLE_FULL),
            },
            CMD_SET_AUTOADD_CONFIG if len >= 2 => {
                self.prefs.autoadd_config = frame[1];
                if len >= 3 {
                    self.prefs.autoadd_max_hops = frame[2].min(64);
                }
                self.effects.save_prefs = true;
                self.write_ok();
            }
            CMD_GET_AUTOADD_CONFIG => {
                let f = [RESP_CODE_AUTOADD_CONFIG, self.prefs.autoadd_config, self.prefs.autoadd_max_hops];
                self.write_frame(&f);
            }
            CMD_GET_ALLOWED_REPEAT_FREQ => {
                let mut f = alloc::vec![RESP_ALLOWED_REPEAT_FREQ];
                for (lo, hi) in REPEAT_FREQ_RANGES {
                    f.extend_from_slice(&lo.to_le_bytes());
                    f.extend_from_slice(&hi.to_le_bytes());
                }
                self.write_frame(&f);
            }
            CMD_SEND_RAW_PACKET if len >= 4 => match Packet::parse(&frame[2..]) {
                Some(pkt) => {
                    ctx.send_packet(Box::new(pkt), frame[1], 0);
                    self.write_ok();
                }
                None => self.write_err(ERR_CODE_ILLEGAL_ARG),
            },
            _ => self.write_err(ERR_CODE_UNSUPPORTED_CMD),
        }
    }

    fn write_sent(&mut self, sent: Sent, tag: u32, est_timeout: u32) {
        let mut f = alloc::vec![RESP_CODE_SENT, sent.flag()];
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&est_timeout.to_le_bytes());
        self.write_frame(&f);
    }

    /// `sendMessage`: a plain text message; returns (route, expected ACK, timeout).
    fn send_message(&mut self, ctx: &mut MeshCtx, idx: usize, timestamp: u32, attempt: u8, text: &[u8]) -> Option<(Sent, u32, u32)> {
        if text.len() > MAX_TEXT_LEN || (attempt > 3 && text.len() > MAX_TEXT_LEN - 2) {
            return None;
        }
        let mut temp = timestamp.to_le_bytes().to_vec();
        temp.push(attempt & 3);
        temp.extend_from_slice(text);
        let mut ack = [0u8; 4];
        crypto::sha256(&mut ack, &[&temp, ctx.self_id.pub_key()]);
        if attempt > 3 {
            temp.push(0); // NUL, then the attempt number hidden at the tail
            temp.push(attempt);
        }
        let secret = self.secret_of(ctx, idx);
        let id = self.contacts[idx].id;
        let pkt = ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &id, &secret, &temp)?;
        let (sent, est) = self.send_with_timeout(ctx, idx, pkt);
        Some((sent, u32::from_le_bytes(ack), est))
    }

    /// `sendCommandData`: CLI text to a server (no ACK expected).
    fn send_command_data(
        &mut self, ctx: &mut MeshCtx, idx: usize, timestamp: u32, attempt: u8, txt_type: u8, text: &[u8],
    ) -> Option<(Sent, u32)> {
        if text.len() > MAX_TEXT_LEN {
            return None;
        }
        let mut temp = timestamp.to_le_bytes().to_vec();
        temp.push((attempt & 3) | (txt_type << 2));
        temp.extend_from_slice(text);
        let secret = self.secret_of(ctx, idx);
        let id = self.contacts[idx].id;
        let pkt = ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &id, &secret, &temp)?;
        Some(self.send_with_timeout(ctx, idx, pkt))
    }

    /// `sendGroupMessage`: "<name>: <text>" on a channel.
    fn send_group_message(&mut self, ctx: &mut MeshCtx, timestamp: u32, channel_idx: usize, text: &[u8]) -> bool {
        let mut temp = timestamp.to_le_bytes().to_vec();
        temp.push(TXT_TYPE_PLAIN);
        temp.extend_from_slice(self.prefs.node_name.as_bytes());
        temp.extend_from_slice(b": ");
        let prefix_len = temp.len() - 5;
        let n = text.len().min(MAX_TEXT_LEN.saturating_sub(prefix_len));
        temp.extend_from_slice(&text[..n]);
        let ch = self.channels[channel_idx].group();
        match ctx.create_group_datagram(PAYLOAD_TYPE_GRP_TXT, &ch, &temp) {
            Some(pkt) => {
                self.send_flood_scoped(ctx, pkt, 0);
                true
            }
            None => false,
        }
    }

    /// `sendGroupData`: a typed datagram on a channel, flooded or along `path`.
    fn send_group_data(&mut self, ctx: &mut MeshCtx, idx: usize, path_len: u8, path: &[u8], data_type: u16, data: &[u8]) -> bool {
        let mut temp = data_type.to_le_bytes().to_vec();
        temp.push(data.len() as u8);
        temp.extend_from_slice(data);
        let ch = self.channels[idx].group();
        let Some(pkt) = ctx.create_group_datagram(PAYLOAD_TYPE_GRP_DATA, &ch, &temp) else {
            return false;
        };
        if path_len == OUT_PATH_UNKNOWN {
            self.send_flood_scoped(ctx, pkt, 0);
        } else {
            ctx.send_direct(pkt, path_len, path, 0);
        }
        true
    }

    /// `sendLogin`: ANON_REQ with the password (and `sync_since` for a room server).
    fn send_login(&mut self, ctx: &mut MeshCtx, idx: usize, password: &[u8]) -> Option<(Sent, u32)> {
        let mut temp = ctx.current_time_unique().to_le_bytes().to_vec();
        if self.contacts[idx].adv_type == ADV_TYPE_ROOM {
            temp.extend_from_slice(&self.contacts[idx].sync_since.to_le_bytes());
        }
        temp.extend_from_slice(&password[..password.len().min(15)]);
        let secret = self.secret_of(ctx, idx);
        let id = self.contacts[idx].id;
        let pkt = ctx.create_anon_datagram(&id, &secret, &temp)?;
        Some(self.send_with_timeout(ctx, idx, pkt))
    }

    fn send_anon_req(&mut self, ctx: &mut MeshCtx, idx: usize, data: &[u8]) -> Option<(Sent, u32, u32)> {
        let tag = ctx.current_time_unique();
        let mut temp = tag.to_le_bytes().to_vec();
        temp.extend_from_slice(data);
        let secret = self.secret_of(ctx, idx);
        let id = self.contacts[idx].id;
        let pkt = ctx.create_anon_datagram(&id, &secret, &temp)?;
        let (sent, est) = self.send_with_timeout(ctx, idx, pkt);
        Some((sent, tag, est))
    }

    /// `sendRequest` with arbitrary request data; returns (route, tag, timeout).
    fn send_request(&mut self, ctx: &mut MeshCtx, idx: usize, data: &[u8]) -> Option<(Sent, u32, u32)> {
        if data.len() > MAX_PACKET_PAYLOAD - 16 {
            return None;
        }
        let tag = ctx.current_time_unique();
        let mut temp = tag.to_le_bytes().to_vec();
        temp.extend_from_slice(data);
        let secret = self.secret_of(ctx, idx);
        let id = self.contacts[idx].id;
        let pkt = ctx.create_datagram(PAYLOAD_TYPE_REQ, &id, &secret, &temp)?;
        let (sent, est) = self.send_with_timeout(ctx, idx, pkt);
        Some((sent, tag, est))
    }

    /// `sendRequest(req_type)`: type, 4 reserved bytes, 4 random bytes.
    fn send_request_type(&mut self, ctx: &mut MeshCtx, idx: usize, req_type: u8) -> Option<(Sent, u32, u32)> {
        let mut data = [0u8; 9];
        data[0] = req_type;
        ctx.rng.fill(&mut data[5..]);
        self.send_request(ctx, idx, &data)
    }

    /// Re-broadcast a contact's last advert zero-hop with null transport codes ("send nowhere").
    fn share_contact_zero_hop(&mut self, ctx: &mut MeshCtx, idx: usize) -> bool {
        let Some(raw) = self.blobs.get(&self.contacts[idx].id.pub_key) else {
            return false;
        };
        let Some(pkt) = Packet::parse(&raw) else {
            return false;
        };
        ctx.send_zero_hop_scoped(Box::new(pkt), [0, 0], 0);
        true
    }

    /// Process an advert packet from the app as if it had been received flood-mode.
    fn import_contact(&mut self, ctx: &mut MeshCtx, raw: &[u8]) -> bool {
        let Some(mut pkt) = Packet::parse(raw) else {
            return false;
        };
        if pkt.payload_type() != PAYLOAD_TYPE_ADVERT {
            return false;
        }
        pkt.header |= ROUTE_TYPE_FLOOD;
        ctx.tables.clear(&pkt); // so it is processed again
        self.loopback = Some(Box::new(pkt));
        true
    }

    // -------------------------------------------------------------------- CLI

    /// `MyMesh::handleCommand` (USB rescue CLI, `CMD_RUN_CLI_COMMAND`, remote CLI). An `xx|`
    /// prefix is echoed back.
    pub fn handle_command(&mut self, _ctx: &mut MeshCtx, command: &str) -> String {
        let mut command = command.trim_start_matches(' ');
        let mut reply = String::new();
        if command.len() > 4 && command.as_bytes()[2] == b'|' {
            reply.push_str(&command[..3]);
            command = &command[3..];
        }
        match self.handle_command_inner(command) {
            Some(r) => reply.push_str(&r),
            None => reply.push_str("Unknown command"),
        }
        reply
    }

    fn handle_command_inner(&mut self, cmd: &str) -> Option<String> {
        if let Some(r) = self.handle_radio_command(cmd) {
            self.effects.save_prefs = true;
            return Some(r);
        }
        let p = &mut self.prefs;
        let r: String = if cmd == "reboot" {
            self.effects.reboot = true;
            String::new()
        } else if cmd == "poweroff" || cmd == "shutdown" {
            self.effects.power_off = true;
            String::new()
        } else if let Some(name) = cmd.strip_prefix("set name ") {
            if is_valid_name(name) {
                p.node_name = crate::util::truncate_str(name, crate::prefs::NAME_MAX);
                self.effects.save_prefs = true;
                "OK".into()
            } else {
                "Error, bad chars".into()
            }
        } else if let Some(v) = cmd.strip_prefix("set rtpath ") {
            p.round_trip_path = v.starts_with("on");
            self.effects.save_prefs = true;
            "OK".into()
        } else if cmd == "get rtpath" {
            format!("> {}", if p.round_trip_path { "on" } else { "off" })
        } else if cmd == "get name" {
            format!("> {}", p.node_name)
        } else if let Some(v) = cmd.strip_prefix("set pin ") {
            p.ble_pin = atoi(v);
            self.effects.save_prefs = true;
            format!("> pin is now {:06}", p.ble_pin)
        } else if let Some(v) = cmd.strip_prefix("set wifi.ssid ") {
            p.wifi_ssid = crate::util::truncate_str(v, 33);
            self.effects.save_prefs = true;
            self.effects.network_changed = true;
            format!("> wifi.ssid is now {} (set wifi.pwd too, then reboot)", p.wifi_ssid)
        } else if let Some(v) = cmd.strip_prefix("set wifi.pwd ") {
            p.wifi_pwd = crate::util::truncate_str(v, 64);
            self.effects.save_prefs = true;
            self.effects.network_changed = true;
            "> wifi.pwd updated (reboot to apply)".into()
        } else if cmd == "get wifi.pwd" {
            format!("> {}", p.wifi_pwd)
        } else if cmd == "set wifi.clear" {
            p.wifi_ssid.clear();
            p.wifi_pwd.clear();
            self.effects.save_prefs = true;
            self.effects.network_changed = true;
            "> wifi config cleared (reboot to apply)".into()
        } else if cmd == "get wifi.ssid" {
            format!("> {}", if p.wifi_ssid.is_empty() { "(not set)" } else { &p.wifi_ssid })
        } else if let Some(v) = cmd.strip_prefix("set wifi.enabled ") {
            p.wifi_enabled = atoi(v) != 0;
            self.effects.save_prefs = true;
            format!("> wifi.enabled is now {} (reboot to apply)", p.wifi_enabled as u8)
        } else if cmd == "get wifi.enabled" {
            format!("> {}", p.wifi_enabled as u8)
        } else if cmd == "get wifi.status" {
            format!("> {}", if self.board.wifi_ip.is_some() { "connected" } else { "disconnected" })
        } else if cmd == "get wifi.ip" {
            format!("> {}", self.board.wifi_ip.as_deref().unwrap_or("(not connected)"))
        } else if cmd == "board" {
            self.board.manufacturer.clone()
        } else if cmd == "ver" {
            format!("{} (Build: {})", FIRMWARE_VERSION, self.board.build_date)
        } else if cmd == "get tz.offset" {
            format!("> {}", p.tz_offset)
        } else {
            let v = cmd.strip_prefix("set tz.offset ")?;
            let tz = atof(v) as i32;
            if !(-12..=14).contains(&tz) {
                "Error, must be from -12 to +14".into()
            } else {
                p.tz_offset = tz as i8;
                self.effects.save_prefs = true;
                "OK".into()
            }
        };
        Some(r)
    }

    /// `CommonRadioPrefs::handleCommand`.
    fn handle_radio_command(&mut self, cmd: &str) -> Option<String> {
        let p = &mut self.prefs;
        let on = |v: &str| v.starts_with("on");
        let onoff = |b: bool| if b { "on" } else { "off" };
        let r: String = if cmd == "get radio" {
            format!("> {},{},{},{}", ftoa(p.radio.freq_mhz as f64, 3), ftoa3(p.radio.bw_khz), p.radio.sf, p.radio.cr)
        } else if let Some(rest) = cmd.strip_prefix("set radio ") {
            let parts: Vec<&str> = rest.splitn(4, ',').collect();
            let get = |i: usize| parts.get(i).copied().unwrap_or("");
            let params =
                RadioParams { freq_mhz: atof(get(0)) as f32, bw_khz: atof(get(1)) as f32, sf: atoi(get(2)) as u8, cr: atoi(get(3)) as u8 };
            if params.is_valid() {
                p.radio = params;
                "OK - reboot to apply".into()
            } else {
                "Error, invalid radio params".into()
            }
        } else if cmd == "get freq" {
            format!("> {}", ftoa(p.radio.freq_mhz as f64, 3))
        } else if cmd == "get af" {
            format!("> {}", ftoa(p.airtime_factor as f64, 3))
        } else if let Some(v) = cmd.strip_prefix("set af ") {
            let af = atof(v) as f32;
            if v.trim().is_empty() || !(0.0..=9.0).contains(&af) {
                "ERROR: af must be 0-9".into()
            } else {
                p.airtime_factor = af;
                "OK".into()
            }
        } else if cmd == "get dutycycle" {
            format!("> {:.1}%", 100.0 / (p.airtime_factor + 1.0))
        } else if let Some(v) = cmd.strip_prefix("set dutycycle ") {
            let dc = atof(v) as f32;
            if !(1.0..=100.0).contains(&dc) {
                "ERROR: dutycycle must be 1-100".into()
            } else {
                p.airtime_factor = 100.0 / dc - 1.0;
                format!("OK - {:.1}%", 100.0 / (p.airtime_factor + 1.0))
            }
        } else if cmd == "get int.thresh" {
            format!("> {}", p.interference_threshold)
        } else if let Some(v) = cmd.strip_prefix("set int.thresh ") {
            p.interference_threshold = atoi(v) as u8;
            "OK".into()
        } else if cmd == "get cad" {
            format!("> {}", onoff(p.cad_enabled))
        } else if let Some(v) = cmd.strip_prefix("set cad ") {
            p.cad_enabled = on(v);
            "OK".into()
        } else if cmd == "get radio.rxgain" {
            format!("> {}", onoff(p.rx_boosted_gain))
        } else if let Some(v) = cmd.strip_prefix("set radio.rxgain ") {
            p.rx_boosted_gain = on(v);
            self.effects.rx_boosted_gain = Some(p.rx_boosted_gain);
            "OK".into()
        } else if cmd == "get tx" || cmd.starts_with("get tx ") {
            format!("> {}", p.tx_power_dbm)
        } else if let Some(v) = cmd.strip_prefix("set tx ") {
            p.tx_power_dbm = atoi_signed(v).clamp(-9, self.board.max_tx_power_dbm as i32) as i8;
            self.effects.tx_power = Some(p.tx_power_dbm);
            "OK".into()
        } else if cmd == "get rxdelay" {
            format!("> {}", ftoa(p.rx_delay_base as f64, 3))
        } else if let Some(v) = cmd.strip_prefix("set rxdelay ") {
            let d = atof(v) as f32;
            if (0.0..=20.0).contains(&d) {
                p.rx_delay_base = d;
                "OK".into()
            } else {
                "Error, must be 0-20".into()
            }
        } else if cmd == "get agc.reset.interval" {
            format!("> {}", p.agc_reset_interval as u32 * 4)
        } else if let Some(v) = cmd.strip_prefix("set agc.reset.interval ") {
            let secs = atoi(v).min(255 * 4);
            p.agc_reset_interval = (secs / 4) as u8;
            format!("OK - interval rounded to {}", p.agc_reset_interval as u32 * 4)
        } else if cmd == "get path.hash.mode" {
            format!("> {}", p.path_hash_mode)
        } else if let Some(v) = cmd.strip_prefix("set path.hash.mode ") {
            let mode = atoi(v);
            if mode < 3 {
                p.path_hash_mode = mode as u8;
                "OK".into()
            } else {
                "Error, must be 0,1, or 2".into()
            }
        } else if cmd == "get multi.acks" {
            format!("> {}", p.multi_acks)
        } else if let Some(v) = cmd.strip_prefix("set multi.acks ") {
            p.multi_acks = atoi(v) as u8;
            "OK".into()
        } else if cmd == "get txdelay" {
            format!("> {}", ftoa(p.tx_delay_factor as f64, 3))
        } else if let Some(v) = cmd.strip_prefix("set txdelay ") {
            let f = atof(v) as f32;
            if (0.0..=2.0).contains(&f) {
                p.tx_delay_factor = f;
                "OK".into()
            } else {
                "Error, must be 0-2".into()
            }
        } else if cmd == "get direct.txdelay" {
            format!("> {}", ftoa(p.direct_tx_delay_factor as f64, 3))
        } else {
            let v = cmd.strip_prefix("set direct.txdelay ")?;
            let f = atof(v) as f32;
            if (0.0..=2.0).contains(&f) {
                p.direct_tx_delay_factor = f;
                "OK".into()
            } else {
                "Error, must be 0-2".into()
            }
        };
        Some(r)
    }
}

fn push_padded(f: &mut Vec<u8>, s: &[u8], width: usize) {
    let n = s.len().min(width - 1);
    f.extend_from_slice(&s[..n]);
    f.resize(f.len() + width - n, 0);
}

/// `updateContactFromFrame`: `[code][key 32][type][flags][path_len][path 64][name 32][last_advert
/// 4]` + optional `[lat 4][lon 4]` + optional `[lastmod 4]`.
fn update_contact_from_frame(c: &mut ContactInfo, last_mod: &mut u32, frame: &[u8]) {
    let at = |i: usize| frame.get(i).copied().unwrap_or(0);
    c.adv_type = at(33);
    c.flags = at(34);
    c.out_path_len = at(35);
    for (k, b) in c.out_path.iter_mut().enumerate() {
        *b = at(36 + k);
    }
    let mut name = [0u8; 32];
    for (k, b) in name.iter_mut().enumerate() {
        *b = at(100 + k);
    }
    c.set_name(&name);
    c.last_advert_timestamp = read_u32(frame, 132);
    if frame.len() >= 136 + 8 {
        c.gps_lat = read_u32(frame, 136) as i32;
        c.gps_lon = read_u32(frame, 140) as i32;
        if frame.len() >= 144 + 4 {
            *last_mod = read_u32(frame, 144);
        }
    }
    c.secret = None;
}
