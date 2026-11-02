//! Bitrate detection. Every probe is listen-only: the link is configured, the mode is
//! read back from the kernel, and the probe aborts if listen-only is not confirmed.

use crate::can::CanSock;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::process::Command;
use std::time::{Duration, Instant};

pub const RATES: [u32; 8] = [1_000_000, 500_000, 250_000, 125_000, 800_000, 100_000, 83_333, 50_000];
const MIN_FRAMES: u64 = 20;
const MIN_IDS: usize = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    pub frames: u64,
    pub ids: usize,
    pub errors: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    Match,
    /// Clean frames, but too few or from one ID.
    Weak,
    /// Bus errors seen: traffic is present at some other rate, or the wiring is bad.
    Errors,
    Silent,
}

pub fn classify(s: &Sample) -> Verdict {
    if s.errors > 0 {
        Verdict::Errors
    } else if s.frames >= MIN_FRAMES && s.ids >= MIN_IDS {
        Verdict::Match
    } else if s.frames > 0 {
        Verdict::Weak
    } else {
        Verdict::Silent
    }
}

pub trait Probe {
    /// Must return an error unless listen-only at `bitrate` is confirmed active.
    fn configure(&mut self, bitrate: u32) -> Result<()>;
    fn listen(&mut self, window: Duration) -> Result<Sample>;
}

#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub detected: Option<u32>,
    pub results: Vec<(u32, Sample, Verdict)>,
}

/// A rate is accepted only if it matches in two consecutive windows.
pub fn sweep(probe: &mut dyn Probe, rates: &[u32], window: Duration) -> Result<Outcome> {
    let mut results = Vec::new();
    for &rate in rates {
        probe.configure(rate)?;
        let sample = probe.listen(window)?;
        let verdict = classify(&sample);
        results.push((rate, sample, verdict));
        if verdict == Verdict::Match {
            let confirm = probe.listen(window)?;
            let confirmed = classify(&confirm);
            results.push((rate, confirm, confirmed));
            if confirmed == Verdict::Match {
                return Ok(Outcome { detected: Some(rate), results });
            }
        }
    }
    Ok(Outcome { detected: None, results })
}

/// Reads the link's control mode and bitrate from `ip -details -json`.
pub fn parse_link(json: &str) -> Result<(bool, u32)> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing ip output")?;
    let data = &v[0]["linkinfo"]["info_data"];
    let listen_only = data["ctrlmode"].as_array().is_some_and(|m| m.iter().any(|x| x == "LISTEN-ONLY"));
    let bitrate = data["bittiming"]["bitrate"].as_u64().unwrap_or(0) as u32;
    Ok((listen_only, bitrate))
}

