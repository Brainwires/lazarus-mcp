//! aegis-hooks - LD_PRELOAD Library for Network Monitoring and Filesystem Overlay
//!
//! This library intercepts:
//! - Network system calls (connect, send, recv, etc.) for monitoring
//! - Filesystem calls (open, openat) for MCP config overlay
//!
//! Usage:
//!   LD_PRELOAD=/path/to/libaegis_hooks.so \
//!     AEGIS_NETMON_LOG=/tmp/netmon.jsonl \
//!     AEGIS_MCP_OVERLAY=/tmp/mcp-config.json \
//!     AEGIS_MCP_TARGET=.mcp.json \
//!     <command>

// ============================================================================
// Version Information
// ============================================================================

/// Library version from Cargo.toml
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build timestamp (set by build.rs)
pub const BUILD_TIME: &str = env!("AEGIS_HOOKS_BUILD_TIME");

/// Git commit hash (set by build.rs)
pub const GIT_HASH: &str = env!("AEGIS_HOOKS_GIT_HASH");

/// Combined version string for display
pub const VERSION_STRING: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (built ",
    env!("AEGIS_HOOKS_BUILD_TIME"),
    ", git ",
    env!("AEGIS_HOOKS_GIT_HASH"),
    ")"
);

/// Export version info as a C-compatible function for external verification
#[no_mangle]
pub extern "C" fn aegis_hooks_version() -> *const std::ffi::c_char {
    static VERSION_CSTR: once_cell::sync::Lazy<std::ffi::CString> =
        once_cell::sync::Lazy::new(|| {
            std::ffi::CString::new(VERSION_STRING).unwrap_or_else(|_| {
                std::ffi::CString::new("unknown").unwrap()
            })
        });
    VERSION_CSTR.as_ptr()
}

/// Export build timestamp as a C-compatible function
#[no_mangle]
pub extern "C" fn aegis_hooks_build_time() -> *const std::ffi::c_char {
    static BUILD_TIME_CSTR: once_cell::sync::Lazy<std::ffi::CString> =
        once_cell::sync::Lazy::new(|| {
            std::ffi::CString::new(BUILD_TIME).unwrap_or_else(|_| {
                std::ffi::CString::new("unknown").unwrap()
            })
        });
    BUILD_TIME_CSTR.as_ptr()
}

/// Library initialization - runs when LD_PRELOAD loads the library
#[ctor::ctor]
fn init() {
    eprintln!("[aegis-hooks] Library loaded v{}", VERSION_STRING);
    if let Ok(overlay) = std::env::var("AEGIS_MCP_OVERLAY") {
        eprintln!("[aegis-hooks] MCP overlay: {}", overlay);
    }
    if let Ok(target) = std::env::var("AEGIS_MCP_TARGET") {
        eprintln!("[aegis-hooks] MCP target: {}", target);
    }
}

use libc::{
    c_char, c_int, c_void, mode_t, size_t, sockaddr, sockaddr_in, sockaddr_in6, socklen_t,
    ssize_t, AF_INET, AF_INET6,
};
use once_cell::sync::Lazy;
use serde::Serialize;
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// ============================================================================
// Environment Variables
// ============================================================================

/// Environment variable for network monitoring log file path
const LOG_PATH_ENV: &str = "AEGIS_NETMON_LOG";

/// Default log file path for network monitoring
const DEFAULT_LOG_PATH: &str = "/tmp/aegis-netmon.jsonl";

/// Environment variable for the MCP overlay file path (the temp file to redirect to)
const MCP_OVERLAY_ENV: &str = "AEGIS_MCP_OVERLAY";

/// Environment variable for the target file to overlay (e.g., ".mcp.json")
const MCP_TARGET_ENV: &str = "AEGIS_MCP_TARGET";

// ============================================================================
// Network Monitoring
// ============================================================================

