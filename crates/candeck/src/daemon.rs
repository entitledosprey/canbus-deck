//! Capture daemon: one thread reads the bus, one writes the log, and each local client
//! gets its own bounded queue so a slow UI can never stall capture.

use crate::can::{CanSock, Frame};
use crate::detect;
use crate::wire::{self, Status, RECORD};
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOG_QUEUE: usize = 1 << 17;
const CLIENT_QUEUE: usize = 1 << 16;
/// Upper bound on capture lost if power fails.
const FLUSH_EVERY: Duration = Duration::from_secs(1);

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("candeck.sock")
}

/// Markers live beside the log, so the log stays plain candump for other tools.
pub fn marker_path(log: &Path) -> PathBuf {
    log.with_extension("markers")
}

pub fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= 32 && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Parses `MARK start|stop <label>` into (verb, label).
pub fn parse_command(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let (mark, verb, label) = (parts.next()?, parts.next()?, parts.next()?);
    (mark == "MARK" && matches!(verb, "start" | "stop") && valid_label(label) && parts.next().is_none()).then_some((verb, label))
}

fn read_commands(stream: UnixStream, markers: Arc<Mutex<File>>) {
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        let Some((verb, label)) = parse_command(&line) else { continue };
        let ts_us = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
        // Written straight through, so a marker survives a power cut.
        if let Err(e) = writeln!(markers.lock().expect("marker file"), "{ts_us} {verb} {label}") {
            eprintln!("writing marker: {e}");
        }
    }
}

fn capture_path(dir: &Path, iface: &str) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    Ok(dir.join(format!("{secs}-{iface}.log")))
}

pub fn candump_line(f: &Frame, iface: &str) -> String {
    use std::fmt::Write as _;
    let mut line = String::with_capacity(64);
    let _ = write!(line, "({}.{:06}) {iface} ", f.ts_us / 1_000_000, f.ts_us % 1_000_000);
    let id = if f.err { f.id | 0x2000_0000 } else { f.id };
    if f.ext || f.err {
        let _ = write!(line, "{id:08X}#");
    } else {
        let _ = write!(line, "{id:03X}#");
    }
    if f.rtr {
        line.push('R');
    }
    for b in f.bytes() {
        let _ = write!(line, "{b:02X}");
    }
    line
}

fn logger(path: PathBuf, iface: String, rx: mpsc::Receiver<Frame>) -> Result<()> {
    let mut out = BufWriter::with_capacity(1 << 20, File::create(&path)?);
    let mut last_flush = Instant::now();
    loop {
        match rx.recv_timeout(FLUSH_EVERY) {
            Ok(f) => writeln!(out, "{}", candump_line(&f, &iface))?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if last_flush.elapsed() >= FLUSH_EVERY {
            out.flush()?;
            last_flush = Instant::now();
        }
    }
    out.flush()?;
    Ok(())
}

struct Client {
    tx: SyncSender<[u8; RECORD]>,
    drops: Arc<AtomicU64>,
}

fn serve_clients(listener: UnixListener, clients: Arc<Mutex<Vec<Client>>>, markers: Arc<Mutex<File>>) {
    for stream in listener.incoming().flatten() {
        if let Ok(commands) = stream.try_clone() {
            let markers = markers.clone();
            thread::spawn(move || read_commands(commands, markers));
        }
        let (tx, rx) = mpsc::sync_channel::<[u8; RECORD]>(CLIENT_QUEUE);
        clients.lock().expect("client list").push(Client { tx, drops: Arc::new(AtomicU64::new(0)) });
        thread::spawn(move || {
            let mut out = BufWriter::with_capacity(1 << 14, stream);
            while let Ok(rec) = rx.recv() {
                if out.write_all(&rec).is_err() {
                    return;
                }
                // Flush when the queue is drained so UIs see frames without delay.
                while let Ok(more) = rx.try_recv() {
                    if out.write_all(&more).is_err() {
                        return;
                    }
                }
                if out.flush().is_err() {
                    return;
                }
            }
        });
    }
}

pub fn run(iface: &str, dir: &Path) -> Result<()> {
    if !iface.starts_with("vcan") {
        let (listen_only, _) = detect::link_state(iface)?;
        if !listen_only {
            bail!("{iface} is not in listen-only mode. Run `sudo candeck detect {iface}` first.");
        }
    }

    let mut sock = CanSock::open(iface, Duration::from_millis(250))?;
    let path = capture_path(dir, iface)?;
    let markers = Arc::new(Mutex::new(File::create(marker_path(&path))?));
    let (log_tx, log_rx) = mpsc::sync_channel::<Frame>(LOG_QUEUE);
    let log_thread = {
        let (path, iface) = (path.clone(), iface.to_string());
        thread::spawn(move || logger(path, iface, log_rx))
    };

    let sock_path = socket_path();
    let _ = fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).with_context(|| format!("binding {}", sock_path.display()))?;
    fs::set_permissions(&sock_path, fs::Permissions::from_mode(0o600))?;
    let clients = Arc::new(Mutex::new(Vec::<Client>::new()));
    {
        let clients = clients.clone();
        thread::spawn(move || serve_clients(listener, clients, markers));
    }

    println!("capturing {iface} to {}", path.display());
    let mut log_drops = 0u64;
    let mut retired_client_drops = 0u64;
    let mut last_status = Instant::now();

    loop {
        let frame = sock.recv()?;
        if let Some(f) = &frame {
            if log_tx.try_send(*f).is_err() {
                log_drops += 1;
            }
        }
        let mut list = clients.lock().expect("client list");
        if let Some(f) = &frame {
            let rec = wire::encode_frame(f);
            list.retain(|c| match c.tx.try_send(rec) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    c.drops.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(TrySendError::Disconnected(_)) => {
                    retired_client_drops += c.drops.load(Ordering::Relaxed);
                    false
                }
            });
        }
        if last_status.elapsed() >= Duration::from_secs(1) {
            last_status = Instant::now();
            let client_drops = retired_client_drops + list.iter().map(|c| c.drops.load(Ordering::Relaxed)).sum::<u64>();
            let rec = wire::encode_status(&Status { kernel_drops: sock.kernel_drops, log_drops, client_drops });
            list.iter().for_each(|c| {
                let _ = c.tx.try_send(rec);
            });
            if log_thread.is_finished() {
                bail!("log writer stopped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(parse_command("MARK start brake"), Some(("start", "brake")));
        assert_eq!(parse_command("MARK stop high-beam_2"), Some(("stop", "high-beam_2")));
        assert_eq!(parse_command("MARK start ../etc"), None);
        assert_eq!(parse_command("MARK pause brake"), None);
        assert_eq!(parse_command("MARK start a b"), None);
        assert_eq!(parse_command("SEND 140"), None);
    }

    #[test]
    fn candump_formats() {
        let f = Frame { ts_us: 1_700_000_000_000_123, id: 0x140, dlc: 2, data: [0xAB, 0x01, 0, 0, 0, 0, 0, 0], ..Default::default() };
        assert_eq!(candump_line(&f, "can0"), "(1700000000.000123) can0 140#AB01");
        let e = Frame { ts_us: 1, id: 0x18DA_F110, ext: true, dlc: 0, ..Default::default() };
        assert_eq!(candump_line(&e, "can0"), "(0.000001) can0 18DAF110#");
    }
}
