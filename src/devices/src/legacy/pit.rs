// SPDX-License-Identifier: Apache-2.0
//
// Minimal i8254 PIT (Programmable Interval Timer) for x86 Linux kernel boot on WHPX.
// Only counter 0 (system timer, IRQ0) is functional. Counters 1 and 2 are stubs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::bus::BusDevice;
use crate::legacy::irqchip::IrqChipT;
use crate::legacy::IrqChip;

/// PIT oscillator frequency in Hz.
const PIT_FREQUENCY: u64 = 1_193_182;

/// Per-counter state for the i8254.
struct PitCounter {
    reload_value: u16,
    mode: u8,
    rw_mode: u8,
    latch_value: Option<u16>,
    read_lsb: bool,
    write_lsb: bool,
}

impl PitCounter {
    fn new() -> Self {
        PitCounter {
            reload_value: 0,
            mode: 0,
            rw_mode: 3, // default: LSB then MSB
            latch_value: None,
            read_lsb: true,
            write_lsb: true,
        }
    }

    /// Read the current counter value (latched or reload).
    /// Returns one byte at a time based on rw_mode sequencing.
    fn read_byte(&mut self) -> u8 {
        let value = self.latch_value.unwrap_or(self.reload_value);

        match self.rw_mode {
            1 => {
                // LSB only
                self.latch_value = None;
                value as u8
            }
            2 => {
                // MSB only
                self.latch_value = None;
                (value >> 8) as u8
            }
            3 => {
                // LSB then MSB
                if self.read_lsb {
                    self.read_lsb = false;
                    value as u8
                } else {
                    self.read_lsb = true;
                    self.latch_value = None;
                    (value >> 8) as u8
                }
            }
            _ => 0,
        }
    }

    /// Write one byte to the counter reload register.
    /// Returns Some(reload_value) when the full value has been written.
    fn write_byte(&mut self, val: u8) -> Option<u16> {
        match self.rw_mode {
            1 => {
                // LSB only
                self.reload_value = u16::from(val);
                Some(self.reload_value)
            }
            2 => {
                // MSB only
                self.reload_value = u16::from(val) << 8;
                Some(self.reload_value)
            }
            3 => {
                // LSB then MSB
                if self.write_lsb {
                    self.reload_value = (self.reload_value & 0xFF00) | u16::from(val);
                    self.write_lsb = false;
                    None
                } else {
                    self.reload_value = (self.reload_value & 0x00FF) | (u16::from(val) << 8);
                    self.write_lsb = true;
                    Some(self.reload_value)
                }
            }
            _ => None,
        }
    }
}

/// Minimal i8254 PIT device. Only counter 0 generates IRQ0 timer interrupts.
pub struct Pit {
    counters: [PitCounter; 3],
    irq_chip: IrqChip,
    worker_running: Arc<AtomicBool>,
}

impl Pit {
    pub fn new(irq_chip: IrqChip) -> Self {
        Pit {
            counters: [PitCounter::new(), PitCounter::new(), PitCounter::new()],
            irq_chip,
            worker_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start (or restart) the timer thread for counter 0.
    fn start_timer(&mut self) {
        // Stop any existing worker.
        self.worker_running.store(false, Ordering::SeqCst);

        let reload = self.counters[0].reload_value;
        if reload == 0 {
            return;
        }

        let mode = self.counters[0].mode;
        // Only mode 2 (rate generator) and mode 3 (square wave) produce periodic interrupts.
        if mode != 2 && mode != 3 {
            return;
        }

        let interval_ns = (reload as u64) * 1_000_000_000 / PIT_FREQUENCY;
        let interval = Duration::from_nanos(interval_ns);
        let running = Arc::new(AtomicBool::new(true));
        self.worker_running = running.clone();
        let irq_chip = self.irq_chip.clone();

        thread::Builder::new()
            .name("pit-timer".to_string())
            .spawn(move || {
                while running.load(Ordering::SeqCst) {
                    thread::sleep(interval);
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(chip) = irq_chip.lock() {
                        let _ = chip.set_irq(Some(0), None);
                    }
                }
            })
            .expect("failed to spawn PIT timer thread");
    }

    /// Handle a write to the command register (port 0x43).
    fn write_command(&mut self, val: u8) {
        let counter_sel = (val >> 6) & 0x03;
        if counter_sel == 3 {
            // Read-back command; ignore for minimal implementation.
            return;
        }

        let idx = counter_sel as usize;
        let rw_mode = (val >> 4) & 0x03;

        if rw_mode == 0 {
            // Counter latch command: latch current value.
            self.counters[idx].latch_value = Some(self.counters[idx].reload_value);
            return;
        }

        self.counters[idx].rw_mode = rw_mode;
        self.counters[idx].mode = (val >> 1) & 0x07;
        self.counters[idx].write_lsb = true;
        self.counters[idx].read_lsb = true;
    }
}

impl Drop for Pit {
    fn drop(&mut self) {
        self.worker_running.store(false, Ordering::SeqCst);
    }
}

impl BusDevice for Pit {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        if data.is_empty() {
            return;
        }
        match offset {
            0x00..=0x02 => {
                data[0] = self.counters[offset as usize].read_byte();
            }
            _ => {
                data[0] = 0;
            }
        }
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let val = data[0];
        match offset {
            0x00..=0x02 => {
                let idx = offset as usize;
                if let Some(_reload) = self.counters[idx].write_byte(val) {
                    if idx == 0 {
                        self.start_timer();
                    }
                }
            }
            0x03 => {
                self.write_command(val);
            }
            _ => {}
        }
    }
}