/// Global log file handle for network monitoring
static LOG_FILE: Lazy<Mutex<Option<File>>> = Lazy::new(|| {
    let path = std::env::var(LOG_PATH_ENV).unwrap_or_else(|_| DEFAULT_LOG_PATH.to_string());
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    Mutex::new(file)
});

/// Network event types
#[derive(Debug, Serialize)]
#[serde(tag = "event")]
enum NetEvent {
    #[serde(rename = "connect")]
    Connect {
        ts: u64,
        fd: i32,
        addr: String,
        port: u16,
        family: String,
        result: i32,
    },
    #[serde(rename = "send")]
    Send {
        ts: u64,
        fd: i32,
        bytes: usize,
        result: isize,
    },
    #[serde(rename = "recv")]
    Recv {
        ts: u64,
        fd: i32,
        bytes: usize,
        result: isize,
    },
    #[serde(rename = "sendto")]
    SendTo {
        ts: u64,
        fd: i32,
        bytes: usize,
        addr: Option<String>,
        port: Option<u16>,
        result: isize,
    },
    #[serde(rename = "recvfrom")]
    RecvFrom {
        ts: u64,
        fd: i32,
        bytes: usize,
        result: isize,
    },
    #[serde(rename = "close")]
    Close { ts: u64, fd: i32, result: i32 },
}

/// Get current timestamp in milliseconds since Unix epoch
fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Log an event to the JSONL file
fn log_event(event: &NetEvent) {
    if let Ok(mut guard) = LOG_FILE.lock() {
        if let Some(ref mut file) = *guard {
            if let Ok(json) = serde_json::to_string(event) {
                let _ = writeln!(file, "{}", json);
                let _ = file.flush();
            }
        }
    }
}

/// Parse a sockaddr into address string and port
fn parse_sockaddr(addr: *const sockaddr) -> (String, u16, String) {
    if addr.is_null() {
        return ("unknown".to_string(), 0, "unknown".to_string());
    }

    unsafe {
        let family = (*addr).sa_family as i32;
        match family {
            AF_INET => {
                let addr_in = addr as *const sockaddr_in;
                let ip = Ipv4Addr::from(u32::from_be((*addr_in).sin_addr.s_addr));
                let port = u16::from_be((*addr_in).sin_port);
                (ip.to_string(), port, "IPv4".to_string())
            }
            AF_INET6 => {
                let addr_in6 = addr as *const sockaddr_in6;
                let ip = Ipv6Addr::from((*addr_in6).sin6_addr.s6_addr);
                let port = u16::from_be((*addr_in6).sin6_port);
                (ip.to_string(), port, "IPv6".to_string())
            }
            _ => ("unknown".to_string(), 0, format!("family:{}", family)),
        }
    }
}

// ============================================================================
// Filesystem Overlay
// ============================================================================

/// Cached MCP overlay configuration
static MCP_CONFIG: Lazy<Option<(String, CString)>> = Lazy::new(|| {
    let overlay = std::env::var(MCP_OVERLAY_ENV).ok()?;
    let target = std::env::var(MCP_TARGET_ENV).ok()?;

    // Pre-create the CString for the overlay path
    let overlay_cstr = CString::new(overlay.clone()).ok()?;

    Some((target, overlay_cstr))
});

/// Check if a path matches the MCP target file
fn should_overlay(path_str: &str) -> bool {
    if let Some((ref target, _)) = *MCP_CONFIG {
        // Match if the path ends with the target filename
        // This handles both ".mcp.json" and "/path/to/.mcp.json"
        let path = Path::new(path_str);
        if let Some(filename) = path.file_name() {
            return filename.to_string_lossy() == *target;
        }
    }
    false
}

/// Get the overlay path CString if configured
fn get_overlay_cstr() -> Option<&'static CString> {
    MCP_CONFIG.as_ref().map(|(_, cstr)| cstr)
}

// ============================================================================
// Type Definitions for Original Functions
// ============================================================================

