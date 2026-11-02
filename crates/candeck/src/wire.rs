//! Fixed-size records the daemon streams to its local clients.

use crate::can::Frame;

pub const RECORD: usize = 24;
const KIND_FRAME: u8 = 0;
const KIND_STATUS: u8 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Status {
    pub kernel_drops: u32,
    pub log_drops: u64,
    pub client_drops: u64,
}

#[derive(Debug, PartialEq)]
pub enum Record {
    Frame(Frame),
    Status(Status),
}

pub fn encode_frame(f: &Frame) -> [u8; RECORD] {
    let mut r = [0u8; RECORD];
    r[0] = KIND_FRAME;
    r[1] = f.ext as u8 | (f.err as u8) << 1 | (f.rtr as u8) << 2;
    r[2] = f.dlc;
    r[4..8].copy_from_slice(&f.id.to_le_bytes());
    r[8..16].copy_from_slice(&f.ts_us.to_le_bytes());
    r[16..24].copy_from_slice(&f.data);
    r
}

pub fn encode_status(s: &Status) -> [u8; RECORD] {
    let mut r = [0u8; RECORD];
    r[0] = KIND_STATUS;
    r[4..8].copy_from_slice(&s.kernel_drops.to_le_bytes());
    r[8..16].copy_from_slice(&s.log_drops.to_le_bytes());
    r[16..24].copy_from_slice(&s.client_drops.to_le_bytes());
    r
}

pub fn decode(r: &[u8; RECORD]) -> Option<Record> {
    let u32_at = u32::from_le_bytes(r[4..8].try_into().ok()?);
    let u64_a = u64::from_le_bytes(r[8..16].try_into().ok()?);
    match r[0] {
        KIND_FRAME => Some(Record::Frame(Frame {
            ts_us: u64_a,
            id: u32_at,
            ext: r[1] & 1 != 0,
            err: r[1] & 2 != 0,
            rtr: r[1] & 4 != 0,
            dlc: r[2],
            data: r[16..24].try_into().ok()?,
        })),
        KIND_STATUS => Some(Record::Status(Status {
            kernel_drops: u32_at,
            log_drops: u64_a,
            client_drops: u64::from_le_bytes(r[16..24].try_into().ok()?),
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let f = Frame { ts_us: 42, id: 0x18DA_F110, ext: true, err: false, rtr: false, dlc: 3, data: [1, 2, 3, 0, 0, 0, 0, 0] };
        assert_eq!(decode(&encode_frame(&f)), Some(Record::Frame(f)));
    }

    #[test]
    fn status_round_trip() {
        let s = Status { kernel_drops: 7, log_drops: 8, client_drops: 9 };
        assert_eq!(decode(&encode_status(&s)), Some(Record::Status(s)));
    }
}
