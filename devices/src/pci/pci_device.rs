// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use byteorder::{ByteOrder, LittleEndian};

use std;
use std::os::unix::io::RawFd;

use pci::pci_configuration::PciConfiguration;
use pci::PciInterruptPin;
use sys_util::{EventFd, GuestMemory};
use resources::SystemAllocator;

use BusDevice;

#[derive(Debug)]
pub enum Error {
    /// Allocating space for an IO BAR failed.
    IoAllocationFailed(u64),
    /// Registering an IO BAR failed.
    IoRegistrationFailed(u64),
}
pub type Result<T> = std::result::Result<T, Error>;

pub trait PciDevice: Send {
    /// A vector of device-specific file descriptors that must be kept open
    /// after jailing. Must be called before the process is jailed.
    fn keep_fds(&self) -> Vec<RawFd>;
    /// Assign a legacy PCI IRQ to this device.
    fn assign_irq(&mut self, _irq_evt: EventFd, _irq_num: u32, _irq_pin: PciInterruptPin) {}
    /// Gives the device guest memory if it is needed.
    fn set_guest_memory(&mut self, mem: GuestMemory) {}
    /// Allocates the needed IO BAR space using the `allocate` function which takes a size and
    /// returns an address. Returns a Vec of (address, length) tuples.
    fn allocate_io_bars(
        &mut self,
        _resources: &mut SystemAllocator,
    ) -> Result<Vec<(u64, u64)>> {
        Ok(Vec::new())
    }
    /// Gets a list of ioeventfds that should be registered with the running VM. The list is
    /// returned as a Vec of (eventfd, addr) tuples.
    fn ioeventfds(&self) -> Vec<(&EventFd, u64)> {
        Vec::new()
    }
    /// Gets the configuration registers of the Pci Device.
    fn config_registers(&self) -> &PciConfiguration; // TODO - remove these
    /// Gets the configuration registers of the Pci Device for modification.
    fn config_registers_mut(&mut self) -> &mut PciConfiguration;
    /// Reads from a BAR region mapped in to the device.
    /// * `addr` - The guest address inside the BAR.
    /// * `data` - Filled with the data from `addr`.
    fn read_bar(&mut self, addr: u64, data: &mut [u8]);
    /// Writes to a BAR region mapped in to the device.
    /// * `addr` - The guest address inside the BAR.
    /// * `data` - The data to write.
    fn write_bar(&mut self, addr: u64, data: &[u8]);
}

impl<T: PciDevice> BusDevice for T {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        self.read_bar(offset, data)
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        self.write_bar(offset, data)
    }

    fn config_register_write(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        if offset as usize + data.len() > 4 {
            return;
        }

        let regs = self.config_registers_mut();

        match data.len() {
            1 => regs.write_byte(reg_idx * 4 + offset as usize, data[0]),
            2 => regs.write_word(
                reg_idx * 4 + offset as usize,
                (data[0] as u16) | (data[1] as u16) << 8,
            ),
            4 => regs.write_reg(reg_idx, LittleEndian::read_u32(data)),
            _ => (),
        }
    }

    fn config_register_read(&self, reg_idx: usize) -> u32 {
        self.config_registers().read_reg(reg_idx)
    }
}

impl<T: PciDevice + ?Sized> PciDevice for Box<T> {
    fn keep_fds(&self) -> Vec<RawFd> {
        (**self).keep_fds()
    }
    fn assign_irq(&mut self, irq_evt: EventFd, irq_num: u32, irq_pin: PciInterruptPin) {
     (**self).assign_irq(irq_evt, irq_num, irq_pin)
    }
    /// Gives the device guest memory if it is needed.
    fn set_guest_memory(&mut self, mem: GuestMemory) {
        (**self).set_guest_memory(mem)
    }
    /// Allocates the needed IO BAR space using the `allocate` function which takes a size and
    /// returns an address. Returns a Vec of (address, length) tuples.
    fn allocate_io_bars(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> Result<Vec<(u64, u64)>> {
        (**self).allocate_io_bars(resources)
    }
    /// Gets a list of ioeventfds that should be registered with the running VM. The list is
    /// returned as a Vec of (eventfd, addr) tuples.
    fn ioeventfds(&self) -> Vec<(&EventFd, u64)> {
        (**self).ioeventfds()
    }
    /// Gets the configuration registers of the Pci Device.
    fn config_registers(&self) -> &PciConfiguration {
        (**self).config_registers()
    }
    /// Gets the configuration registers of the Pci Device for modification.
    fn config_registers_mut(&mut self) -> &mut PciConfiguration {
        (**self).config_registers_mut()
    }
    /// Reads from a BAR region mapped in to the device.
    /// * `addr` - The guest address inside the BAR.
    /// * `data` - Filled with the data from `addr`.
    fn read_bar(&mut self, addr: u64, data: &mut [u8]) {
        (**self).read_bar(addr, data)
    }
    /// Writes to a BAR region mapped in to the device.
    /// * `addr` - The guest address inside the BAR.
    /// * `data` - The data to write.
    fn write_bar(&mut self, addr: u64, data: &[u8]) {
        (**self).write_bar(addr, data)
    }
}
