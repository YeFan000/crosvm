// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use address_allocator::AddressAllocator;
use sys_util::pagesize;

/// Manages allocating system resources such as address space and interrupt numbers.
///
/// # Example - Use the `SystemAddress` builder.
///
/// ```
/// # use sys_util::AddressRanges;
///   if let Some(mut a) = AddressRanges::new()
///           .add_io_addresses(0x1000, 0x10000)
///           .add_device_addresses(0x10000000, 0x10000000)
///           .add_mmio_addresses(0x30000000, 0x10000)
///           .create_allocator(5) {
///       assert_eq!(a.allocate_irq(), Some(5));
///       assert_eq!(a.allocate_irq(), Some(6));
///       assert_eq!(a.allocate_device_addresses(0x100), Some(0x10000000));
///   }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct SystemAllocator {
    io_address_space: AddressAllocator,
    device_address_space: AddressAllocator,
    mmio_address_space: AddressAllocator,
    next_irq: u32,
}

impl SystemAllocator {
    /// Creates a new `SystemAllocator` for managing addresses and irq numvers.
    /// Can return `None` if `base` + `size` overflows a u64 or if alignment isn't a power
    /// of two.
    ///
    /// * `io_base` - The starting address of IO memory.
    /// * `io_size` - The size of IO memory.
    /// * `dev_base` - The starting address of device memory.
    /// * `dev_size` - The size of device memory.
    /// * `mmio_base` - The starting address of MMIO space.
    /// * `mmio_size` - The size of MMIO space.
    /// * `first_irq` - The first irq number to give out.
    fn new(io_base: u64, io_size: u64,
               dev_base: u64, dev_size: u64,
               mmio_base: u64, mmio_size: u64,
               first_irq: u32)
            -> Option<Self> {
        let page_size = pagesize() as u64;
        Some(SystemAllocator {
            io_address_space: AddressAllocator::new(io_base, io_size, Some(0x400))?,
            device_address_space: AddressAllocator::new(dev_base, dev_size, Some(page_size))?,
            mmio_address_space: AddressAllocator::new(mmio_base, mmio_size, Some(page_size))?,
            next_irq: first_irq,
        })
    }

    /// Reserves the next available system irq number.
    pub fn allocate_irq(&mut self) -> Option<u32> {
        if let Some(irq_num) = self.next_irq.checked_add(1) {
            self.next_irq = irq_num;
            Some(irq_num - 1)
        } else {
            None
        }
    }

    /// Reserves a section of `size` bytes of IO address space.
    pub fn allocate_io_addresses(&mut self, size: u64) -> Option<u64> {
        self.io_address_space.allocate(size)
    }

    /// Reserves a section of `size` bytes of device address space.
    pub fn allocate_device_addresses(&mut self, size: u64) -> Option<u64> {
        self.device_address_space.allocate(size)
    }

    /// Reserves a section of `size` bytes of device address space.
    pub fn allocate_mmio_addresses(&mut self, size: u64) -> Option<u64> {
        self.mmio_address_space.allocate(size)
    }
}

/// Used to build a system address map for use in creating a `SystemAllocator`.
pub struct AddressRanges {
    io_base: Option<u64>,
    io_size: Option<u64>,
    mmio_base: Option<u64>,
    mmio_size: Option<u64>,
    device_base: Option<u64>,
    device_size: Option<u64>,
}

impl AddressRanges {
    pub fn new() -> Self {
        AddressRanges {
            io_base: None,
            io_size: None,
            mmio_base: None,
            mmio_size: None,
            device_base: None,
            device_size: None,
        }
    }

    pub fn add_io_addresses(mut self, base: u64, size: u64) -> Self {
        self.io_base = Some(base);
        self.io_size = Some(size);
        self
    }

    pub fn add_mmio_addresses(mut self, base: u64, size: u64) -> Self {
        self.mmio_base = Some(base);
        self.mmio_size = Some(size);
        self
    }

    pub fn add_device_addresses(mut self, base: u64, size: u64) -> Self {
        self.device_base = Some(base);
        self.device_size = Some(size);
        self
    }

    pub fn create_allocator(&self, first_irq: u32) -> Option<SystemAllocator> {
        SystemAllocator::new(self.io_base?, self.io_size?,
                             self.device_base?, self.device_size?,
                             self.mmio_base?, self.mmio_size?,
                             first_irq)
    }
}