//! Runtime-loaded libsrt 1.5 adapter.
//!
//! Keeping symbol loading in this crate lets CPU-only builds compile without libsrt. A live
//! connection fails explicitly when a compatible shared library is unavailable.

#![cfg_attr(not(unix), allow(dead_code))]

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs},
    sync::{Arc, Mutex},
    time::Instant,
};

use aimedia_core::{
    SrtRuntimeStats,
    backend::{BackendError, Transport},
    config::{SecretError, SrtConfig, SrtMode},
};
use async_trait::async_trait;
use libloading::Library;
use thiserror::Error;
use url::Url;

const SRT_ERROR: c_int = -1;
const SRT_INVALID_SOCKET: c_int = -1;
const SRTS_CONNECTED: c_int = 5;
const SRT_EPOLL_IN: c_int = 0x1;
const SRT_EPOLL_OUT: c_int = 0x4;
const SRT_EPOLL_ERR: c_int = 0x8;
const SRTO_SNDSYN: c_int = 1;
const SRTO_RCVSYN: c_int = 2;
const SRTO_PASSPHRASE: c_int = 26;
const SRTO_PBKEYLEN: c_int = 27;
const SRTO_TLPKTDROP: c_int = 31;
const SRTO_CONNTIMEO: c_int = 36;
const SRTO_RCVLATENCY: c_int = 43;
const SRTO_PEERLATENCY: c_int = 44;
const SRTO_STREAMID: c_int = 46;
const SRTO_MESSAGEAPI: c_int = 48;
const SRTO_PAYLOADSIZE: c_int = 49;
const SRTO_TRANSTYPE: c_int = 50;
const SRTT_LIVE: c_int = 0;
const MPEG_TS_PAYLOAD_SIZE: usize = 1_316;

