// SPDX-License-Identifier: Apache-2.0
//
// Slirp-based in-process network backend for Windows.
// Uses libslirp via FFI and crossbeam channels for frame exchange.

#![cfg(target_os = "windows")]

use std::os::windows::io::RawHandle;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{self, Receiver, Sender, TrySendError};

use super::backend::{ConnectError, NetBackend, ReadError, WriteError};
use super::write_virtio_net_hdr;
use super::VNET_HDR_LEN;

use libslirp_sys::*;

use std::ffi::c_void;
use std::os::raw::c_int;
use std::ptr;

use windows_sys::Win32::Foundation::HANDLE;

/// Maximum number of socket FDs we track for polling.
const MAX_POLLFDS: usize = 256;

/// Wrapper around a raw handle value (stored as usize) to allow Send.
/// On Windows, RawHandle is *mut c_void which is not Send.
/// We store the underlying value as usize and convert back when needed.
#[derive(Clone, Copy)]
struct SendableHandle(usize);

// SAFETY: The handle is only used within the slirp thread after being moved there.
unsafe impl Send for SendableHandle {}

impl SendableHandle {
    fn from_raw(h: RawHandle) -> Self {
        SendableHandle(h as usize)
    }

    fn as_raw_handle(self) -> RawHandle {
        self.0 as RawHandle
    }

    fn as_handle(self) -> HANDLE {
        self.0 as HANDLE
    }
}

/// Slirp network backend using in-process libslirp.
///
/// Frames are exchanged between the virtio-net worker and the slirp event loop
/// thread via crossbeam channels.
pub struct SlirpBackend {
    /// Channel to send frames FROM guest TO slirp
    tx_sender: Sender<Vec<u8>>,
    /// Channel to receive frames FROM slirp TO guest
    rx_receiver: Receiver<Vec<u8>>,
    /// Pending received frame (partially consumed)
    pending_rx: Option<Vec<u8>>,
    /// Event handle to signal when RX data is available (for epoll/IOCP integration)
    rx_event: SendableHandle,
}

// Safety: All fields are Send (channels are Send, SendableHandle wraps usize).
unsafe impl Send for SlirpBackend {}

/// Context passed through libslirp's opaque pointer to callbacks.
struct SlirpContext {
    /// Send frames from slirp back to guest
    rx_sender: Sender<Vec<u8>>,
    /// Receive frames from guest to feed into slirp
    tx_receiver: Receiver<Vec<u8>>,
    /// Event handle to signal when a frame is available for guest RX
    rx_event: SendableHandle,
    /// Timer storage: Vec of (id, expiry_ms) — simple polling-based timers
    timers: Vec<SlirpTimer>,
    /// Next timer ID
    next_timer_id: usize,
}

struct SlirpTimer {
    id: usize,
    expire_time_ms: i64,
    cb: SlirpTimerCb,
    cb_opaque: *mut c_void,
}

// SAFETY: The SlirpTimer contains raw pointers that are only used within the
// slirp thread. We need Send to store them in SlirpContext which is moved
// to the slirp thread.
unsafe impl Send for SlirpTimer {}

impl SlirpBackend {
    /// Create a new SlirpBackend. Spawns the slirp event loop thread.
    pub fn new() -> Result<Self, ConnectError> {
        // Channels for frame exchange. Bounded to avoid unbounded memory growth.
        let (tx_sender, tx_receiver) = crossbeam_channel::bounded::<Vec<u8>>(256);
        let (rx_sender, rx_receiver) = crossbeam_channel::bounded::<Vec<u8>>(256);

        // Create a Windows event for signaling RX availability.
        let rx_event_handle: HANDLE = unsafe {
            windows_sys::Win32::System::Threading::CreateEventW(
                ptr::null(),
                0, // auto-reset
                0, // initial state non-signaled
                ptr::null(),
            )
        };
        if rx_event_handle.is_null() {
            return Err(ConnectError::CreateSocket(
                std::io::Error::last_os_error().raw_os_error().unwrap_or(-1),
            ));
        }

        let rx_event_raw = rx_event_handle as RawHandle;
        let ctx_rx_event = SendableHandle::from_raw(rx_event_raw);
        let backend_rx_event = SendableHandle::from_raw(rx_event_raw);

        thread::Builder::new()
            .name("slirp-event-loop".into())
            .spawn(move || {
                slirp_event_loop(tx_receiver, rx_sender, ctx_rx_event);
            })
            .expect("Failed to spawn slirp event loop thread");

        Ok(SlirpBackend {
            tx_sender,
            rx_receiver,
            pending_rx: None,
            rx_event: backend_rx_event,
        })
    }
}

