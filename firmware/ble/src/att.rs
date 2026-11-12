//! The attribute server ([Vol 3] Part F and G) of one fixed table: the GAP and GATT services every
//! device has, and the Nordic UART service the MeshCore apps talk through.
//!
//! The table is the one the firmware had with trouble-host, handle for handle: a bonded phone
//! keeps the handles it once found, and no Service Changed tells it otherwise.
//!
//! | Handle | Attribute                                   | Access                        |
//! |--------|---------------------------------------------|-------------------------------|
//! | 1      | service: GAP (0x1800)                       | read                          |
//! | 2, 3   | characteristic: Device Name (0x2A00)        | read                          |
//! | 4, 5   | characteristic: Appearance (0x2A01)         | read                          |
//! | 6      | service: GATT (0x1801), empty               | read                          |
//! | 7      | service: Nordic UART                        | read                          |
//! | 8, 9   | characteristic: RX, what the app writes     | write, authenticated          |
//! | 10, 11 | characteristic: TX, what the app is told    | read (empty), authenticated   |
//! | 12     | its Client Characteristic Configuration     | read; write, authenticated    |

use alloc::vec::Vec;

/// The Attribute Protocol's channel.
pub(crate) const CID: u16 = 4;

/// The MTU before any exchange, and the one this server takes.
pub(crate) const MTU_DEFAULT: usize = 23;
pub(crate) const MTU_MAX: usize = 251;

const NAME: u16 = 3;
const SERVICES: [u16; 3] = [1, 6, 7];
pub(crate) const RX: u16 = 9;
pub(crate) const TX: u16 = 11;
const CCCD: u16 = 12;
const LAST: u16 = 12;

const PRIMARY_SERVICE: u16 = 0x2800;
const CHARACTERISTIC: u16 = 0x2803;

/// The Nordic UART service's UUID, as on the air; its characteristics differ in one byte.
pub(crate) const UART: [u8; 16] = uart(1);

const fn uart(n: u8) -> [u8; 16] {
    [0x9E, 0xCA, 0xDC, 0x24, 0x0E, 0xE5, 0xA9, 0xE0, 0x93, 0xF3, 0xA3, 0xB5, n, 0x00, 0x40, 0x6E]
}

const ERROR_RESPONSE: u8 = 0x01;
const EXCHANGE_MTU_REQUEST: u8 = 0x02;
const FIND_INFORMATION_REQUEST: u8 = 0x04;
const FIND_BY_TYPE_VALUE_REQUEST: u8 = 0x06;
const READ_BY_TYPE_REQUEST: u8 = 0x08;
const READ_REQUEST: u8 = 0x0A;
const READ_BLOB_REQUEST: u8 = 0x0C;
const READ_BY_GROUP_TYPE_REQUEST: u8 = 0x10;
const WRITE_REQUEST: u8 = 0x12;
const WRITE_RESPONSE: u8 = 0x13;
const HANDLE_VALUE_NOTIFICATION: u8 = 0x1B;
const WRITE_COMMAND: u8 = 0x52;
/// The bit of an opcode that makes it a command: never answered.
const COMMAND: u8 = 0x40;

const INVALID_HANDLE: u8 = 0x01;
const READ_NOT_PERMITTED: u8 = 0x02;
const WRITE_NOT_PERMITTED: u8 = 0x03;
const INVALID_PDU: u8 = 0x04;
const INSUFFICIENT_AUTHENTICATION: u8 = 0x05;
const REQUEST_NOT_SUPPORTED: u8 = 0x06;
const INVALID_OFFSET: u8 = 0x07;
const ATTRIBUTE_NOT_FOUND: u8 = 0x0A;
const INVALID_ATTRIBUTE_VALUE_LENGTH: u8 = 0x0D;
pub(crate) const INSUFFICIENT_ENCRYPTION: u8 = 0x0F;
const UNSUPPORTED_GROUP_TYPE: u8 = 0x10;

/// An attribute's type: a 16-bit UUID of the Bluetooth SIG or one of the service's own.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Uuid {
    Short(u16),
    Long([u8; 16]),
}

impl Uuid {
    fn parse(bytes: &[u8]) -> Option<Self> {
        match bytes.len() {
            2 => Some(Self::Short(u16::from_le_bytes([bytes[0], bytes[1]]))),
            16 => bytes.try_into().ok().map(Self::Long),
            _ => None,
        }
    }

