// Copyright 2026 libkrun contributors.
// SPDX-License-Identifier: Apache-2.0
//
// WHPX (Windows Hypervisor Platform) backend for libkrun.
// Provides the same public interface as the Linux KVM and macOS HVF backends
// but uses the Windows Hypervisor Platform APIs via the `windows-sys` crate.

use std::cell::Cell;
use std::fmt::{Display, Formatter};
use std::io;
use std::mem::{size_of, zeroed};
use std::result;
use std::sync::Arc;
use std::thread;

use super::super::{FC_EXIT_CODE_GENERIC_ERROR, FC_EXIT_CODE_OK};
use super::hyperv_tlfs::*;
use crate::vmm_config::machine_config::CpuFeaturesTemplate;

use crossbeam_channel::{unbounded, Receiver, Sender};
use utils::eventfd::EventFd;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use windows_sys::Win32::Foundation::S_OK;
use windows_sys::Win32::System::Hypervisor::*;

// ---------------------------------------------------------------------------
// HRESULT check macro -- converts non-S_OK HRESULT to Result<(), i32>.
// ---------------------------------------------------------------------------

macro_rules! check_whpx {
    ($expr:expr) => {{
        let hr: i32 = $expr;
        if hr == S_OK {
            Ok(())
        } else {
            Err(hr)
        }
    }};
}

// ---------------------------------------------------------------------------
// Error / Result
// ---------------------------------------------------------------------------

