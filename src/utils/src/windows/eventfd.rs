// Copyright 2026 libkrun contributors.
// SPDX-License-Identifier: Apache-2.0

//! EventFd emulation using Windows auto-reset Event objects.

use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle, RawHandle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, DUPLICATE_SAME_ACCESS, FALSE, HANDLE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    CreateEventA, GetCurrentProcess, ResetEvent, SetEvent, WaitForSingleObject,
};

pub const EFD_NONBLOCK: i32 = 1;
pub const EFD_SEMAPHORE: i32 = 2;

#[derive(Debug)]
pub struct EventFd {
    handle: OwnedHandle,
    /// Shared counter so cloned EventFds see the same value.
    counter: Arc<AtomicU64>,
    nonblock: bool,
}

impl EventFd {
    pub fn new(flag: i32) -> Result<EventFd, io::Error> {
        // Create an auto-reset event (bManualReset = FALSE).
        let h = unsafe { CreateEventA(std::ptr::null(), FALSE, FALSE, std::ptr::null()) };
        if h == 0 {
            return Err(io::Error::from_raw_os_error(unsafe { GetLastError() } as i32));
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(h as RawHandle) };
        Ok(EventFd {
            handle,
            counter: Arc::new(AtomicU64::new(0)),
            nonblock: (flag & EFD_NONBLOCK) != 0,
        })
    }

    pub fn write(&self, v: u64) -> Result<(), io::Error> {
        self.counter.fetch_add(v, Ordering::SeqCst);
        let ret = unsafe { SetEvent(self.handle.as_raw_handle() as HANDLE) };
        if ret == 0 {
            return Err(io::Error::from_raw_os_error(unsafe { GetLastError() } as i32));
        }
        Ok(())
    }

    pub fn read(&self) -> Result<u64, io::Error> {
        let timeout_ms = if self.nonblock { 0 } else { 0xFFFFFFFF }; // INFINITE
        let ret =
            unsafe { WaitForSingleObject(self.handle.as_raw_handle() as HANDLE, timeout_ms) };
        if ret != WAIT_OBJECT_0 {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "event not signaled"));
        }
        let val = self.counter.swap(0, Ordering::SeqCst);
        Ok(val)
    }

    pub fn try_clone(&self) -> Result<EventFd, io::Error> {
        let mut new_handle: HANDLE = 0;
        let ret = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.handle.as_raw_handle() as HANDLE,
                GetCurrentProcess(),
                &mut new_handle,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ret == 0 {
            return Err(io::Error::from_raw_os_error(unsafe { GetLastError() } as i32));
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(new_handle as RawHandle) };
        Ok(EventFd {
            handle,
            counter: Arc::clone(&self.counter),
            nonblock: self.nonblock,
        })
    }

    /// Returns the raw handle, usable with WaitForMultipleObjects.
    pub fn get_write_fd(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

impl AsRawHandle for EventFd {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        EventFd::new(EFD_NONBLOCK).unwrap();
        EventFd::new(0).unwrap();
    }

    #[test]
    fn test_read_write() {
        let evt = EventFd::new(EFD_NONBLOCK).unwrap();
        evt.write(55).unwrap();
        assert_eq!(evt.read().unwrap(), 55);
    }

    #[test]
    fn test_read_nothing() {
        let evt = EventFd::new(EFD_NONBLOCK).unwrap();
        let r = evt.read();
        match r {
            Err(ref inner) if inner.kind() == io::ErrorKind::WouldBlock => (),
            _ => panic!("Unexpected"),
        }
    }

    #[test]
    fn test_clone() {
        let evt = EventFd::new(EFD_NONBLOCK).unwrap();
        let evt_clone = evt.try_clone().unwrap();
        evt.write(923).unwrap();
        assert_eq!(evt_clone.read().unwrap(), 923);
    }
}