#[derive(Debug, Error)]
pub enum SrtError {
    #[error("SRT transport is only supported on Unix in the alpha profile")]
    UnsupportedPlatform,
    #[error("could not load libsrt 1.5 shared library: {0}")]
    Library(String),
    #[error("libsrt symbol {symbol} is unavailable: {message}")]
    Symbol {
        symbol: &'static str,
        message: String,
    },
    #[error("invalid SRT URI: {0}")]
    InvalidUri(String),
    #[error("SRT host could not be resolved: {0}")]
    Resolve(String),
    #[error("SRT passphrase must contain 10 to 79 bytes")]
    InvalidPassphrase,
    #[error("secret resolution failed: {0}")]
    Secret(#[from] SecretError),
    #[error("libsrt {operation} failed: {message}")]
    Operation {
        operation: &'static str,
        message: String,
    },
    #[error("SRT operation timed out after {0}ms")]
    Timeout(u64),
    #[error("SRT transport lock is poisoned")]
    Poisoned,
    #[error("SRT transport is closed")]
    Closed,
}

impl From<SrtError> for BackendError {
    fn from(error: SrtError) -> Self {
        match error {
            SrtError::UnsupportedPlatform | SrtError::Library(_) | SrtError::Symbol { .. } => {
                Self::Unavailable(error.to_string())
            }
            SrtError::InvalidUri(_)
            | SrtError::InvalidPassphrase
            | SrtError::Secret(_)
            | SrtError::Resolve(_) => Self::Unsupported(error.to_string()),
            SrtError::Closed => Self::EndOfStream,
            SrtError::Poisoned | SrtError::Operation { .. } | SrtError::Timeout(_) => {
                Self::Io(error.to_string())
            }
        }
    }
}

#[derive(Clone)]
pub struct Endpoint {
    pub uri: String,
    pub mode: SrtMode,
    pub latency_ms: u64,
    pub connect_timeout_ms: u64,
    pub stream_id: Option<String>,
    pub passphrase: Option<String>,
    pub key_length: u16,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("mode", &self.mode)
            .field("latency_ms", &self.latency_ms)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("has_stream_id", &self.stream_id.is_some())
            .field("encrypted", &self.passphrase.is_some())
            .field("key_length", &self.key_length)
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    pub fn from_config(
        uri: &str,
        srt: &SrtConfig,
        passphrase: Option<&aimedia_core::config::SecretRef>,
    ) -> Result<Self, SrtError> {
        let passphrase = passphrase
            .map(|reference| reference.resolve())
            .transpose()?;
        if passphrase
            .as_deref()
            .is_some_and(|value| !(10..=79).contains(&value.len()))
        {
            return Err(SrtError::InvalidPassphrase);
        }
        Ok(Self {
            uri: uri.to_owned(),
            mode: srt.effective_mode(uri),
            latency_ms: srt.latency_ms,
            connect_timeout_ms: srt.connect_timeout_ms,
            stream_id: srt.resolve_stream_id()?,
            passphrase,
            key_length: srt.key_length,
        })
    }
}

type Socket = c_int;
type Startup = unsafe extern "C" fn() -> c_int;
type Cleanup = unsafe extern "C" fn() -> c_int;
type CreateSocket = unsafe extern "C" fn() -> Socket;
type Bind = unsafe extern "C" fn(Socket, *const libc::sockaddr, c_int) -> c_int;
type Listen = unsafe extern "C" fn(Socket, c_int) -> c_int;
type Accept = unsafe extern "C" fn(Socket, *mut libc::sockaddr, *mut c_int) -> Socket;
type Connect = unsafe extern "C" fn(Socket, *const libc::sockaddr, c_int) -> c_int;
type Close = unsafe extern "C" fn(Socket) -> c_int;
type SetSockFlag = unsafe extern "C" fn(Socket, c_int, *const c_void, c_int) -> c_int;
type RecvMsg = unsafe extern "C" fn(Socket, *mut c_char, c_int) -> c_int;
type SendMsg = unsafe extern "C" fn(Socket, *const c_char, c_int, c_int, c_int) -> c_int;
type LastError = unsafe extern "C" fn() -> *const c_char;
type GetSockState = unsafe extern "C" fn(Socket) -> c_int;
type GetVersion = unsafe extern "C" fn() -> u32;
type Bistats = unsafe extern "C" fn(Socket, *mut SrtTraceStats, c_int, c_int) -> c_int;
type EpollCreate = unsafe extern "C" fn() -> c_int;
type EpollAdd = unsafe extern "C" fn(c_int, Socket, *const c_int) -> c_int;
type EpollRemove = unsafe extern "C" fn(c_int, Socket) -> c_int;
type EpollWait = unsafe extern "C" fn(c_int, *mut SrtEpollEvent, c_int, i64) -> c_int;
type EpollRelease = unsafe extern "C" fn(c_int) -> c_int;

struct Api {
    _library: Library,
    cleanup: Cleanup,
    create_socket: CreateSocket,
    bind: Bind,
    listen: Listen,
    accept: Accept,
    connect: Connect,
    close: Close,
    set_sock_flag: SetSockFlag,
    recv_msg: RecvMsg,
    send_msg: SendMsg,
    last_error: LastError,
    get_sock_state: GetSockState,
    get_version: GetVersion,
    bistats: Bistats,
    epoll_create: EpollCreate,
    epoll_add: EpollAdd,
    epoll_remove: EpollRemove,
    epoll_wait: EpollWait,
    epoll_release: EpollRelease,
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

impl Api {
    #[cfg(unix)]
    fn load() -> Result<Arc<Self>, SrtError> {
        let candidates = ["libsrt.so.1.5", "libsrt.so.1", "libsrt.so"];
        let mut last_error = String::new();
        for candidate in candidates {
            // SAFETY: loading a native library is isolated in this FFI crate. Every required
            // symbol is resolved before the Api can be returned.
            let library = match unsafe { Library::new(candidate) } {
                Ok(library) => library,
                Err(error) => {
                    last_error = error.to_string();
                    continue;
                }
            };
            // SAFETY: names and signatures are from libsrt 1.5.5 srt.h.
            let startup: Startup =
                unsafe { load_symbol(&library, b"srt_startup\0", "srt_startup")? };
            // SAFETY: libsrt requires one process-level startup before socket operations.
            if unsafe { startup() } == SRT_ERROR {
                return Err(SrtError::Library(
                    "srt_startup returned SRT_ERROR".to_owned(),
                ));
            }
            // SAFETY: all signatures are copied from the version-pinned C header.
            let api = unsafe {
                Self {
                    cleanup: load_symbol(&library, b"srt_cleanup\0", "srt_cleanup")?,
                    create_socket: load_symbol(
                        &library,
                        b"srt_create_socket\0",
                        "srt_create_socket",
                    )?,
                    bind: load_symbol(&library, b"srt_bind\0", "srt_bind")?,
                    listen: load_symbol(&library, b"srt_listen\0", "srt_listen")?,
                    accept: load_symbol(&library, b"srt_accept\0", "srt_accept")?,
                    connect: load_symbol(&library, b"srt_connect\0", "srt_connect")?,
                    close: load_symbol(&library, b"srt_close\0", "srt_close")?,
                    set_sock_flag: load_symbol(&library, b"srt_setsockflag\0", "srt_setsockflag")?,
                    recv_msg: load_symbol(&library, b"srt_recvmsg\0", "srt_recvmsg")?,
                    send_msg: load_symbol(&library, b"srt_sendmsg\0", "srt_sendmsg")?,
                    last_error: load_symbol(
                        &library,
                        b"srt_getlasterror_str\0",
                        "srt_getlasterror_str",
                    )?,
                    get_sock_state: load_symbol(
                        &library,
                        b"srt_getsockstate\0",
                        "srt_getsockstate",
                    )?,
                    get_version: load_symbol(&library, b"srt_getversion\0", "srt_getversion")?,
                    bistats: load_symbol(&library, b"srt_bistats\0", "srt_bistats")?,
                    epoll_create: load_symbol(&library, b"srt_epoll_create\0", "srt_epoll_create")?,
                    epoll_add: load_symbol(
                        &library,
                        b"srt_epoll_add_usock\0",
                        "srt_epoll_add_usock",
                    )?,
                    epoll_remove: load_symbol(
                        &library,
                        b"srt_epoll_remove_usock\0",
                        "srt_epoll_remove_usock",
                    )?,
                    epoll_wait: load_symbol(&library, b"srt_epoll_uwait\0", "srt_epoll_uwait")?,
                    epoll_release: load_symbol(
                        &library,
                        b"srt_epoll_release\0",
                        "srt_epoll_release",
                    )?,
                    _library: library,
                }
            };
            return Ok(Arc::new(api));
        }
        Err(SrtError::Library(last_error))
    }

