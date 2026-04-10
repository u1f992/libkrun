// Copyright 2026 libkrun contributors.
// SPDX-License-Identifier: Apache-2.0
//
// WHPX IOAPIC implementation with proper interrupt level tracking.
// Reference: crosvm devices/src/irqchip/ioapic.rs for service_irq logic.

use std::sync::atomic::{AtomicBool, Ordering};

use utils::eventfd::EventFd;

use crate::bus::BusDevice;
use crate::legacy::irqchip::IrqChipT;
use crate::Error as DeviceError;

use windows_sys::Win32::System::Hypervisor::*;

const IOAPIC_BASE: u64 = 0xfec0_0000;
const IOAPIC_SIZE: u64 = 0x100;
const IOAPIC_NUM_PINS: usize = 24;

// Redirection table entry bit positions
const RTE_VECTOR_MASK: u64 = 0xFF;
const RTE_DELIVERY_MODE_SHIFT: u64 = 8;
const RTE_DELIVERY_MODE_MASK: u64 = 0x7;
const RTE_DEST_MODE_SHIFT: u64 = 11;
const RTE_TRIGGER_MODE_SHIFT: u64 = 15;
const RTE_MASK_SHIFT: u64 = 16;
const RTE_REMOTE_IRR_SHIFT: u64 = 14;
const RTE_DEST_SHIFT: u64 = 56;

pub struct WhpxIoapic {
    partition: WHV_PARTITION_HANDLE,
    ioregsel: u8,
    ioredtbl: [u64; IOAPIC_NUM_PINS],
    /// Tracks the current level of each IRQ line for edge detection.
    /// AtomicBool for interior mutability (set_irq takes &self).
    interrupt_level: [AtomicBool; IOAPIC_NUM_PINS],
}

impl WhpxIoapic {
    pub fn new(partition: WHV_PARTITION_HANDLE) -> Self {
        // All entries start masked (bit 16 = 1).
        let mut ioredtbl = [0u64; IOAPIC_NUM_PINS];
        for entry in &mut ioredtbl {
            *entry = 1 << RTE_MASK_SHIFT;
        }

        // const array of AtomicBool::new(false)
        const INIT: AtomicBool = AtomicBool::new(false);
        WhpxIoapic {
            partition,
            ioregsel: 0,
            ioredtbl,
            interrupt_level: [INIT; IOAPIC_NUM_PINS],
        }
    }

    /// Deliver an interrupt to the guest via WHvRequestInterrupt.
    fn deliver_interrupt(&self, pin: usize) {
        let entry = self.ioredtbl[pin];
        let vector = (entry & RTE_VECTOR_MASK) as u32;
        let delivery_mode = ((entry >> RTE_DELIVERY_MODE_SHIFT) & RTE_DELIVERY_MODE_MASK) as u64;
        let dest_mode = ((entry >> RTE_DEST_MODE_SHIFT) & 0x1) as u64;
        let trigger_mode = ((entry >> RTE_TRIGGER_MODE_SHIFT) & 0x1) as u64;
        let destination = ((entry >> RTE_DEST_SHIFT) & 0xFF) as u32;

        // WHV_INTERRUPT_CONTROL._bitfield layout (MS official doc):
        // bits 0-7:   Type (WHV_INTERRUPT_TYPE, 8 bits)
        // bits 8-11:  DestinationMode (WHV_INTERRUPT_DESTINATION_MODE, 4 bits)
        // bits 12-15: TriggerMode (WHV_INTERRUPT_TRIGGER_MODE, 4 bits)
        // bits 16-63: Reserved
        let interrupt_type = delivery_mode & 0xFF;
        let bitfield: u64 = interrupt_type | (dest_mode << 8) | (trigger_mode << 12);

        // Force destination to 0 (BSP / vCPU 0) for single-CPU VMs.
        // The guest kernel may set destination=1 based on its APIC ID
        // enumeration, but WHPX uses vpindex (0) as the APIC target.
        let interrupt = WHV_INTERRUPT_CONTROL {
            _bitfield: bitfield,
            Destination: 0,
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
                "WHvRequestInterrupt FAILED pin={} vec={} dest={} trigger={}: HRESULT 0x{:08X}",
                pin, vector, destination, trigger_mode, result as u32
            );
        } else if pin != 0 {
            // Log non-PIT IRQ delivery
            static IRQ_LOG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if IRQ_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
                log::info!(
                    "WHvRequestInterrupt OK pin={} vec={} dest=0 trigger={}",
                    pin, vector, trigger_mode
                );
            }
        }
    }

    /// Service an IRQ: assert (level=true) or deassert (level=false).
    /// Returns true if interrupt was actually injected.
    fn service_irq(&self, pin: usize, level: bool) -> bool {
        let entry = self.ioredtbl[pin];
        let is_edge = ((entry >> RTE_TRIGGER_MODE_SHIFT) & 1) == 0;
        let masked = ((entry >> RTE_MASK_SHIFT) & 1) != 0;
        let vector = (entry & RTE_VECTOR_MASK) as u8;

        // Deassert
        if !level {
            self.interrupt_level[pin].store(false, Ordering::SeqCst);
            return true;
        }

        // Edge-triggered: ignore if already high (no new edge)
        if is_edge && self.interrupt_level[pin].load(Ordering::SeqCst) {
            return false;
        }

        self.interrupt_level[pin].store(true, Ordering::SeqCst);

        // Masked → don't inject
        if masked || vector == 0 {
            return false;
        }

        // Level-triggered with remote IRR already set → coalesce
        let remote_irr = ((entry >> RTE_REMOTE_IRR_SHIFT) & 1) != 0;
        if !is_edge && remote_irr {
            return false;
        }

        self.deliver_interrupt(pin);
        true
    }
}