    fn put(&self, out: &mut Vec<u8>) {
        match self {
            Self::Short(uuid) => out.extend_from_slice(&uuid.to_le_bytes()),
            Self::Long(uuid) => out.extend_from_slice(uuid),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Short(_) => 2,
            Self::Long(_) => 16,
        }
    }
}

fn kind(handle: u16) -> Option<Uuid> {
    Some(match handle {
        1 | 6 | 7 => Uuid::Short(PRIMARY_SERVICE),
        2 | 4 | 8 | 10 => Uuid::Short(CHARACTERISTIC),
        3 => Uuid::Short(0x2A00),
        5 => Uuid::Short(0x2A01),
        RX => Uuid::Long(uart(2)),
        TX => Uuid::Long(uart(3)),
        CCCD => Uuid::Short(0x2902),
        _ => return None,
    })
}

/// The last handle of the group a handle begins: of a service, its attributes; the last
/// service's reaches to the end of all handles.
fn group_end(handle: u16) -> u16 {
    match handle {
        1 => 5,
        7 => 0xFFFF,
        h => h,
    }
}

/// What a connection's phone has set up with the server.
pub(crate) struct Att {
    /// The largest PDU either side sends.
    pub mtu: usize,
    /// The Client Characteristic Configuration of TX: bit 0 asks for notifications.
    pub cccd: u16,
    /// A frame's Write Request waits for its response: the frame is not taken yet.
    write_pending: bool,
    /// The other side has the key of an earlier pairing and the link is not encrypted with it yet.
    pub keyed: bool,
}

/// What a PDU brought about.
#[derive(Default)]
pub(crate) struct Outcome {
    /// The PDU to answer with.
    pub reply: Option<Vec<u8>>,
    /// A frame written to RX.
    pub frame: Option<Vec<u8>>,
}

impl Att {
    pub(crate) fn new(cccd: u16) -> Self {
        Self { mtu: MTU_DEFAULT, cccd, write_pending: false, keyed: false }
    }

    /// What a link that is not authenticated is refused with ([Vol 3] Part C, Section 10.3.1):
    /// with a key to its name and no encryption yet, it is to start encryption; else it is to
    /// pair. The difference is acted on: told to pair, a phone that has the key pairs anew, and
    /// asks its user for the passkey each time it comes back.
    fn refusal(&self) -> u8 {
        if self.keyed { INSUFFICIENT_ENCRYPTION } else { INSUFFICIENT_AUTHENTICATION }
    }

    /// The phone asked for notifications of TX.
    pub(crate) fn notifying(&self) -> bool {
        self.cccd & 1 != 0
    }

    /// A notification of TX, cut to what the MTU takes.
    pub(crate) fn notification(&self, value: &[u8]) -> Vec<u8> {
        let mut pdu = Vec::with_capacity(3 + value.len());
        pdu.push(HANDLE_VALUE_NOTIFICATION);
        pdu.extend_from_slice(&TX.to_le_bytes());
        pdu.extend_from_slice(&value[..value.len().min(self.mtu - 3)]);
        pdu
    }

    /// The response of a frame's Write Request, once the frame is taken; `None` for a frame of a
    /// Write Command, which has none.
    pub(crate) fn written(&mut self) -> Option<Vec<u8>> {
        core::mem::take(&mut self.write_pending).then(|| alloc::vec![WRITE_RESPONSE])
    }

    /// The value of an attribute as this link may read it.
    fn read(&self, handle: u16, name: &[u8], authenticated: bool) -> Result<Vec<u8>, u8> {
        let characteristic = |properties: u8, value: u16, uuid: Uuid| {
            let mut v = alloc::vec![properties];
            v.extend_from_slice(&value.to_le_bytes());
            uuid.put(&mut v);
            v
        };
        const READ: u8 = 0x02;
        const WRITE_WITHOUT_RESPONSE: u8 = 0x04;
        const WRITE: u8 = 0x08;
        const NOTIFY: u8 = 0x10;
        Ok(match handle {
            1 => alloc::vec![0x00, 0x18],
            2 => characteristic(READ, NAME, Uuid::Short(0x2A00)),
            NAME => name.to_vec(),
            4 => characteristic(READ, 5, Uuid::Short(0x2A01)),
            5 => alloc::vec![0, 0], // appearance: unknown
            6 => alloc::vec![0x01, 0x18],
            7 => UART.to_vec(),
            8 => characteristic(WRITE | WRITE_WITHOUT_RESPONSE, RX, Uuid::Long(uart(2))),
            RX => return Err(READ_NOT_PERMITTED),
            10 => characteristic(READ | NOTIFY, TX, Uuid::Long(uart(3))),
            // frames are notified, never kept to be read
            TX if authenticated => Vec::new(),
            TX => return Err(self.refusal()),
            CCCD => self.cccd.to_le_bytes().to_vec(),
            _ => return Err(INVALID_HANDLE),
        })
    }