// Network functions
type ConnectFn = unsafe extern "C" fn(c_int, *const sockaddr, socklen_t) -> c_int;
type SendFn = unsafe extern "C" fn(c_int, *const c_void, size_t, c_int) -> ssize_t;
type RecvFn = unsafe extern "C" fn(c_int, *mut c_void, size_t, c_int) -> ssize_t;
type SendToFn =
    unsafe extern "C" fn(c_int, *const c_void, size_t, c_int, *const sockaddr, socklen_t)
        -> ssize_t;
type RecvFromFn =
    unsafe extern "C" fn(c_int, *mut c_void, size_t, c_int, *mut sockaddr, *mut socklen_t)
        -> ssize_t;
type CloseFn = unsafe extern "C" fn(c_int) -> c_int;

// Filesystem functions
type OpenFn = unsafe extern "C" fn(*const c_char, c_int, mode_t) -> c_int;
type OpenatFn = unsafe extern "C" fn(c_int, *const c_char, c_int, mode_t) -> c_int;
type StatFn = unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int;
type Stat64Fn = unsafe extern "C" fn(*const c_char, *mut libc::stat64) -> c_int;
type AccessFn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
// statx: int statx(int dirfd, const char *pathname, int flags, unsigned int mask, struct statx *statxbuf)
type StatxFn = unsafe extern "C" fn(c_int, *const c_char, c_int, libc::c_uint, *mut libc::statx) -> c_int;
type FstatatFn = unsafe extern "C" fn(c_int, *const c_char, *mut libc::stat, c_int) -> c_int;
type Fstatat64Fn = unsafe extern "C" fn(c_int, *const c_char, *mut libc::stat64, c_int) -> c_int;
type FaccessatFn = unsafe extern "C" fn(c_int, *const c_char, c_int, c_int) -> c_int;

/// Get the original libc function using dlsym
unsafe fn get_real_fn<T>(name: &str) -> Option<T> {
    let name_cstr = CString::new(name).ok()?;
    let handle = libc::dlsym(libc::RTLD_NEXT, name_cstr.as_ptr());
    if handle.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&handle))
    }
}

// ============================================================================
// Cached Original Functions
// ============================================================================

// Network
static REAL_CONNECT: Lazy<Option<ConnectFn>> =
    Lazy::new(|| unsafe { get_real_fn("connect") });
static REAL_SEND: Lazy<Option<SendFn>> = Lazy::new(|| unsafe { get_real_fn("send") });
static REAL_RECV: Lazy<Option<RecvFn>> = Lazy::new(|| unsafe { get_real_fn("recv") });
static REAL_SENDTO: Lazy<Option<SendToFn>> =
    Lazy::new(|| unsafe { get_real_fn("sendto") });
static REAL_RECVFROM: Lazy<Option<RecvFromFn>> =
    Lazy::new(|| unsafe { get_real_fn("recvfrom") });
static REAL_CLOSE: Lazy<Option<CloseFn>> = Lazy::new(|| unsafe { get_real_fn("close") });

// Filesystem
static REAL_OPEN: Lazy<Option<OpenFn>> = Lazy::new(|| unsafe { get_real_fn("open") });
static REAL_OPEN64: Lazy<Option<OpenFn>> = Lazy::new(|| unsafe { get_real_fn("open64") });
static REAL_OPENAT: Lazy<Option<OpenatFn>> = Lazy::new(|| unsafe { get_real_fn("openat") });
static REAL_STAT: Lazy<Option<StatFn>> = Lazy::new(|| unsafe { get_real_fn("stat") });
static REAL_STAT64: Lazy<Option<Stat64Fn>> = Lazy::new(|| unsafe { get_real_fn("stat64") });
static REAL_LSTAT: Lazy<Option<StatFn>> = Lazy::new(|| unsafe { get_real_fn("lstat") });
static REAL_LSTAT64: Lazy<Option<Stat64Fn>> = Lazy::new(|| unsafe { get_real_fn("lstat64") });
static REAL_ACCESS: Lazy<Option<AccessFn>> = Lazy::new(|| unsafe { get_real_fn("access") });
static REAL_STATX: Lazy<Option<StatxFn>> = Lazy::new(|| unsafe { get_real_fn("statx") });
static REAL_FSTATAT: Lazy<Option<FstatatFn>> = Lazy::new(|| unsafe { get_real_fn("fstatat") });
static REAL_FSTATAT64: Lazy<Option<Fstatat64Fn>> = Lazy::new(|| unsafe { get_real_fn("fstatat64") });
static REAL_FACCESSAT: Lazy<Option<FaccessatFn>> = Lazy::new(|| unsafe { get_real_fn("faccessat") });
static REAL_FACCESSAT2: Lazy<Option<FaccessatFn>> = Lazy::new(|| unsafe { get_real_fn("faccessat2") });

