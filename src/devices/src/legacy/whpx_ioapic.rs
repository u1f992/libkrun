// Copyright 2026 libkrun contributors.
// SPDX-License-Identifier: Apache-2.0
//
// WHPX IOAPIC implementation.
// When WHPX APIC emulation mode is set to XApic, the platform handles
// interrupt routing internally via WHvRequestInterrupt. This module provides
// the IrqChipT interface that libkrun expects, forwarding interrupt requests
// to the WHPX interrupt controller.
//
// Reference: crosvm devices/src/irqchip/whpx.rs (WhpxSplitIrqChip)

use utils::eventfd::EventFd;

use crate::bus::BusDevice;
use crate::legacy::irqchip::IrqChipT;
use crate::Error as DeviceError;

use windows_sys::Win32::System::Hypervisor::*;

const IOAPIC_BASE: u64 = 0xfec0_0000;
const IOAPIC_SIZE: u64 = 0x100;
const IOAPIC_NUM_PINS: usize = 24;

/// WHPX-backed IOAPIC that forwards interrupts via WHvRequestInterrupt.
///
/// WHPX with XApic emulation mode handles the in-VM APIC state. We only need
/// to track IRQ pin → vector mapping and call WHvRequestInterrupt when a
/// device asserts an IRQ line.
pub struct WhpxIoapic {
    partition: WHV_PARTITION_HANDLE,
    /// IOAPIC register select.
    ioregsel: u8,
    /// Redirection table entries.
    /// Format per Intel IOAPIC spec: 64-bit per pin.
    /// Bits 7:0 = vector, 10:8 = delivery mode, 11 = dest mode,
    /// 15 = trigger mode, 16 = mask, 63:56 = destination.
    ioredtbl: [u64; IOAPIC_NUM_PINS],
}

impl WhpxIoapic {
    pub fn new(partition: WHV_PARTITION_HANDLE) -> Self {
        // All entries start masked (bit 16 = 1).
        let mut ioredtbl = [0u64; IOAPIC_NUM_PINS];
        for entry in &mut ioredtbl {
            *entry = 1 << 16; // masked
        }

        WhpxIoapic {
            partition,
            ioregsel: 0,
            ioredtbl,
        }
    }

    /// Deliver an interrupt to the guest via WHPX.
    /// Reads trigger mode, destination, and delivery mode from the redirection
    /// table entry for the given pin.
    fn deliver_interrupt(&self, pin: usize) {
        let entry = self.ioredtbl[pin];
        let vector = (entry & 0xFF) as u32;
        let delivery_mode = ((entry >> 8) & 0x7) as u64;  // bits 10:8
        let dest_mode = ((entry >> 11) & 0x1) as u64;      // bit 11 (0=physical, 1=logical)
        let trigger_mode = ((entry >> 15) & 0x1) as u64;   // bit 15 (0=edge, 1=level)
        let destination = ((entry >> 56) & 0xFF) as u32;    // bits 63:56

        // WHV_INTERRUPT_CONTROL._bitfield layout:
        // bits 0-3: Type (delivery mode: 0=Fixed, 1=Lowest, etc.)
        // bit 4: DestinationMode (0=Physical, 1=Logical)
        // bit 5: TriggerMode (0=Edge, 1=Level)
        let bitfield: u64 = delivery_mode | (dest_mode << 4) | (trigger_mode << 5);

        let interrupt = WHV_INTERRUPT_CONTROL {
            _bitfield: bitfield,
            Destination: destination,
            Vector: vector,
        };

        let result = unsafe {
            WHvRequestInterrupt(
                self.partition,
                &interrupt as *const WHV_INTERRUPT_CONTROL,
                std::mem::size_of::<WHV_INTERRUPT_CONTROL>() as u32,
            )
        };

        if result != 0 {
            log::warn!(
                "WHvRequestInterrupt failed for pin {} vector {}: HRESULT 0x{:08X}",
                pin, vector, result as u32
            );
        }
    }
}

impl BusDevice for WhpxIoapic {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 4 {
            return;
        }
        let val: u32 = match offset {
            0x00 => self.ioregsel as u32, // IOREGSEL
            0x10 => {
                // IOWIN - read the selected register
                match self.ioregsel {
                    0x00 => 0, // IOAPIC ID
                    0x01 => {
                        // IOAPIC Version + max redirection entries
                        0x11 | (((IOAPIC_NUM_PINS - 1) as u32) << 16)
                    }
                    0x02 => 0, // Arbitration ID
                    sel if sel >= 0x10 && sel < 0x10 + (IOAPIC_NUM_PINS as u8) * 2 => {
                        let pin = ((sel - 0x10) / 2) as usize;
                        let entry = self.ioredtbl[pin];
                        if sel % 2 == 0 {
                            entry as u32
                        } else {
                            (entry >> 32) as u32
                        }
                    }
                    _ => 0,
                }
            }
            _ => 0,
        };
        data.copy_from_slice(&val.to_le_bytes());
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if data.len() != 4 {
            return;
        }
        let val = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        match offset {
            0x00 => self.ioregsel = val as u8,
            0x10 => {
                // IOWIN - write to the selected register
                let sel = self.ioregsel;
                if sel >= 0x10 && sel < 0x10 + (IOAPIC_NUM_PINS as u8) * 2 {
                    let pin = ((sel - 0x10) / 2) as usize;
                    if sel % 2 == 0 {
                        self.ioredtbl[pin] =
                            (self.ioredtbl[pin] & 0xFFFF_FFFF_0000_0000) | val as u64;
                    } else {
                        self.ioredtbl[pin] =
                            (self.ioredtbl[pin] & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32);
                    }
                }
            }
            0x40 => {
                // EOI register
                let vector = val as u8;
                log::debug!("IOAPIC EOI for vector {}", vector);
            }
            _ => {}
        }
    }
}

impl IrqChipT for WhpxIoapic {
    fn get_mmio_addr(&self) -> u64 {
        IOAPIC_BASE
    }

    fn get_mmio_size(&self) -> u64 {
        IOAPIC_SIZE
    }

    fn set_irq(
        &self,
        irq_line: Option<u32>,
        _interrupt_evt: Option<&EventFd>,
    ) -> Result<(), DeviceError> {
        if let Some(irq) = irq_line {
            let pin = irq as usize;
            if pin < IOAPIC_NUM_PINS {
                let entry = self.ioredtbl[pin];
                let vector = (entry & 0xFF) as u8;
                let masked = (entry >> 16) & 1 != 0;
                if !masked && vector != 0 {
                    self.deliver_interrupt(pin);
                }
            }
        }
        Ok(())
    }
}