    #[cfg(not(unix))]
    fn load() -> Result<Arc<Self>, SrtError> {
        Err(SrtError::UnsupportedPlatform)
    }

    fn error(&self, operation: &'static str) -> SrtError {
        // SAFETY: libsrt returns a process-owned NUL-terminated error string.
        let pointer = unsafe { (self.last_error)() };
        let message = if pointer.is_null() {
            "unknown libsrt error".to_owned()
        } else {
            // SAFETY: non-null pointer contract above.
            unsafe { CStr::from_ptr(pointer) }
                .to_string_lossy()
                .into_owned()
        };
        SrtError::Operation { operation, message }
    }
}

pub fn probe_version() -> Result<u32, SrtError> {
    let api = Api::load()?;
    // SAFETY: srt_getversion takes no arguments and the library remains loaded.
    Ok(unsafe { (api.get_version)() })
}

impl Drop for Api {
    fn drop(&mut self) {
        // SAFETY: this is the matching process-level cleanup for startup.
        let _ = unsafe { (self.cleanup)() };
    }
}

unsafe fn load_symbol<T: Copy>(
    library: &Library,
    name: &[u8],
    display_name: &'static str,
) -> Result<T, SrtError> {
    // SAFETY: the caller supplies the exact signature from the pinned header.
    unsafe { library.get::<T>(name) }
        .map(|symbol| *symbol)
        .map_err(|error| SrtError::Symbol {
            symbol: display_name,
            message: error.to_string(),
        })
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SrtEpollEvent {
    socket: Socket,
    events: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SrtTraceStats {
    ms_time_stamp: i64,
    pkt_sent_total: i64,
    pkt_recv_total: i64,
    pkt_snd_loss_total: c_int,
    pkt_rcv_loss_total: c_int,
    pkt_retrans_total: c_int,
    pkt_sent_ack_total: c_int,
    pkt_recv_ack_total: c_int,
    pkt_sent_nak_total: c_int,
    pkt_recv_nak_total: c_int,
    us_snd_duration_total: i64,
    pkt_snd_drop_total: c_int,
    pkt_rcv_drop_total: c_int,
    pkt_rcv_undecrypt_total: c_int,
    byte_sent_total: u64,
    byte_recv_total: u64,
    byte_rcv_loss_total: u64,
    byte_retrans_total: u64,
    byte_snd_drop_total: u64,
    byte_rcv_drop_total: u64,
    byte_rcv_undecrypt_total: u64,
    pkt_sent: i64,
    pkt_recv: i64,
    pkt_snd_loss: c_int,
    pkt_rcv_loss: c_int,
    pkt_retrans: c_int,
    pkt_rcv_retrans: c_int,
    pkt_sent_ack: c_int,
    pkt_recv_ack: c_int,
    pkt_sent_nak: c_int,
    pkt_recv_nak: c_int,
    mbps_send_rate: f64,
    mbps_recv_rate: f64,
    us_snd_duration: i64,
    pkt_reorder_distance: c_int,
    pkt_rcv_avg_belated_time: f64,
    pkt_rcv_belated: i64,
    pkt_snd_drop: c_int,
    pkt_rcv_drop: c_int,
    pkt_rcv_undecrypt: c_int,
    byte_sent: u64,
    byte_recv: u64,
    byte_rcv_loss: u64,
    byte_retrans: u64,
    byte_snd_drop: u64,
    byte_rcv_drop: u64,
    byte_rcv_undecrypt: u64,
    us_pkt_snd_period: f64,
    pkt_flow_window: c_int,
    pkt_congestion_window: c_int,
    pkt_flight_size: c_int,
    ms_rtt: f64,
    mbps_bandwidth: f64,
    byte_avail_snd_buf: c_int,
    byte_avail_rcv_buf: c_int,
    mbps_max_bw: f64,
    byte_mss: c_int,
    pkt_snd_buf: c_int,
    byte_snd_buf: c_int,
    ms_snd_buf: c_int,
    ms_snd_tsbpd_delay: c_int,
    pkt_rcv_buf: c_int,
    byte_rcv_buf: c_int,
    ms_rcv_buf: c_int,
    ms_rcv_tsbpd_delay: c_int,
    pkt_snd_filter_extra_total: c_int,
    pkt_rcv_filter_extra_total: c_int,
    pkt_rcv_filter_supply_total: c_int,
    pkt_rcv_filter_loss_total: c_int,
    pkt_snd_filter_extra: c_int,
    pkt_rcv_filter_extra: c_int,
    pkt_rcv_filter_supply: c_int,
    pkt_rcv_filter_loss: c_int,
    pkt_reorder_tolerance: c_int,
    pkt_sent_unique_total: i64,
    pkt_recv_unique_total: i64,
    byte_sent_unique_total: u64,
    byte_recv_unique_total: u64,
    pkt_sent_unique: i64,
    pkt_recv_unique: i64,
    byte_sent_unique: u64,
    byte_recv_unique: u64,
}

impl Default for SrtTraceStats {
    fn default() -> Self {
        // SAFETY: this C statistics struct is valid when zero-initialized before srt_bistats.
        unsafe { std::mem::zeroed() }
    }
}

struct SocketHandle {
    api: Arc<Api>,
    socket: Socket,
    epoll: c_int,
    last_data_at: Option<Instant>,
    reconnects: u64,
}

impl SocketHandle {
    fn connect(endpoint: &Endpoint) -> Result<Self, SrtError> {
        let api = Api::load()?;
        let address = resolve_uri(&endpoint.uri, endpoint.mode)?;
        // SAFETY: no arguments and the library has been initialized.
        let socket = unsafe { (api.create_socket)() };
        if socket == SRT_INVALID_SOCKET {
            return Err(api.error("create_socket"));
        }
        let mut handle = Self {
            api,
            socket,
            epoll: SRT_ERROR,
            last_data_at: None,
            reconnects: 0,
        };
        handle.configure(endpoint).and_then(|()| {
            let epoll = unsafe { (handle.api.epoll_create)() };
            if epoll == SRT_ERROR {
                return Err(handle.api.error("epoll_create"));
            }
            handle.epoll = epoll;
            let events = SRT_EPOLL_IN | SRT_EPOLL_OUT | SRT_EPOLL_ERR;
            // SAFETY: valid epoll id, socket, and event pointer.
            if unsafe { (handle.api.epoll_add)(epoll, socket, &events) } == SRT_ERROR {
                return Err(handle.api.error("epoll_add_usock"));
            }
            match endpoint.mode {
                SrtMode::Caller => handle.connect_caller(address, endpoint.connect_timeout_ms),
                SrtMode::Listener => handle.connect_listener(address, endpoint.connect_timeout_ms),
            }
        })?;
        Ok(handle)
    }

    fn configure(&self, endpoint: &Endpoint) -> Result<(), SrtError> {
        self.set_bool(SRTO_SNDSYN, false)?;
        self.set_bool(SRTO_RCVSYN, false)?;
        self.set_i32(SRTO_TRANSTYPE, SRTT_LIVE)?;
        self.set_bool(SRTO_MESSAGEAPI, true)?;
        self.set_bool(SRTO_TLPKTDROP, true)?;
        self.set_i32(SRTO_PAYLOADSIZE, MPEG_TS_PAYLOAD_SIZE as c_int)?;
        self.set_i32(SRTO_RCVLATENCY, endpoint.latency_ms as c_int)?;
        self.set_i32(SRTO_PEERLATENCY, endpoint.latency_ms as c_int)?;
        self.set_i32(SRTO_CONNTIMEO, endpoint.connect_timeout_ms as c_int)?;
        self.set_i32(SRTO_PBKEYLEN, c_int::from(endpoint.key_length))?;
        if let Some(passphrase) = &endpoint.passphrase {
            self.set_string(SRTO_PASSPHRASE, passphrase)?;
        }
        if endpoint.mode == SrtMode::Caller {
            if let Some(stream_id) = &endpoint.stream_id {
                self.set_string(SRTO_STREAMID, stream_id)?;
            }
        }
        Ok(())
    }

    fn set_bool(&self, option: c_int, value: bool) -> Result<(), SrtError> {
        self.set_i32(option, c_int::from(value))
    }

    fn set_i32(&self, option: c_int, value: c_int) -> Result<(), SrtError> {
        // SAFETY: value points to a c_int for the documented option.
        let result = unsafe {
            (self.api.set_sock_flag)(
                self.socket,
                option,
                (&value as *const c_int).cast(),
                std::mem::size_of::<c_int>() as c_int,
            )
        };
        if result == SRT_ERROR {
            Err(self.api.error("setsockflag"))
        } else {
            Ok(())
        }
    }

    fn set_string(&self, option: c_int, value: &str) -> Result<(), SrtError> {
        let value = CString::new(value)
            .map_err(|_| SrtError::InvalidUri("SRT option contains NUL".to_owned()))?;
        // SRT string options use byte length without the terminating NUL.
        let length = value.as_bytes().len() as c_int;
        // SAFETY: pointer remains valid for the duration of the call.
        let result =
            unsafe { (self.api.set_sock_flag)(self.socket, option, value.as_ptr().cast(), length) };
        if result == SRT_ERROR {
            Err(self.api.error("setsockflag"))
        } else {
            Ok(())
        }
    }

    fn connect_caller(&mut self, address: SocketAddrV4, timeout_ms: u64) -> Result<(), SrtError> {
        let native = native_address(address);
        // SAFETY: native is a valid IPv4 sockaddr.
        let result = unsafe {
            (self.api.connect)(
                self.socket,
                (&native as *const libc::sockaddr_in).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as c_int,
            )
        };
        if result != SRT_ERROR
            || unsafe { (self.api.get_sock_state)(self.socket) } == SRTS_CONNECTED
        {
            return Ok(());
        }
        self.wait_for(SRT_EPOLL_OUT, timeout_ms)?;
        if unsafe { (self.api.get_sock_state)(self.socket) } == SRTS_CONNECTED {
            Ok(())
        } else {
            Err(self.api.error("connect"))
        }
    }

    fn connect_listener(&mut self, address: SocketAddrV4, timeout_ms: u64) -> Result<(), SrtError> {
        let native = native_address(address);
        // SAFETY: native is a valid IPv4 sockaddr.
        if unsafe {
            (self.api.bind)(
                self.socket,
                (&native as *const libc::sockaddr_in).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as c_int,
            )
        } == SRT_ERROR
        {
            return Err(self.api.error("bind"));
        }
        // SAFETY: socket has been bound.
        if unsafe { (self.api.listen)(self.socket, 1) } == SRT_ERROR {
            return Err(self.api.error("listen"));
        }
        self.wait_for(SRT_EPOLL_IN, timeout_ms)?;
        let mut peer: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::sockaddr_in>() as c_int;
        // SAFETY: peer and length are valid output pointers.
        let accepted = unsafe {
            (self.api.accept)(
                self.socket,
                (&mut peer as *mut libc::sockaddr_in).cast(),
                &mut length,
            )
        };
        if accepted == SRT_INVALID_SOCKET {
            return Err(self.api.error("accept"));
        }
        let listener = std::mem::replace(&mut self.socket, accepted);
        // SAFETY: listener is registered in this epoll and will no longer accept peers.
        let _ = unsafe { (self.api.epoll_remove)(self.epoll, listener) };
        // SAFETY: the Alpha listener serves one peer at a time.
        let _ = unsafe { (self.api.close)(listener) };
        let events = SRT_EPOLL_IN | SRT_EPOLL_OUT | SRT_EPOLL_ERR;
        // SAFETY: accepted socket is valid.
        if unsafe { (self.api.epoll_add)(self.epoll, accepted, &events) } == SRT_ERROR {
            return Err(self.api.error("epoll_add_usock"));
        }
        Ok(())
    }

    fn wait_for(&self, desired: c_int, timeout_ms: u64) -> Result<(), SrtError> {
        let started = Instant::now();
        loop {
            let elapsed = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            if elapsed >= timeout_ms {
                return Err(SrtError::Timeout(timeout_ms));
            }
            let remaining = (timeout_ms - elapsed).min(100);
            let mut events = [SrtEpollEvent {
                socket: SRT_INVALID_SOCKET,
                events: 0,
            }; 4];
            // SAFETY: events is a writable fixed-capacity output buffer.
            let count = unsafe {
                (self.api.epoll_wait)(
                    self.epoll,
                    events.as_mut_ptr(),
                    events.len() as c_int,
                    remaining as i64,
                )
            };
            if count == SRT_ERROR {
                continue;
            }
            if events[..count as usize]
                .iter()
                .any(|event| event.events & desired != 0)
            {
                return Ok(());
            }
            if events[..count as usize]
                .iter()
                .any(|event| event.events & SRT_EPOLL_ERR != 0)
            {
                return Err(self.api.error("epoll_wait"));
            }
        }
    }

    fn receive(&mut self) -> Result<Vec<u8>, SrtError> {
        self.wait_for(SRT_EPOLL_IN, 1_000)?;
        let mut buffer = vec![0_u8; MPEG_TS_PAYLOAD_SIZE];
        // SAFETY: buffer is writable for the supplied length.
        let received = unsafe {
            (self.api.recv_msg)(
                self.socket,
                buffer.as_mut_ptr().cast(),
                buffer.len() as c_int,
            )
        };
        if received == SRT_ERROR {
            return Err(self.api.error("recvmsg"));
        }
        buffer.truncate(received as usize);
        self.last_data_at = Some(Instant::now());
        Ok(buffer)
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), SrtError> {
        for chunk in payload.chunks(MPEG_TS_PAYLOAD_SIZE) {
            self.wait_for(SRT_EPOLL_OUT, 1_000)?;
            // SAFETY: chunk is readable for the supplied length.
            let sent = unsafe {
                (self.api.send_msg)(
                    self.socket,
                    chunk.as_ptr().cast(),
                    chunk.len() as c_int,
                    -1,
                    1,
                )
            };
            if sent == SRT_ERROR {
                return Err(self.api.error("sendmsg"));
            }
            if sent as usize != chunk.len() {
                return Err(SrtError::Operation {
                    operation: "sendmsg",
                    message: format!("short SRT message: sent {sent} of {}", chunk.len()),
                });
            }
        }
        self.last_data_at = Some(Instant::now());
        Ok(())
    }

    fn stats(&self) -> Result<SrtRuntimeStats, SrtError> {
        let mut stats = SrtTraceStats::default();
        // SAFETY: stats exactly mirrors SRT_TRACEBSTATS in libsrt 1.5.5.
        if unsafe { (self.api.bistats)(self.socket, &mut stats, 0, 1) } == SRT_ERROR {
            return Err(self.api.error("bistats"));
        }
        Ok(SrtRuntimeStats {
            connected: unsafe { (self.api.get_sock_state)(self.socket) } == SRTS_CONNECTED,
            rtt_ms: stats.ms_rtt,
            packets_lost: u64::try_from(stats.pkt_rcv_loss_total.max(stats.pkt_snd_loss_total))
                .unwrap_or(0),
            packets_retransmitted: u64::try_from(stats.pkt_retrans_total).unwrap_or(0),
            receive_buffer_bytes: u64::try_from(stats.byte_rcv_buf).unwrap_or(0),
            reconnects: self.reconnects,
            last_data_age_ms: self
                .last_data_at
                .map(|instant| instant.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
        })
    }
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        if self.epoll != SRT_ERROR {
            // SAFETY: epoll was created by this handle.
            let _ = unsafe { (self.api.epoll_release)(self.epoll) };
        }
        // SAFETY: sockets belong to this handle and close tolerates broken sockets.
        let _ = unsafe { (self.api.close)(self.socket) };
    }
}

fn resolve_uri(uri: &str, mode: SrtMode) -> Result<SocketAddrV4, SrtError> {
    let parsed = Url::parse(uri).map_err(|error| SrtError::InvalidUri(error.to_string()))?;
    if parsed.scheme() != "srt" {
        return Err(SrtError::InvalidUri("scheme must be srt".to_owned()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(SrtError::InvalidUri(
            "URI userinfo credentials are forbidden; use secretRef".to_owned(),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| SrtError::InvalidUri("host is required".to_owned()))?;
    let port = parsed
        .port()
        .ok_or_else(|| SrtError::InvalidUri("port is required".to_owned()))?;
    if mode == SrtMode::Listener && (host == "0.0.0.0" || host == "*") {
        return Ok(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
    }
    (host, port)
        .to_socket_addrs()
        .map_err(|error| SrtError::Resolve(error.to_string()))?
        .find_map(|address| match address {
            SocketAddr::V4(address) => Some(address),
            SocketAddr::V6(_) => None,
        })
        .ok_or_else(|| SrtError::Resolve(format!("{host}:{port} has no IPv4 address")))
}

#[cfg(unix)]
fn native_address(address: SocketAddrV4) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: address.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(address.ip().octets()),
        },
        sin_zero: [0; 8],
    }
}

#[cfg(not(unix))]
fn native_address(_address: SocketAddrV4) -> libc::sockaddr_in {
    unreachable!("SRT is Unix-only in the alpha profile")
}

#[derive(Clone)]
pub struct SrtTransport {
    inner: Arc<Mutex<Option<SocketHandle>>>,
}

impl std::fmt::Debug for SrtTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SrtTransport")
            .finish_non_exhaustive()
    }
}

impl SrtTransport {
    pub async fn connect(endpoint: Endpoint) -> Result<Self, SrtError> {
        let handle = tokio::task::spawn_blocking(move || SocketHandle::connect(&endpoint))
            .await
            .map_err(|error| SrtError::Operation {
                operation: "connect task",
                message: error.to_string(),
            })??;
        Ok(Self {
            inner: Arc::new(Mutex::new(Some(handle))),
        })
    }

    pub async fn stats(&self) -> Result<SrtRuntimeStats, SrtError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner.lock().map_err(|_| SrtError::Poisoned)?;
            guard.as_ref().ok_or(SrtError::Closed)?.stats()
        })
        .await
        .map_err(|error| SrtError::Operation {
            operation: "stats task",
            message: error.to_string(),
        })?
    }
}

#[async_trait]
impl Transport for SrtTransport {
    async fn receive(&mut self) -> Result<Vec<u8>, BackendError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.lock().map_err(|_| SrtError::Poisoned)?;
            guard.as_mut().ok_or(SrtError::Closed)?.receive()
        })
        .await
        .map_err(|error| BackendError::Io(error.to_string()))?
        .map_err(BackendError::from)
    }

    async fn send(&mut self, payload: &[u8]) -> Result<(), BackendError> {
        let inner = Arc::clone(&self.inner);
        let payload = payload.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.lock().map_err(|_| SrtError::Poisoned)?;
            guard.as_mut().ok_or(SrtError::Closed)?.send(&payload)
        })
        .await
        .map_err(|error| BackendError::Io(error.to_string()))?
        .map_err(BackendError::from)
    }

    async fn close(&mut self) -> Result<(), BackendError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .map_err(|_| SrtError::Poisoned)?
                .take()
                .ok_or(SrtError::Closed)?;
            Ok::<(), SrtError>(())
        })
        .await
        .map_err(|error| BackendError::Io(error.to_string()))?
        .map_err(BackendError::from)
    }
}

