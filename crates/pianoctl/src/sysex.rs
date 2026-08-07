use std::fmt;

use serde::Deserialize;

const F0: u8 = 0xF0;
const F7: u8 = 0xF7;
const ROLAND: u8 = 0x41;
const UNIVERSAL_NON_REALTIME: u8 = 0x7E;
const ALL_DEVICES: u8 = 0x7F;
const GENERAL_INFORMATION: u8 = 0x06;
const IDENTITY_REQUEST: u8 = 0x01;
const IDENTITY_REPLY: u8 = 0x02;
const RQ1: u8 = 0x11;
const DT1: u8 = 0x12;

const ADDRESS_LEN: usize = 4;

pub fn checksum(bytes: &[u8]) -> u8 {
    let sum: u32 = bytes.iter().map(|b| u32::from(*b)).sum();
    ((128 - sum % 128) % 128) as u8
}

pub fn hex(bytes: &[u8]) -> String {
    use fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn spaced_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let mut digits = Vec::with_capacity(s.len());
    for c in s.chars().filter(|c| !c.is_whitespace()) {
        match c.to_digit(16) {
            Some(digit) => digits.push(digit as u8),
            None => return Err(format!("not a hex digit: {c:?}")),
        }
    }
    if !digits.len().is_multiple_of(2) {
        return Err(format!("hex has an odd number of digits: {s:?}"));
    }
    Ok(digits
        .chunks(2)
        .map(|pair| pair[0] << 4 | pair[1])
        .collect())
}

pub fn parse_hex_field(s: &str) -> Result<Vec<u8>, String> {
    if s.is_empty()
        || s.chars().any(|c| c != ' ' && !c.is_ascii_hexdigit())
        || s.split(' ')
            .any(|run| run.is_empty() || !run.len().is_multiple_of(2))
    {
        return Err(format!(
            "{s:?} is not whole hex bytes separated by single spaces"
        ));
    }
    parse_hex(s)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Deserialize)]
#[serde(try_from = "String")]
pub struct Address([u8; ADDRESS_LEN]);

impl Address {
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        let bytes: [u8; ADDRESS_LEN] = bytes
            .try_into()
            .map_err(|_| format!("an address is {ADDRESS_LEN} bytes, got {}", bytes.len()))?;
        match bytes.iter().find(|b| **b > 0x7F) {
            Some(b) => Err(format!("address byte {b:#04x} is not 7-bit")),
            None => Ok(Self(bytes)),
        }
    }

    pub fn bytes(&self) -> [u8; ADDRESS_LEN] {
        self.0
    }
}

impl TryFrom<String> for Address {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::from_slice(&parse_hex_field(&s)?)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&spaced_hex(&self.0))
    }
}

#[derive(Clone, Debug)]
pub struct Codec {
    model_id: Vec<u8>,
    device_id: u8,
}

impl Codec {
    pub fn new(model_id: Vec<u8>, device_id: u8) -> Self {
        Self {
            model_id,
            device_id,
        }
    }

    pub fn with_device_id(self, device_id: u8) -> Self {
        Self { device_id, ..self }
    }

    pub fn device_id(&self) -> u8 {
        self.device_id
    }

    pub fn dt1(&self, addr: Address, data: &[u8]) -> Vec<u8> {
        let mut checked = addr.bytes().to_vec();
        checked.extend_from_slice(data);
        self.frame(DT1, checked)
    }

    fn rq1(&self, addr: Address, size: u32) -> Vec<u8> {
        let mut checked = addr.bytes().to_vec();
        checked.extend_from_slice(&split7(size));
        self.frame(RQ1, checked)
    }

    fn frame(&self, command: u8, checked: Vec<u8>) -> Vec<u8> {
        let mut out = vec![F0, ROLAND, self.device_id];
        out.extend_from_slice(&self.model_id);
        out.push(command);
        out.extend_from_slice(&checked);
        out.push(checksum(&checked));
        out.push(F7);
        out
    }

