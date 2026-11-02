//! Finds what changed during marked actions. The standard test is one block per action:
//! start the mark, do the action as many times as you like, stop the mark.
//!
//! Two checks run on each block. The pulse scan finds bits that switch on and off inside
//! the block and stay still elsewhere. The level check compares the block with the seconds
//! around it, so slow drift like warm-up does not score. It also finds gradual values.

use crate::daemon::marker_path;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const SURROUND_US: u64 = 10_000_000;
/// Excluded around each edge, since a key press never lines up exactly with the signal.
const GUARD_US: u64 = 200_000;
const MIN_ACTION_FRAMES: usize = 2;
const MIN_BASE_FRAMES: usize = 5;
const MIN_BIT_SCORE: f64 = 0.6;
const MIN_BYTE_SCORE: f64 = 0.5;
const MAX_ROWS: usize = 15;
/// An edge this close to a block still counts as inside it, for key press timing.
const PULSE_EDGE_TOLERANCE_US: u64 = 500_000;
/// More edges than this in a block is a noisy bit, not a response to an action.
const MAX_PULSE_EDGES: usize = 40;
/// Once a bit shows at least two full pulses in a block, it may also change elsewhere up
/// to half as often, because real controls get used outside their test. A clutch bit, for
/// example, also fires when the clutch is held to start the engine.
const MIN_EDGES_ALLOWING_STRAY: usize = 4;
const SAME_TIMING_US: u64 = 200_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub label: String,
    pub start: u64,
    pub end: u64,
}

pub type Key = (bool, u32);
pub type Series = BTreeMap<Key, Vec<(u64, [u8; 8], u8)>>;

/// Returns the closed windows and any warnings about markers that could not be paired.
pub fn parse_markers(text: &str) -> (Vec<Window>, Vec<String>) {
    let mut open: BTreeMap<String, u64> = BTreeMap::new();
    let (mut windows, mut warnings) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        let [ts, verb, label] = parts[..] else {
            warnings.push(format!("ignored marker line: {line}"));
            continue;
        };
        let Ok(ts) = ts.parse::<u64>() else {
            warnings.push(format!("ignored marker line: {line}"));
            continue;
        };
        match verb {
            "start" => {
                if open.insert(label.to_string(), ts).is_some() {
                    warnings.push(format!("{label}: started twice, using the later start"));
                }
            }
            "stop" => match open.remove(label) {
                Some(start) if ts > start => windows.push(Window { label: label.to_string(), start, end: ts }),
                _ => warnings.push(format!("{label}: stop with no matching start")),
            },
            _ => warnings.push(format!("ignored marker line: {line}")),
        }
    }
    for label in open.keys() {
        warnings.push(format!("{label}: started but never stopped, ignored"));
    }
    windows.sort_by_key(|w| w.start);
    (windows, warnings)
}

pub fn parse_log(text: &str) -> Series {
    let mut series = Series::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(ts), Some(_), Some(body)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Some((secs, frac)) = ts.trim_start_matches('(').trim_end_matches(')').split_once('.') else { continue };
        let (Ok(secs), Ok(frac)) = (secs.parse::<u64>(), frac.parse::<u64>()) else { continue };
        let Some((id, hex)) = body.split_once('#') else { continue };
        let Ok(id) = u32::from_str_radix(id, 16) else { continue };
        let ext = body.find('#') == Some(8);
        // Error frames carry the error flag in the ID. Remote frames have no data.
        if id & 0x2000_0000 != 0 || hex.starts_with('R') || hex.len() % 2 != 0 || hex.len() > 16 {
            continue;
        }
        let mut data = [0u8; 8];
        let mut ok = true;
        for i in 0..hex.len() / 2 {
            match u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16) {
                Ok(b) => data[i] = b,
                Err(_) => ok = false,
            }
        }
        if ok {
            series.entry((ext, id)).or_default().push((secs * 1_000_000 + frac, data, (hex.len() / 2) as u8));
        }
    }
    series
}

#[derive(Debug, PartialEq)]
pub struct BitHit {
    pub key: Key,
    pub byte: usize,
    pub bit: u8,
    pub score: f64,
    pub rises: bool,
    pub action_pct: f64,
    pub base_pct: f64,
}

#[derive(Debug, PartialEq)]
pub struct ByteHit {
    pub key: Key,
    pub byte: usize,
    pub score: f64,
    pub base: (u8, u8),
    pub action: (u8, u8),
}

