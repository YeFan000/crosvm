// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Handles IPC for controlling the main VM process.
//!
//! The VM Control IPC protocol is synchronous, meaning that each `VmRequest` sent over a connection
//! will receive a `VmResponse` for that request next time data is received over that connection.
//!
//! The wire message format is a little-endian C-struct of fixed size, along with a file descriptor
//! if the request type expects one.

extern crate byteorder;
extern crate data_model;
extern crate kvm;
extern crate libc;
extern crate sys_util;
extern crate resources;

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::result;

use libc::{ERANGE, EINVAL, ENODEV};

use byteorder::{LittleEndian, WriteBytesExt};
use data_model::{DataInit, Le32, Le64, VolatileMemory};
use sys_util::{EventFd, Result, Error as SysError, MmapError, MemoryMapping, ScmSocket,
               GuestAddress};
use resources::{GpuMemoryDesc, GpuMemoryPlaneDesc, SystemAllocator};
use kvm::{IoeventAddress, Vm};

#[derive(Debug, PartialEq)]
/// An error during a request or response transaction.
pub enum VmControlError {
    /// Error while sending a request or response.
    Send(SysError),
    /// Error while receiving a request or response.
    Recv(SysError),
    /// The type of a received request or response is unknown.
    InvalidType,
    /// There was not the expected amount of data when receiving a request or response. The inner
    /// value is how much data was read.
    BadSize(usize),
    /// There was no associated file descriptor received for a request that expected it.
    ExpectFd,
}

pub type VmControlResult<T> = result::Result<T, VmControlError>;

/// A file descriptor either borrowed or owned by this.
pub enum MaybeOwnedFd {
    /// Owned by this enum variant, and will be destructed automatically if not moved out.
    Owned(File),
    /// A file descriptor borrwed by this enum.
    Borrowed(RawFd),
}

impl AsRawFd for MaybeOwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            &MaybeOwnedFd::Owned(ref f) => f.as_raw_fd(),
            &MaybeOwnedFd::Borrowed(fd) => fd,
        }
    }
}

/// A request to the main process to perform some operation on the VM.
///
/// Unless otherwise noted, each request should expect a `VmResponse::Ok` to be received on success.
pub enum VmRequest {
    /// Try to grow or shrink the VM's balloon.
    BalloonAdjust(i32),
    /// Break the VM's run loop and exit.
    Exit,
    /// Register the given ioevent address along with given datamatch to trigger the `EventFd`.
    RegisterIoevent(EventFd, IoeventAddress, u32),
    /// Register the given IRQ number to be triggered when the `EventFd` is triggered.
    RegisterIrqfd(EventFd, u32),
    /// Register shared memory represented by the given fd into guest address space. The response
    /// variant is `VmResponse::RegisterMemory`.
    RegisterMemory(MaybeOwnedFd, usize),
    /// Unregister the given memory slot that was previously registereed with `RegisterMemory`.
    UnregisterMemory(u32),
    /// Allocate GPU buffer of a given size/format and register the memory into guest address space.
    /// The response variant is `VmResponse::AllocateAndRegisterGpuMemory`
    AllocateAndRegisterGpuMemory { width: u32, height: u32, format: u32 },
}

