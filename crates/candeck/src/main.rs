mod can;
mod compare;
mod daemon;
mod detect;
mod senders;
mod sim;
mod tui;
mod wire;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "candeck", version, about = "CAN bus capture and reverse engineering")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Find the bus bitrate, always listen-only. Needs root to configure the link.
    Detect {
        iface: String,
        /// Seconds to listen at each rate
        #[arg(long, default_value_t = 2)]
        window: u64,
    },
    /// Capture to a log file and serve local clients
    Daemon {
        iface: String,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Live sniffer
    Sniff,
    /// Start or stop a marked action in the running capture
    Mark {
        #[arg(value_parser = ["start", "stop"])]
        verb: String,
        label: String,
    },
    /// Show what changed during marked actions (newest capture by default)
    Compare {
        capture: Option<PathBuf>,
        #[arg(long)]
        label: Option<String>,
    },
    /// Print the sniffer view after a few seconds, as plain text
    Snapshot {
        #[arg(long, default_value_t = 3)]
        secs: u64,
    },
    /// Group IDs by likely sender using clock drift (newest capture by default)
    Senders {
        capture: Option<PathBuf>,
    },
    /// Generate simulated traffic on a vcan interface
    Sim {
        #[arg(default_value = "vcan0")]
        iface: String,
        #[arg(long)]
        secs: Option<u64>,
    },
}

fn default_capture_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into());
    home.join(".local/share/candeck/captures")
}

fn run_detect(iface: &str, window: u64) -> Result<()> {
    if iface.starts_with("vcan") {
        bail!("{iface} is virtual and has no bitrate to detect");
    }
    let mut probe = detect::LinkProbe { iface: iface.to_string() };
    let outcome = detect::sweep(&mut probe, &detect::RATES, Duration::from_secs(window));
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            let _ = detect::link_down(iface);
            return Err(e);
        }
    };
    for (rate, s, verdict) in &outcome.results {
        println!("{rate:>8} bit/s  {:>6} frames  {:>4} ids  {:>5} errors  {verdict:?}", s.frames, s.ids, s.errors);
    }
    match outcome.detected {
        Some(rate) => println!("detected {rate} bit/s. {iface} is up and listen-only."),
        None => {
            detect::link_down(iface)?;
            let noisy = outcome.results.iter().any(|r| r.2 == detect::Verdict::Errors);
            if noisy {
                println!("no rate matched, but bus errors were seen: check wiring, or try a rate not on the list. {iface} is down.");
            } else {
                println!("no traffic seen at any rate: the bus may be quiet, so check ignition and wiring. {iface} is down.");
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Detect { iface, window } => run_detect(&iface, window),
        Command::Daemon { iface, dir } => daemon::run(&iface, &dir.unwrap_or_else(default_capture_dir)),
        Command::Sniff => tui::run(),
        Command::Mark { verb, label } => tui::mark(&verb, &label),
        Command::Compare { capture, label } => {
            let capture = match capture {
                Some(path) => path,
                None => compare::latest_capture(&default_capture_dir())?,
            };
            print!("{}", compare::run(&capture, label.as_deref())?);
            print!("{}", senders::run(&capture)?);
            Ok(())
        }
        Command::Senders { capture } => {
            let capture = match capture {
                Some(path) => path,
                None => compare::latest_capture(&default_capture_dir())?,
            };
            print!("{}", senders::run(&capture)?);
            Ok(())
        }
        Command::Snapshot { secs } => {
            println!("{}", tui::snapshot(secs)?);
            Ok(())
        }
        Command::Sim { iface, secs } => sim::run(&iface, secs),
    }
}