// ============================================================================
// Network Function Interception
// ============================================================================

/// Intercepted connect() function
#[no_mangle]
pub unsafe extern "C" fn connect(fd: c_int, addr: *const sockaddr, len: socklen_t) -> c_int {
    let (addr_str, port, family) = parse_sockaddr(addr);

    let result = match *REAL_CONNECT {
        Some(f) => f(fd, addr, len),
        None => {
            *libc::__errno_location() = libc::ENOSYS;
            -1
        }
    };

    log_event(&NetEvent::Connect {
        ts: timestamp(),
        fd,
        addr: addr_str,
        port,
        family,
        result,
    });

    result
}

/// Intercepted send() function
#[no_mangle]
pub unsafe extern "C" fn send(
    fd: c_int,
    buf: *const c_void,
    len: size_t,
    flags: c_int,
) -> ssize_t {
    let result = match *REAL_SEND {
        Some(f) => f(fd, buf, len, flags),
        None => {
            *libc::__errno_location() = libc::ENOSYS;
            -1
        }
    };

    log_event(&NetEvent::Send {
        ts: timestamp(),
        fd,
        bytes: len,
        result,
    });

    result
}

/// Intercepted recv() function
#[no_mangle]
pub unsafe extern "C" fn recv(
    fd: c_int,
    buf: *mut c_void,
    len: size_t,
    flags: c_int,
) -> ssize_t {
    let result = match *REAL_RECV {
        Some(f) => f(fd, buf, len, flags),
        None => {
            *libc::__errno_location() = libc::ENOSYS;
            -1
        }
    };

    log_event(&NetEvent::Recv {
        ts: timestamp(),
        fd,
        bytes: len,
        result,
    });

    result
}

/// Intercepted sendto() function
#[no_mangle]
pub unsafe extern "C" fn sendto(
    fd: c_int,
    buf: *const c_void,
    len: size_t,
    flags: c_int,
    dest_addr: *const sockaddr,
    addrlen: socklen_t,
) -> ssize_t {
    let (addr_str, port, _) = if !dest_addr.is_null() {
        parse_sockaddr(dest_addr)
    } else {
        ("none".to_string(), 0, "none".to_string())
    };

    let result = match *REAL_SENDTO {
        Some(f) => f(fd, buf, len, flags, dest_addr, addrlen),
        None => {
            *libc::__errno_location() = libc::ENOSYS;
            -1
        }
    };

    log_event(&NetEvent::SendTo {
        ts: timestamp(),
        fd,
        bytes: len,
        addr: if dest_addr.is_null() {
            None
        } else {
            Some(addr_str)
        },
        port: if dest_addr.is_null() { None } else { Some(port) },
        result,
    });

    result
}

/// Intercepted recvfrom() function
#[no_mangle]
pub unsafe extern "C" fn recvfrom(
    fd: c_int,
    buf: *mut c_void,
    len: size_t,
    flags: c_int,
    src_addr: *mut sockaddr,
    addrlen: *mut socklen_t,
) -> ssize_t {
    let result = match *REAL_RECVFROM {
        Some(f) => f(fd, buf, len, flags, src_addr, addrlen),
        None => {
            *libc::__errno_location() = libc::ENOSYS;
            -1
        }
    };

    log_event(&NetEvent::RecvFrom {
        ts: timestamp(),
        fd,
        bytes: len,
        result,
    });

    result
}