const VM_REQUEST_TYPE_EXIT: u32 = 1;
const VM_REQUEST_TYPE_REGISTER_MEMORY: u32 = 2;
const VM_REQUEST_TYPE_UNREGISTER_MEMORY: u32 = 3;
const VM_REQUEST_TYPE_BALLOON_ADJUST: u32 = 4;
const VM_REQUEST_TYPE_ALLOCATE_AND_REGISTER_GPU_MEMORY: u32 = 5;
const VM_REQUEST_SIZE: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VmRequestStruct {
    type_: Le32,
    slot: Le32,
    size: Le64,
    num_pages: Le32,
    width: Le32,
    height: Le32,
    format: Le32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for VmRequestStruct {}

fn register_memory(vm: &mut Vm, allocator: &mut SystemAllocator,
                   fd: &AsRawFd, size: usize) -> Result<(u64, u32)> {
    let mmap = match MemoryMapping::from_fd(fd, size) {
        Ok(v) => v,
        Err(MmapError::SystemCallFailed(e)) => return Err(e),
        _ => return Err(SysError::new(EINVAL)),
    };
    let addr = match allocator.allocate_device_addresses(size as u64) {
        Some(a) => a,
        None => return Err(SysError::new(EINVAL)),
    };
    let slot =
        match vm.add_device_memory(GuestAddress(addr), mmap, false, false) {
            Ok(v) => v,
            Err(e) => return Err(e),
        };

    Ok((addr >> 12, slot))
}

impl VmRequest {
    /// Receive a `VmRequest` from the given socket.
    ///
    /// A `VmResponse` should be sent out over the given socket before another request is received.
    pub fn recv(s: &UnixDatagram) -> VmControlResult<VmRequest> {
        assert_eq!(VM_REQUEST_SIZE, std::mem::size_of::<VmRequestStruct>());
        let mut buf = [0; VM_REQUEST_SIZE];
        let (read, file) = s.recv_with_fd(&mut buf)
            .map_err(|e| VmControlError::Recv(e))?;
        if read != VM_REQUEST_SIZE {
            return Err(VmControlError::BadSize(read));
        }
        // The unwrap() will never fail because it's referencing a buf statically sized to be large
        // enough for a VmRequestStruct.
        let req: VmRequestStruct = buf.as_mut().get_ref(0).unwrap().load();

        match req.type_.into() {
            VM_REQUEST_TYPE_EXIT => Ok(VmRequest::Exit),
            VM_REQUEST_TYPE_REGISTER_MEMORY => {
                let fd = file.ok_or(VmControlError::ExpectFd)?;
                Ok(VmRequest::RegisterMemory(MaybeOwnedFd::Owned(fd),
                                             req.size.to_native() as usize))
            }
            VM_REQUEST_TYPE_UNREGISTER_MEMORY => Ok(VmRequest::UnregisterMemory(req.slot.into())),
            VM_REQUEST_TYPE_BALLOON_ADJUST => {
                Ok(VmRequest::BalloonAdjust(req.num_pages.to_native() as i32))
            },
            VM_REQUEST_TYPE_ALLOCATE_AND_REGISTER_GPU_MEMORY => {
                Ok(VmRequest::AllocateAndRegisterGpuMemory { width: req.width.to_native(),
                                                             height: req.height.to_native(),
                                                             format: req.format.to_native()
                    })
            },
            _ => Err(VmControlError::InvalidType),
        }
    }

    /// Send a `VmRequest` over the given socket.
    ///
    /// After this request is a sent, a `VmResponse` should be received before sending another
    /// request.
    pub fn send(&self, s: &UnixDatagram) -> VmControlResult<()> {
        assert_eq!(VM_REQUEST_SIZE, std::mem::size_of::<VmRequestStruct>());
        let mut req = VmRequestStruct::default();
        let mut fd_buf = [0; 1];
        let mut fd_len = 0;
        match self {
            &VmRequest::Exit => req.type_ = Le32::from(VM_REQUEST_TYPE_EXIT),
            &VmRequest::RegisterMemory(ref fd, size) => {
                req.type_ = Le32::from(VM_REQUEST_TYPE_REGISTER_MEMORY);
                req.size = Le64::from(size as u64);
                fd_buf[0] = fd.as_raw_fd();
                fd_len = 1;
            }
            &VmRequest::UnregisterMemory(slot) => {
                req.type_ = Le32::from(VM_REQUEST_TYPE_UNREGISTER_MEMORY);
                req.slot = Le32::from(slot);
            }
            &VmRequest::BalloonAdjust(pages) => {
                req.type_ = Le32::from(VM_REQUEST_TYPE_BALLOON_ADJUST);
                req.num_pages = Le32::from(pages as u32);
            },
            &VmRequest::AllocateAndRegisterGpuMemory { width, height, format } => {
                req.type_ = Le32::from(VM_REQUEST_TYPE_ALLOCATE_AND_REGISTER_GPU_MEMORY);
                req.width = Le32::from(width as u32);
                req.height = Le32::from(height as u32);
                req.format = Le32::from(format as u32);
            },
            _ => return Err(VmControlError::InvalidType),
        }
        let mut buf = [0; VM_REQUEST_SIZE];
        buf.as_mut().get_ref(0).unwrap().store(req);
        s.send_with_fds(buf.as_ref(), &fd_buf[..fd_len])
            .map_err(|e| VmControlError::Send(e))?;
        Ok(())
    }

    /// Executes this request on the given Vm and other mutable state.
    ///
    /// # Arguments
    /// * `vm` - The `Vm` to perform the request on.
    /// * `allocator` - Used to allocate addresses.
    /// * `running` - Out argument that is set to false if the request was to stop running the VM.
    ///
    /// This does not return a result, instead encapsulating the success or failure in a
    /// `VmResponse` with the intended purpose of sending the response back over the  socket that
    /// received this `VmRequest`.
    pub fn execute(&self, vm: &mut Vm, sys_allocator: &mut SystemAllocator, running: &mut bool,
                   balloon_host_socket: &UnixDatagram) -> VmResponse {
        *running = true;
        match self {
            &VmRequest::Exit => {
                *running = false;
                VmResponse::Ok
            }
            &VmRequest::RegisterIoevent(ref evt, addr, datamatch) => {
                match vm.register_ioevent(evt, addr, datamatch) {
                    Ok(_) => VmResponse::Ok,
                    Err(e) => VmResponse::Err(e),
                }
            }
            &VmRequest::RegisterIrqfd(ref evt, irq) => {
                match vm.register_irqfd(evt, irq) {
                    Ok(_) => VmResponse::Ok,
                    Err(e) => return VmResponse::Err(e),
                }
            }
            &VmRequest::RegisterMemory(ref fd, size) => {
                match register_memory(vm, sys_allocator, fd, size) {
                    Ok((pfn, slot)) => VmResponse::RegisterMemory { pfn, slot },
                    Err(e) => VmResponse::Err(e),
                }
            }
            &VmRequest::UnregisterMemory(slot) => {
                match vm.remove_device_memory(slot) {
                    Ok(_) => VmResponse::Ok,
                    Err(e) => VmResponse::Err(e),
                }
            }
            &VmRequest::BalloonAdjust(num_pages) => {
                let mut buf = [0u8; 4];
                // write_i32 can't fail as the buffer is 4 bytes long.
                (&mut buf[0..]).write_i32::<LittleEndian>(num_pages).unwrap();
                match balloon_host_socket.send(&buf) {
                    Ok(_) => VmResponse::Ok,
                    Err(_) => VmResponse::Err(SysError::last()),
                }
            }
            &VmRequest::AllocateAndRegisterGpuMemory {width, height, format} => {
                let (mut fd, desc) = match sys_allocator.gpu_memory_allocator() {
                    Some(gpu_allocator) => {
                        match gpu_allocator.allocate(width, height, format) {
                            Ok(v) => v,
                            Err(e) => return VmResponse::Err(e),
                        }
                    }
                    None => return VmResponse::Err(SysError::new(ENODEV)),
                };
                // Determine size of buffer using 0 byte seek from end. This is preferred over
                // `stride * height` as it's not limited to packed pixel formats.
                let size = match fd.seek(SeekFrom::End(0)) {
                    Ok(v) => v,
                    Err(e) => return VmResponse::Err(SysError::from(e)),
                };
                match register_memory(vm, sys_allocator, &fd, size as usize) {
                    Ok((pfn, slot)) => VmResponse::AllocateAndRegisterGpuMemory {
                        fd: MaybeOwnedFd::Owned(fd),
                        pfn,
                        slot,
                        desc },
                    Err(e) => VmResponse::Err(e),
                }
            }
        }
    }
}

/// Indication of success or failure of a `VmRequest`.
///
/// Success is usually indicated `VmResponse::Ok` unless there is data associated with the response.
pub enum VmResponse {
    /// Indicates the request was executed successfully.
    Ok,
    /// Indicates the request encountered some error during execution.
    Err(SysError),
    /// The request to register memory into guest address space was successfully done at page frame
    /// number `pfn` and memory slot number `slot`.
    RegisterMemory { pfn: u64, slot: u32 },
    /// The request to allocate and register GPU memory into guest address space was successfully
    /// done at page frame number `pfn` and memory slot number `slot` for buffer with `desc`.
    AllocateAndRegisterGpuMemory { fd: MaybeOwnedFd, pfn: u64, slot: u32, desc: GpuMemoryDesc },
}

const VM_RESPONSE_TYPE_OK: u32 = 1;
const VM_RESPONSE_TYPE_ERR: u32 = 2;
const VM_RESPONSE_TYPE_REGISTER_MEMORY: u32 = 3;
const VM_RESPONSE_TYPE_ALLOCATE_AND_REGISTER_GPU_MEMORY: u32 = 4;
const VM_RESPONSE_SIZE: usize = 48;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VmResponseStruct {
    type_: Le32,
    errno: Le32,
    pfn: Le64,
    slot: Le32,
    stride0: Le32,
    stride1: Le32,
    stride2: Le32,
    offset0: Le32,
    offset1: Le32,
    offset2: Le32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for VmResponseStruct {}

impl VmResponse {
    /// Receive a `VmResponse` from the given socket.
    ///
    /// This should be called after the sending a `VmRequest` before sending another request.
    pub fn recv(s: &UnixDatagram) -> VmControlResult<VmResponse> {
        let mut buf = [0; VM_RESPONSE_SIZE];
        let (read, file) = s.recv_with_fd(&mut buf)
            .map_err(|e| VmControlError::Recv(e))?;
        if read != VM_RESPONSE_SIZE {
            return Err(VmControlError::BadSize(read));
        }
        let resp: VmResponseStruct = buf.as_mut().get_ref(0).unwrap().load();

        match resp.type_.into() {
            VM_RESPONSE_TYPE_OK => Ok(VmResponse::Ok),
            VM_RESPONSE_TYPE_ERR => {
                Ok(VmResponse::Err(SysError::new(resp.errno.to_native() as i32)))
            }
            VM_RESPONSE_TYPE_REGISTER_MEMORY => {
                Ok(VmResponse::RegisterMemory {
                       pfn: resp.pfn.into(),
                       slot: resp.slot.into(),
                   })
            }
            VM_RESPONSE_TYPE_ALLOCATE_AND_REGISTER_GPU_MEMORY => {
                let fd = file.ok_or(VmControlError::ExpectFd)?;
                Ok(VmResponse::AllocateAndRegisterGpuMemory {
                       fd: MaybeOwnedFd::Owned(fd),
                       pfn: resp.pfn.into(),
                       slot: resp.slot.into(),
                       desc: GpuMemoryDesc {
                           planes: [ GpuMemoryPlaneDesc { stride: resp.stride0.into(),
                                                          offset: resp.offset0.into() },
                                     GpuMemoryPlaneDesc { stride: resp.stride1.into(),
                                                          offset: resp.offset1.into() },
                                     GpuMemoryPlaneDesc { stride: resp.stride2.into(),
                                                          offset: resp.offset2.into() } ],
                       },
                  })
            }
            _ => Err(VmControlError::InvalidType),
        }
    }

    /// Send a `VmResponse` over the given socket.
    ///
    /// This must be called after receiving a `VmRequest` to indicate the outcome of that request's
    /// execution.
    pub fn send(&self, s: &UnixDatagram) -> VmControlResult<()> {
        let mut resp = VmResponseStruct::default();
        let mut fd_buf = [0; 1];
        let mut fd_len = 0;
        match self {
            &VmResponse::Ok => resp.type_ = Le32::from(VM_RESPONSE_TYPE_OK),
            &VmResponse::Err(e) => {
                resp.type_ = Le32::from(VM_RESPONSE_TYPE_ERR);
                resp.errno = Le32::from(e.errno().checked_abs().unwrap_or(ERANGE) as u32);
            }
            &VmResponse::RegisterMemory { pfn, slot } => {
                resp.type_ = Le32::from(VM_RESPONSE_TYPE_REGISTER_MEMORY);
                resp.pfn = Le64::from(pfn);
                resp.slot = Le32::from(slot);
            }
            &VmResponse::AllocateAndRegisterGpuMemory {ref fd, pfn, slot, desc } => {
                fd_buf[0] = fd.as_raw_fd();
                fd_len = 1;
                resp.type_ = Le32::from(VM_RESPONSE_TYPE_ALLOCATE_AND_REGISTER_GPU_MEMORY);
                resp.pfn = Le64::from(pfn);
                resp.slot = Le32::from(slot);
                resp.stride0 = Le32::from(desc.planes[0].stride);
                resp.stride1 = Le32::from(desc.planes[1].stride);
                resp.stride2 = Le32::from(desc.planes[2].stride);
                resp.offset0 = Le32::from(desc.planes[0].offset);
                resp.offset1 = Le32::from(desc.planes[1].offset);
                resp.offset2 = Le32::from(desc.planes[2].offset);
            }
        }
        let mut buf = [0; VM_RESPONSE_SIZE];
        buf.as_mut().get_ref(0).unwrap().store(resp);
        s.send_with_fds(buf.as_ref(), &fd_buf[..fd_len])
            .map_err(|e| VmControlError::Send(e))?;
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    use std::net::Shutdown;

    use sys_util::kernel_has_memfd;
    use sys_util::SharedMemory;

    #[test]
    fn request_exit() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        VmRequest::Exit.send(&s1).unwrap();
        match VmRequest::recv(&s2).unwrap() {
            VmRequest::Exit => {}
            _ => panic!("recv wrong request variant"),
        }
    }

    #[test]
    fn request_register_memory() {
        if !kernel_has_memfd() { return; }
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        let shm_size: usize = 4096;
        let mut shm = SharedMemory::new(None).unwrap();
        shm.set_size(shm_size as u64).unwrap();
        VmRequest::RegisterMemory(MaybeOwnedFd::Borrowed(shm.as_raw_fd()), shm_size)
            .send(&s1)
            .unwrap();
        match VmRequest::recv(&s2).unwrap() {
            VmRequest::RegisterMemory(MaybeOwnedFd::Owned(fd), size) => {
                assert!(fd.as_raw_fd() >= 0);
                assert_eq!(size, shm_size);
            }
            _ => panic!("recv wrong request variant"),
        }
    }

    #[test]
    fn request_unregister_memory() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        VmRequest::UnregisterMemory(77).send(&s1).unwrap();
        match VmRequest::recv(&s2).unwrap() {
            VmRequest::UnregisterMemory(slot) => assert_eq!(slot, 77),
            _ => panic!("recv wrong request variant"),
        }
    }

    #[test]
    fn request_expect_fd() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        let mut bad_request = [0; VM_REQUEST_SIZE];
        bad_request[0] = VM_REQUEST_TYPE_REGISTER_MEMORY as u8;
        s2.send_with_fds(bad_request.as_ref(), &[]).unwrap();
        match VmRequest::recv(&s1) {
            Err(VmControlError::ExpectFd) => {}
            _ => panic!("recv wrong error variant"),
        }
    }

    #[test]
    fn request_no_data() {
        let (s1, _) = UnixDatagram::pair().expect("failed to create socket pair");
        s1.shutdown(Shutdown::Both).unwrap();
        match VmRequest::recv(&s1) {
            Err(VmControlError::BadSize(s)) => assert_eq!(s, 0),
            _ => panic!("recv wrong error variant"),
        }
    }

    #[test]
    fn request_bad_size() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        s2.send_with_fds([12; 7].as_ref(), &[]).unwrap();
        match VmRequest::recv(&s1) {
            Err(VmControlError::BadSize(_)) => {}
            _ => panic!("recv wrong error variant"),
        }
    }

    #[test]
    fn request_invalid_type() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        s2.send_with_fds([12; VM_REQUEST_SIZE].as_ref(), &[])
            .unwrap();
        match VmRequest::recv(&s1) {
            Err(VmControlError::InvalidType) => {}
            _ => panic!("recv wrong error variant"),
        }
    }

    #[test]
    fn request_allocate_and_register_gpu_memory() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        let gpu_width: u32 = 32;
        let gpu_height: u32 = 32;
        let gpu_format: u32 = 0x34325258;
        let r = VmRequest::AllocateAndRegisterGpuMemory {
            width: gpu_width,
            height: gpu_height,
            format: gpu_format,
        };
        r.send(&s1).unwrap();
        match VmRequest::recv(&s2).unwrap() {
            VmRequest::AllocateAndRegisterGpuMemory {
                width,
                height,
                format,
            } => {
                assert_eq!(width, gpu_width);
                assert_eq!(height, gpu_width);
                assert_eq!(format, gpu_format);
            }
            _ => panic!("recv wrong request variant"),
        }
    }

    #[test]
    fn resp_ok() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        VmResponse::Ok.send(&s1).unwrap();
        match VmResponse::recv(&s2).unwrap() {
            VmResponse::Ok => {}
            _ => panic!("recv wrong response variant"),
        }
    }

    #[test]
    fn resp_err() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        let r1 = VmResponse::Err(SysError::new(libc::EDESTADDRREQ));
        r1.send(&s1).unwrap();
        match VmResponse::recv(&s2).unwrap() {
            VmResponse::Err(e) => {
                assert_eq!(e, SysError::new(libc::EDESTADDRREQ));
            }
            _ => panic!("recv wrong response variant"),
        }
    }

    #[test]
    fn resp_memory() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        let memory_pfn = 55;
        let memory_slot = 66;
        let r1 = VmResponse::RegisterMemory {
            pfn: memory_pfn,
            slot: memory_slot,
        };
        r1.send(&s1).unwrap();
        match VmResponse::recv(&s2).unwrap() {
            VmResponse::RegisterMemory { pfn, slot } => {
                assert_eq!(pfn, memory_pfn);
                assert_eq!(slot, memory_slot);
            }
            _ => panic!("recv wrong response variant"),
        }
    }

    #[test]
    fn resp_no_data() {
        let (s1, _) = UnixDatagram::pair().expect("failed to create socket pair");
        s1.shutdown(Shutdown::Both).unwrap();
        match VmResponse::recv(&s1) {
            Err(e) => {
                assert_eq!(e, VmControlError::BadSize(0));
            }
            _ => panic!("recv wrong response"),
        }
    }

    #[test]
    fn resp_bad_size() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        s2.send_with_fds([12; 7].as_ref(), &[]).unwrap();
        match VmResponse::recv(&s1) {
            Err(e) => {
                assert_eq!(e, VmControlError::BadSize(7));
            }
            _ => panic!("recv wrong response"),
        }
    }

    #[test]
    fn resp_invalid_type() {
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        s2.send_with_fds([12; VM_RESPONSE_SIZE].as_ref(), &[])
            .unwrap();
        match VmResponse::recv(&s1) {
            Err(e) => {
                assert_eq!(e, VmControlError::InvalidType);
            }
            _ => panic!("recv wrong response"),
        }
    }

    #[test]
    fn resp_allocate_and_register_gpu_memory() {
        if !kernel_has_memfd() { return; }
        let (s1, s2) = UnixDatagram::pair().expect("failed to create socket pair");
        let shm_size: usize = 4096;
        let mut shm = SharedMemory::new(None).unwrap();
        shm.set_size(shm_size as u64).unwrap();
        let memory_pfn = 55;
        let memory_slot = 66;
        let memory_planes = [
            GpuMemoryPlaneDesc { stride: 32, offset: 84 },
            GpuMemoryPlaneDesc { stride: 48, offset: 96 },
            GpuMemoryPlaneDesc { stride: 64, offset: 112 }
        ];
        let r1 = VmResponse::AllocateAndRegisterGpuMemory {
            fd: MaybeOwnedFd::Borrowed(shm.as_raw_fd()),
            pfn: memory_pfn,
            slot: memory_slot,
            desc: GpuMemoryDesc { planes: memory_planes },
        };
        r1.send(&s1).unwrap();
        match VmResponse::recv(&s2).unwrap() {
            VmResponse::AllocateAndRegisterGpuMemory {
                fd,
                pfn,
                slot,
                desc,
            } => {
                assert!(fd.as_raw_fd() >= 0);
                assert_eq!(pfn, memory_pfn);
                assert_eq!(slot, memory_slot);
                assert_eq!(desc.planes, memory_planes);
            }
            _ => panic!("recv wrong response variant"),
        }
    }
}