#[derive(Debug, PartialEq)]
pub struct PulseHit {
    pub key: Key,
    pub byte: usize,
    pub bit: u8,
    pub edges: Vec<u64>,
    pub stray: usize,
}

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub pulses: Vec<Vec<PulseHit>>,
    pub bits: Vec<BitHit>,
    pub bytes: Vec<ByteHit>,
}

fn in_any(t: u64, spans: &[(u64, u64)]) -> bool {
    spans.iter().any(|&(a, b)| t >= a && t <= b)
}

/// Splits one ID's frames into (action, surrounding baseline) samples for a window.
fn split<'a>(
    frames: &'a [(u64, [u8; 8], u8)],
    w: &Window,
    keepout: &[(u64, u64)],
) -> (Vec<&'a [u8; 8]>, Vec<&'a [u8; 8]>) {
    // Very short actions keep every frame, or the guard would remove them all.
    let (a0, a1) = if w.end - w.start > GUARD_US * 3 { (w.start + GUARD_US, w.end - GUARD_US) } else { (w.start, w.end) };
    let from = w.start.saturating_sub(SURROUND_US);
    let to = w.end + SURROUND_US;
    let first = frames.partition_point(|f| f.0 < from);
    let (mut action, mut base) = (Vec::new(), Vec::new());
    for f in frames[first..].iter().take_while(|f| f.0 <= to) {
        if f.0 >= a0 && f.0 <= a1 {
            action.push(&f.1);
        } else if !in_any(f.0, keepout) {
            base.push(&f.1);
        }
    }
    (action, base)
}

/// Finds bits that switch inside every block of this action and stay still elsewhere,
/// grouped so bits with identical timing are shown together.
pub fn pulse_scan(series: &Series, targets: &[&Window]) -> Vec<Vec<PulseHit>> {
    let inside = |t: u64, w: &Window| t + PULSE_EDGE_TOLERANCE_US >= w.start && t <= w.end + PULSE_EDGE_TOLERANCE_US;
    let mut hits = Vec::new();
    for (&key, frames) in series {
        let dlc = frames.iter().map(|f| f.2).max().unwrap_or(0) as usize;
        for byte in 0..dlc.min(8) {
            for bit in 0..8u8 {
                let edges: Vec<u64> = frames
                    .windows(2)
                    .filter(|w| (w[0].1[byte] ^ w[1].1[byte]) >> bit & 1 == 1)
                    .map(|w| w[1].0)
                    .collect();
                if edges.is_empty() {
                    continue;
                }
                let (within, outside): (Vec<u64>, Vec<u64>) = edges.iter().partition(|&&t| targets.iter().any(|w| inside(t, w)));
                let in_every_block = targets.iter().all(|w| within.iter().filter(|&&t| inside(t, w)).count() >= 2);
                let allowed_stray = if within.len() >= MIN_EDGES_ALLOWING_STRAY { within.len() / 2 } else { 0 };
                if in_every_block && within.len() <= MAX_PULSE_EDGES && outside.len() <= allowed_stray {
                    hits.push(PulseHit { key, byte, bit, edges: within, stray: outside.len() });
                }
            }
        }
    }

    let same_timing = |a: &PulseHit, b: &PulseHit| {
        a.edges.len() == b.edges.len() && a.edges.iter().zip(&b.edges).all(|(x, y)| x.abs_diff(*y) <= SAME_TIMING_US)
    };
    let mut groups: Vec<Vec<PulseHit>> = Vec::new();
    for hit in hits {
        match groups.iter_mut().find(|g| same_timing(&g[0], &hit)) {
            Some(g) => g.push(hit),
            None => groups.push(vec![hit]),
        }
    }
    groups.sort_by_key(|g| (std::cmp::Reverse(g.len()), std::cmp::Reverse(g[0].edges.len())));
    groups
}