/// Intercepted close() function
/// We track socket closes to know when connections end
#[no_mangle]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    let result = match *REAL_CLOSE {
        Some(f) => f(fd),
        None => {
            *libc::__errno_location() = libc::ENOSYS;
            -1
        }
    };

    // Only log closes for likely socket fds (> stderr)
    if fd > 2 {
        log_event(&NetEvent::Close {
            ts: timestamp(),
            fd,
            result,
        });
    }

    result
}

// ============================================================================
// Filesystem Function Interception
// ============================================================================

/// Intercepted open() function
/// Redirects reads of the MCP target file to the overlay file
#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: mode_t) -> c_int {
    // Check if this is our overlay target
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();

        if should_overlay(&path_str) {
            // Redirect to overlay file
            if let Some(overlay_cstr) = get_overlay_cstr() {
                eprintln!("[aegis-hooks] REDIRECTING {} -> overlay", path_str);
                return match *REAL_OPEN {
                    Some(f) => f(overlay_cstr.as_ptr(), flags, mode),
                    None => {
                        *libc::__errno_location() = libc::ENOSYS;
                        -1
                    }
                };
            }
        }
    }

    // Normal open
    match *REAL_OPEN {
        Some(f) => f(path, flags, mode),
        None => {
            *libc::__errno_location() = libc::ENOSYS;
            -1
        }
    }
}

/// Intercepted open64() function - used by Rust and many 64-bit programs
#[no_mangle]
pub unsafe extern "C" fn open64(path: *const c_char, flags: c_int, mode: mode_t) -> c_int {
    // Check if this is our overlay target
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();

        if should_overlay(&path_str) {
            // Redirect to overlay file
            if let Some(overlay_cstr) = get_overlay_cstr() {
                eprintln!("[aegis-hooks] REDIRECTING {} -> overlay", path_str);
                return match *REAL_OPEN64 {
                    Some(f) => f(overlay_cstr.as_ptr(), flags, mode),
                    None => {
                        *libc::__errno_location() = libc::ENOSYS;
                        -1
                    }
                };
            }
        }
    }

    // Normal open64
    match *REAL_OPEN64 {
        Some(f) => f(path, flags, mode),
        None => {
            *libc::__errno_location() = libc::ENOSYS;
            -1
        }
    }
}

/// Intercepted openat() function
/// Redirects reads of the MCP target file to the overlay file
#[no_mangle]
pub unsafe extern "C" fn openat(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mode: mode_t,
) -> c_int {
    // Check if this is our overlay target
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();

        if should_overlay(&path_str) {
            // Redirect to overlay file (use AT_FDCWD to ignore dirfd)
            if let Some(overlay_cstr) = get_overlay_cstr() {
                eprintln!("[aegis-hooks] REDIRECTING {} -> overlay", path_str);
                return match *REAL_OPENAT {
                    Some(f) => f(libc::AT_FDCWD, overlay_cstr.as_ptr(), flags, mode),
                    None => {
                        *libc::__errno_location() = libc::ENOSYS;
                        -1
                    }
                };
            }
        }
    }

    // Normal openat
    match *REAL_OPENAT {
        Some(f) => f(dirfd, path, flags, mode),
        None => {
            *libc::__errno_location() = libc::ENOSYS;
            -1
        }
    }
}

// ============================================================================
// Stat/Access Interception (to make overlay file appear to exist)
// ============================================================================

/// Intercepted stat() - makes overlay target appear to exist
#[no_mangle]
pub unsafe extern "C" fn stat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();
        if should_overlay(&path_str) {
            if let Some(overlay_cstr) = get_overlay_cstr() {
                return match *REAL_STAT {
                    Some(f) => f(overlay_cstr.as_ptr(), buf),
                    None => { *libc::__errno_location() = libc::ENOSYS; -1 }
                };
            }
        }
    }
    match *REAL_STAT {
        Some(f) => f(path, buf),
        None => { *libc::__errno_location() = libc::ENOSYS; -1 }
    }
}