    /// A write to an attribute: `Ok` with the frame, if it was one written to RX.
    fn write(&mut self, handle: u16, value: &[u8], authenticated: bool, frame_max: usize) -> Result<Option<Vec<u8>>, u8> {
        match handle {
            RX | CCCD if !authenticated => Err(self.refusal()),
            RX if value.len() > frame_max => Err(INVALID_ATTRIBUTE_VALUE_LENGTH),
            RX => Ok(Some(value.to_vec())),
            CCCD if value.len() != 2 => Err(INVALID_ATTRIBUTE_VALUE_LENGTH),
            CCCD => {
                self.cccd = u16::from_le_bytes([value[0], value[1]]);
                Ok(None)
            }
            1..=LAST => Err(WRITE_NOT_PERMITTED),
            _ => Err(INVALID_HANDLE),
        }
    }

    /// A PDU of the phone. `name` is the Device Name, `authenticated` whether the link is
    /// encrypted with a key of a pairing by passkey, `frame_max` the longest frame RX takes.
    pub(crate) fn pdu(&mut self, pdu: &[u8], name: &[u8], authenticated: bool, frame_max: usize) -> Outcome {
        let Some((&opcode, body)) = pdu.split_first() else {
            return Outcome::default();
        };
        let mut frame = None;
        let result = match opcode {
            EXCHANGE_MTU_REQUEST => self.exchange_mtu(body),
            FIND_INFORMATION_REQUEST => self.find_information(body),
            FIND_BY_TYPE_VALUE_REQUEST => self.find_by_type_value(body, name, authenticated),
            READ_BY_TYPE_REQUEST => self.read_by_type(body, name, authenticated),
            READ_BY_GROUP_TYPE_REQUEST => self.read_by_group_type(body),
            READ_REQUEST | READ_BLOB_REQUEST => self.read_request(opcode, body, name, authenticated),
            WRITE_REQUEST | WRITE_COMMAND => match body {
                [low, high, value @ ..] => {
                    let handle = u16::from_le_bytes([*low, *high]);
                    match self.write(handle, value, authenticated, frame_max) {
                        // a frame's response waits until the frame is taken
                        Ok(Some(written)) => {
                            frame = Some(written);
                            self.write_pending = opcode == WRITE_REQUEST;
                            Ok(None)
                        }
                        Ok(None) => Ok(Some(alloc::vec![WRITE_RESPONSE])),
                        Err(code) => Err((handle, code)),
                    }
                }
                _ => Err((0, INVALID_PDU)),
            },
            // a response or a confirmation: nothing this server waits for
            0x01 | 0x03 | 0x05 | 0x07 | 0x09 | 0x0B | 0x0D | 0x0F | 0x11 | 0x13 | 0x17 | 0x19 | 0x1E => Ok(None),
            _ => Err((0, REQUEST_NOT_SUPPORTED)),
        };
        let reply = match result {
            _ if opcode & COMMAND != 0 => None,
            Ok(reply) => reply,
            Err((handle, code)) => {
                let handle = handle.to_le_bytes();
                Some(alloc::vec![ERROR_RESPONSE, opcode, handle[0], handle[1], code])
            }
        };
        Outcome { reply, frame }
    }

    fn exchange_mtu(&mut self, body: &[u8]) -> Result<Option<Vec<u8>>, (u16, u8)> {
        let &[low, high] = body else {
            return Err((0, INVALID_PDU));
        };
        self.mtu = (u16::from_le_bytes([low, high]) as usize).clamp(MTU_DEFAULT, MTU_MAX);
        let ours = (MTU_MAX as u16).to_le_bytes();
        Ok(Some(alloc::vec![0x03, ours[0], ours[1]]))
    }