pub fn analyze(series: &Series, all: &[Window], label: &str) -> Report {
    let targets: Vec<&Window> = all.iter().filter(|w| w.label == label).collect();
    let keepout: Vec<(u64, u64)> = all.iter().map(|w| (w.start.saturating_sub(GUARD_US), w.end + GUARD_US)).collect();
    let mut report = Report::default();
    if targets.is_empty() {
        return report;
    }
    report.pulses = pulse_scan(series, &targets);

    for (&key, frames) in series {
        let splits: Vec<_> = targets.iter().map(|w| split(frames, w, &keepout)).collect();
        if splits.iter().any(|(a, b)| a.len() < MIN_ACTION_FRAMES || b.len() < MIN_BASE_FRAMES) {
            continue;
        }
        let dlc = frames.iter().map(|f| f.2).max().unwrap_or(0) as usize;
        for byte in 0..dlc.min(8) {
            for bit in 0..8u8 {
                let rate = |s: &[&[u8; 8]]| s.iter().filter(|d| d[byte] >> bit & 1 == 1).count() as f64 / s.len() as f64;
                let deltas: Vec<(f64, f64)> = splits.iter().map(|(a, b)| (rate(a), rate(b))).collect();
                let rises = deltas.iter().all(|(a, b)| a > b);
                let falls = deltas.iter().all(|(a, b)| a < b);
                if !rises && !falls {
                    continue;
                }
                let score = deltas.iter().map(|(a, b)| (a - b).abs()).fold(f64::MAX, f64::min);
                if score >= MIN_BIT_SCORE {
                    let n = deltas.len() as f64;
                    report.bits.push(BitHit {
                        key,
                        byte,
                        bit,
                        score,
                        rises,
                        action_pct: deltas.iter().map(|d| d.0).sum::<f64>() / n * 100.0,
                        base_pct: deltas.iter().map(|d| d.1).sum::<f64>() / n * 100.0,
                    });
                }
            }

            let mut score = f64::MAX;
            let (mut base_range, mut action_range) = ((u8::MAX, 0u8), (u8::MAX, 0u8));
            for (a, b) in &splits {
                let lo = b.iter().map(|d| d[byte]).min().unwrap_or(0);
                let hi = b.iter().map(|d| d[byte]).max().unwrap_or(0);
                let outside = a.iter().filter(|d| d[byte] < lo || d[byte] > hi).count() as f64 / a.len() as f64;
                score = score.min(outside);
                base_range = (base_range.0.min(lo), base_range.1.max(hi));
                for d in a {
                    action_range = (action_range.0.min(d[byte]), action_range.1.max(d[byte]));
                }
            }
            if score >= MIN_BYTE_SCORE {
                report.bytes.push(ByteHit { key, byte, score, base: base_range, action: action_range });
            }
        }
    }
    report.bits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.key.cmp(&b.key)));
    report.bytes.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.key.cmp(&b.key)));
    report
}

fn id_text(key: Key) -> String {
    if key.0 { format!("{:08X}", key.1) } else { format!("{:03X}", key.1) }
}

pub fn render(label: &str, windows: usize, report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "== {label}: {windows} block{}", if windows == 1 { "" } else { "s" });
    if report.pulses.is_empty() && report.bits.is_empty() && report.bytes.is_empty() {
        let _ = writeln!(out, "   Nothing changed consistently.");
        return out;
    }
    if !report.pulses.is_empty() {
        let _ = writeln!(out, "   Bits that pulsed only during this action. Check the count against what you did:");
        for group in report.pulses.iter().take(MAX_ROWS) {
            let pulses = group[0].edges.len() as f64 / 2.0;
            let bits: Vec<String> = group.iter().map(|h| format!("{} {}.{}", id_text(h.key), h.byte, h.bit)).collect();
            let stray = group.iter().map(|h| h.stray).max().unwrap_or(0);
            let note = if stray > 0 { format!("  ({stray} changes elsewhere)") } else { String::new() };
            let noun = if pulses == 1.0 { "pulse " } else { "pulses" };
            let _ = writeln!(out, "   {pulses:>4} {noun}   {}{note}", bits.join(", "));
        }
        if report.pulses.len() > MAX_ROWS {
            let _ = writeln!(out, "   ...and {} more", report.pulses.len() - MAX_ROWS);
        }
    }
    if !report.bits.is_empty() {
        let _ = writeln!(out, "   Bits at a different level during the action, weaker evidence:");
        let _ = writeln!(out, "   {:<9}{:<6}{:<6}{:>7}{:>9}{:>7}", "ID", "Bit", "Dir", "Score", "Action", "Rest");
        for h in report.bits.iter().take(MAX_ROWS) {
            let _ = writeln!(
                out,
                "   {:<9}{:<6}{:<6}{:>7.2}{:>8.0}%{:>6.0}%",
                id_text(h.key),
                format!("{}.{}", h.byte, h.bit),
                if h.rises { "0>1" } else { "1>0" },
                h.score,
                h.action_pct,
                h.base_pct
            );
        }
        if report.bits.len() > MAX_ROWS {
            let _ = writeln!(out, "   ...and {} more", report.bits.len() - MAX_ROWS);
        }
    }
    if !report.bytes.is_empty() {
        let _ = writeln!(out, "   Bytes moving outside their resting range, for gradual values:");
        let _ = writeln!(out, "   {:<9}{:<6}{:>7}   {:<11}Action", "ID", "Byte", "Score", "Rest");
        for h in report.bytes.iter().take(MAX_ROWS) {
            let _ = writeln!(
                out,
                "   {:<9}{:<6}{:>7.2}   {:<11}{:02X}-{:02X}",
                id_text(h.key),
                h.byte,
                h.score,
                format!("{:02X}-{:02X}", h.base.0, h.base.1),
                h.action.0,
                h.action.1
            );
        }
        if report.bytes.len() > MAX_ROWS {
            let _ = writeln!(out, "   ...and {} more", report.bytes.len() - MAX_ROWS);
        }
    }
    out
}

