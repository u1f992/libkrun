// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use devices::virtio::{Vsock, VsockError};
use crate::resources::TsiFlags as ResTsiFlags;

#[cfg(unix)]
type MutexVsock = Arc<Mutex<Vsock>>;

/// Errors associated with `NetworkInterfaceConfig`.
#[derive(Debug)]
pub enum VsockConfigError {
    /// Failed to create the vsock device.
    #[cfg(unix)]
    CreateVsockDevice(VsockError),
    /// Vsock is not supported on this platform.
    #[cfg(windows)]
    NotSupported,
}

impl fmt::Display for VsockConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::VsockConfigError::*;
        match *self {
            #[cfg(unix)]
            CreateVsockDevice(ref e) => write!(f, "Cannot create vsock device: {e:?}"),
            #[cfg(windows)]
            NotSupported => write!(f, "Vsock is not supported on Windows"),
        }
    }
}

type Result<T> = std::result::Result<T, VsockConfigError>;

/// This struct represents the strongly typed equivalent of the json body
/// from vsock related requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VsockDeviceConfig {
    /// ID of the vsock device.
    pub vsock_id: String,
    /// A 32-bit Context Identifier (CID) used to identify the guest.
    pub guest_cid: u32,
    /// An optional map of host to guest port mappings.
    pub host_port_map: Option<HashMap<u16, u16>>,
    /// An optional map of guest port to host UNIX domain sockets for IPC.
    pub unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
    /// TSI feature flags
    pub tsi_flags: ResTsiFlags,
}

#[cfg(unix)]
struct VsockWrapper {
    vsock: MutexVsock,
}

/// A builder of Vsock from 'VsockDeviceConfig'.
#[derive(Default)]
pub struct VsockBuilder {
    #[cfg(unix)]
    inner: Option<VsockWrapper>,
    tsi_flags: ResTsiFlags,
}

impl VsockBuilder {
    /// Creates an empty Vsock.
    pub fn new() -> Self {
        Self {
            #[cfg(unix)]
            inner: None,
            tsi_flags: ResTsiFlags::default(),
        }
    }

    /// Inserts a Vsock in the store.
    /// If an entry already exists, it will overwrite it.
    #[cfg(unix)]
    pub fn insert(&mut self, cfg: VsockDeviceConfig) -> Result<()> {
        self.tsi_flags = cfg.tsi_flags;
        self.inner = Some(VsockWrapper {
            vsock: Arc::new(Mutex::new(Self::create_vsock(cfg)?)),
        });
        Ok(())
    }

    #[cfg(windows)]
    pub fn insert(&mut self, _cfg: VsockDeviceConfig) -> Result<()> {
        Err(VsockConfigError::NotSupported)
    }

    /// Provides a reference to the Vsock if present.
    #[cfg(unix)]
    pub fn get(&self) -> Option<&MutexVsock> {
        self.inner.as_ref().map(|pair| &pair.vsock)
    }

    #[cfg(windows)]
    pub fn get(&self) -> Option<&Arc<Mutex<()>>> {
        None
    }

    pub fn tsi_flags(&self) -> ResTsiFlags {
        self.tsi_flags
    }

    /// Creates a Vsock device from a VsockDeviceConfig.
    #[cfg(unix)]
    pub fn create_vsock(cfg: VsockDeviceConfig) -> Result<Vsock> {
        Vsock::new(
            u64::from(cfg.guest_cid),
            cfg.host_port_map,
            cfg.unix_ipc_port_map,
            cfg.tsi_flags,
        )
        .map_err(VsockConfigError::CreateVsockDevice)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use utils::tempfile::TempFile;

    // Placeholder for the path where a socket file will be created.
    // The socket file will be removed when the scope ends.
    pub(crate) struct TempSockFile {
        path: String,
    }

    impl TempSockFile {
        pub fn new(tmp_file: TempFile) -> Self {
            TempSockFile {
                path: String::from(tmp_file.as_path().to_str().unwrap()),
            }
        }
    }

    impl Drop for TempSockFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub(crate) fn default_config(_tmp_sock_file: &TempSockFile) -> VsockDeviceConfig {
        let vsock_dev_id = "vsock";
        VsockDeviceConfig {
            vsock_id: vsock_dev_id.to_string(),
            guest_cid: 3,
            host_port_map: None,
            unix_ipc_port_map: None,
            tsi_flags: ResTsiFlags::default(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_vsock_insert() {
        let mut store = VsockBuilder::new();
        let tmp_sock_file = TempSockFile::new(TempFile::new().unwrap());
        let mut vsock_config = default_config(&tmp_sock_file);

        store.insert(vsock_config.clone()).unwrap();
        let vsock = store.get().unwrap();
        assert_eq!(vsock.lock().unwrap().id(), &vsock_config.vsock_id);

        let new_cid = vsock_config.guest_cid + 1;
        vsock_config.guest_cid = new_cid;
        store.insert(vsock_config).unwrap();
        let vsock = store.get().unwrap();
        assert_eq!(vsock.lock().unwrap().cid(), new_cid as u64);
    }
}