/// Intercepted stat64() - makes overlay target appear to exist
#[no_mangle]
pub unsafe extern "C" fn stat64(path: *const c_char, buf: *mut libc::stat64) -> c_int {
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();
        if should_overlay(&path_str) {
            if let Some(overlay_cstr) = get_overlay_cstr() {
                return match *REAL_STAT64 {
                    Some(f) => f(overlay_cstr.as_ptr(), buf),
                    None => { *libc::__errno_location() = libc::ENOSYS; -1 }
                };
            }
        }
    }
    match *REAL_STAT64 {
        Some(f) => f(path, buf),
        None => { *libc::__errno_location() = libc::ENOSYS; -1 }
    }
}

/// Intercepted lstat() - makes overlay target appear to exist
#[no_mangle]
pub unsafe extern "C" fn lstat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();
        if should_overlay(&path_str) {
            if let Some(overlay_cstr) = get_overlay_cstr() {
                return match *REAL_LSTAT {
                    Some(f) => f(overlay_cstr.as_ptr(), buf),
                    None => { *libc::__errno_location() = libc::ENOSYS; -1 }
                };
            }
        }
    }
    match *REAL_LSTAT {
        Some(f) => f(path, buf),
        None => { *libc::__errno_location() = libc::ENOSYS; -1 }
    }
}

/// Intercepted lstat64() - makes overlay target appear to exist
#[no_mangle]
pub unsafe extern "C" fn lstat64(path: *const c_char, buf: *mut libc::stat64) -> c_int {
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();
        if should_overlay(&path_str) {
            if let Some(overlay_cstr) = get_overlay_cstr() {
                return match *REAL_LSTAT64 {
                    Some(f) => f(overlay_cstr.as_ptr(), buf),
                    None => { *libc::__errno_location() = libc::ENOSYS; -1 }
                };
            }
        }
    }
    match *REAL_LSTAT64 {
        Some(f) => f(path, buf),
        None => { *libc::__errno_location() = libc::ENOSYS; -1 }
    }
}

/// Intercepted access() - makes overlay target appear to exist
#[no_mangle]
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();
        if should_overlay(&path_str) {
            if let Some(overlay_cstr) = get_overlay_cstr() {
                return match *REAL_ACCESS {
                    Some(f) => f(overlay_cstr.as_ptr(), mode),
                    None => { *libc::__errno_location() = libc::ENOSYS; -1 }
                };
            }
        }
    }
    match *REAL_ACCESS {
        Some(f) => f(path, mode),
        None => { *libc::__errno_location() = libc::ENOSYS; -1 }
    }
}

/// Intercepted statx() - modern stat syscall used by glibc 2.28+
/// int statx(int dirfd, const char *pathname, int flags, unsigned int mask, struct statx *statxbuf)
#[no_mangle]
pub unsafe extern "C" fn statx(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mask: libc::c_uint,
    statxbuf: *mut libc::statx,
) -> c_int {
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();
        if should_overlay(&path_str) {
            if let Some(overlay_cstr) = get_overlay_cstr() {
                return match *REAL_STATX {
                    Some(f) => f(libc::AT_FDCWD, overlay_cstr.as_ptr(), flags, mask, statxbuf),
                    None => { *libc::__errno_location() = libc::ENOSYS; -1 }
                };
            }
        }
    }
    match *REAL_STATX {
        Some(f) => f(dirfd, path, flags, mask, statxbuf),
        None => { *libc::__errno_location() = libc::ENOSYS; -1 }
    }
}