    fn find_information(&self, body: &[u8]) -> Result<Option<Vec<u8>>, (u16, u8)> {
        let (start, end, rest) = range(body)?;
        if !rest.is_empty() {
            return Err((start, INVALID_PDU));
        }
        let mut reply = Vec::new();
        let mut first = None;
        for handle in start..=end.min(LAST) {
            let Some(uuid) = kind(handle) else { break };
            // one response holds UUIDs of one size
            let format = *first.get_or_insert(uuid.len());
            if uuid.len() != format || reply.len() + 2 + format > self.mtu - 2 {
                break;
            }
            reply.extend_from_slice(&handle.to_le_bytes());
            uuid.put(&mut reply);
        }
        let Some(format) = first else {
            return Err((start, ATTRIBUTE_NOT_FOUND));
        };
        reply.splice(0..0, [0x05, if format == 2 { 1 } else { 2 }]);
        Ok(Some(reply))
    }

    fn find_by_type_value(&self, body: &[u8], name: &[u8], authenticated: bool) -> Result<Option<Vec<u8>>, (u16, u8)> {
        let (start, end, rest) = range(body)?;
        let [low, high, value @ ..] = rest else {
            return Err((start, INVALID_PDU));
        };
        let uuid = Uuid::Short(u16::from_le_bytes([*low, *high]));
        let mut reply = alloc::vec![0x07];
        for handle in start..=end.min(LAST) {
            let found = kind(handle) == Some(uuid) && self.read(handle, name, authenticated).is_ok_and(|v| v == value);
            if found && reply.len() + 4 <= self.mtu {
                reply.extend_from_slice(&handle.to_le_bytes());
                reply.extend_from_slice(&group_end(handle).to_le_bytes());
            }
        }
        if reply.len() == 1 {
            return Err((start, ATTRIBUTE_NOT_FOUND));
        }
        Ok(Some(reply))
    }

    fn read_by_type(&self, body: &[u8], name: &[u8], authenticated: bool) -> Result<Option<Vec<u8>>, (u16, u8)> {
        let (start, end, rest) = range(body)?;
        let uuid = Uuid::parse(rest).ok_or((start, INVALID_PDU))?;
        let mut reply = alloc::vec![0x09, 0];
        // every value of one response is as long as its first, and but for the first, whole
        let mut size = None;
        for handle in start..=end.min(LAST) {
            if kind(handle) != Some(uuid) {
                continue;
            }
            let value = match self.read(handle, name, authenticated) {
                Ok(value) => value,
                Err(code) if size.is_none() => return Err((handle, code)),
                Err(_) => break,
            };
            match size {
                None => {
                    let kept = value.len().min(self.mtu - 4).min(253);
                    size = Some(value.len());
                    reply[1] = 2 + kept as u8;
                    reply.extend_from_slice(&handle.to_le_bytes());
                    reply.extend_from_slice(&value[..kept]);
                }
                Some(size) if size == value.len() && reply.len() + 2 + size <= self.mtu => {
                    reply.extend_from_slice(&handle.to_le_bytes());
                    reply.extend_from_slice(&value);
                }
                Some(_) => break,
            }
        }
        if size.is_none() {
            return Err((start, ATTRIBUTE_NOT_FOUND));
        }
        Ok(Some(reply))
    }

    fn read_by_group_type(&self, body: &[u8]) -> Result<Option<Vec<u8>>, (u16, u8)> {
        let (start, end, rest) = range(body)?;
        let uuid = Uuid::parse(rest).ok_or((start, INVALID_PDU))?;
        if uuid != Uuid::Short(PRIMARY_SERVICE) {
            return Err((start, UNSUPPORTED_GROUP_TYPE));
        }
        let mut reply = alloc::vec![0x11, 0];
        let mut size = None;
        for handle in SERVICES.into_iter().filter(|h| (start..=end).contains(h)) {
            let service: &[u8] = match handle {
                1 => &[0x00, 0x18],
                6 => &[0x01, 0x18],
                _ => &UART,
            };
            // as above: one size of UUID in a response
            if *size.get_or_insert(service.len()) != service.len() || reply.len() + 4 + service.len() > self.mtu {
                break;
            }
            reply[1] = 4 + service.len() as u8;
            reply.extend_from_slice(&handle.to_le_bytes());
            reply.extend_from_slice(&group_end(handle).to_le_bytes());
            reply.extend_from_slice(service);
        }
        if size.is_none() {
            return Err((start, ATTRIBUTE_NOT_FOUND));
        }
        Ok(Some(reply))
    }

