//! Capture throughput check. Sends numbered frames on a virtual CAN interface and
//! records them through the same read -> channel -> log path the capture daemon
//! will use, then reports lost frames and per-thread CPU.
//!
//! Refuses anything but a vcan interface, because it transmits.

use socketcan::{CanFrame, CanSocket, EmbeddedFrame, Socket, StandardId};
use std::fs::File;
use std::io::{self, BufWriter, ErrorKind, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const IDS: u16 = 64;
const CHANNEL_DEPTH: usize = 65_536;

fn thread_cpu_secs() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut usage) };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    secs(usage.ru_utime) + secs(usage.ru_stime)
}

fn sender(iface: String, rate: u64, secs: u64) -> io::Result<(u64, f64)> {
    let sock = CanSocket::open(&iface)?;
    let start = Instant::now();
    let run_for = Duration::from_secs(secs);
    let mut sent = 0u64;

    while start.elapsed() < run_for {
        let target = (start.elapsed().as_secs_f64() * rate as f64) as u64;
        while sent < target {
            let id = StandardId::new(0x100 + (sent % IDS as u64) as u16).expect("id in range");
            let frame = CanFrame::new(id, &sent.to_le_bytes()).expect("8 byte frame");
            match sock.write_frame(&frame) {
                Ok(()) => sent += 1,
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::ENOBUFS) => {
                    thread::yield_now()
                }
                Err(e) => return Err(e),
            }
        }
        thread::sleep(Duration::from_micros(200));
    }
    Ok((sent, thread_cpu_secs()))
}

struct RxStats {
    received: u64,
    lost: u64,
    reordered: u64,
    cpu: f64,
}

fn reader(iface: String, done: Arc<AtomicBool>, tx: mpsc::SyncSender<(f64, CanFrame)>) -> io::Result<RxStats> {
    let sock = CanSocket::open(&iface)?;
    sock.set_read_timeout(Duration::from_millis(500))?;
    let mut stats = RxStats { received: 0, lost: 0, reordered: 0, cpu: 0.0 };
    let mut next = 0u64;

    loop {
        match sock.read_frame() {
            Ok(frame) => {
                let mut raw = [0u8; 8];
                raw.copy_from_slice(frame.data());
                let seq = u64::from_le_bytes(raw);
                if seq < next {
                    stats.reordered += 1;
                } else {
                    stats.lost += seq - next;
                    next = seq + 1;
                }
                stats.received += 1;
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
                if tx.send((now, frame)).is_err() {
                    break;
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if done.load(Ordering::Relaxed) {
                    break;
                }
            }
            Err(e) => return Err(e),
        }
    }
    stats.cpu = thread_cpu_secs();
    Ok(stats)
}

fn logger(path: String, iface: String, rx: mpsc::Receiver<(f64, CanFrame)>) -> io::Result<(u64, f64)> {
    let mut out = BufWriter::with_capacity(1 << 20, File::create(&path)?);
    let mut bytes = 0u64;
    let mut line = String::with_capacity(64);
    for (ts, frame) in rx {
        use std::fmt::Write as _;
        line.clear();
        let id = match frame.id() {
            socketcan::Id::Standard(id) => id.as_raw() as u32,
            socketcan::Id::Extended(id) => id.as_raw(),
        };
        let _ = write!(line, "({ts:.6}) {iface} {id:03X}#");
        for byte in frame.data() {
            let _ = write!(line, "{byte:02X}");
        }
        line.push('\n');
        out.write_all(line.as_bytes())?;
        bytes += line.len() as u64;
    }
    out.flush()?;
    Ok((bytes, thread_cpu_secs()))
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let iface = args.next().unwrap_or_else(|| "vcan0".into());
    let rate: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let secs: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(30);
    let path = args.next().unwrap_or_else(|| "/tmp/can-bench.log".into());

    if !iface.starts_with("vcan") {
        eprintln!("refusing to transmit on {iface}: can-bench only runs on vcan interfaces");
        std::process::exit(2);
    }

    let done = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel(CHANNEL_DEPTH);

    let log = {
        let (path, iface) = (path.clone(), iface.clone());
        thread::spawn(move || logger(path, iface, rx))
    };
    let read = {
        let (iface, done) = (iface.clone(), done.clone());
        thread::spawn(move || reader(iface, done, tx))
    };
    thread::sleep(Duration::from_millis(100));

    let (sent, tx_cpu) = {
        let iface = iface.clone();
        thread::spawn(move || sender(iface, rate, secs)).join().expect("sender thread")?
    };
    done.store(true, Ordering::Relaxed);

    let stats = read.join().expect("reader thread")?;
    let (bytes, log_cpu) = log.join().expect("logger thread")?;

    let wall = secs as f64;
    println!("rate      {rate} frames/s for {secs} s on {iface}");
    println!("sent      {sent}");
    println!("received  {}", stats.received);
    println!("lost      {}", stats.lost + sent.saturating_sub(stats.received + stats.lost));
    println!("reordered {}", stats.reordered);
    println!("log       {:.1} MB ({:.2} MB/min)", bytes as f64 / 1e6, bytes as f64 / 1e6 / wall * 60.0);
    println!("cpu core  reader {:.1}%  logger {:.1}%  sender {:.1}%",
        stats.cpu / wall * 100.0, log_cpu / wall * 100.0, tx_cpu / wall * 100.0);
    Ok(())
}