#[cfg(test)]
mod tests {
    use aimedia_core::{
        backend::Transport,
        config::{ReconnectConfig, SrtConfig, SrtMode},
    };

    use super::{Endpoint, SrtTransport, resolve_uri};

    #[test]
    fn trace_stats_layout_matches_libsrt_1_5_5_x86_64() {
        assert_eq!(std::mem::size_of::<super::SrtTraceStats>(), 496);
    }

    #[test]
    fn parses_listener_wildcard_and_keeps_secrets_out_of_uri() {
        let address =
            resolve_uri("srt://0.0.0.0:9001", SrtMode::Listener).expect("listener URI parses");
        assert_eq!(address.port(), 9001);
        assert!(address.ip().is_unspecified());

        let config = SrtConfig {
            mode: Some(SrtMode::Caller),
            latency_ms: 120,
            connect_timeout_ms: 3_000,
            reconnect: ReconnectConfig::default(),
            stream_id: Some("camera/one".to_owned()),
            stream_id_ref: None,
            key_length: 16,
        };
        let endpoint =
            Endpoint::from_config("srt://127.0.0.1:9001", &config, None).expect("endpoint builds");
        assert_eq!(endpoint.stream_id.as_deref(), Some("camera/one"));
    }

    #[ignore = "requires libsrt 1.5 and an available local UDP port"]
    #[tokio::test]
    async fn native_caller_listener_loopback_preserves_one_ts_message() {
        let listener = Endpoint {
            uri: "srt://127.0.0.1:19091".to_owned(),
            mode: SrtMode::Listener,
            latency_ms: 20,
            connect_timeout_ms: 3_000,
            stream_id: None,
            passphrase: None,
            key_length: 16,
        };
        let caller = Endpoint {
            mode: SrtMode::Caller,
            ..listener.clone()
        };
        let listener_task = tokio::spawn(SrtTransport::connect(listener));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut caller = SrtTransport::connect(caller)
            .await
            .expect("caller connects");
        let mut listener = listener_task
            .await
            .expect("listener task joins")
            .expect("listener accepts");
        let payload = vec![0x47_u8; 1_316];
        caller.send(&payload).await.expect("caller sends");
        let received = listener.receive().await.expect("listener receives");
        assert_eq!(received, payload);
        caller.close().await.expect("caller closes");
        listener.close().await.expect("listener closes");
    }
}