    pub fn decode(&self, frame: &[u8]) -> Result<Incoming, DecodeError> {
        let body = frame
            .strip_prefix(&[F0])
            .and_then(|b| b.strip_suffix(&[F7]))
            .ok_or(DecodeError::NotAFrame)?;
        if let Some(b) = body.iter().find(|b| **b > 0x7F) {
            return Err(DecodeError::NonDataByte(*b));
        }
        match body.first() {
            Some(&ROLAND) => self.decode_roland(body),
            Some(&UNIVERSAL_NON_REALTIME) => decode_universal(body),
            Some(_) => Ok(Incoming::Foreign),
            None => Err(DecodeError::Truncated),
        }
    }

    fn decode_roland(&self, body: &[u8]) -> Result<Incoming, DecodeError> {
        let device_id = *body.get(1).ok_or(DecodeError::Truncated)?;
        let rest = body.get(2..).ok_or(DecodeError::Truncated)?;
        // Compare as far as both go: a frame that disagrees with our model ID
        // belongs to another device, while one that agrees but stops short is
        // ours and torn. `watch` must not file a lost frame under "not mine".
        let shared = rest.len().min(self.model_id.len());
        if rest[..shared] != self.model_id[..shared] {
            return Ok(Incoming::Foreign);
        }
        let Some(rest) = rest.strip_prefix(self.model_id.as_slice()) else {
            return Err(DecodeError::Truncated);
        };
        let (&command, rest) = rest.split_first().ok_or(DecodeError::Truncated)?;
        let (&found, checked) = rest.split_last().ok_or(DecodeError::Truncated)?;
        if checked.len() <= ADDRESS_LEN {
            return Err(DecodeError::Truncated);
        }
        let computed = checksum(checked);
        if found != computed {
            return Err(DecodeError::Checksum { found, computed });
        }
        let (addr, payload) = checked.split_at(ADDRESS_LEN);
        let addr = Address::from_slice(addr).expect("the whole body was checked to be 7-bit");
        match command {
            DT1 => Ok(Incoming::Dt1 {
                device_id,
                addr,
                data: payload.to_vec(),
            }),
            RQ1 => match payload.try_into() {
                Ok(size) => Ok(Incoming::Rq1 {
                    device_id,
                    addr,
                    size: join7(size),
                }),
                Err(_) => Err(DecodeError::Truncated),
            },
            other => Err(DecodeError::UnknownCommand(other)),
        }
    }
}

fn decode_universal(body: &[u8]) -> Result<Incoming, DecodeError> {
    match body {
        [
            _,
            device_id,
            GENERAL_INFORMATION,
            IDENTITY_REPLY,
            manufacturer,
            f0,
            f1,
            m0,
            m1,
            r0,
            r1,
            r2,
            r3,
        ] => Ok(Incoming::IdentityReply(IdentityReply {
            device_id: *device_id,
            manufacturer: *manufacturer,
            family: [*f0, *f1],
            member: [*m0, *m1],
            revision: [*r0, *r1, *r2, *r3],
        })),
        _ if universal_identity_prefix(body) => Err(DecodeError::Truncated),
        _ => Ok(Incoming::Foreign),
    }
}

fn universal_identity_prefix(body: &[u8]) -> bool {
    body.first() == Some(&UNIVERSAL_NON_REALTIME)
        && body.get(2).is_none_or(|b| *b == GENERAL_INFORMATION)
        && body.get(3).is_none_or(|b| *b == IDENTITY_REPLY)
        && body.get(4).is_none_or(|b| *b == ROLAND)
        && body.len() < 13
}

#[derive(Clone, Debug)]
pub struct Request(Vec<u8>);

impl Request {
    pub fn identity() -> Self {
        Self(vec![
            F0,
            UNIVERSAL_NON_REALTIME,
            ALL_DEVICES,
            GENERAL_INFORMATION,
            IDENTITY_REQUEST,
            F7,
        ])
    }

    pub fn read(codec: &Codec, addr: Address, size: u32) -> Self {
        Self(codec.rq1(addr, size))
    }