pub fn latest_capture(dir: &Path) -> Result<PathBuf> {
    let entries = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
        .with_context(|| format!("no captures in {}", dir.display()))
}

pub fn run(capture: &Path, label: Option<&str>) -> Result<String> {
    if !capture.is_file() {
        bail!("capture not found: {}", capture.display());
    }
    let markers = marker_path(capture);
    let text = match fs::read_to_string(&markers) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(format!("{}: no actions were marked, so there is nothing to compare.\n", capture.display()));
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", markers.display())),
    };
    let (windows, warnings) = parse_markers(&text);
    let series = parse_log(&fs::read_to_string(capture).with_context(|| format!("reading {}", capture.display()))?);

    let labels: BTreeSet<&str> = windows.iter().map(|w| w.label.as_str()).filter(|l| label.is_none_or(|want| want == *l)).collect();
    if let Some(want) = label {
        if labels.is_empty() {
            bail!("no completed {want} actions in {}", markers.display());
        }
    }

    let mut out = format!("{}\n", capture.display());
    for w in &warnings {
        let _ = writeln!(out, "warning: {w}");
    }
    if labels.is_empty() {
        out.push_str("No completed actions to compare.\n");
    }
    for l in labels {
        let count = windows.iter().filter(|w| w.label == l).count();
        out.push_str(&render(l, count, &analyze(&series, &windows, l)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    /// 100 Hz for 90 s with a switch in byte 0 bit 2 active during `on`, a free-running
    /// counter in byte 1, a slow drift in byte 2, and a level in byte 3 that rises when on.
    fn fixture(on: &[(u64, u64)]) -> Series {
        let mut frames = Vec::new();
        for i in 0..9000u64 {
            let t = i * 10_000;
            let active = on.iter().any(|&(a, b)| t >= a * S && t < b * S);
            let data = [(active as u8) << 2, i as u8, (t / (2 * S)) as u8, if active { 0x80 } else { 0x10 }, 0, 0, 0, 0];
            frames.push((t, data, 8));
        }
        let mut series = Series::new();
        series.insert((false, 0x152), frames);
        series
    }

    fn windows(spans: &[(u64, u64)]) -> Vec<Window> {
        spans.iter().map(|&(a, b)| Window { label: "brake".into(), start: a * S, end: b * S }).collect()
    }

    #[test]
    fn finds_switch_and_level_but_not_counter_or_drift() {
        let spans = [(20, 25), (45, 50), (70, 75)];
        let report = analyze(&fixture(&spans), &windows(&spans), "brake");
        assert_eq!(report.bits.iter().map(|h| (h.byte, h.bit)).collect::<Vec<_>>(), vec![(0, 2), (3, 4), (3, 7)]);
        assert!(report.bits[0].rises);
        let bytes: Vec<_> = report.bytes.iter().map(|h| h.byte).collect();
        assert!(bytes.contains(&0) && bytes.contains(&3));
        assert!(!bytes.contains(&1), "counter scored: {bytes:?}");
        assert!(!bytes.contains(&2), "drift scored: {bytes:?}");
    }

    #[test]
    fn rejects_signal_that_only_matches_one_repetition() {
        // The signal is active in the first marked window only.
        let marked = [(20, 25), (45, 50), (70, 75)];
        let report = analyze(&fixture(&[(20, 25)]), &windows(&marked), "brake");
        assert_eq!(report, Report::default());
    }

    #[test]
    fn another_action_does_not_pollute_the_baseline() {
        // A throttle action 3 s after each brake must not count as resting behaviour.
        let brake = [(20, 25), (45, 50), (70, 75)];
        let mut marked = windows(&brake);
        marked.extend(brake.iter().map(|&(a, _)| Window { label: "throttle".into(), start: (a + 8) * S, end: (a + 11) * S }));
        let on: Vec<_> = brake.iter().flat_map(|&(a, b)| [(a, b), (a + 8, a + 11)]).collect();
        let report = analyze(&fixture(&on), &marked, "brake");
        assert!(report.bits.iter().any(|h| (h.byte, h.bit) == (0, 2)), "{report:?}");
    }

    fn block_hits(on: &[(u64, u64)]) -> Vec<Vec<PulseHit>> {
        let w = Window { label: "brake".into(), start: 20 * S, end: 31 * S };
        pulse_scan(&fixture(on), &[&w])
    }

    #[test]
    fn pulse_scan_counts_repeats_inside_one_block() {
        let groups = block_hits(&[(22, 23), (25, 26), (28, 29)]);
        // The switch and both level bits share timing, so they form one group.
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].len(), 3);
        assert_eq!(groups[0][0].edges.len(), 6);
    }

    #[test]
    fn pulse_scan_accepts_one_held_action() {
        let groups = block_hits(&[(22, 29)]);
        assert_eq!(groups[0][0].edges.len(), 2);
    }

    #[test]
    fn pulse_scan_rejects_bits_that_also_pulse_elsewhere() {
        assert!(block_hits(&[(22, 23), (25, 26), (50, 51), (60, 61)]).is_empty());
        // A single pulse must be completely clean, with no stray edges at all.
        assert!(block_hits(&[(22, 23), (50, 51)]).is_empty());
    }

    #[test]
    fn pulse_scan_keeps_a_control_also_used_outside_its_block() {
        // Five pulses in the block, plus two uses elsewhere, like the clutch at engine start.
        let groups = block_hits(&[(21, 22), (23, 24), (25, 26), (27, 28), (29, 30), (5, 6), (60, 61)]);
        assert_eq!(groups[0][0].edges.len(), 10);
        assert_eq!(groups[0][0].stray, 4);
    }

    #[test]
    fn pulse_scan_needs_every_block() {
        let w1 = Window { label: "brake".into(), start: 20 * S, end: 31 * S };
        let w2 = Window { label: "brake".into(), start: 60 * S, end: 70 * S };
        assert!(pulse_scan(&fixture(&[(22, 23), (25, 26)]), &[&w1, &w2]).is_empty());
        assert!(!pulse_scan(&fixture(&[(22, 23), (62, 63)]), &[&w1, &w2]).is_empty());
    }

    #[test]
    fn render_shows_pulses_and_says_blocks() {
        let spans = [(20, 31)];
        let text = render("brake", 1, &analyze(&fixture(&[(22, 23), (25, 26)]), &windows(&spans), "brake"));
        assert!(text.contains("== brake: 1 block\n"), "{text}");
        assert!(text.contains("2 pulses"), "{text}");
        assert!(text.contains("152 0.2"), "{text}");
        assert!(!text.contains("repetition"), "{text}");
    }

    #[test]
    fn markers_pair_and_warn() {
        let (w, warn) = parse_markers("100 start brake\n200 stop brake\n300 start clutch\n400 stop lights\nnonsense\n");
        assert_eq!(w, vec![Window { label: "brake".into(), start: 100, end: 200 }]);
        assert_eq!(warn.len(), 3, "{warn:?}");
    }

    #[test]
    fn parses_candump() {
        let s = parse_log("(1.000500) can0 140#AB01\n(2.000000) can0 18DAF110#FF\n(3.000000) can0 20000004#00\n(4.000000) can0 141#R\nbad line\n");
        assert_eq!(s[&(false, 0x140)][0], (1_000_500, [0xAB, 1, 0, 0, 0, 0, 0, 0], 2));
        assert_eq!(s[&(true, 0x18DA_F110)][0].2, 1);
        assert_eq!(s.len(), 2);
    }
}
