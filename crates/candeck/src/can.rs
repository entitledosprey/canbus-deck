//! Raw SocketCAN access. The receive path uses recvmsg so every frame carries its
//! kernel timestamp and the socket reports frames the kernel had to drop.
//!
//! `CanSock` has no send method. Transmitting needs `VcanTx`, which only opens vcan.

use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

const AF_CAN: i32 = 29;
const CAN_RAW: i32 = 1;
const SOL_CAN_RAW: i32 = 101;
const CAN_RAW_ERR_FILTER: i32 = 2;
const SO_RXQ_OVFL: i32 = 40;
const CAN_EFF_FLAG: u32 = 0x8000_0000;
const CAN_RTR_FLAG: u32 = 0x4000_0000;
const CAN_ERR_FLAG: u32 = 0x2000_0000;
const CAN_EFF_MASK: u32 = 0x1FFF_FFFF;
const CAN_SFF_MASK: u32 = 0x7FF;
const RCVBUF_BYTES: i32 = 8 << 20;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Frame {
    pub ts_us: u64,
    pub id: u32,
    pub ext: bool,
    pub err: bool,
    pub rtr: bool,
    pub dlc: u8,
    pub data: [u8; 8],
}

impl Frame {
    pub fn bytes(&self) -> &[u8] {
        &self.data[..(self.dlc as usize).min(8)]
    }
}

#[repr(C)]
#[derive(Default)]
struct RawFrame {
    can_id: u32,
    len: u8,
    pad: u8,
    res0: u8,
    len8: u8,
    data: [u8; 8],
}

#[repr(C)]
struct SockaddrCan {
    family: u16,
    ifindex: i32,
    addr: [u64; 2],
}

fn setopt<T>(fd: i32, level: i32, name: i32, value: &T) -> io::Result<()> {
    let rc = unsafe {
        libc::setsockopt(fd, level, name, value as *const T as *const libc::c_void, mem::size_of::<T>() as u32)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_bound(iface: &str) -> Result<OwnedFd> {
    let name = CString::new(iface)?;
    let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if ifindex == 0 {
        bail!("no such interface: {iface}");
    }
    let raw = unsafe { libc::socket(AF_CAN, libc::SOCK_RAW, CAN_RAW) };
    if raw < 0 {
        return Err(io::Error::last_os_error()).context("opening CAN socket");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let addr = SockaddrCan { family: AF_CAN as u16, ifindex: ifindex as i32, addr: [0; 2] };
    let rc = unsafe {
        libc::bind(raw, &addr as *const SockaddrCan as *const libc::sockaddr, mem::size_of::<SockaddrCan>() as u32)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("binding to {iface}"));
    }
    Ok(fd)
}

pub struct CanSock {
    fd: OwnedFd,
    /// Cumulative frames the kernel dropped because this socket's queue was full.
    pub kernel_drops: u32,
}

impl CanSock {
    pub fn open(iface: &str, timeout: Duration) -> Result<Self> {
        let fd = open_bound(iface)?;
        let raw = fd.as_raw_fd();
        setopt(raw, SOL_CAN_RAW, CAN_RAW_ERR_FILTER, &CAN_EFF_MASK)?;
        setopt(raw, libc::SOL_SOCKET, libc::SO_TIMESTAMPNS, &1i32)?;
        setopt(raw, libc::SOL_SOCKET, SO_RXQ_OVFL, &1i32)?;
        setopt(raw, libc::SOL_SOCKET, libc::SO_RCVBUF, &RCVBUF_BYTES)?;
        let tv = libc::timeval { tv_sec: timeout.as_secs() as _, tv_usec: timeout.subsec_micros() as _ };
        setopt(raw, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv)?;
        Ok(Self { fd, kernel_drops: 0 })
    }

    /// Returns `None` when the receive timeout expires with no frame.
    pub fn recv(&mut self) -> Result<Option<Frame>> {
        let mut raw = RawFrame::default();
        let mut iov = libc::iovec { iov_base: &mut raw as *mut RawFrame as *mut libc::c_void, iov_len: mem::size_of::<RawFrame>() };
        let mut control = [0u64; 16];
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = mem::size_of_val(&control) as _;

        let n = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut msg, 0) };
        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted => Ok(None),
                _ => Err(err).context("reading CAN socket"),
            };
        }

        let mut ts_us = 0u64;
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET {
                    if (*cmsg).cmsg_type == libc::SO_TIMESTAMPNS {
                        let ts = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const libc::timespec);
                        ts_us = ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000;
                    } else if (*cmsg).cmsg_type == SO_RXQ_OVFL {
                        self.kernel_drops = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const u32);
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }

        let ext = raw.can_id & CAN_EFF_FLAG != 0;
        Ok(Some(Frame {
            ts_us,
            id: raw.can_id & if ext { CAN_EFF_MASK } else { CAN_SFF_MASK },
            ext,
            err: raw.can_id & CAN_ERR_FLAG != 0,
            rtr: raw.can_id & CAN_RTR_FLAG != 0,
            dlc: raw.len.min(8),
            data: raw.data,
        }))
    }
}

/// Transmit handle for virtual interfaces only.
pub struct VcanTx {
    fd: OwnedFd,
}

impl VcanTx {
    pub fn open(iface: &str) -> Result<Self> {
        if !iface.starts_with("vcan") {
            bail!("refusing to transmit on {iface}: only vcan interfaces can transmit");
        }
        Ok(Self { fd: open_bound(iface)? })
    }

    pub fn send(&self, id: u32, ext: bool, data: &[u8]) -> Result<()> {
        let mut raw = RawFrame { can_id: id | if ext { CAN_EFF_FLAG } else { 0 }, len: data.len().min(8) as u8, ..Default::default() };
        raw.data[..raw.len as usize].copy_from_slice(&data[..raw.len as usize]);
        let rc = unsafe {
            libc::write(self.fd.as_raw_fd(), &raw as *const RawFrame as *const libc::c_void, mem::size_of::<RawFrame>())
        };
        if rc < 0 {
            return Err(io::Error::last_os_error()).context("writing vcan frame");
        }
        Ok(())
    }
}