/// Errors associated with the WHPX hypervisor backend.
#[derive(Debug)]
pub enum Error {
    /// WHPX API returned an HRESULT failure when creating a partition.
    CreatePartition(i32),
    /// Failed to set a partition property.
    SetPartitionProperty(i32),
    /// Failed to finalise the partition setup.
    SetupPartition(i32),
    /// Failed to map a guest physical address range.
    MapGpaRange(i32),
    /// Failed to unmap a guest physical address range.
    UnmapGpaRange(i32),
    /// Failed to create a virtual processor.
    CreateVirtualProcessor(i32),
    /// Failed to delete a virtual processor.
    DeleteVirtualProcessor(i32),
    /// Failed to run a virtual processor.
    RunVirtualProcessor(i32),
    /// Failed to get virtual processor registers.
    GetRegisters(i32),
    /// Failed to set virtual processor registers.
    SetRegisters(i32),
    /// Failed to translate a guest virtual address.
    TranslateGva(i32),
    /// Failed to create the instruction emulator.
    CreateEmulator(i32),
    /// Emulator IO emulation failed.
    IoEmulation(i32),
    /// Emulator MMIO emulation failed.
    MmioEmulation(i32),
    /// Invalid guest memory configuration.
    GuestMemoryMmap(vm_memory::GuestMemoryError),
    /// The number of configured slots exceeds the maximum.
    NotEnoughMemorySlots,
    /// Cannot run the vCPUs.
    VcpuRun,
    /// Cannot spawn a new vCPU thread.
    VcpuSpawn(io::Error),
    /// Cannot cleanly initialise vCPU TLS.
    VcpuTlsInit,
    /// vCPU not present in TLS.
    VcpuTlsNotPresent,
    /// Unexpected WHPX exit reason.
    VcpuUnhandledExit(u32),
    /// vCPU count is not initialised.
    VcpuCountNotInitialized,
    /// Cannot configure the microvm.
    VmSetup(i32),
    /// Failed to signal vCPU.
    SignalVcpu(io::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use Error::*;
        match self {
            CreatePartition(hr) => write!(f, "Failed to create WHPX partition: HRESULT 0x{hr:08X}"),
            SetPartitionProperty(hr) => {
                write!(f, "Failed to set partition property: HRESULT 0x{hr:08X}")
            }
            SetupPartition(hr) => write!(f, "Failed to setup partition: HRESULT 0x{hr:08X}"),
            MapGpaRange(hr) => write!(f, "Failed to map GPA range: HRESULT 0x{hr:08X}"),
            UnmapGpaRange(hr) => write!(f, "Failed to unmap GPA range: HRESULT 0x{hr:08X}"),
            CreateVirtualProcessor(hr) => {
                write!(f, "Failed to create virtual processor: HRESULT 0x{hr:08X}")
            }
            DeleteVirtualProcessor(hr) => {
                write!(
                    f,
                    "Failed to delete virtual processor: HRESULT 0x{hr:08X}"
                )
            }
            RunVirtualProcessor(hr) => {
                write!(f, "Failed to run virtual processor: HRESULT 0x{hr:08X}")
            }
            GetRegisters(hr) => write!(f, "Failed to get registers: HRESULT 0x{hr:08X}"),
            SetRegisters(hr) => write!(f, "Failed to set registers: HRESULT 0x{hr:08X}"),
            TranslateGva(hr) => write!(f, "Failed to translate GVA: HRESULT 0x{hr:08X}"),
            CreateEmulator(hr) => write!(f, "Failed to create emulator: HRESULT 0x{hr:08X}"),
            IoEmulation(hr) => write!(f, "IO emulation failed: HRESULT 0x{hr:08X}"),
            MmioEmulation(hr) => write!(f, "MMIO emulation failed: HRESULT 0x{hr:08X}"),
            GuestMemoryMmap(e) => write!(f, "Guest memory error: {e:?}"),
            NotEnoughMemorySlots => write!(f, "Not enough memory slots"),
            VcpuRun => write!(f, "Cannot run the VCPUs"),
            VcpuSpawn(e) => write!(f, "Cannot spawn a new vCPU thread: {e}"),
            VcpuTlsInit => write!(f, "Cannot clean init vcpu TLS"),
            VcpuTlsNotPresent => write!(f, "Vcpu not present in TLS"),
            VcpuUnhandledExit(reason) => write!(f, "Unexpected WHPX exit reason: {reason}"),
            VcpuCountNotInitialized => write!(f, "vCPU count is not initialized"),
            VmSetup(hr) => write!(f, "Cannot configure the microvm: HRESULT 0x{hr:08X}"),
            SignalVcpu(e) => write!(f, "Failed to signal Vcpu: {e}"),
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

// ---------------------------------------------------------------------------
// SafePartition -- thin RAII wrapper around WHV_PARTITION_HANDLE
// ---------------------------------------------------------------------------

/// RAII wrapper around a WHPX partition handle.
///
/// In `windows-sys`, `WHV_PARTITION_HANDLE` is `isize`.
/// The handle is *not* a Win32 `HANDLE` -- it is opaque to the WHPX API set.
struct SafePartition {
    partition: WHV_PARTITION_HANDLE,
}

// The partition handle can be shared across threads for register get/set.
// Safety: WHPX partition handles are thread-safe for concurrent API calls
// (the platform serialises internally where needed).
unsafe impl Send for SafePartition {}
unsafe impl Sync for SafePartition {}

impl SafePartition {
    fn new() -> Result<Self> {
        let mut handle: WHV_PARTITION_HANDLE = 0;
        // Safety: we pass a valid pointer for the output handle.
        check_whpx!(unsafe { WHvCreatePartition(&mut handle) })
            .map_err(Error::CreatePartition)?;
        Ok(SafePartition { partition: handle })
    }

    /// Returns the raw partition handle for use with WHPX APIs.
    pub(crate) fn handle(&self) -> WHV_PARTITION_HANDLE {
        self.partition
    }
}

impl Drop for SafePartition {
    fn drop(&mut self) {
        // Safety: we own this partition and it has not been deleted yet.
        let _ = unsafe { WHvDeletePartition(self.partition) };
    }
}

// ---------------------------------------------------------------------------
// SafeEmulator -- RAII wrapper for the WHPX instruction emulator
// ---------------------------------------------------------------------------

/// RAII wrapper around the WHPX emulator handle (`*mut c_void`).
///
/// The instruction emulator is used to decode IO-port and MMIO instructions
/// that WHPX does not decode in-kernel (unlike KVM).
struct SafeEmulator {
    handle: *mut std::ffi::c_void,
}

// Safety: The WHPX emulator handle is an opaque pointer that can be safely
// sent between threads. The emulator API is thread-safe when used with
// proper synchronization (which we provide via the per-vCPU ownership model).
unsafe impl Send for SafeEmulator {}

/// Context passed into the emulator callbacks so the callbacks can reach
/// the partition handle, vCPU index, and device buses.
struct EmulatorContext<'a> {
    partition: WHV_PARTITION_HANDLE,
    vcpu_index: u32,
    io_bus: Option<&'a devices::Bus>,
    mmio_bus: Option<&'a devices::Bus>,
}

impl SafeEmulator {
    fn new() -> Result<Self> {
        let callbacks = WHV_EMULATOR_CALLBACKS {
            Size: size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
            Reserved: 0,
            WHvEmulatorIoPortCallback: Some(Self::io_port_cb),
            WHvEmulatorMemoryCallback: Some(Self::memory_cb),
            WHvEmulatorGetVirtualProcessorRegisters: Some(Self::get_registers_cb),
            WHvEmulatorSetVirtualProcessorRegisters: Some(Self::set_registers_cb),
            WHvEmulatorTranslateGvaPage: Some(Self::translate_gva_cb),
        };
        let mut handle: *mut std::ffi::c_void = std::ptr::null_mut();
        // Safety: callbacks are valid function pointers; handle receives the
        // emulator that the kernel allocates.
        check_whpx!(unsafe { WHvEmulatorCreateEmulator(&callbacks, &mut handle) })
            .map_err(Error::CreateEmulator)?;
        Ok(SafeEmulator { handle })
    }

    // -- Emulator callbacks (stdcall ABI, invoked by WHPX during emulation) --

    unsafe extern "system" fn io_port_cb(
        context: *const std::ffi::c_void,
        io_access: *mut WHV_EMULATOR_IO_ACCESS_INFO,
    ) -> i32 {
        let ctx = &*(context as *const EmulatorContext);
        let info = &mut *io_access;
        let port = info.Port as u64;
        let size = info.AccessSize as usize;
        // Safety: we trust WHPX to provide a valid access-size <= 4.
        let data_ptr = &mut info.Data as *mut u32 as *mut u8;
        let data = std::slice::from_raw_parts_mut(data_ptr, size);

        if let Some(bus) = ctx.io_bus {
            if info.Direction == 0 {
                // PIO IN (read from device)
                bus.read(ctx.vcpu_index as u64, port, data);
            } else {
                // PIO OUT (write to device)
                bus.write(ctx.vcpu_index as u64, port, data);
            }
        }
        S_OK
    }

    unsafe extern "system" fn memory_cb(
        context: *const std::ffi::c_void,
        memory_access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
    ) -> i32 {
        let ctx = &*(context as *const EmulatorContext);
        let info = &mut *memory_access;
        let gpa = info.GpaAddress;
        let size = info.AccessSize as usize;
        let data = &mut info.Data[..size];

        if let Some(bus) = ctx.mmio_bus {
            if info.Direction == 0 {
                // MMIO read
                bus.read(ctx.vcpu_index as u64, gpa, data);
            } else {
                // MMIO write
                bus.write(ctx.vcpu_index as u64, gpa, data);
            }
        }
        S_OK
    }

    unsafe extern "system" fn get_registers_cb(
        context: *const std::ffi::c_void,
        register_names: *const WHV_REGISTER_NAME,
        register_count: u32,
        register_values: *mut WHV_REGISTER_VALUE,
    ) -> i32 {
        let ctx = &*(context as *const EmulatorContext);
        WHvGetVirtualProcessorRegisters(
            ctx.partition,
            ctx.vcpu_index,
            register_names,
            register_count,
            register_values,
        )
    }

    unsafe extern "system" fn set_registers_cb(
        context: *const std::ffi::c_void,
        register_names: *const WHV_REGISTER_NAME,
        register_count: u32,
        register_values: *const WHV_REGISTER_VALUE,
    ) -> i32 {
        let ctx = &*(context as *const EmulatorContext);
        WHvSetVirtualProcessorRegisters(
            ctx.partition,
            ctx.vcpu_index,
            register_names,
            register_count,
            register_values,
        )
    }

    unsafe extern "system" fn translate_gva_cb(
        context: *const std::ffi::c_void,
        gva: u64,
        translate_flags: WHV_TRANSLATE_GVA_FLAGS,
        translation_result: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
        gpa: *mut u64,
    ) -> i32 {
        let ctx = &*(context as *const EmulatorContext);
        let mut result: WHV_TRANSLATE_GVA_RESULT = zeroed();
        let hr = WHvTranslateGva(
            ctx.partition,
            ctx.vcpu_index,
            gva,
            translate_flags,
            &mut result,
            gpa,
        );
        if hr == S_OK {
            *translation_result = result.ResultCode;
        }
        hr
    }
}

impl Drop for SafeEmulator {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // Safety: we own this emulator handle.
            unsafe {
                WHvEmulatorDestroyEmulator(self.handle);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Vm
// ---------------------------------------------------------------------------

/// A WHPX partition representing the virtual machine.
pub struct Vm {
    partition: Arc<SafePartition>,
}

impl Vm {
    /// Creates a new WHPX partition with the given number of virtual processors.
    ///
    /// This performs the full partition-creation sequence:
    /// 1. `WHvCreatePartition`
    /// 2. Set processor count
    /// 3. Configure CPUID result list (Hyper-V enlightenments)
    /// 4. Enable extended VM exits (CPUID + MSR exits)
    /// 5. Set APIC emulation mode (XApic)
    /// 6. `WHvSetupPartition`
    pub fn new(num_cpus: u32) -> Result<Self> {
        let partition = SafePartition::new()?;

        // -- Set processor count --
        let mut property: WHV_PARTITION_PROPERTY = unsafe { zeroed() };
        property.ProcessorCount = num_cpus;
        // Safety: we own the partition; property is stack-allocated and valid.
        check_whpx!(unsafe {
            WHvSetPartitionProperty(
                partition.partition,
                WHvPartitionPropertyCodeProcessorCount,
                &property as *const _ as *const std::ffi::c_void,
                size_of::<WHV_PARTITION_PROPERTY>() as u32,
            )
        })
        .map_err(Error::SetPartitionProperty)?;

        // -- Pre-set Hyper-V CPUID enlightenment leaves --
        //
        // These tell a Linux guest that it is running under Hyper-V and which
        // features the synthetic hypervisor interface provides.
        let cpuid_results: [WHV_X64_CPUID_RESULT; 2] = [
            // Leaf 0x40000000 -- vendor string "Microsoft Hv"
            WHV_X64_CPUID_RESULT {
                Function: HYPERV_CPUID_VENDOR_AND_MAX_FUNCTIONS,
                Reserved: [0u32; 3],
                Eax: HYPERV_CPUID_MIN,
                Ebx: u32::from_le_bytes([b'M', b'i', b'c', b'r']),
                Ecx: u32::from_le_bytes([b'o', b's', b'o', b'f']),
                Edx: u32::from_le_bytes([b't', b' ', b'H', b'v']),
            },
            // Leaf 0x40000003 -- feature flags
            WHV_X64_CPUID_RESULT {
                Function: HYPERV_CPUID_FEATURES,
                Reserved: [0u32; 3],
                Eax: HV_ACCESS_FREQUENCY_MSRS
                    | HV_ACCESS_TSC_INVARIANT
                    | HV_MSR_REFERENCE_TSC_AVAILABLE,
                Ebx: 0,
                Ecx: 0,
                Edx: HV_FEATURE_FREQUENCY_MSRS_AVAILABLE,
            },
        ];
        // Safety: we own this partition; the results array is stack-local.
        check_whpx!(unsafe {
            WHvSetPartitionProperty(
                partition.partition,
                WHvPartitionPropertyCodeCpuidResultList,
                cpuid_results.as_ptr() as *const std::ffi::c_void,
                (size_of::<WHV_X64_CPUID_RESULT>() * cpuid_results.len()) as u32,
            )
        })
        .map_err(Error::SetPartitionProperty)?;

        // -- Set CPUID exit list --
        // Leaves whose results depend on per-vCPU state (topology, etc.)
        // need to be intercepted so we can adjust them.
        let cpuid_exit_list: [u32; 5] = [0x1, 0x4, 0xB, 0x1F, 0x15];
        // Safety: we own this partition; the array is stack-local.
        check_whpx!(unsafe {
            WHvSetPartitionProperty(
                partition.partition,
                WHvPartitionPropertyCodeCpuidExitList,
                cpuid_exit_list.as_ptr() as *const std::ffi::c_void,
                (size_of::<u32>() * cpuid_exit_list.len()) as u32,
            )
        })
        .map_err(Error::SetPartitionProperty)?;

        // -- Enable extended VM exits for CPUID and MSR instructions --
        let mut exit_property: WHV_PARTITION_PROPERTY = unsafe { zeroed() };
        // The ExtendedVmExits field is a union/bitfield. In windows-sys it is
        // represented as a u64 where bit 1 = X64CpuidExit, bit 2 = X64MsrExit.
        // We set both bits.
        exit_property.ExtendedVmExits.AsUINT64 = (1 << 0) | (1 << 1); // CpuidExit | MsrExit
        // Safety: we own this partition; property is stack-allocated.
        check_whpx!(unsafe {
            WHvSetPartitionProperty(
                partition.partition,
                WHvPartitionPropertyCodeExtendedVmExits,
                &exit_property as *const _ as *const std::ffi::c_void,
                size_of::<WHV_PARTITION_PROPERTY>() as u32,
            )
        })
        .map_err(Error::SetPartitionProperty)?;

        // -- Set APIC emulation mode to XApic --
        let mut apic_property: WHV_PARTITION_PROPERTY = unsafe { zeroed() };
        // WHvX64LocalApicEmulationModeXApic = 1
        apic_property.LocalApicEmulationMode = 1;
        // Safety: we own this partition; property is stack-allocated.
        check_whpx!(unsafe {
            WHvSetPartitionProperty(
                partition.partition,
                WHvPartitionPropertyCodeLocalApicEmulationMode,
                &apic_property as *const _ as *const std::ffi::c_void,
                size_of::<WHV_PARTITION_PROPERTY>() as u32,
            )
        })
        .map_err(Error::SetPartitionProperty)?;

        // -- Finalise partition setup --
        // Safety: all required properties have been set.
        check_whpx!(unsafe { WHvSetupPartition(partition.partition) })
            .map_err(Error::SetupPartition)?;

        info!("WHPX partition created with {num_cpus} vCPU(s)");
        Ok(Vm {
            partition: Arc::new(partition),
        })
    }

    /// Returns a reference-counted handle to the underlying partition.
    /// vCPUs need this to issue register get/set and run calls.
    pub(crate) fn partition(&self) -> &Arc<SafePartition> {
        &self.partition
    }

    /// Returns the raw WHPX partition handle for use with interrupt delivery, etc.
    pub fn partition_handle(&self) -> WHV_PARTITION_HANDLE {
        self.partition.handle()
    }

    /// Maps all regions of `guest_mem` into the partition's GPA space.
    pub fn memory_init(&mut self, guest_mem: &GuestMemoryMmap) -> Result<()> {
        for region in guest_mem.iter() {
            let host_addr = guest_mem
                .get_host_address(region.start_addr())
                .expect("guest memory region has no host mapping");
            let guest_addr = region.start_addr().raw_value();
            let len = region.len();

            debug!(
                "WHPX MapGpaRange: host={host_addr:?} guest=0x{guest_addr:x} len=0x{len:x}"
            );

            // Flags: Read | Write | Execute
            let flags: WHV_MAP_GPA_RANGE_FLAGS =
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute;

            // Safety: host_addr is a valid host pointer for `len` bytes;
            // the guest address range does not overlap with other mapped ranges.
            check_whpx!(unsafe {
                WHvMapGpaRange(
                    self.partition.partition,
                    host_addr as *const std::ffi::c_void,
                    guest_addr,
                    len,
                    flags,
                )
            })
            .map_err(Error::MapGpaRange)?;
        }
        Ok(())
    }

    /// Maps an additional host region into the partition's GPA space.
    /// Unmaps any existing mapping at that GPA first.
    pub fn add_mapping(
        &self,
        reply_sender: Sender<bool>,
        host_addr: u64,
        guest_addr: u64,
        len: u64,
    ) {
        debug!("add_mapping: host=0x{host_addr:x} guest=0x{guest_addr:x} len=0x{len:x}");

        // Unmap first (ignore errors -- the range may not be mapped).
        let _ = unsafe { WHvUnmapGpaRange(self.partition.partition, guest_addr, len) };

        let flags: WHV_MAP_GPA_RANGE_FLAGS =
            WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute;
        let hr = unsafe {
            WHvMapGpaRange(
                self.partition.partition,
                host_addr as *const std::ffi::c_void,
                guest_addr,
                len,
                flags,
            )
        };
        if hr != S_OK {
            error!("add_mapping MapGpaRange failed: HRESULT 0x{:08X}", hr as u32);
            let _ = reply_sender.send(false);
        } else {
            let _ = reply_sender.send(true);
        }
    }

    /// Removes a guest physical address mapping.
    pub fn remove_mapping(&self, reply_sender: Sender<bool>, guest_addr: u64, len: u64) {
        debug!("remove_mapping: guest=0x{guest_addr:x} len=0x{len:x}");
        let hr = unsafe { WHvUnmapGpaRange(self.partition.partition, guest_addr, len) };
        if hr != S_OK {
            error!(
                "remove_mapping UnmapGpaRange failed: HRESULT 0x{:08X}",
                hr as u32
            );
            let _ = reply_sender.send(false);
        } else {
            let _ = reply_sender.send(true);
        }
    }
}

// ---------------------------------------------------------------------------
// VcpuConfig
// ---------------------------------------------------------------------------

/// Encapsulates configuration parameters for the guest vCPUs.
#[derive(Debug, Eq, PartialEq)]
pub struct VcpuConfig {
    /// Number of guest VCPUs.
    pub vcpu_count: u8,
    /// Enable hyperthreading in the CPUID configuration.
    pub ht_enabled: bool,
    /// CPUID template to use.
    pub cpu_template: Option<CpuFeaturesTemplate>,
}

// ---------------------------------------------------------------------------
// Vcpu
// ---------------------------------------------------------------------------

// Helper for thread-local storage of the Vcpu pointer.
type VcpuCell = Cell<Option<*const Vcpu>>;

/// A WHPX virtual processor.
pub struct Vcpu {
    /// Logical CPU index (0-based).
    id: u8,
    /// Index used for WHPX API calls (matches `id` but u32).
    whpx_index: u32,
    /// Reference to the partition this vCPU belongs to.
    partition: Arc<SafePartition>,
    /// Instruction emulator for IO/MMIO decoding.
    emulator: SafeEmulator,
    /// Last exit context buffer, reused across run calls.
    exit_context: WHV_RUN_VP_EXIT_CONTEXT,

    /// IO port bus (PIO).
    io_bus: Option<devices::Bus>,
    /// MMIO bus.
    mmio_bus: Option<devices::Bus>,

    /// EventFd written when this vCPU exits.
    exit_evt: EventFd,

    // -- Channel plumbing (matches the macOS/Linux pattern) --
    event_receiver: Receiver<VcpuEvent>,
    event_sender: Option<Sender<VcpuEvent>>,
    response_receiver: Option<Receiver<VcpuResponse>>,
    response_sender: Sender<VcpuResponse>,
}

impl Vcpu {
    thread_local!(static TLS_VCPU_PTR: VcpuCell = const { Cell::new(None) });

    /// Associates `self` with the current thread.
    fn init_thread_local_data(&mut self) -> Result<()> {
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if cell.get().is_some() {
                return Err(Error::VcpuTlsInit);
            }
            cell.set(Some(self as *const Vcpu));
            Ok(())
        })
    }

    /// Deassociates `self` from the current thread.
    fn reset_thread_local_data(&mut self) -> Result<()> {
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if let Some(vcpu_ptr) = cell.get() {
                if std::ptr::eq(vcpu_ptr, self) {
                    Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| cell.take());
                    return Ok(());
                }
            }
            Err(Error::VcpuTlsNotPresent)
        })
    }

    /// Registers a signal handler to kick the vCPU.
    ///
    /// On Windows there are no POSIX signals; this is intentionally a no-op.
    /// vCPU cancellation is done via `WHvCancelRunVirtualProcessor` instead.
    pub fn register_kick_signal_handler() {
        // No-op on Windows. See `WHvCancelRunVirtualProcessor`.
    }

    /// Constructs a new x86-64 VCPU.
    ///
    /// # Arguments
    ///
    /// * `id`       - Logical CPU number in [0, max_vcpus).
    /// * `vm`       - The Vm whose partition this vCPU will belong to.
    /// * `exit_evt` - EventFd signalled when this vCPU exits.
    pub fn new_x86_64(
        id: u8,
        vm: &Vm,
        exit_evt: EventFd,
    ) -> Result<Self> {
        let partition = Arc::clone(vm.partition());
        let index = id as u32;

        // Safety: we own the partition and the index is within the processor
        // count set during Vm::new.
        check_whpx!(unsafe {
            WHvCreateVirtualProcessor(partition.partition, index, 0)
        })
        .map_err(Error::CreateVirtualProcessor)?;

        let emulator = SafeEmulator::new()?;

        let (event_sender, event_receiver) = unbounded();
        let (response_sender, response_receiver) = unbounded();

        info!("WHPX vCPU {id} created");

        Ok(Vcpu {
            id,
            whpx_index: index,
            partition,
            emulator,
            exit_context: unsafe { zeroed() },
            io_bus: None,
            mmio_bus: None,
            exit_evt,
            event_receiver,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
            response_sender,
        })
    }

    /// Returns the CPU index as seen by the guest.
    pub fn cpu_index(&self) -> u8 {
        self.id
    }

    /// Sets the MMIO bus for this vCPU.
    pub fn set_mmio_bus(&mut self, mmio_bus: devices::Bus) {
        self.mmio_bus = Some(mmio_bus);
    }

    /// Sets the IO port bus for this vCPU.
    pub fn set_io_bus(&mut self, io_bus: devices::Bus) {
        self.io_bus = Some(io_bus);
    }

    /// Configures an x86-64 specific vCPU for Linux direct boot.
    ///
    /// This is the WHPX equivalent of the KVM backend's `setup_regs`,
    /// `setup_sregs`, and `setup_page_tables`. It:
    /// 1. Writes identity-mapped page tables (PML4/PDPTE/PDE) to guest memory
    /// 2. Writes a GDT with NULL, CODE64, DATA, and TSS entries
    /// 3. Sets all segment, control, and general-purpose registers via
    ///    `WHvSetVirtualProcessorRegisters` to enter 64-bit long mode
    ///
    /// # Arguments
    ///
    /// * `guest_mem`         - The guest memory map.
    /// * `kernel_start_addr` - Guest physical address of the kernel entry point.
    /// * `vcpu_config`       - Vcpu configuration (count, HT, template).
    /// * `kernel_boot`       - Whether we are performing a direct kernel boot.
    pub fn configure_x86_64(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        kernel_start_addr: GuestAddress,
        _vcpu_config: &VcpuConfig,
        kernel_boot: bool,
    ) -> Result<()> {
        if !kernel_boot {
            // For non-kernel boots (e.g. firmware), WHPX uses the default
            // real-mode reset vector. Nothing to configure.
            return Ok(());
        }

        // --------------------------------------------------------------------
        // Layout constants (mirroring arch/src/x86_64/layout.rs)
        // --------------------------------------------------------------------
        const BOOT_STACK_POINTER: u64 = 0x8ff0;
        const ZERO_PAGE_START: u64 = 0x7000;

        // Page table addresses (mirroring arch/src/x86_64/regs.rs)
        const PML4_START: u64 = 0x9000;
        const PDPTE_START: u64 = 0xA000;
        const PDE_START: u64 = 0xB000;

        // GDT address (mirroring arch/src/x86_64/regs.rs)
        const GDT_START: u64 = 0x500;
        const IDT_START: u64 = 0x520;

        // --------------------------------------------------------------------
        // 1. Write page tables to guest memory
        //
        // Identity map the first 1 GB using 2 MB pages:
        //   PML4[0]  -> PDPTE  (flags: Present | Writable = 0x03)
        //   PDPTE[0] -> PDE    (flags: Present | Writable = 0x03)
        //   PDE[i]   -> i*2MB  (flags: Present | Writable | PageSize = 0x83)
        // --------------------------------------------------------------------
        guest_mem
            .write_obj(PDPTE_START | 0x03u64, GuestAddress(PML4_START))
            .map_err(Error::GuestMemoryMmap)?;

        guest_mem
            .write_obj(PDE_START | 0x03u64, GuestAddress(PDPTE_START))
            .map_err(Error::GuestMemoryMmap)?;

        for i in 0u64..512 {
            let pde_entry: u64 = (i << 21) | 0x83;
            guest_mem
                .write_obj(pde_entry, GuestAddress(PDE_START + i * 8))
                .map_err(Error::GuestMemoryMmap)?;
        }

        // --------------------------------------------------------------------
        // 2. Write GDT to guest memory
        //
        // GDT entry encoding (from arch/src/x86_64/gdt.rs):
        //   gdt_entry(flags, base, limit) -> u64
        // Entries:
        //   [0] = 0x0000_0000_0000_0000  (NULL)
        //   [1] = gdt_entry(0xa09b, 0, 0xfffff)  CODE64
        //   [2] = gdt_entry(0xc093, 0, 0xfffff)  DATA
        //   [3] = gdt_entry(0x808b, 0, 0xfffff)  TSS
        // --------------------------------------------------------------------

        /// Encode a GDT entry from flags, base, and limit.
        /// Identical to arch::x86_64::gdt::gdt_entry but inlined here
        /// because the original is behind `cfg(target_os = "linux")`.
        fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
            ((u64::from(base) & 0xff00_0000u64) << (56 - 24))
                | ((u64::from(flags) & 0x0000_f0ffu64) << 40)
                | ((u64::from(limit) & 0x000f_0000u64) << (48 - 16))
                | ((u64::from(base) & 0x00ff_ffffu64) << 16)
                | (u64::from(limit) & 0x0000_ffffu64)
        }

        let gdt: [u64; 4] = [
            0,                              // NULL
            gdt_entry(0xa09b, 0, 0xfffff),  // CODE64
            gdt_entry(0xc093, 0, 0xfffff),  // DATA
            gdt_entry(0x808b, 0, 0xfffff),  // TSS
        ];

        for (i, entry) in gdt.iter().enumerate() {
            guest_mem
                .write_obj(*entry, GuestAddress(GDT_START + (i as u64) * 8))
                .map_err(Error::GuestMemoryMmap)?;
        }

        // IDT: write a zero entry at the IDT address.
        guest_mem
            .write_obj(0u64, GuestAddress(IDT_START))
            .map_err(Error::GuestMemoryMmap)?;

        // --------------------------------------------------------------------
        // 3. Build segment register attributes from GDT entries
        //
        // WHPX WHV_X64_SEGMENT_REGISTER.Attributes uses the VMX access-rights
        // encoding packed into a u16:
        //   bits 3:0  = Type
        //   bit  4    = S (descriptor type: 1=code/data, 0=system)
        //   bits 6:5  = DPL
        //   bit  7    = Present
        //   bit  12   = AVL
        //   bit  13   = L (64-bit mode)
        //   bit  14   = D/B
        //   bit  15   = G (granularity)
        // --------------------------------------------------------------------

        /// Extract VMX-style segment attributes from a raw GDT entry.
        fn seg_attributes(entry: u64) -> u16 {
            // Low byte of attributes: type(4) | S(1) | DPL(2) | P(1) = bits 40..47
            let lo = ((entry >> 40) & 0xFF) as u16;
            // High nibble: AVL(1) | L(1) | D/B(1) | G(1) = bits 52..55
            let hi = ((entry >> 52) & 0x0F) as u16;
            lo | (hi << 12)
        }

        /// Extract the base address from a GDT entry.
        fn seg_base(entry: u64) -> u64 {
            ((entry & 0xFF00_0000_0000_0000) >> 32)
                | ((entry & 0x0000_00FF_0000_0000) >> 16)
                | ((entry & 0x0000_0000_FFFF_0000) >> 16)
        }

        /// Extract the limit from a GDT entry.
        fn seg_limit(entry: u64) -> u32 {
            (((entry & 0x000F_0000_0000_0000) >> 32) | (entry & 0x0000_0000_0000_FFFF)) as u32
        }

        // --------------------------------------------------------------------
        // 4. Set all registers via WHvSetVirtualProcessorRegisters
        //
        // Register layout (18 registers total):
        //   [0]  RIP     = kernel entry address
        //   [1]  RFLAGS  = 0x2
        //   [2]  RSP     = BOOT_STACK_POINTER
        //   [3]  RBP     = BOOT_STACK_POINTER
        //   [4]  RSI     = ZERO_PAGE_START
        //   [5]  CR0     = PE | PG = 0x8000_0001
        //   [6]  CR3     = PML4_START (0x9000)
        //   [7]  CR4     = PAE = 0x20
        //   [8]  EFER    = LME | LMA = 0x500
        //   [9]  CS      = code segment (selector=8)
        //   [10] DS      = data segment (selector=16)
        //   [11] ES      = data segment (selector=16)
        //   [12] FS      = data segment (selector=16)
        //   [13] GS      = data segment (selector=16)
        //   [14] SS      = data segment (selector=16)
        //   [15] TR      = TSS segment  (selector=24)
        //   [16] GDTR    = base=0x500, limit=31
        //   [17] IDTR    = base=0x520, limit=7
        // --------------------------------------------------------------------

        const NUM_REGS: usize = 18;
        let reg_names: [WHV_REGISTER_NAME; NUM_REGS] = [
            WHvX64RegisterRip,     // 0
            WHvX64RegisterRflags,  // 1
            WHvX64RegisterRsp,     // 2
            WHvX64RegisterRbp,     // 3
            WHvX64RegisterRsi,     // 4
            WHvX64RegisterCr0,     // 5
            WHvX64RegisterCr3,     // 6
            WHvX64RegisterCr4,     // 7
            WHvX64RegisterEfer,    // 8
            WHvX64RegisterCs,      // 9
            WHvX64RegisterDs,      // 10
            WHvX64RegisterEs,      // 11
            WHvX64RegisterFs,      // 12
            WHvX64RegisterGs,      // 13
            WHvX64RegisterSs,      // 14
            WHvX64RegisterTr,      // 15
            WHvX64RegisterGdtr,    // 16
            WHvX64RegisterIdtr,    // 17
        ];

        let mut reg_values: [WHV_REGISTER_VALUE; NUM_REGS] = unsafe { zeroed() };

        // General-purpose and control registers (use Reg64 union field)
        reg_values[0].Reg64 = kernel_start_addr.raw_value(); // RIP
        reg_values[1].Reg64 = 0x2;                           // RFLAGS
        reg_values[2].Reg64 = BOOT_STACK_POINTER;            // RSP
        reg_values[3].Reg64 = BOOT_STACK_POINTER;            // RBP
        reg_values[4].Reg64 = ZERO_PAGE_START;               // RSI
        reg_values[5].Reg64 = 0x8000_0001;                   // CR0: PE | PG
        reg_values[6].Reg64 = PML4_START;                    // CR3
        reg_values[7].Reg64 = 0x20;                          // CR4: PAE
        reg_values[8].Reg64 = 0x500;                         // EFER: LME | LMA

        // CS: code segment from gdt[1], selector = 1*8 = 8
        let code_entry = gdt[1];
        reg_values[9].Segment = WHV_X64_SEGMENT_REGISTER {
            Base: seg_base(code_entry),
            Limit: seg_limit(code_entry),
            Selector: 8,
            Anonymous: WHV_X64_SEGMENT_REGISTER_0 { Attributes: seg_attributes(code_entry) },
        };

        // DS/ES/FS/GS/SS: data segment from gdt[2], selector = 2*8 = 16
        let data_entry = gdt[2];
        let data_seg = WHV_X64_SEGMENT_REGISTER {
            Base: seg_base(data_entry),
            Limit: seg_limit(data_entry),
            Selector: 16,
            Anonymous: WHV_X64_SEGMENT_REGISTER_0 { Attributes: seg_attributes(data_entry) },
        };
        reg_values[10].Segment = data_seg; // DS
        reg_values[11].Segment = data_seg; // ES
        reg_values[12].Segment = data_seg; // FS
        reg_values[13].Segment = data_seg; // GS
        reg_values[14].Segment = data_seg; // SS

        // TR: TSS segment from gdt[3], selector = 3*8 = 24
        let tss_entry = gdt[3];
        reg_values[15].Segment = WHV_X64_SEGMENT_REGISTER {
            Base: seg_base(tss_entry),
            Limit: seg_limit(tss_entry),
            Selector: 24,
            Anonymous: WHV_X64_SEGMENT_REGISTER_0 { Attributes: seg_attributes(tss_entry) },
        };

        // GDTR: base = GDT_START, limit = (4 entries * 8 bytes) - 1 = 31
        reg_values[16].Table = WHV_X64_TABLE_REGISTER {
            Pad: [0u16; 3],
            Base: GDT_START,
            Limit: 31,
        };

        // IDTR: base = IDT_START, limit = 7
        reg_values[17].Table = WHV_X64_TABLE_REGISTER {
            Pad: [0u16; 3],
            Base: IDT_START,
            Limit: 7,
        };

        // Safety: register arrays are correctly sized, the vCPU belongs to
        // this partition, and all union fields are initialized.
        check_whpx!(unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition.partition,
                self.whpx_index,
                reg_names.as_ptr(),
                reg_names.len() as u32,
                reg_values.as_ptr(),
            )
        })
        .map_err(Error::SetRegisters)?;

        debug!(
            "WHPX vCPU {} configured for long mode: RIP=0x{:x} RSP=0x{:x} CR3=0x{:x}",
            self.id,
            kernel_start_addr.raw_value(),
            BOOT_STACK_POINTER,
            PML4_START,
        );

        Ok(())
    }

    /// Moves the vCPU onto its own thread and returns a `VcpuHandle` for
    /// controlling it.
    pub fn start_threaded(mut self) -> Result<VcpuHandle> {
        let event_sender = self.event_sender.take().unwrap();
        let response_receiver = self.response_receiver.take().unwrap();
        let (init_tls_sender, init_tls_receiver) = unbounded();

        let vcpu_thread = thread::Builder::new()
            .name(format!("fc_vcpu {}", self.cpu_index()))
            .spawn(move || {
                self.init_thread_local_data()
                    .expect("Cannot cleanly initialize vcpu TLS.");

                init_tls_sender
                    .send(true)
                    .expect("Cannot notify vcpu TLS initialization.");

                self.run();
            })
            .map_err(Error::VcpuSpawn)?;

        init_tls_receiver
            .recv()
            .expect("Error waiting for TLS initialization.");

        Ok(VcpuHandle::new(
            event_sender,
            response_receiver,
            vcpu_thread,
        ))
    }

    // -------------------------------------------------------------------
    // Private: run loop
    // -------------------------------------------------------------------

    /// Main vCPU run loop.
    ///
    /// This mirrors the structure of the macOS HVF backend:
    /// - run the vCPU
    /// - dispatch the exit reason
    /// - loop until halt / error
    fn run(&mut self) {
        eprintln!("[WHPX] vCPU {} run loop starting", self.id);
        loop {
            match self.run_emulation() {
                Ok(VcpuEmulation::Handled) => {}
                Ok(VcpuEmulation::Stopped) => {
                    self.exit(FC_EXIT_CODE_OK);
                    break;
                }
                Err(e) => {
                    error!("WHPX vCPU {} error: {e}", self.id);
                    self.exit(FC_EXIT_CODE_GENERIC_ERROR);
                    break;
                }
            }
        }
    }

    /// Single run-dispatch-handle cycle.
    fn run_emulation(&mut self) -> Result<VcpuEmulation> {
        // Safety: we own this vCPU; the exit context buffer is correctly sized.
        check_whpx!(unsafe {
            WHvRunVirtualProcessor(
                self.partition.partition,
                self.whpx_index,
                &mut self.exit_context as *mut _ as *mut std::ffi::c_void,
                size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        })
        .map_err(Error::RunVirtualProcessor)?;

        eprintln!(
            "[WHPX] vCPU {} exit reason: {}",
            self.id, self.exit_context.ExitReason
        );
        self.handle_exit()
    }

    /// Dispatch on the exit reason stored in `self.exit_context`.
    #[allow(non_upper_case_globals)]
    fn handle_exit(&mut self) -> Result<VcpuEmulation> {
        match self.exit_context.ExitReason {
            // ---- Memory access (MMIO) ----
            WHvRunVpExitReasonMemoryAccess => {
                self.handle_mmio_exit()?;
                Ok(VcpuEmulation::Handled)
            }

            // ---- IO port access ----
            WHvRunVpExitReasonX64IoPortAccess => {
                self.handle_io_exit()?;
                Ok(VcpuEmulation::Handled)
            }

            // ---- Halt ----
            WHvRunVpExitReasonX64Halt => {
                info!("WHPX vCPU {} halted", self.id);
                Ok(VcpuEmulation::Stopped)
            }

            // ---- Cancelled (by WHvCancelRunVirtualProcessor) ----
            WHvRunVpExitReasonCanceled => {
                debug!("WHPX vCPU {} run cancelled", self.id);
                Ok(VcpuEmulation::Stopped)
            }

            // ---- CPUID exit ----
            WHvRunVpExitReasonX64Cpuid => {
                self.handle_cpuid_exit()?;
                Ok(VcpuEmulation::Handled)
            }

            // ---- MSR access exit ----
            WHvRunVpExitReasonX64MsrAccess => {
                self.handle_msr_exit()?;
                Ok(VcpuEmulation::Handled)
            }

            // ---- APIC EOI ----
            WHvRunVpExitReasonX64ApicEoi => {
                // TODO(Phase 2): forward to IOAPIC device.
                debug!("WHPX vCPU {} APIC EOI", self.id);
                Ok(VcpuEmulation::Handled)
            }

            // ---- Interrupt window ----
            WHvRunVpExitReasonX64InterruptWindow => {
                // TODO(Phase 2): inject pending interrupts.
                debug!("WHPX vCPU {} interrupt window", self.id);
                Ok(VcpuEmulation::Handled)
            }

            // ---- Unrecoverable exception ----
            WHvRunVpExitReasonUnrecoverableException => {
                error!("WHPX vCPU {} unrecoverable exception", self.id);
                Err(Error::VcpuUnhandledExit(
                    self.exit_context.ExitReason as u32,
                ))
            }

            // ---- Invalid VP register value ----
            WHvRunVpExitReasonInvalidVpRegisterValue => {
                error!("WHPX vCPU {} invalid VP register value", self.id);
                Err(Error::VcpuUnhandledExit(
                    self.exit_context.ExitReason as u32,
                ))
            }

            // ---- Unsupported feature ----
            WHvRunVpExitReasonUnsupportedFeature => {
                error!("WHPX vCPU {} unsupported feature exit", self.id);
                Err(Error::VcpuUnhandledExit(
                    self.exit_context.ExitReason as u32,
                ))
            }

            // ---- Unknown/unhandled exit ----
            other => {
                error!("WHPX vCPU {} unknown exit reason: {}", self.id, other);
                Err(Error::VcpuUnhandledExit(other as u32))
            }
        }
    }

    /// Handle MMIO exit via the WHPX instruction emulator.
    fn handle_mmio_exit(&mut self) -> Result<()> {
        let mut ctx = EmulatorContext {
            partition: self.partition.partition,
            vcpu_index: self.whpx_index,
            io_bus: self.io_bus.as_ref(),
            mmio_bus: self.mmio_bus.as_ref(),
        };
        let mut status: WHV_EMULATOR_STATUS = unsafe { zeroed() };

        // Safety: the emulator handle is valid; the exit context was populated
        // by the preceding WHvRunVirtualProcessor call; ctx references live
        // bus objects.
        let hr = unsafe {
            WHvEmulatorTryMmioEmulation(
                self.emulator.handle,
                &mut ctx as *mut _ as *mut std::ffi::c_void,
                &self.exit_context.VpContext,
                &self.exit_context.Anonymous.MemoryAccess,
                &mut status,
            )
        };

        check_whpx!(hr).map_err(Error::MmioEmulation)?;

        // Bit 0 of the status is EmulationSuccessful.
        let emulation_ok = (unsafe { status.AsUINT32 } & 1) != 0;
        if !emulation_ok {
            warn!(
                "WHPX MMIO emulation was not successful for vCPU {}",
                self.id
            );
        }
        Ok(())
    }

    /// Handle IO port exit via the WHPX instruction emulator.
    fn handle_io_exit(&mut self) -> Result<()> {
        let mut ctx = EmulatorContext {
            partition: self.partition.partition,
            vcpu_index: self.whpx_index,
            io_bus: self.io_bus.as_ref(),
            mmio_bus: self.mmio_bus.as_ref(),
        };
        let mut status: WHV_EMULATOR_STATUS = unsafe { zeroed() };

        // Safety: same as handle_mmio_exit.
        let hr = unsafe {
            WHvEmulatorTryIoEmulation(
                self.emulator.handle,
                &mut ctx as *mut _ as *mut std::ffi::c_void,
                &self.exit_context.VpContext,
                &self.exit_context.Anonymous.IoPortAccess,
                &mut status,
            )
        };

        check_whpx!(hr).map_err(Error::IoEmulation)?;

        let emulation_ok = (unsafe { status.AsUINT32 } & 1) != 0;
        if !emulation_ok {
            warn!(
                "WHPX IO emulation was not successful for vCPU {}",
                self.id
            );
        }
        Ok(())
    }

    /// Handle CPUID exit.
    ///
    /// For now, we accept the default result that WHPX provides and advance
    /// RIP past the CPUID instruction.
    fn handle_cpuid_exit(&mut self) -> Result<()> {
        // The default CPUID result is already in the exit context.
        // We need to write the result registers (EAX/EBX/ECX/EDX) and advance
        // RIP past the 2-byte CPUID instruction.
        let cpuid = unsafe { &self.exit_context.Anonymous.CpuidAccess };

        let reg_names = [
            WHvX64RegisterRax,
            WHvX64RegisterRbx,
            WHvX64RegisterRcx,
            WHvX64RegisterRdx,
            WHvX64RegisterRip,
        ];
        let mut reg_values: [WHV_REGISTER_VALUE; 5] = unsafe { zeroed() };
        reg_values[0].Reg64 = cpuid.DefaultResultRax;
        reg_values[1].Reg64 = cpuid.DefaultResultRbx;
        reg_values[2].Reg64 = cpuid.DefaultResultRcx;
        reg_values[3].Reg64 = cpuid.DefaultResultRdx;
        // Advance RIP past the 2-byte CPUID instruction.
        reg_values[4].Reg64 = self.exit_context.VpContext.Rip + 2;

        check_whpx!(unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition.partition,
                self.whpx_index,
                reg_names.as_ptr(),
                reg_names.len() as u32,
                reg_values.as_ptr(),
            )
        })
        .map_err(Error::SetRegisters)?;

        Ok(())
    }

    /// Handle MSR access exit.
    ///
    /// WHPX exits to us when the guest accesses an MSR that would otherwise
    /// cause a GP fault. We handle a few Hyper-V synthetic MSRs and inject a
    /// GP fault for everything else.
    fn handle_msr_exit(&mut self) -> Result<()> {
        let msr = unsafe { &self.exit_context.Anonymous.MsrAccess };
        let msr_number = msr.MsrNumber;
        // In windows-sys, AccessInfo is a union with AsUINT32.
        // Bit 0 of the raw value is the IsWrite flag.
        let is_write = (unsafe { msr.AccessInfo.AsUINT32 } & 1) != 0;

        if is_write {
            // For writes to known Hyper-V MSRs, just accept and advance RIP.
            match msr_number {
                HV_X64_MSR_TSC_INVARIANT_CONTROL
                | HV_X64_MSR_APIC_FREQUENCY
                | HV_X64_MSR_TSC_FREQUENCY => {
                    debug!(
                        "WHPX vCPU {}: MSR write 0x{msr_number:x} (accepted)",
                        self.id
                    );
                }
                _ => {
                    debug!(
                        "WHPX vCPU {}: MSR write 0x{msr_number:x} (ignored, inject GP)",
                        self.id
                    );
                    // TODO(Phase 2): inject #GP(0) for truly unknown MSRs.
                    // For now, silently drop and advance.
                }
            }
        } else {
            // Reads: return 0 for known MSRs, inject GP for unknown.
            match msr_number {
                HV_X64_MSR_TSC_FREQUENCY => {
                    // TODO: return actual TSC frequency.
                    self.set_register(WHvX64RegisterRax, 0)?;
                    self.set_register(WHvX64RegisterRdx, 0)?;
                }
                HV_X64_MSR_APIC_FREQUENCY => {
                    // TODO: return actual APIC bus frequency.
                    self.set_register(WHvX64RegisterRax, 0)?;
                    self.set_register(WHvX64RegisterRdx, 0)?;
                }
                HV_X64_MSR_TSC_INVARIANT_CONTROL => {
                    self.set_register(WHvX64RegisterRax, 0)?;
                    self.set_register(WHvX64RegisterRdx, 0)?;
                }
                _ => {
                    debug!(
                        "WHPX vCPU {}: MSR read 0x{msr_number:x} (unknown, returning 0)",
                        self.id
                    );
                    self.set_register(WHvX64RegisterRax, 0)?;
                    self.set_register(WHvX64RegisterRdx, 0)?;
                }
            }
        }

        // Advance RIP past the 2-byte RDMSR/WRMSR instruction.
        let current_rip = self.exit_context.VpContext.Rip;
        self.set_register(WHvX64RegisterRip, current_rip + 2)?;

        Ok(())
    }

    /// Helper: set a single 64-bit register on this vCPU.
    fn set_register(&self, name: WHV_REGISTER_NAME, value: u64) -> Result<()> {
        let names = [name];
        let mut values: [WHV_REGISTER_VALUE; 1] = unsafe { zeroed() };
        values[0].Reg64 = value;

        check_whpx!(unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition.partition,
                self.whpx_index,
                names.as_ptr(),
                1,
                values.as_ptr(),
            )
        })
        .map_err(Error::SetRegisters)
    }

    /// Helper: get a single 64-bit register from this vCPU.
    #[allow(dead_code)]
    fn get_register(&self, name: WHV_REGISTER_NAME) -> Result<u64> {
        let names = [name];
        let mut values: [WHV_REGISTER_VALUE; 1] = unsafe { zeroed() };

        check_whpx!(unsafe {
            WHvGetVirtualProcessorRegisters(
                self.partition.partition,
                self.whpx_index,
                names.as_ptr(),
                1,
                values.as_mut_ptr(),
            )
        })
        .map_err(Error::GetRegisters)?;

        Ok(unsafe { values[0].Reg64 })
    }

    /// Signal exit and notify the response channel.
    fn exit(&mut self, exit_code: u8) {
        self.response_sender
            .send(VcpuResponse::Exited(exit_code))
            .expect("failed to send Exited status");

        if let Err(e) = self.exit_evt.write(1) {
            error!("Failed signaling vcpu exit event: {e}");
        }
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        let _ = self.reset_thread_local_data();
        // Delete the virtual processor from the partition.
        // Safety: we own this vCPU.
        let hr = unsafe {
            WHvDeleteVirtualProcessor(self.partition.partition, self.whpx_index)
        };
        if hr != S_OK {
            error!(
                "WHvDeleteVirtualProcessor failed for vCPU {}: HRESULT 0x{:08X}",
                self.id, hr as u32
            );
        }
    }
}