    fn read_request(&self, opcode: u8, body: &[u8], name: &[u8], authenticated: bool) -> Result<Option<Vec<u8>>, (u16, u8)> {
        let (handle, offset) = match (opcode, body) {
            (READ_REQUEST, &[low, high]) => (u16::from_le_bytes([low, high]), 0),
            (READ_BLOB_REQUEST, &[low, high, offset_low, offset_high]) => {
                (u16::from_le_bytes([low, high]), u16::from_le_bytes([offset_low, offset_high]) as usize)
            }
            _ => return Err((0, INVALID_PDU)),
        };
        let value = self.read(handle, name, authenticated).map_err(|code| (handle, code))?;
        let rest = value.get(offset..).ok_or((handle, INVALID_OFFSET))?;
        let mut reply = alloc::vec![opcode + 1];
        reply.extend_from_slice(&rest[..rest.len().min(self.mtu - 1)]);
        Ok(Some(reply))
    }
}

/// The handle range a request begins with, and what follows it.
fn range(body: &[u8]) -> Result<(u16, u16, &[u8]), (u16, u8)> {
    let [a, b, c, d, rest @ ..] = body else {
        return Err((0, INVALID_PDU));
    };
    let (start, end) = (u16::from_le_bytes([*a, *b]), u16::from_le_bytes([*c, *d]));
    if start == 0 || start > end {
        return Err((start, INVALID_HANDLE));
    }
    Ok((start, end, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &[u8] = b"MeshCore-Test";

    fn ask(att: &mut Att, pdu: &[u8], authenticated: bool) -> Vec<u8> {
        att.pdu(pdu, NAME, authenticated, 176).reply.unwrap_or_default()
    }

    /// What a phone asks when it looks a device over, and the answers the table gave so far.
    #[test]
    fn discovery_as_before() {
        let mut att = Att::new(0);
        assert_eq!(ask(&mut att, &[0x02, 0x05, 0x02], false), [0x03, 251, 0]);
        assert_eq!(att.mtu, 251);
        let mut att = Att::new(0);

        // primary services, at the smallest MTU: the 16-bit ones, then the UART service
        assert_eq!(
            ask(&mut att, &[0x10, 1, 0, 0xFF, 0xFF, 0x00, 0x28], false),
            [0x11, 6, 1, 0, 5, 0, 0x00, 0x18, 6, 0, 6, 0, 0x01, 0x18]
        );
        let mut uart_service = alloc::vec![0x11, 20, 7, 0, 0xFF, 0xFF];
        uart_service.extend_from_slice(&UART);
        assert_eq!(ask(&mut att, &[0x10, 7, 0, 0xFF, 0xFF, 0x00, 0x28], false), uart_service);
        assert_eq!(ask(&mut att, &[0x10, 8, 0, 0xFF, 0xFF, 0x00, 0x28], false), [0x01, 0x10, 8, 0, 0x0A]);
        assert_eq!(ask(&mut att, &[0x10, 1, 0, 0xFF, 0xFF, 0x01, 0x28], false), [0x01, 0x10, 1, 0, 0x10]);

        // the service by its UUID
        let mut by_uuid = alloc::vec![0x06, 1, 0, 0xFF, 0xFF, 0x00, 0x28];
        by_uuid.extend_from_slice(&UART);
        assert_eq!(ask(&mut att, &by_uuid, false), [0x07, 7, 0, 0xFF, 0xFF]);
        assert_eq!(ask(&mut att, &[0x06, 1, 0, 0xFF, 0xFF, 0x00, 0x28, 0x00, 0x18], false), [0x07, 1, 0, 5, 0]);

        // characteristics: of GAP, then one of the UART service at a time (a 128-bit UUID each)
        assert_eq!(
            ask(&mut att, &[0x08, 1, 0, 5, 0, 0x03, 0x28], false),
            [0x09, 7, 2, 0, 0x02, 3, 0, 0x00, 0x2A, 4, 0, 0x02, 5, 0, 0x01, 0x2A]
        );
        let mut rx = alloc::vec![0x09, 21, 8, 0, 0x0C, 9, 0];
        rx.extend_from_slice(&uart(2));
        assert_eq!(ask(&mut att, &[0x08, 7, 0, 0xFF, 0xFF, 0x03, 0x28], false), rx);
        let mut tx = alloc::vec![0x09, 21, 10, 0, 0x12, 11, 0];
        tx.extend_from_slice(&uart(3));
        assert_eq!(ask(&mut att, &[0x08, 9, 0, 0xFF, 0xFF, 0x03, 0x28], false), tx);
        assert_eq!(ask(&mut att, &[0x08, 11, 0, 0xFF, 0xFF, 0x03, 0x28], false), [0x01, 0x08, 11, 0, 0x0A]);
        // both at once with room for them
        att.mtu = 64;
        let both = ask(&mut att, &[0x08, 7, 0, 0xFF, 0xFF, 0x03, 0x28], false);
        assert_eq!(both.len(), 2 + 21 * 2);
        assert_eq!(both[23..26], [10, 0, 0x12]);
        att.mtu = 23;

        // descriptors: the one after TX
        assert_eq!(ask(&mut att, &[0x04, 12, 0, 0xFF, 0xFF], false), [0x05, 1, 12, 0, 0x02, 0x29]);
        assert_eq!(ask(&mut att, &[0x04, 13, 0, 0xFF, 0xFF], false), [0x01, 0x04, 13, 0, 0x0A]);
        let mut value = alloc::vec![0x05, 2, 9, 0];
        value.extend_from_slice(&uart(2));
        assert_eq!(ask(&mut att, &[0x04, 9, 0, 0xFF, 0xFF], false), value);
        assert_eq!(ask(&mut att, &[0x04, 1, 0, 0xFF, 0xFF], false).len(), 2 + 5 * 4, "as many as the MTU takes");

        // the name, by handle and by type
        let mut name = alloc::vec![0x0B];
        name.extend_from_slice(NAME);
        assert_eq!(ask(&mut att, &[0x0A, 3, 0], false), name);
        assert_eq!(ask(&mut att, &[0x0C, 3, 0, 9, 0], false), [0x0D, b'T', b'e', b's', b't']);
        assert_eq!(ask(&mut att, &[0x0C, 3, 0, 14, 0], false), [0x01, 0x0C, 3, 0, 0x07]);
        assert_eq!(ask(&mut att, &[0x08, 1, 0, 0xFF, 0xFF, 0x00, 0x2A], false)[..4], [0x09, 2 + 13, 3, 0]);
        assert_eq!(ask(&mut att, &[0x0A, 5, 0], false), [0x0B, 0, 0]);
    }

    #[test]
    fn the_service_takes_an_authenticated_link() {
        let mut att = Att::new(0);
        // a phone not paired: told so, which makes it pair
        assert_eq!(ask(&mut att, &[0x12, 12, 0, 1, 0], false), [0x01, 0x12, 12, 0, 0x05]);
        assert_eq!(ask(&mut att, &[0x12, 9, 0, 0xAA], false), [0x01, 0x12, 9, 0, 0x05]);
        assert_eq!(ask(&mut att, &[0x0A, 11, 0], false), [0x01, 0x0A, 11, 0, 0x05]);
        assert!(att.pdu(&[0x52, 9, 0, 0xAA], NAME, false, 176).frame.is_none());
        assert!(!att.notifying());
        assert_eq!(ask(&mut att, &[0x0A, 12, 0], false), [0x0B, 0, 0]);

        // paired
        assert_eq!(ask(&mut att, &[0x12, 12, 0, 1, 0], true), [0x13]);
        assert!(att.notifying());
        assert_eq!(ask(&mut att, &[0x0A, 12, 0], true), [0x0B, 1, 0]);
        assert_eq!(ask(&mut att, &[0x0A, 11, 0], true), [0x0B]);
        assert_eq!(ask(&mut att, &[0x12, 12, 0, 1], true), [0x01, 0x12, 12, 0, 0x0D]);

        // a frame by request: answered once it is taken
        let outcome = att.pdu(&[0x12, 9, 0, 1, 2, 3], NAME, true, 176);
        assert_eq!((outcome.reply, outcome.frame), (None, Some(alloc::vec![1, 2, 3])));
        assert_eq!(att.written(), Some(alloc::vec![0x13]));
        assert_eq!(att.written(), None);
        // and by command: never answered
        let outcome = att.pdu(&[0x52, 9, 0, 4, 5], NAME, true, 176);
        assert_eq!((outcome.reply, outcome.frame), (None, Some(alloc::vec![4, 5])));
        assert_eq!(att.written(), None);

        let mut long = alloc::vec![0x12, 9, 0];
        long.resize(3 + 177, 0);
        assert_eq!(ask(&mut att, &long, true), [0x01, 0x12, 9, 0, 0x0D]);

        // what nobody may do
        assert_eq!(ask(&mut att, &[0x0A, 9, 0], true), [0x01, 0x0A, 9, 0, 0x02]);
        assert_eq!(ask(&mut att, &[0x12, 3, 0, b'x'], true), [0x01, 0x12, 3, 0, 0x03]);
        assert_eq!(ask(&mut att, &[0x12, 13, 0, 0], true), [0x01, 0x12, 13, 0, 0x01]);
        assert_eq!(ask(&mut att, &[0x0A, 0, 0], true), [0x01, 0x0A, 0, 0, 0x01]);
        assert_eq!(ask(&mut att, &[0x52, 3, 0, b'x'], true), [], "a command is never answered");
    }

    #[test]
    fn requests_out_of_the_ordinary() {
        let mut att = Att::new(0);
        assert_eq!(ask(&mut att, &[0x16, 9, 0, 0, 0, 1], true), [0x01, 0x16, 0, 0, 0x06], "prepare write");
        assert_eq!(ask(&mut att, &[0x0E, 3, 0, 5, 0], true), [0x01, 0x0E, 0, 0, 0x06], "read multiple");
        assert_eq!(ask(&mut att, &[0x04, 5, 0, 1, 0], true), [0x01, 0x04, 5, 0, 0x01]);
        assert_eq!(ask(&mut att, &[0x04, 0, 0, 1, 0], true), [0x01, 0x04, 0, 0, 0x01]);
        assert_eq!(ask(&mut att, &[0x08, 1, 0, 5, 0, 1, 2, 3], true), [0x01, 0x08, 1, 0, 0x04]);
        assert_eq!(ask(&mut att, &[0x0A], true), [0x01, 0x0A, 0, 0, 0x04]);
        assert_eq!(ask(&mut att, &[0x02, 5], true), [0x01, 0x02, 0, 0, 0x04]);
        assert_eq!(ask(&mut att, &[0x1E], true), [], "a confirmation");
        assert_eq!(ask(&mut att, &[0xD2, 9, 0, 1], true), [], "a signed write is a command");
        assert_eq!(ask(&mut att, &[], true), []);
        // an MTU below the least is the least
        assert_eq!(ask(&mut att, &[0x02, 5, 0], true), [0x03, 251, 0]);
        assert_eq!(att.mtu, 23);
        // a notification is cut to the MTU
        assert_eq!(att.notification(&[7; 40]).len(), 23);
        // TX by its type: as by its handle
        let mut by_type = alloc::vec![0x08, 1, 0, 0xFF, 0xFF];
        by_type.extend_from_slice(&uart(3));
        assert_eq!(ask(&mut att, &by_type, false), [0x01, 0x08, 11, 0, 0x05]);
        assert_eq!(ask(&mut att, &by_type, true), [0x09, 2, 11, 0]);
    }

    /// Whatever arrives, the server answers or keeps silent: a panic would reset the node.
    #[test]
    fn any_bytes_are_survived() {
        let mut att = Att::new(0);
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..200_000 {
            let mut pdu = [0u8; 24];
            for b in &mut pdu {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                // low values: handles and opcodes that exist
                *b = if x & 0x300 == 0 { (x >> 16) as u8 } else { (x >> 16) as u8 % 0x14 };
            }
            let len = (x >> 40) as usize % 25;
            att.mtu = 23 + (x >> 48) as usize % 229;
            let outcome = att.pdu(&pdu[..len], NAME, x & 1 == 0, 176);
            assert!(outcome.reply.is_none_or(|reply| reply.len() <= att.mtu));
            att.written();
        }
    }
}
