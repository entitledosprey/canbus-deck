//! Groups IDs by likely sender. Modules run on separate crystals, so periodic IDs from
//! one module share a clock offset. This shows which IDs travel together, not which
//! module sent them.

use crate::compare::{parse_log, Key, Series};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

const MIN_FRAMES: usize = 50;
/// IDs measured worse than this are left out rather than grouped on noise.
const MAX_ERROR_PPM: f64 = 3.0;
/// Real crystals sit well inside this. Further from a whole millisecond means the
/// nominal period is unknown, so no offset can be measured.
const MAX_OFFSET_PPM: f64 = 1000.0;
/// IDs this close in offset share a group.
const GROUP_GAP_PPM: f64 = 6.0;
/// Gaps of several periods should land on a whole number of periods if frames were
/// merely dropped. Too many that do not means the ID pauses and restarts at random.
const MAX_OFF_PHASE_GAPS: f64 = 0.01;
const OFF_PHASE: f64 = 0.3;
/// Timing error is not white noise, so short captures look more certain than they are.
/// On a real capture, 20 s misgrouped IDs, 40 s got the fast IDs right, and 1 Hz IDs
/// needed about 90 s.
pub const MIN_SPAN_US: u64 = 40_000_000;
const SLOW_ID_SPAN_US: u64 = 90_000_000;
const QUIET_MIN_US: u64 = 5_000_000;
const QUIET_MERGE_US: u64 = 1_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Clock {
    pub key: Key,
    pub period_ms: f64,
    pub offset_ppm: f64,
}

/// Fits a line through the frame times. Each frame is numbered by rounding the gap to
/// the median interval, so dropped frames do not count as a slower clock.
fn measure(key: Key, times: &[u64]) -> Option<Clock> {
    if times.len() < MIN_FRAMES {
        return None;
    }
    let mut gaps: Vec<u64> = times.windows(2).map(|w| w[1].saturating_sub(w[0])).collect();
    gaps.sort_unstable();
    let median = gaps[gaps.len() / 2] as f64;
    if median <= 0.0 {
        return None;
    }

    let t0 = times[0];
    let mut index = 0.0f64;
    let mut prev = t0;
    let mut points = Vec::with_capacity(times.len());
    let mut off_phase = 0usize;
    for &t in times {
        let periods = (t - prev) as f64 / median;
        if periods.round() >= 2.0 && (periods - periods.round()).abs() > OFF_PHASE {
            off_phase += 1;
        }
        index += periods.round();
        prev = t;
        points.push((index, (t - t0) as f64));
    }

    if off_phase as f64 > MAX_OFF_PHASE_GAPS * times.len() as f64 {
        return None;
    }

    let n = points.len() as f64;
    let (mx, my) = (points.iter().map(|p| p.0).sum::<f64>() / n, points.iter().map(|p| p.1).sum::<f64>() / n);
    let sxx: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
    if sxx <= 0.0 {
        return None;
    }
    let period_us = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>() / sxx;
    let residual: f64 = points.iter().map(|p| (p.1 - (my + (p.0 - mx) * period_us)).powi(2)).sum();
    let error_ppm = (residual / (n - 2.0)).sqrt() / sxx.sqrt() / period_us * 1e6;

    let period_ms = period_us / 1000.0;
    let nominal = period_ms.round();
    if nominal < 1.0 || error_ppm > MAX_ERROR_PPM {
        return None;
    }
    let offset_ppm = (period_ms / nominal - 1.0) * 1e6;
    (offset_ppm.abs() <= MAX_OFFSET_PPM).then_some(Clock { key, period_ms, offset_ppm })
}

pub fn clocks(series: &Series) -> Vec<Clock> {
    let mut out: Vec<Clock> = series
        .iter()
        .filter_map(|(&key, frames)| measure(key, &frames.iter().map(|f| f.0).collect::<Vec<_>>()))
        .collect();
    out.sort_by(|a, b| a.offset_ppm.total_cmp(&b.offset_ppm));
    out
}