pub fn link_state(iface: &str) -> Result<(bool, u32)> {
    let out = Command::new("ip").args(["-details", "-json", "link", "show", "dev", iface]).output().context("running ip")?;
    if !out.status.success() {
        bail!("ip link show {iface}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    parse_link(&String::from_utf8_lossy(&out.stdout))
}

fn ip_link(args: &[&str]) -> Result<()> {
    let out = Command::new("ip").arg("link").args(args).output().context("running ip")?;
    if !out.status.success() {
        bail!("ip link {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Takes the link down, leaving it silent. Used whenever a sweep finds nothing or fails.
pub fn link_down(iface: &str) -> Result<()> {
    ip_link(&["set", "dev", iface, "down"])
}

pub struct LinkProbe {
    pub iface: String,
}

impl LinkProbe {
    fn rx_errors(&self) -> u64 {
        std::fs::read_to_string(format!("/sys/class/net/{}/statistics/rx_errors", self.iface))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
}

impl Probe for LinkProbe {
    fn configure(&mut self, bitrate: u32) -> Result<()> {
        let rate = bitrate.to_string();
        // Set the mode while the link is down, and verify it before the link comes up.
        ip_link(&["set", "dev", &self.iface, "down"])?;
        ip_link(&["set", "dev", &self.iface, "type", "can", "bitrate", &rate, "listen-only", "on"])?;
        let (listen_only, actual) = link_state(&self.iface)?;
        if !listen_only {
            bail!("{}: kernel did not confirm listen-only, aborting before link up", self.iface);
        }
        if actual != bitrate {
            bail!("{}: asked for {bitrate} bit/s, kernel reports {actual}", self.iface);
        }
        ip_link(&["set", "dev", &self.iface, "up"])
    }

    fn listen(&mut self, window: Duration) -> Result<Sample> {
        let mut sock = CanSock::open(&self.iface, Duration::from_millis(100))?;
        let errors_before = self.rx_errors();
        let mut sample = Sample::default();
        let mut ids = HashSet::new();
        let end = Instant::now() + window;
        while Instant::now() < end {
            if let Some(f) = sock.recv()? {
                if f.err {
                    sample.errors += 1;
                } else {
                    sample.frames += 1;
                    ids.insert((f.ext, f.id));
                }
            }
        }
        sample.ids = ids.len();
        sample.errors += self.rx_errors().saturating_sub(errors_before);
        Ok(sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        fail_configure_at: Option<u32>,
        good_rate: u32,
        current: u32,
        configured: Vec<u32>,
    }

    impl Probe for Fake {
        fn configure(&mut self, bitrate: u32) -> Result<()> {
            if self.fail_configure_at == Some(bitrate) {
                bail!("listen-only not confirmed");
            }
            self.current = bitrate;
            self.configured.push(bitrate);
            Ok(())
        }
        fn listen(&mut self, _: Duration) -> Result<Sample> {
            Ok(if self.current == self.good_rate {
                Sample { frames: 500, ids: 12, errors: 0 }
            } else {
                Sample { frames: 0, ids: 0, errors: 3 }
            })
        }
    }

    #[test]
    fn classify_cases() {
        assert_eq!(classify(&Sample { frames: 100, ids: 5, errors: 0 }), Verdict::Match);
        assert_eq!(classify(&Sample { frames: 100, ids: 5, errors: 1 }), Verdict::Errors);
        assert_eq!(classify(&Sample { frames: 100, ids: 1, errors: 0 }), Verdict::Weak);
        assert_eq!(classify(&Sample { frames: 3, ids: 3, errors: 0 }), Verdict::Weak);
        assert_eq!(classify(&Sample::default()), Verdict::Silent);
    }

    #[test]
    fn sweep_finds_rate_and_stops() {
        let mut p = Fake { fail_configure_at: None, good_rate: 500_000, current: 0, configured: vec![] };
        let out = sweep(&mut p, &RATES, Duration::ZERO).unwrap();
        assert_eq!(out.detected, Some(500_000));
        assert_eq!(p.configured, vec![1_000_000, 500_000]);
    }

    #[test]
    fn sweep_aborts_when_listen_only_unconfirmed() {
        let mut p = Fake { fail_configure_at: Some(500_000), good_rate: 250_000, current: 0, configured: vec![] };
        assert!(sweep(&mut p, &RATES, Duration::ZERO).is_err());
        assert_eq!(p.configured, vec![1_000_000]);
    }

    #[test]
    fn sweep_reports_nothing_found() {
        let mut p = Fake { fail_configure_at: None, good_rate: 1, current: 0, configured: vec![] };
        assert_eq!(sweep(&mut p, &RATES, Duration::ZERO).unwrap().detected, None);
    }

    #[test]
    fn parses_link_json() {
        let on = r#"[{"linkinfo":{"info_kind":"can","info_data":{"ctrlmode":["LISTEN-ONLY"],"bittiming":{"bitrate":1000000}}}}]"#;
        assert_eq!(parse_link(on).unwrap(), (true, 1_000_000));
        let off = r#"[{"linkinfo":{"info_kind":"can","info_data":{"bittiming":{"bitrate":500000}}}}]"#;
        assert_eq!(parse_link(off).unwrap(), (false, 500_000));
        assert_eq!(parse_link(r#"[{"ifname":"lo"}]"#).unwrap(), (false, 0));
    }
}