impl NetBackend for SlirpBackend {
    fn read_frame(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        // Try to get a frame from the channel
        let frame = match self.pending_rx.take() {
            Some(f) => f,
            None => match self.rx_receiver.try_recv() {
                Ok(f) => f,
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    return Err(ReadError::NothingRead);
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    return Err(ReadError::NothingRead);
                }
            },
        };

        // Prepend virtio_net_hdr (all zeros)
        let hdr_len = write_virtio_net_hdr(buf);
        let frame_len = frame.len();

        if hdr_len + frame_len > buf.len() {
            log::warn!(
                "SlirpBackend: frame too large ({} bytes), dropping",
                frame_len
            );
            return Err(ReadError::NothingRead);
        }

        buf[hdr_len..hdr_len + frame_len].copy_from_slice(&frame);
        Ok(hdr_len + frame_len)
    }

    fn write_frame(&mut self, hdr_len: usize, buf: &mut [u8]) -> Result<(), WriteError> {
        if buf.len() <= hdr_len {
            return Ok(()); // No payload after header
        }

        // Strip virtio_net_hdr, send raw ethernet frame
        let frame = buf[hdr_len..].to_vec();

        match self.tx_sender.try_send(frame) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                log::warn!("SlirpBackend: TX channel full, dropping frame");
                Err(WriteError::NothingWritten)
            }
            Err(TrySendError::Disconnected(_)) => {
                log::error!("SlirpBackend: TX channel disconnected");
                Err(WriteError::ProcessNotRunning)
            }
        }
    }

    fn has_unfinished_write(&self) -> bool {
        false
    }

    fn try_finish_write(&mut self, _hdr_len: usize, _buf: &[u8]) -> Result<(), WriteError> {
        Ok(())
    }

    fn raw_socket_fd(&self) -> RawHandle {
        self.rx_event.as_raw_handle()
    }
}

// ---------------------------------------------------------------------------
// libslirp callbacks (C FFI)
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_send_packet(
    buf: *const c_void,
    len: usize,
    opaque: *mut c_void,
) -> isize {
    let ctx = &*(opaque as *const SlirpContext);
    let slice = std::slice::from_raw_parts(buf as *const u8, len);
    let frame = slice.to_vec();

    match ctx.rx_sender.try_send(frame) {
        Ok(()) => {
            // Signal the RX event so the worker thread wakes up
            windows_sys::Win32::System::Threading::SetEvent(ctx.rx_event.as_handle());
            len as isize
        }
        Err(_) => {
            log::warn!("slirp cb_send_packet: RX channel full, dropping frame");
            -1
        }
    }
}

unsafe extern "C" fn cb_guest_error(msg: *const i8, _opaque: *mut c_void) {
    if !msg.is_null() {
        let c_str = std::ffi::CStr::from_ptr(msg);
        log::error!("slirp guest error: {:?}", c_str);
    }
}

unsafe extern "C" fn cb_clock_get_ns(_opaque: *mut c_void) -> i64 {
    // Return monotonic time in nanoseconds
    use std::time::Instant;
    // We use a thread-local base instant for monotonic time
    thread_local! {
        static BASE: Instant = Instant::now();
    }
    BASE.with(|base| base.elapsed().as_nanos() as i64)
}

unsafe extern "C" fn cb_timer_new(
    cb: SlirpTimerCb,
    cb_opaque: *mut c_void,
    opaque: *mut c_void,
) -> *mut c_void {
    let ctx = &mut *(opaque as *mut SlirpContext);
    let id = ctx.next_timer_id;
    ctx.next_timer_id += 1;
    ctx.timers.push(SlirpTimer {
        id,
        expire_time_ms: i64::MAX, // not armed
        cb,
        cb_opaque,
    });
    // Return the timer ID as a pointer (it's just an opaque handle)
    id as *mut c_void
}