pub fn group(sorted: &[Clock]) -> Vec<Vec<Clock>> {
    let mut groups: Vec<Vec<Clock>> = Vec::new();
    for clock in sorted {
        match groups.last_mut() {
            Some(g) if clock.offset_ppm - g[g.len() - 1].offset_ppm <= GROUP_GAP_PPM => g.push(clock.clone()),
            _ => groups.push(vec![clock.clone()]),
        }
    }
    groups
}

/// IDs that stopped before the capture ended, as (seconds into capture, IDs).
pub fn went_quiet(series: &Series) -> Vec<(u64, Vec<Key>)> {
    let Some(first) = series.values().filter_map(|f| f.first()).map(|f| f.0).min() else { return Vec::new() };
    let end = series.values().filter_map(|f| f.last()).map(|f| f.0).max().unwrap_or(first);

    let mut quiet: Vec<(u64, Key)> = series
        .iter()
        .filter_map(|(&key, frames)| {
            let last = frames.last()?.0;
            let span = last - frames.first()?.0;
            let period = span / (frames.len() as u64).saturating_sub(1).max(1);
            (frames.len() >= 2 && end - last > QUIET_MIN_US.max(period * 10)).then_some((last, key))
        })
        .collect();
    quiet.sort();

    let mut out: Vec<(u64, u64, Vec<Key>)> = Vec::new();
    for (last, key) in quiet {
        match out.last_mut() {
            Some((_, newest, keys)) if last - *newest <= QUIET_MERGE_US => {
                *newest = last;
                keys.push(key);
            }
            _ => out.push((last, last, vec![key])),
        }
    }
    out.into_iter().map(|(start, _, keys)| ((start - first) / 1_000_000, keys)).collect()
}

fn id_text(key: Key) -> String {
    if key.0 { format!("{:08X}", key.1) } else { format!("{:03X}", key.1) }
}

fn sorted_ids(keys: impl Iterator<Item = Key>) -> String {
    keys.collect::<BTreeSet<_>>().into_iter().map(id_text).collect::<Vec<_>>().join(" ")
}

fn span_us(series: &Series) -> u64 {
    let first = series.values().filter_map(|f| f.first()).map(|f| f.0).min().unwrap_or(0);
    let last = series.values().filter_map(|f| f.last()).map(|f| f.0).max().unwrap_or(0);
    last - first
}

pub fn render(series: &Series) -> String {
    if span_us(series) < MIN_SPAN_US {
        return format!(
            "== Likely senders\n   Capture too short: {} s. Needs at least {} s to measure clocks reliably.\n",
            span_us(series) / 1_000_000,
            MIN_SPAN_US / 1_000_000
        );
    }
    let clocks = clocks(series);
    let groups = group(&clocks);
    let mut out = String::from("== Likely senders (IDs that share a clock)\n");
    let mut letter = b'A';
    let mut alone = Vec::new();
    for g in &groups {
        if g.len() < 2 {
            alone.push(g[0].key);
            continue;
        }
        let mean = g.iter().map(|c| c.offset_ppm).sum::<f64>() / g.len() as f64;
        let _ = writeln!(out, "   {}  {mean:>+7.0} ppm   {}", letter as char, sorted_ids(g.iter().map(|c| c.key)));
        letter = if letter == b'Z' { b'Z' } else { letter + 1 };
    }
    if letter == b'A' && alone.is_empty() {
        let _ = writeln!(out, "   Not enough steady traffic to measure. Longer captures measure better.");
    }
    if !alone.is_empty() {
        let _ = writeln!(out, "   Alone: {}", sorted_ids(alone.into_iter()));
    }
    let measured: BTreeSet<Key> = clocks.iter().map(|c| c.key).collect();
    let unmeasured: Vec<Key> = series.keys().copied().filter(|k| !measured.contains(k)).collect();
    if !unmeasured.is_empty() {
        let _ = writeln!(out, "   Too irregular to measure: {}", sorted_ids(unmeasured.into_iter()));
    }
    for (secs, keys) in went_quiet(series) {
        let _ = writeln!(out, "   Went quiet at {secs} s: {}", sorted_ids(keys.into_iter()));
    }
    if span_us(series) < SLOW_ID_SPAN_US {
        let _ = writeln!(out, "   Slow IDs need about {} s to group reliably.", SLOW_ID_SPAN_US / 1_000_000);
    }
    let _ = writeln!(out, "   Groups are evidence of a shared sender, not proof, and they do not name the module.");
    out
}