    pub fn write(frame: &[u8]) -> Self {
        Self(frame.to_vec())
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Incoming {
    Dt1 {
        device_id: u8,
        addr: Address,
        data: Vec<u8>,
    },
    Rq1 {
        device_id: u8,
        addr: Address,
        size: u32,
    },
    IdentityReply(IdentityReply),
    Foreign,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IdentityReply {
    pub device_id: u8,
    pub manufacturer: u8,
    pub family: [u8; 2],
    pub member: [u8; 2],
    pub revision: [u8; 4],
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DecodeError {
    NotAFrame,
    Truncated,
    NonDataByte(u8),
    Checksum {
        found: u8,
        computed: u8,
    },
    UnknownCommand(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAFrame => write!(f, "not an F0..F7 frame"),
            Self::Truncated => write!(f, "frame ends mid-field"),
            Self::NonDataByte(b) => write!(f, "byte {b:#04x} inside the frame is not 7-bit"),
            Self::Checksum { found, computed } => {
                write!(f, "checksum {found:#04x}, computed {computed:#04x}")
            }
            Self::UnknownCommand(c) => write!(f, "unknown Roland command {c:#04x}"),
        }
    }
}

#[derive(Default)]
pub struct Reassembler {
    open: Option<Vec<u8>>,
    discarding: bool,
    abandoned: usize,
}

const MAX_FRAME: usize = 64 * 1024;

impl Reassembler {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        let mut done = Vec::new();
        for &b in chunk {
            match b {
                // A fresh start abandons whatever was torn off before it.
                F0 => {
                    self.abandoned += usize::from(self.open.is_some());
                    self.open = Some(vec![F0]);
                    self.discarding = false;
                }
                F7 if self.discarding => self.discarding = false,
                F7 => {
                    match self.open.take() {
                        Some(mut frame) => {
                            frame.push(F7);
                            done.push(frame);
                        }
                        // A terminator with no start: the beginning of that frame
                        // was lost before we were listening.
                        None => self.abandoned += 1,
                    }
                }
                _ => {
                    if self.discarding {
                        continue;
                    }
                    if let Some(frame) = self.open.as_mut() {
                        frame.push(b);
                        if frame.len() > MAX_FRAME {
                            self.open = None;
                            self.discarding = true;
                            self.abandoned += 1;
                        }
                    }
                }
            }
        }
        done
    }

    pub fn abandon_open(&mut self) {
        if self.open.take().is_some() {
            self.discarding = true;
            self.abandoned += 1;
        }
    }

    pub fn abandoned(&self) -> usize {
        self.abandoned
    }
}

fn split7(v: u32) -> [u8; 4] {
    [
        (v >> 21) as u8 & 0x7F,
        (v >> 14) as u8 & 0x7F,
        (v >> 7) as u8 & 0x7F,
        v as u8 & 0x7F,
    ]
}

fn join7(bytes: [u8; 4]) -> u32 {
    bytes
        .iter()
        .fold(0u32, |acc, b| (acc << 7) | u32::from(*b & 0x7F))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER_VOLUME_100_DT1: &str = "f0 41 10 00 00 00 28 12 01 00 02 13 64 06 f7";
    const CONNECTION_DT1: &str = "f0 41 10 00 00 00 28 12 01 00 03 06 01 75 f7";
    const METRONOME_STATUS_RQ1: &str = "f0 41 10 00 00 00 28 11 01 00 01 0f 00 00 00 01 6e f7";
    const TEMPO_RQ1: &str = "f0 41 10 00 00 00 28 11 01 00 01 08 00 00 00 02 74 f7";
    const MASTER_VOLUME_RQ1: &str = "f0 41 10 00 00 00 28 11 01 00 02 13 00 00 00 01 69 f7";
    const IDENTITY_REPLY_FRAME: &str = "f0 7e 10 06 02 41 19 03 00 00 1c 01 00 00 f7";

    fn codec() -> Codec {
        Codec::new(vec![0x00, 0x00, 0x00, 0x28], 0x10)
    }

    fn addr(s: &str) -> Address {
        Address::try_from(s.to_string()).unwrap()
    }

    fn bytes(s: &str) -> Vec<u8> {
        parse_hex(s).unwrap()
    }

    #[test]
    fn checksum_completes_the_sum_to_a_multiple_of_128() {
        for target in 0..1024u32 {
            let mut payload = Vec::new();
            let mut rest = target;
            while rest > 0 {
                let b = rest.min(0x7F);
                payload.push(b as u8);
                rest -= b;
            }
            assert_eq!(
                (target + u32::from(checksum(&payload))) % 128,
                0,
                "sum {target}"
            );
        }
        assert_eq!(checksum(&[0x00]), 0x00);
        assert_eq!(checksum(&[0x40, 0x40]), 0x00);
    }

    #[test]
    fn rq1_reproduces_documented_requests() {
        let c = codec();
        assert_eq!(
            Request::read(&c, addr("01 00 01 0F"), 1).bytes(),
            bytes(METRONOME_STATUS_RQ1)
        );
        assert_eq!(
            Request::read(&c, addr("01 00 01 08"), 2).bytes(),
            bytes(TEMPO_RQ1)
        );
        assert_eq!(
            Request::read(&c, addr("01 00 02 13"), 1).bytes(),
            bytes(MASTER_VOLUME_RQ1)
        );
    }

    #[test]
    fn dt1_reproduces_documented_writes() {
        let c = codec();
        assert_eq!(
            c.dt1(addr("01 00 02 13"), &[100]),
            bytes(MASTER_VOLUME_100_DT1)
        );
        assert_eq!(c.dt1(addr("01 00 03 06"), &[1]), bytes(CONNECTION_DT1));
    }

    #[test]
    fn identity_request_is_the_universal_broadcast() {
        assert_eq!(Request::identity().bytes(), bytes("f0 7e 7f 06 01 f7"));
    }

    #[test]
    fn decodes_documented_frames() {
        let c = codec();
        assert_eq!(
            c.decode(&bytes(MASTER_VOLUME_100_DT1)),
            Ok(Incoming::Dt1 {
                device_id: 0x10,
                addr: addr("01 00 02 13"),
                data: vec![100],
            })
        );
        assert_eq!(
            c.decode(&bytes(TEMPO_RQ1)),
            Ok(Incoming::Rq1 {
                device_id: 0x10,
                addr: addr("01 00 01 08"),
                size: 2,
            })
        );
        assert_eq!(
            c.decode(&bytes(IDENTITY_REPLY_FRAME)),
            Ok(Incoming::IdentityReply(IdentityReply {
                device_id: 0x10,
                manufacturer: 0x41,
                family: [0x19, 0x03],
                member: [0x00, 0x00],
                revision: [0x1C, 0x01, 0x00, 0x00],
            }))
        );
    }

    #[test]
    fn rejects_a_frame_whose_checksum_does_not_hold() {
        let mut frame = bytes(MASTER_VOLUME_100_DT1);
        let sum = frame.len() - 2;
        frame[sum] = 0x07;
        assert_eq!(
            codec().decode(&frame),
            Err(DecodeError::Checksum {
                found: 0x07,
                computed: 0x06,
            })
        );
    }

    #[test]
    fn other_devices_stay_foreign_rather_than_being_misread() {
        let c = codec();
        assert_eq!(
            c.decode(&bytes("f0 41 10 42 12 40 00 7f 00 41 f7")),
            Ok(Incoming::Foreign)
        );
        assert_eq!(
            c.decode(&bytes("f0 43 10 4c 00 00 7e 00 f7")),
            Ok(Incoming::Foreign)
        );
        assert_eq!(
            c.decode(&bytes("f0 7f 7f 04 01 00 64 f7")),
            Ok(Incoming::Foreign)
        );
        assert_eq!(c.decode(&bytes("f0 7e 7f 06 01 f7")), Ok(Incoming::Foreign));
        assert_eq!(
            c.decode(&bytes("f0 7e 10 06 02 00 20 33 19 03 00 00 1c 01 00 00 f7")),
            Ok(Incoming::Foreign)
        );
    }

    #[test]
    fn a_torn_identity_reply_is_not_classified_as_foreign() {
        assert_eq!(
            codec().decode(&bytes("f0 7e 10 06 02 41 19 03 f7")),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn rejects_malformed_frames() {
        let c = codec();
        assert_eq!(c.decode(&bytes("41 10 00")), Err(DecodeError::NotAFrame));
        assert_eq!(
            c.decode(&bytes("f0 41 10 00 00 00 28 12 01 00 02 f7")),
            Err(DecodeError::Truncated)
        );
        assert_eq!(
            c.decode(&bytes("f0 41 10 00 00 00 28 13 01 00 02 13 64 06 f7")),
            Err(DecodeError::UnknownCommand(0x13))
        );
        assert_eq!(
            c.decode(&bytes("f0 41 10 00 00 00 28 12 01 00 02 13 f8 06 f7")),
            Err(DecodeError::NonDataByte(0xF8))
        );
    }

    #[test]
    fn reassembles_frames_split_the_way_alsa_splits_them() {
        let mut r = Reassembler::default();
        let long: Vec<u8> = [F0]
            .into_iter()
            .chain(std::iter::repeat_n(0x01, 300))
            .chain([F7])
            .collect();
        assert!(r.push(&long[..256]).is_empty());
        assert_eq!(r.push(&long[256..]), vec![long.clone()]);
    }

    #[test]
    fn reassembles_several_frames_from_one_chunk() {
        let mut r = Reassembler::default();
        let a = bytes(METRONOME_STATUS_RQ1);
        let b = bytes(TEMPO_RQ1);
        let joined: Vec<u8> = a.iter().chain(b.iter()).copied().collect();
        assert_eq!(r.push(&joined), vec![a, b]);
    }

    #[test]
    fn a_new_start_abandons_a_torn_frame_and_counts_it() {
        let mut r = Reassembler::default();
        let good = bytes(TEMPO_RQ1);
        assert!(r.push(&bytes("f0 41 10 00 00")).is_empty());
        assert_eq!(r.push(&good), vec![good]);
        assert_eq!(r.abandoned(), 1);
    }

    #[test]
    fn a_frame_without_end_is_dropped_rather_than_grown_without_bound() {
        let mut r = Reassembler::default();
        let flood: Vec<u8> = [F0]
            .into_iter()
            .chain(std::iter::repeat_n(0x01, MAX_FRAME + 1))
            .collect();
        assert!(r.push(&flood).is_empty());
        assert_eq!(r.abandoned(), 1);
        let good = bytes(TEMPO_RQ1);
        assert_eq!(r.push(&good), vec![good]);
    }

    #[test]
    fn the_tail_of_an_abandoned_frame_is_not_counted_twice() {
        let mut r = Reassembler::default();
        assert!(r.push(&bytes("f0 41 10 00 00")).is_empty());
        r.abandon_open();
        assert!(r.push(&bytes("01 02 f7")).is_empty());
        assert_eq!(r.abandoned(), 1);

        let flood: Vec<u8> = [F0]
            .into_iter()
            .chain(std::iter::repeat_n(0x01, MAX_FRAME + 1))
            .collect();
        assert!(r.push(&flood).is_empty());
        assert!(r.push(&[0x01, F7]).is_empty());
        assert_eq!(r.abandoned(), 2);
    }

    #[test]
    fn a_stray_terminator_is_a_lost_frame_not_a_non_event() {
        let mut r = Reassembler::default();
        assert!(r.push(&[F7]).is_empty());
        assert_eq!(r.abandoned(), 1);
    }

    #[test]
    fn addresses_round_trip_through_their_text_form() {
        assert_eq!(addr("01 00 01 0F").to_string(), "01 00 01 0F");
        assert_eq!(addr("0100010f").bytes(), [0x01, 0x00, 0x01, 0x0F]);
        assert!(Address::try_from("01 00 01".to_string()).is_err());
        assert!(Address::try_from("01 00 01 8F".to_string()).is_err());
        assert!(Address::try_from("0 100 010 F".to_string()).is_err());
        assert!(Address::try_from("01\t\t00 01 0F".to_string()).is_err());
    }

    #[test]
    fn sizes_span_four_seven_bit_bytes() {
        assert_eq!(split7(1), [0, 0, 0, 1]);
        assert_eq!(split7(128), [0, 0, 1, 0]);
        assert_eq!(join7(split7(300)), 300);
    }
}