unsafe extern "C" fn cb_timer_free(timer: *mut c_void, opaque: *mut c_void) {
    let ctx = &mut *(opaque as *mut SlirpContext);
    let id = timer as usize;
    ctx.timers.retain(|t| t.id != id);
}

unsafe extern "C" fn cb_timer_mod(
    timer: *mut c_void,
    expire_time_ms: i64,
    opaque: *mut c_void,
) {
    let ctx = &mut *(opaque as *mut SlirpContext);
    let id = timer as usize;
    if let Some(t) = ctx.timers.iter_mut().find(|t| t.id == id) {
        t.expire_time_ms = expire_time_ms;
    }
}

unsafe extern "C" fn cb_register_poll_fd(_fd: c_int, _opaque: *mut c_void) {
    // No-op: we rebuild the pollfd array each iteration
}

unsafe extern "C" fn cb_unregister_poll_fd(_fd: c_int, _opaque: *mut c_void) {
    // No-op
}

unsafe extern "C" fn cb_notify(_opaque: *mut c_void) {
    // No-op: our loop polls frequently enough
}

// ---------------------------------------------------------------------------
// Slirp event loop (runs in dedicated thread)
// ---------------------------------------------------------------------------

fn slirp_event_loop(
    tx_receiver: Receiver<Vec<u8>>,
    rx_sender: Sender<Vec<u8>>,
    rx_event: SendableHandle,
) {
    use windows_sys::Win32::Networking::WinSock;

    let mut ctx = SlirpContext {
        rx_sender,
        tx_receiver,
        rx_event,
        timers: Vec::new(),
        next_timer_id: 1,
    };

    let ctx_ptr = &mut ctx as *mut SlirpContext as *mut c_void;

    // Configure slirp network: 10.0.2.0/24
    let cfg = SlirpConfig {
        version: 4, // SLIRP_CONFIG_VERSION_MAX in libslirp 4.x
        restricted: 0,
        in_enabled: true,
        vnetwork: in_addr {
            s_addr: u32::from_ne_bytes([10, 0, 2, 0]),
        },
        vnetmask: in_addr {
            s_addr: u32::from_ne_bytes([255, 255, 255, 0]),
        },
        vhost: in_addr {
            s_addr: u32::from_ne_bytes([10, 0, 2, 2]),
        },
        in6_enabled: false,
        vprefix_addr6: in6_addr {
            s6_addr: [0; 16],
        },
        vprefix_len: 0,
        vhost6: in6_addr {
            s6_addr: [0; 16],
        },
        vhostname: ptr::null(),
        tftp_server_name: ptr::null(),
        tftp_path: ptr::null(),
        bootfile: ptr::null(),
        vdhcp_start: in_addr {
            s_addr: u32::from_ne_bytes([10, 0, 2, 15]),
        },
        vnameserver: in_addr {
            s_addr: u32::from_ne_bytes([10, 0, 2, 3]),
        },
        vnameserver6: in6_addr {
            s6_addr: [0; 16],
        },
        vdnssearch: ptr::null_mut(),
        vdomainname: ptr::null(),
        if_mtu: 0,
        if_mru: 0,
        disable_host_loopback: false,
        enable_emu: false,
        outbound_addr: ptr::null(),
        outbound_addr6: ptr::null(),
        disable_dns: false,
        disable_dhcp: false,
    };

    let callbacks = SlirpCb {
        send_packet: Some(cb_send_packet),
        guest_error: Some(cb_guest_error),
        clock_get_ns: Some(cb_clock_get_ns),
        timer_new: Some(cb_timer_new),
        timer_free: Some(cb_timer_free),
        timer_mod: Some(cb_timer_mod),
        register_poll_fd: Some(cb_register_poll_fd),
        unregister_poll_fd: Some(cb_unregister_poll_fd),
        notify: Some(cb_notify),
    };

    let slirp = unsafe { slirp_new(&cfg, &callbacks, ctx_ptr) };
    if slirp.is_null() {
        log::error!("slirp_new() returned null, slirp thread exiting");
        return;
    }

    log::info!("Slirp network backend initialized (10.0.2.0/24)");

    // Main event loop
    let mut pollfds: Vec<WinSock::WSAPOLLFD> = Vec::with_capacity(MAX_POLLFDS);

    loop {
        // 1. Drain guest TX frames and feed them to slirp
        while let Ok(frame) = ctx.tx_receiver.try_recv() {
            unsafe {
                slirp_input(slirp, frame.as_ptr(), frame.len() as c_int);
            }
        }

        // 2. Check and fire expired timers
        let now_ms = unsafe { cb_clock_get_ns(ctx_ptr) } / 1_000_000;
        let mut fired = Vec::new();
        for timer in &ctx.timers {
            if timer.expire_time_ms != i64::MAX && timer.expire_time_ms <= now_ms {
                fired.push((timer.cb, timer.cb_opaque));
            }
        }
        for (cb, cb_opaque) in &fired {
            if let Some(cb_fn) = cb {
                unsafe {
                    cb_fn(*cb_opaque);
                }
            }
        }
        // Reset fired timers
        for timer in &mut ctx.timers {
            if timer.expire_time_ms != i64::MAX && timer.expire_time_ms <= now_ms {
                timer.expire_time_ms = i64::MAX;
            }
        }

        // 3. Fill poll FDs from slirp
        pollfds.clear();
        let mut timeout: u32 = 50; // default 50ms timeout

        // We need an "add_poll" callback for slirp_pollfds_fill
        unsafe extern "C" fn add_poll_cb(
            fd: c_int,
            events: c_int,
            opaque: *mut c_void,
        ) -> c_int {
            let pollfds = &mut *(opaque as *mut Vec<WinSock::WSAPOLLFD>);
            let mut wsa_events: i16 = 0;
            if events & SLIRP_POLL_IN != 0 {
                wsa_events |= WinSock::POLLRDNORM;
            }
            if events & SLIRP_POLL_OUT != 0 {
                wsa_events |= WinSock::POLLWRNORM;
            }
            // SLIRP_POLL_PRI and SLIRP_POLL_HUP/ERR are handled automatically
            if events & SLIRP_POLL_PRI != 0 {
                wsa_events |= WinSock::POLLRDBAND;
            }
            let idx = pollfds.len();
            pollfds.push(WinSock::WSAPOLLFD {
                fd: fd as usize,
                events: wsa_events,
                revents: 0,
            });
            idx as c_int
        }

        unsafe {
            slirp_pollfds_fill(
                slirp,
                &mut timeout,
                Some(add_poll_cb),
                &mut pollfds as *mut Vec<WinSock::WSAPOLLFD> as *mut c_void,
            );
        }

        // 4. Poll sockets
        if !pollfds.is_empty() {
            unsafe {
                WinSock::WSAPoll(
                    pollfds.as_mut_ptr(),
                    pollfds.len() as u32,
                    timeout as c_int,
                );
            }
        } else {
            // No sockets to poll, just sleep briefly
            std::thread::sleep(std::time::Duration::from_millis(timeout as u64));
        }

        // 5. Notify slirp of poll results
        unsafe extern "C" fn get_revents_cb(
            idx: c_int,
            opaque: *mut c_void,
        ) -> c_int {
            let pollfds = &*(opaque as *const Vec<WinSock::WSAPOLLFD>);
            if (idx as usize) >= pollfds.len() {
                return 0;
            }
            let pfd = &pollfds[idx as usize];
            let mut revents: c_int = 0;
            if pfd.revents & WinSock::POLLRDNORM != 0 {
                revents |= SLIRP_POLL_IN;
            }
            if pfd.revents & WinSock::POLLWRNORM != 0 {
                revents |= SLIRP_POLL_OUT;
            }
            if pfd.revents & WinSock::POLLRDBAND != 0 {
                revents |= SLIRP_POLL_PRI;
            }
            if pfd.revents & WinSock::POLLERR != 0 {
                revents |= SLIRP_POLL_ERR;
            }
            if pfd.revents & WinSock::POLLHUP != 0 {
                revents |= SLIRP_POLL_HUP;
            }
            revents
        }

        unsafe {
            slirp_pollfds_poll(
                slirp,
                0, // error (select_error) - not used with poll
                Some(get_revents_cb),
                &mut pollfds as *mut Vec<WinSock::WSAPOLLFD> as *mut c_void,
            );
        }
    }
}