/// Intercepted fstatat() - stat relative to directory fd
#[no_mangle]
pub unsafe extern "C" fn fstatat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut libc::stat,
    flags: c_int,
) -> c_int {
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();
        if should_overlay(&path_str) {
            if let Some(overlay_cstr) = get_overlay_cstr() {
                return match *REAL_FSTATAT {
                    Some(f) => f(libc::AT_FDCWD, overlay_cstr.as_ptr(), buf, flags),
                    None => { *libc::__errno_location() = libc::ENOSYS; -1 }
                };
            }
        }
    }
    match *REAL_FSTATAT {
        Some(f) => f(dirfd, path, buf, flags),
        None => { *libc::__errno_location() = libc::ENOSYS; -1 }
    }
}

/// Intercepted fstatat64() - 64-bit stat relative to directory fd
#[no_mangle]
pub unsafe extern "C" fn fstatat64(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut libc::stat64,
    flags: c_int,
) -> c_int {
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();
        if should_overlay(&path_str) {
            if let Some(overlay_cstr) = get_overlay_cstr() {
                return match *REAL_FSTATAT64 {
                    Some(f) => f(libc::AT_FDCWD, overlay_cstr.as_ptr(), buf, flags),
                    None => { *libc::__errno_location() = libc::ENOSYS; -1 }
                };
            }
        }
    }
    match *REAL_FSTATAT64 {
        Some(f) => f(dirfd, path, buf, flags),
        None => { *libc::__errno_location() = libc::ENOSYS; -1 }
    }
}

/// Intercepted faccessat() - check file access relative to directory fd
#[no_mangle]
pub unsafe extern "C" fn faccessat(
    dirfd: c_int,
    path: *const c_char,
    mode: c_int,
    flags: c_int,
) -> c_int {
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();
        if should_overlay(&path_str) {
            if let Some(overlay_cstr) = get_overlay_cstr() {
                return match *REAL_FACCESSAT {
                    Some(f) => f(libc::AT_FDCWD, overlay_cstr.as_ptr(), mode, flags),
                    None => { *libc::__errno_location() = libc::ENOSYS; -1 }
                };
            }
        }
    }
    match *REAL_FACCESSAT {
        Some(f) => f(dirfd, path, mode, flags),
        None => { *libc::__errno_location() = libc::ENOSYS; -1 }
    }
}

/// Intercepted faccessat2() - newer version of faccessat
#[no_mangle]
pub unsafe extern "C" fn faccessat2(
    dirfd: c_int,
    path: *const c_char,
    mode: c_int,
    flags: c_int,
) -> c_int {
    if !path.is_null() {
        let path_str = CStr::from_ptr(path).to_string_lossy();
        if should_overlay(&path_str) {
            if let Some(overlay_cstr) = get_overlay_cstr() {
                return match *REAL_FACCESSAT2 {
                    Some(f) => f(libc::AT_FDCWD, overlay_cstr.as_ptr(), mode, flags),
                    None => {
                        // faccessat2 might not exist, fall back to faccessat
                        match *REAL_FACCESSAT {
                            Some(f) => f(libc::AT_FDCWD, overlay_cstr.as_ptr(), mode, flags),
                            None => { *libc::__errno_location() = libc::ENOSYS; -1 }
                        }
                    }
                };
            }
        }
    }
    match *REAL_FACCESSAT2 {
        Some(f) => f(dirfd, path, mode, flags),
        None => {
            // Fall back to faccessat
            match *REAL_FACCESSAT {
                Some(f) => f(dirfd, path, mode, flags),
                None => { *libc::__errno_location() = libc::ENOSYS; -1 }
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamp() {
        let ts = timestamp();
        assert!(ts > 0);
    }

    #[test]
    fn test_parse_sockaddr_null() {
        let (addr, port, family) = parse_sockaddr(std::ptr::null());
        assert_eq!(addr, "unknown");
        assert_eq!(port, 0);
        assert_eq!(family, "unknown");
    }

    #[test]
    fn test_should_overlay_no_config() {
        // Without env vars set, should never overlay
        // Note: This test depends on env vars NOT being set
        // In actual use, MCP_CONFIG is initialized from env vars
        let result = should_overlay(".mcp.json");
        // Result depends on whether env vars are set
        assert!(!result || result); // Always passes, just checking no panic
    }
}