impl BusDevice for WhpxIoapic {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 4 {
            return;
        }
        let val: u32 = match offset {
            0x00 => self.ioregsel as u32,
            0x10 => {
                match self.ioregsel {
                    0x00 => 0, // IOAPIC ID
                    0x01 => 0x11 | (((IOAPIC_NUM_PINS - 1) as u32) << 16), // Version
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
                let sel = self.ioregsel;
                if sel >= 0x10 && sel < 0x10 + (IOAPIC_NUM_PINS as u8) * 2 {
                    let pin = ((sel - 0x10) / 2) as usize;
                    let old_entry = self.ioredtbl[pin];
                    if sel % 2 == 0 {
                        // Low 32 bits - mask RO bits (remote IRR = bit 14, delivery status = bit 12)
                        let ro_mask = (1u64 << 14) | (1u64 << 12);
                        let new_low = (val as u64) & !ro_mask;
                        self.ioredtbl[pin] =
                            (old_entry & (0xFFFF_FFFF_0000_0000u64 | ro_mask)) | new_low;
                    } else {
                        // High 32 bits (destination)
                        self.ioredtbl[pin] =
                            (old_entry & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32);
                    }

                    let new_entry = self.ioredtbl[pin];
                    let new_vector = (new_entry & RTE_VECTOR_MASK) as u8;
                    let new_dest = ((new_entry >> RTE_DEST_SHIFT) & 0xFF) as u8;
                    let new_trigger = ((new_entry >> RTE_TRIGGER_MODE_SHIFT) & 1) as u8;
                    let new_mask = ((new_entry >> RTE_MASK_SHIFT) & 1) as u8;
                    log::debug!("IOAPIC pin {} write: vec={} dest={} trigger={} mask={}", pin, new_vector, new_dest, new_trigger, new_mask);

                    // If entry was just unmasked, check if there's a pending level
                    let was_masked = ((old_entry >> RTE_MASK_SHIFT) & 1) != 0;
                    let now_masked = ((self.ioredtbl[pin] >> RTE_MASK_SHIFT) & 1) != 0;
                    if was_masked && !now_masked && self.interrupt_level[pin].load(Ordering::SeqCst)
                    {
                        let vector = (self.ioredtbl[pin] & RTE_VECTOR_MASK) as u8;
                        if vector != 0 {
                            self.deliver_interrupt(pin);
                        }
                    }
                }
            }
            0x40 => {
                // EOI - clear remote IRR for the matching vector
                let eoi_vector = val as u8;
                for pin in 0..IOAPIC_NUM_PINS {
                    let entry = &mut self.ioredtbl[pin];
                    let pin_vector = (*entry & RTE_VECTOR_MASK) as u8;
                    if pin_vector == eoi_vector {
                        // Clear remote IRR (bit 14)
                        *entry &= !(1 << RTE_REMOTE_IRR_SHIFT);
                        // If level is still asserted, re-inject
                        if self.interrupt_level[pin].load(Ordering::SeqCst) {
                            let masked = ((*entry >> RTE_MASK_SHIFT) & 1) != 0;
                            if !masked {
                                // Set remote IRR and deliver
                                *entry |= 1 << RTE_REMOTE_IRR_SHIFT;
                                self.deliver_interrupt(pin);
                            }
                        }
                    }
                }
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
                let masked = ((entry >> RTE_MASK_SHIFT) & 1) != 0;
                let vector = (entry & RTE_VECTOR_MASK) as u8;
                // Only log non-PIT (pin != 0) IRQs
                if pin != 0 {
                    static SET_IRQ_LOG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    if SET_IRQ_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
                        log::info!("set_irq pin={} vec={} masked={}", pin, vector, masked);
                    }
                }
                let injected = self.service_irq(pin, true);
                self.service_irq(pin, false);
                if pin != 0 {
                    static SVC_LOG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    if SVC_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 10 {
                        log::info!("service_irq pin={} injected={}", pin, injected);
                    }
                }
            }
        }
        Ok(())
    }
}
