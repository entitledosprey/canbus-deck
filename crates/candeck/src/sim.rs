//! Simulated bus for vcan. The IDs and layouts are invented and do not represent any car.

use crate::can::VcanTx;
use anyhow::Result;
use std::time::{Duration, Instant};

struct Msg {
    id: u32,
    ext: bool,
    period_ms: u64,
    next_ms: u64,
    count: u8,
}

fn payload(id: u32, count: u8, t: f64) -> [u8; 8] {
    let wave = |period: f64, amp: f64, base: f64| (base + amp * (t * std::f64::consts::TAU / period).sin()) as u16;
    let mut d = [0u8; 8];
    match id {
        0x100 => {
            d[..2].copy_from_slice(&wave(9.0, 2500.0, 3500.0).to_be_bytes());
            d[6] = count & 0x0F;
            d[7] = d[..7].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        }
        0x120 => {
            d[0] = wave(6.0, 40.0, 50.0) as u8;
            d[6] = count;
        }
        0x200 => d[2..4].copy_from_slice(&wave(14.0, 900.0, 1100.0).to_le_bytes()),
        0x360 => d[0] = u8::from(t % 8.0 < 2.0),
        0x4A0 => d[..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]),
        _ => d[0] = count,
    }
    d
}

pub fn run(iface: &str, secs: Option<u64>) -> Result<()> {
    let tx = VcanTx::open(iface)?;
    let mut msgs: Vec<Msg> = [(0x100, false, 10), (0x120, false, 20), (0x200, false, 50), (0x360, false, 100), (0x4A0, false, 1000), (0x18DA_F110, true, 200)]
        .into_iter()
        .map(|(id, ext, period_ms)| Msg { id, ext, period_ms, next_ms: 0, count: 0 })
        .collect();
    let start = Instant::now();
    println!("simulating traffic on {iface}. These IDs are invented, not from any vehicle.");
    while secs.is_none_or(|s| start.elapsed().as_secs() < s) {
        let now_ms = start.elapsed().as_millis() as u64;
        for m in &mut msgs {
            if now_ms >= m.next_ms {
                tx.send(m.id, m.ext, &payload(m.id, m.count, now_ms as f64 / 1000.0))?;
                m.count = m.count.wrapping_add(1);
                m.next_ms += m.period_ms;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
