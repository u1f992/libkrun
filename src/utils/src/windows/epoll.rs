// Copyright 2026 libkrun contributors.
// SPDX-License-Identifier: Apache-2.0

//! Epoll-compatible event notification using WaitForMultipleObjects.
//!
//! This provides the same `Epoll`, `EpollEvent`, `EventSet`, and
//! `ControlOperation` API as the Linux epoll and macOS kqueue backends,
//! implemented on top of Windows wait primitives.

use std::collections::HashMap;
use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::sync::Mutex;

use bitflags::bitflags;
use log::debug;

use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::WaitForMultipleObjects;

const FALSE: i32 = 0;

/// Pollable handle identifier. Handles are cast to usize for use as HashMap keys.
pub type Pollable = usize;

#[repr(i32)]
pub enum ControlOperation {
    Add,
    Modify,
    Delete,
}

bitflags! {
    pub struct EventSet: u32 {
        const IN = 0b00000001;
        const OUT = 0b00000010;
        const HANG_UP = 0b00000100;
        const READ_HANG_UP = 0b00001000;
        const EDGE_TRIGGERED = 0b00010000;
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct EpollEvent {
    pub events: u32,
    u64: u64,
}

impl EpollEvent {
    pub fn new(events: EventSet, data: u64) -> Self {
        debug!("EpollEvent new: {data}");
        EpollEvent {
            events: events.bits(),
            u64: data,
        }
    }

    pub fn events(&self) -> u32 {
        self.events
    }

    pub fn event_set(&self) -> EventSet {
        EventSet::from_bits(self.events()).unwrap()
    }

    pub fn data(&self) -> u64 {
        debug!("EpollEvent data: {}", self.u64);
        self.u64
    }

    pub fn fd(&self) -> Pollable {
        self.u64 as Pollable
    }
}

/// Registration entry: the handle to wait on and the associated event data.
#[derive(Clone)]
struct Registration {
    handle: RawHandle,
    event: EpollEvent,
}

/// Epoll-like event notifier backed by `WaitForMultipleObjects`.
///
/// Handles are keyed by the `RawHandle` value passed to `ctl()`.
/// The Windows `MAXIMUM_WAIT_OBJECTS` limit (64) applies; libkrun
/// typically uses fewer than 20 handles so this is not a concern.
#[derive(Debug)]
pub struct Epoll {
    registrations: Mutex<HashMap<usize, Registration>>,
}

impl Epoll {
    pub fn new() -> io::Result<Self> {
        Ok(Epoll {
            registrations: Mutex::new(HashMap::new()),
        })
    }

    pub fn ctl(
        &self,
        operation: ControlOperation,
        pollable: Pollable,
        event: &EpollEvent,
    ) -> io::Result<()> {
        let mut regs = self.registrations.lock().unwrap();

        match operation {
            ControlOperation::Add => {
                debug!("epoll add handle: {pollable}");
                regs.insert(
                    pollable,
                    Registration {
                        handle: pollable as RawHandle,
                        event: *event,
                    },
                );
            }
            ControlOperation::Modify => {
                debug!("epoll modify handle: {pollable}");
                if let Some(reg) = regs.get_mut(&pollable) {
                    reg.event = *event;
                }
            }
            ControlOperation::Delete => {
                debug!("epoll delete handle: {pollable}");
                regs.remove(&pollable);
            }
        }
        Ok(())
    }

    pub fn wait(
        &self,
        max_events: usize,
        timeout: i32,
        events: &mut [EpollEvent],
    ) -> io::Result<usize> {
        let regs = self.registrations.lock().unwrap();

        if regs.is_empty() {
            // Nothing to wait on; sleep for the timeout duration.
            if timeout > 0 {
                std::thread::sleep(std::time::Duration::from_millis(timeout as u64));
            }
            return Ok(0);
        }

        let entries: Vec<Registration> = regs.values().cloned().collect();
        drop(regs); // Release lock during wait.

        let handles: Vec<HANDLE> = entries.iter().map(|r| r.handle as HANDLE).collect();
        let timeout_ms = if timeout < 0 {
            0xFFFFFFFF // INFINITE
        } else {
            timeout as u32
        };

        let ret = unsafe {
            WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), FALSE, timeout_ms)
        };

        if ret == WAIT_TIMEOUT {
            return Ok(0);
        }

        if ret >= WAIT_OBJECT_0 && ret < WAIT_OBJECT_0 + handles.len() as u32 {
            // At least one handle is signaled. Check all of them with zero timeout.
            let mut count = 0usize;
            for entry in &entries {
                if count >= max_events || count >= events.len() {
                    break;
                }
                let single_ret = unsafe {
                    WaitForMultipleObjects(1, &(entry.handle as HANDLE), FALSE, 0)
                };
                if single_ret == WAIT_OBJECT_0 {
                    events[count] = EpollEvent {
                        events: entry.event.events,
                        u64: entry.event.u64,
                    };
                    count += 1;
                }
            }
            // The first signaled handle was already consumed by the initial
            // WaitForMultipleObjects call (auto-reset events reset on wake).
            // Re-add it if not already found.
            let first_idx = (ret - WAIT_OBJECT_0) as usize;
            let first_already_included = events[..count]
                .iter()
                .any(|e| e.u64 == entries[first_idx].event.u64);
            if !first_already_included && count < max_events && count < events.len() {
                events[count] = EpollEvent {
                    events: entries[first_idx].event.events,
                    u64: entries[first_idx].event.u64,
                };
                count += 1;
            }

            debug!("epoll wait: {count} events ready");
            Ok(count)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl AsRawHandle for Epoll {
    /// Returns a dummy handle. Windows Epoll does not have a single underlying handle.
    fn as_raw_handle(&self) -> RawHandle {
        std::ptr::null_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eventfd::{EventFd, EFD_NONBLOCK};

    #[test]
    fn test_event_ops() {
        let mut event = EpollEvent::default();
        assert_eq!(event.events(), 0);
        assert_eq!(event.data(), 0);

        event = EpollEvent::new(EventSet::IN, 2);
        assert_eq!(event.events(), 1);
        assert_eq!(event.event_set(), EventSet::IN);
        assert_eq!(event.data(), 2);
    }

    #[test]
    fn test_epoll_timeout() {
        let epoll = Epoll::new().unwrap();
        let mut ready_events = vec![EpollEvent::default(); 10];
        let ev_count = epoll.wait(10, 10, &mut ready_events[..]).unwrap();
        assert_eq!(ev_count, 0);
    }

    #[test]
    fn test_epoll_add_wait() {
        let epoll = Epoll::new().unwrap();

        let evt = EventFd::new(EFD_NONBLOCK).unwrap();
        evt.write(1).unwrap();

        let p = evt.as_raw_handle() as Pollable;
        let event = EpollEvent::new(EventSet::IN, p as u64);
        epoll.ctl(ControlOperation::Add, p, &event).unwrap();

        let mut ready_events = vec![EpollEvent::default(); 10];
        let ev_count = epoll.wait(10, 1000, &mut ready_events[..]).unwrap();
        assert!(ev_count >= 1);
        assert_eq!(ready_events[0].data(), p as u64);
    }

    #[test]
    fn test_epoll_delete() {
        let epoll = Epoll::new().unwrap();

        let evt = EventFd::new(EFD_NONBLOCK).unwrap();
        evt.write(1).unwrap();

        let p = evt.as_raw_handle() as Pollable;
        let event = EpollEvent::new(EventSet::IN, p as u64);
        epoll.ctl(ControlOperation::Add, p, &event).unwrap();
        epoll
            .ctl(ControlOperation::Delete, p, &EpollEvent::default())
            .unwrap();

        let mut ready_events = vec![EpollEvent::default(); 10];
        let ev_count = epoll.wait(10, 10, &mut ready_events[..]).unwrap();
        assert_eq!(ev_count, 0);
    }
}