// ---------------------------------------------------------------------------
// VcpuEvent / VcpuResponse / VcpuHandle
// ---------------------------------------------------------------------------

/// Events that can be sent to a vCPU thread.
#[allow(unused)]
#[derive(Debug)]
pub enum VcpuEvent {
    /// Pause the vCPU.
    Pause,
    /// Resume the vCPU.
    Resume,
}

/// Responses from the vCPU thread.
#[derive(Debug, Eq, PartialEq)]
pub enum VcpuResponse {
    /// vCPU is paused.
    Paused,
    /// vCPU is resumed.
    Resumed,
    /// vCPU has exited.
    Exited(u8),
}

/// Handle for controlling a vCPU running on its own thread.
pub struct VcpuHandle {
    event_sender: Sender<VcpuEvent>,
    response_receiver: Receiver<VcpuResponse>,
}

impl VcpuHandle {
    pub fn new(
        event_sender: Sender<VcpuEvent>,
        response_receiver: Receiver<VcpuResponse>,
        _vcpu_thread: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            event_sender,
            response_receiver,
        }
    }

    pub fn send_event(&self, event: VcpuEvent) -> Result<()> {
        self.event_sender
            .send(event)
            .expect("event sender channel closed on vcpu end.");
        // On Windows we would use WHvCancelRunVirtualProcessor to kick the
        // vCPU out of WHvRunVirtualProcessor. TODO(Phase 3): store the
        // partition handle + index so we can cancel here.
        Ok(())
    }

    pub fn response_receiver(&self) -> &Receiver<VcpuResponse> {
        &self.response_receiver
    }
}

// ---------------------------------------------------------------------------
// VcpuEmulation (internal)
// ---------------------------------------------------------------------------

/// Internal enum for the vCPU run-loop dispatch.
enum VcpuEmulation {
    /// The exit was handled; continue running.
    Handled,
    /// The guest has stopped (halt, shutdown, cancel).
    Stopped,
}