pub fn run(capture: &Path) -> Result<String> {
    let text = fs::read_to_string(capture).with_context(|| format!("reading {}", capture.display()))?;
    Ok(render(&parse_log(&text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic jitter of up to about +/- 500 us.
    fn jitter(i: u64) -> i64 {
        (i.wrapping_mul(2_654_435_761) % 1000) as i64 - 500
    }

    fn frames(period_us: f64, count: u64, salt: u64) -> Vec<(u64, [u8; 8], u8)> {
        (0..count)
            .map(|i| ((1_000_000.0 + i as f64 * period_us) as i64 + jitter(i + salt)) as u64)
            .map(|t| (t, [0; 8], 8))
            .collect()
    }

    fn id(n: u32) -> Key {
        (false, n)
    }

    #[test]
    fn groups_by_clock_across_different_rates() {
        let mut s = Series::new();
        // Clock A is +0 ppm, clock B is -100 ppm, at mixed periods.
        s.insert(id(0x140), frames(10_000.0, 20_000, 1));
        s.insert(id(0x360), frames(50_000.0, 4_000, 2));
        s.insert(id(0x0D0), frames(20_000.0 * (1.0 - 100e-6), 10_000, 3));
        s.insert(id(0x282), frames(60_000.0 * (1.0 - 100e-6), 3_400, 4));
        let groups = group(&clocks(&s));
        assert_eq!(groups.len(), 2, "{groups:?}");
        let keys = |g: &Vec<Clock>| g.iter().map(|c| c.key.1).collect::<BTreeSet<_>>();
        assert_eq!(keys(&groups[0]), [0x0D0, 0x282].into());
        assert_eq!(keys(&groups[1]), [0x140, 0x360].into());
        assert!((groups[0][0].offset_ppm + 100.0).abs() < 2.0, "{groups:?}");
    }

    #[test]
    fn dropped_frames_do_not_shift_the_clock() {
        let all = frames(10_000.0, 20_000, 1);
        let kept: Vec<u64> = all.iter().enumerate().filter(|(i, _)| i % 97 != 0).map(|(_, f)| f.0).collect();
        let c = measure(id(1), &kept).expect("measured");
        assert!(c.offset_ppm.abs() < 2.0, "{c:?}");
    }

    #[test]
    fn irregular_and_short_ids_are_left_out() {
        // Bursts: mostly 10 ms apart with long random pauses.
        let mut t = 0u64;
        let bursty: Vec<u64> = (0..2000u64)
            .map(|i| {
                t += 10_000 + if i % 7 == 0 { i.wrapping_mul(2_654_435_761) % 3_000_000 } else { 0 };
                t
            })
            .collect();
        assert!(measure(id(1), &bursty).is_none());
        assert!(measure(id(2), &[1000, 2000, 3000]).is_none());
    }

    #[test]
    fn reports_ids_that_stop_early() {
        let mut s = Series::new();
        s.insert(id(0x140), frames(10_000.0, 10_000, 1));
        s.insert(id(0x0D0), frames(20_000.0, 5_000, 2));
        s.insert(id(0x152), frames(20_000.0, 6_000, 3));
        assert_eq!(went_quiet(&s), vec![(99, vec![id(0x0D0), id(0x140)])]);
    }

    #[test]
    fn short_capture_is_refused_instead_of_guessed() {
        let mut s = Series::new();
        let frames_in_span = MIN_SPAN_US / 10_000 / 2;
        s.insert(id(0x140), frames(10_000.0, frames_in_span, 1));
        s.insert(id(0x141), frames(10_000.0, frames_in_span, 2));
        let text = render(&s);
        assert!(text.contains("too short"), "{text}");
        assert!(!text.contains("140"), "{text}");
    }

    #[test]
    fn empty_capture_renders() {
        assert!(render(&Series::new()).contains("too short"));
    }
}
