// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cmp;
use std::io::{self, Seek, SeekFrom, Read, Write};
use std::mem::{size_of, size_of_val};
use std::os::unix::io::{AsRawFd, RawFd};
use std::result;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;
use std::u32;

use sys_util::Error as SysError;
use sys_util::Result as SysResult;
use sys_util::{
    EventFd, GuestAddress, GuestMemory, GuestMemoryError, PollContext, PollToken, TimerFd,
    WriteZeroes,
};

use data_model::{DataInit, Le16, Le32, Le64};

use super::{VirtioDevice, Queue, DescriptorChain, INTERRUPT_STATUS_USED_RING, TYPE_BLOCK};

const QUEUE_SIZE: u16 = 256;
const QUEUE_SIZES: &'static [u16] = &[QUEUE_SIZE];
const SECTOR_SHIFT: u8 = 9;
const SECTOR_SIZE: u64 = 0x01 << SECTOR_SHIFT;
const MAX_DISCARD_SECTORS: u32 = u32::MAX;
const MAX_WRITE_ZEROES_SECTORS: u32 = u32::MAX;
// Hard-coded to 64 KiB (in 512-byte sectors) for now,
// but this should probably be based on cluster size for qcow.
const DISCARD_SECTOR_ALIGNMENT: u32 = 128;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_DISCARD: u32 = 11;
const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const VIRTIO_BLK_F_RO: u32 = 5;
const VIRTIO_BLK_F_FLUSH: u32 = 9;
const VIRTIO_BLK_F_DISCARD: u32 = 13;
const VIRTIO_BLK_F_WRITE_ZEROES: u32 = 14;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_blk_geometry {
    cylinders: Le16,
    heads: u8,
    sectors: u8,
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_blk_geometry {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_blk_topology {
    physical_block_exp: u8,
    alignment_offset: u8,
    min_io_size: Le16,
    opt_io_size: Le32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_blk_topology {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_blk_config {
    capacity: Le64,
    size_max: Le32,
    seg_max: Le32,
    geometry: virtio_blk_geometry,
    blk_size: Le32,
    topology: virtio_blk_topology,
    writeback: u8,
    unused0: [u8; 3],
    max_discard_sectors: Le32,
    max_discard_seg: Le32,
    discard_sector_alignment: Le32,
    max_write_zeroes_sectors: Le32,
    max_write_zeroes_seg: Le32,
    write_zeroes_may_unmap: u8,
    unused1: [u8; 3],
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_blk_config {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_blk_discard_write_zeroes {
    sector: Le64,
    num_sectors: Le32,
    flags: Le32,
}

const VIRTIO_BLK_DISCARD_WRITE_ZEROES_FLAG_UNMAP: u32 = 1 << 0;

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_blk_discard_write_zeroes {}

pub trait DiskFile: Read + Seek + Write + WriteZeroes {}
impl<D: Read + Seek + Write + WriteZeroes> DiskFile for D {}

#[derive(PartialEq)]
enum RequestType {
    In,
    Out,
    Flush,
    Discard,
    WriteZeroes,
    Unsupported(u32),
}

#[derive(Debug)]
enum ParseError {
    /// Guest gave us bad memory addresses
    GuestMemory(GuestMemoryError),
    /// Guest gave us offsets that would have overflowed a usize.
    CheckedOffset(GuestAddress, u64),
    /// Guest gave us a write only descriptor that protocol says to read from.
    UnexpectedWriteOnlyDescriptor,
    /// Guest gave us a read only descriptor that protocol says to write to.
    UnexpectedReadOnlyDescriptor,
    /// Guest gave us too few descriptors in a descriptor chain.
    DescriptorChainTooShort,
    /// Guest gave us a descriptor that was too short to use.
    DescriptorLengthTooSmall,
}

fn request_type(mem: &GuestMemory,
                desc_addr: GuestAddress)
                -> result::Result<RequestType, ParseError> {
    let type_ = mem.read_obj_from_addr(desc_addr)
        .map_err(ParseError::GuestMemory)?;
    match type_ {
        VIRTIO_BLK_T_IN => Ok(RequestType::In),
        VIRTIO_BLK_T_OUT => Ok(RequestType::Out),
        VIRTIO_BLK_T_FLUSH => Ok(RequestType::Flush),
        VIRTIO_BLK_T_DISCARD => Ok(RequestType::Discard),
        VIRTIO_BLK_T_WRITE_ZEROES => Ok(RequestType::WriteZeroes),
        t => Ok(RequestType::Unsupported(t)),
    }
}

fn sector(mem: &GuestMemory, desc_addr: GuestAddress) -> result::Result<u64, ParseError> {
    const SECTOR_OFFSET: u64 = 8;
    let addr = match mem.checked_offset(desc_addr, SECTOR_OFFSET) {
        Some(v) => v,
        None => return Err(ParseError::CheckedOffset(desc_addr, SECTOR_OFFSET)),
    };

    mem.read_obj_from_addr(addr)
        .map_err(ParseError::GuestMemory)
}

fn discard_write_zeroes_segment(
    mem: &GuestMemory,
    seg_addr: GuestAddress,
) -> result::Result<virtio_blk_discard_write_zeroes, ParseError> {
    mem.read_obj_from_addr(seg_addr)
        .map_err(ParseError::GuestMemory)
}

#[derive(Debug)]
enum ExecuteError {
    /// Error arming the flush timer.
    ArmingTimer(SysError),
    Flush(io::Error),
    Read {
        addr: GuestAddress,
        length: u32,
        sector: u64,
        guestmemerr: GuestMemoryError
    },
    Seek {
        ioerr: io::Error,
        sector: u64
    },
    TimerFd(SysError),
    Write {
        addr: GuestAddress,
        length: u32,
        sector: u64,
        guestmemerr: GuestMemoryError
    },
    DiscardWriteZeroes {
        ioerr: Option<io::Error>,
        sector: u64,
        num_sectors: u32,
        flags: u32,
    },
    Unsupported(u32),
}

impl ExecuteError {
    fn status(&self) -> u8 {
        match self {
            &ExecuteError::ArmingTimer(_) => VIRTIO_BLK_S_IOERR,
            &ExecuteError::Flush(_) => VIRTIO_BLK_S_IOERR,
            &ExecuteError::Read{ .. } => VIRTIO_BLK_S_IOERR,
            &ExecuteError::Seek{ .. } => VIRTIO_BLK_S_IOERR,
            &ExecuteError::TimerFd(_) => VIRTIO_BLK_S_IOERR,
            &ExecuteError::Write{ .. } => VIRTIO_BLK_S_IOERR,
            &ExecuteError::DiscardWriteZeroes{ .. } => VIRTIO_BLK_S_IOERR,
            &ExecuteError::Unsupported(_) => VIRTIO_BLK_S_UNSUPP,
        }
    }
}

struct Request {
    request_type: RequestType,
    sector: u64,
    data_addr: GuestAddress,
    data_len: u32,
    status_addr: GuestAddress,
    discard_write_zeroes_seg: Option<virtio_blk_discard_write_zeroes>,
}

impl Request {
    fn parse(avail_desc: &DescriptorChain,
             mem: &GuestMemory)
             -> result::Result<Request, ParseError> {
        // The head contains the request type which MUST be readable.
        if avail_desc.is_write_only() {
            return Err(ParseError::UnexpectedWriteOnlyDescriptor);
        }

        let req_type = request_type(&mem, avail_desc.addr)?;
        if req_type == RequestType::Flush {
            Request::parse_flush(avail_desc, mem)
        } else if req_type == RequestType::Discard || req_type == RequestType::WriteZeroes {
            Request::parse_discard_write_zeroes(avail_desc, mem, req_type)
        } else {
            Request::parse_read_write(avail_desc, mem, req_type)
        }
    }

    fn parse_flush(avail_desc: &DescriptorChain,
                   mem: &GuestMemory)
        -> result::Result<Request, ParseError>
    {
        let sector = sector(&mem, avail_desc.addr)?;
        let status_desc = avail_desc
            .next_descriptor()
            .ok_or(ParseError::DescriptorChainTooShort)?;

        // The status MUST always be writable
        if !status_desc.is_write_only() {
            return Err(ParseError::UnexpectedReadOnlyDescriptor);
        }

        if status_desc.len < 1 {
            return Err(ParseError::DescriptorLengthTooSmall);
        }

        Ok(Request {
               request_type: RequestType::Flush,
               sector: sector,
               data_addr: GuestAddress(0),
               data_len: 0,
               status_addr: status_desc.addr,
               discard_write_zeroes_seg: None,
           })
    }

    fn parse_discard_write_zeroes(
        avail_desc: &DescriptorChain,
        mem: &GuestMemory,
        req_type: RequestType,
    ) -> result::Result<Request, ParseError> {
        let seg_desc = avail_desc
            .next_descriptor()
            .ok_or(ParseError::DescriptorChainTooShort)?;
        let status_desc = seg_desc
            .next_descriptor()
            .ok_or(ParseError::DescriptorChainTooShort)?;

        if seg_desc.is_write_only() {
            return Err(ParseError::UnexpectedWriteOnlyDescriptor);
        }

        // For simplicity, we currently only support a single segment
        // for discard and write zeroes commands.  This allows the
        // request to be represented as a single Request object.
        if seg_desc.len < size_of::<virtio_blk_discard_write_zeroes>() as u32 {
            return Err(ParseError::DescriptorLengthTooSmall);
        }

        let seg = discard_write_zeroes_segment(&mem, seg_desc.addr)?;

        // The status MUST always be writable
        if !status_desc.is_write_only() {
            return Err(ParseError::UnexpectedReadOnlyDescriptor);
        }

        if status_desc.len < 1 {
            return Err(ParseError::DescriptorLengthTooSmall);
        }

        Ok(Request {
            request_type: req_type,
            sector: 0,
            data_addr: GuestAddress(0),
            data_len: 0,
            status_addr: status_desc.addr,
            discard_write_zeroes_seg: Some(seg),
        })
    }

    fn parse_read_write(avail_desc: &DescriptorChain,
                        mem: &GuestMemory,
                        req_type: RequestType)
        -> result::Result<Request, ParseError>
    {
        let sector = sector(&mem, avail_desc.addr)?;
        let data_desc = avail_desc
            .next_descriptor()
            .ok_or(ParseError::DescriptorChainTooShort)?;
        let status_desc = data_desc
            .next_descriptor()
            .ok_or(ParseError::DescriptorChainTooShort)?;

        if data_desc.is_write_only() && req_type == RequestType::Out {
            return Err(ParseError::UnexpectedWriteOnlyDescriptor);
        }

        if !data_desc.is_write_only() && req_type == RequestType::In {
            return Err(ParseError::UnexpectedReadOnlyDescriptor);
        }

        // The status MUST always be writable
        if !status_desc.is_write_only() {
            return Err(ParseError::UnexpectedReadOnlyDescriptor);
        }

        if status_desc.len < 1 {
            return Err(ParseError::DescriptorLengthTooSmall);
        }

        Ok(Request {
               request_type: req_type,
               sector: sector,
               data_addr: data_desc.addr,
               data_len: data_desc.len,
               status_addr: status_desc.addr,
               discard_write_zeroes_seg: None,
           })
    }

    fn execute<T: DiskFile>(
        &self,
        disk: &mut T,
        flush_timer: &mut TimerFd,
        mem: &GuestMemory,
    ) -> result::Result<u32, ExecuteError> {
        // Delay after a write when the file is auto-flushed.
        let flush_delay = Duration::from_secs(60);

        disk.seek(SeekFrom::Start(self.sector << SECTOR_SHIFT))
            .map_err(|e| ExecuteError::Seek{ ioerr: e, sector: self.sector })?;
        match self.request_type {
            RequestType::In => {
                mem.read_to_memory(self.data_addr, disk, self.data_len as usize)
                    .map_err(|e| ExecuteError::Read{ addr: self.data_addr,
                                                     length: self.data_len,
                                                     sector: self.sector,
                                                     guestmemerr: e })?;
                return Ok(self.data_len);
            }
            RequestType::Out => {
                mem.write_from_memory(self.data_addr, disk, self.data_len as usize)
                    .map_err(|e| ExecuteError::Write {
                        addr: self.data_addr,
                        length: self.data_len,
                        sector: self.sector,
                        guestmemerr: e,
                    })?;
                if !flush_timer.is_armed().map_err(ExecuteError::ArmingTimer)? {
                    flush_timer
                        .reset(flush_delay, None)
                        .map_err(ExecuteError::TimerFd)?;
                }
            }
            RequestType::Discard | RequestType::WriteZeroes => {
                if let Some(seg) = self.discard_write_zeroes_seg {
                    let sector = seg.sector.to_native();
                    let num_sectors = seg.num_sectors.to_native();
                    let flags = seg.flags.to_native();

                    let valid_flags = if self.request_type == RequestType::WriteZeroes {
                        VIRTIO_BLK_DISCARD_WRITE_ZEROES_FLAG_UNMAP
                    } else {
                        0
                    };

                    if (flags & !valid_flags) != 0 {
                        return Err(ExecuteError::DiscardWriteZeroes{
                                ioerr: None,
                                sector,
                                num_sectors,
                                flags
                            });
                    }

                    disk.seek(SeekFrom::Start(sector << SECTOR_SHIFT))
                        .map_err(|e| ExecuteError::Seek{ ioerr: e, sector })?;
                    disk.write_zeroes((num_sectors as usize) << SECTOR_SHIFT)
                        .map_err(|e| ExecuteError::DiscardWriteZeroes {
                            ioerr: Some(e),
                            sector,
                            num_sectors,
                            flags
                        })?;
                }
            }
            RequestType::Flush => {
                disk.flush().map_err(ExecuteError::Flush)?;
                flush_timer.clear().map_err(ExecuteError::TimerFd)?;
            }
            RequestType::Unsupported(t) => return Err(ExecuteError::Unsupported(t)),
        };
        Ok(0)
    }
}

struct Worker<T: DiskFile> {
    queues: Vec<Queue>,
    mem: GuestMemory,
    disk_image: T,
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: EventFd,
}

impl<T: DiskFile> Worker<T> {
    fn process_queue(&mut self, queue_index: usize, flush_timer: &mut TimerFd) -> bool {
        let queue = &mut self.queues[queue_index];

        let mut used_desc_heads = [(0, 0); QUEUE_SIZE as usize];
        let mut used_count = 0;
        for avail_desc in queue.iter(&self.mem) {
            let len;
            match Request::parse(&avail_desc, &self.mem) {
                Ok(request) => {
                    let status = match request.execute(&mut self.disk_image, flush_timer, &self.mem)
                    {
                        Ok(l) => {
                            len = l;
                            VIRTIO_BLK_S_OK
                        }
                        Err(e) => {
                            error!("failed executing disk request: {:?}", e);
                            len = 1; // 1 byte for the status
                            e.status()
                        }
                    };
                    // We use unwrap because the request parsing process already checked that the
                    // status_addr was valid.
                    self.mem
                        .write_obj_at_addr(status, request.status_addr)
                        .unwrap();
                }
                Err(e) => {
                    error!("failed processing available descriptor chain: {:?}", e);
                    len = 0;
                }
            }
            used_desc_heads[used_count] = (avail_desc.index, len);
            used_count += 1;
        }

        for &(desc_index, len) in &used_desc_heads[..used_count] {
            queue.add_used(&self.mem, desc_index, len);
        }
        used_count > 0
    }

    fn signal_used_queue(&self) {
        self.interrupt_status
            .fetch_or(INTERRUPT_STATUS_USED_RING as usize, Ordering::SeqCst);
        self.interrupt_evt.write(1).unwrap();
    }

    fn run(&mut self, queue_evt: EventFd, kill_evt: EventFd) {
        #[derive(PollToken)]
        enum Token {
            FlushTimer,
            QueueAvailable,
            Kill,
        }

        let mut flush_timer = match TimerFd::new() {
            Ok(t) => t,
            Err(e) => {
                error!("Failed to create the flush timer: {:?}", e);
                return;
            }
        };

        let poll_ctx: PollContext<Token> =
            match PollContext::new()
                      .and_then(|pc| pc.add(&flush_timer, Token::FlushTimer).and(Ok(pc)))
                      .and_then(|pc| pc.add(&queue_evt, Token::QueueAvailable).and(Ok(pc)))
                      .and_then(|pc| pc.add(&kill_evt, Token::Kill).and(Ok(pc))) {
                Ok(pc) => pc,
                Err(e) => {
                    error!("failed creating PollContext: {:?}", e);
                    return;
                }
            };

        'poll: loop {
            let events = match poll_ctx.wait() {
                Ok(v) => v,
                Err(e) => {
                    error!("failed polling for events: {:?}", e);
                    break;
                }
            };

            let mut needs_interrupt = false;
            for event in events.iter_readable() {
                match event.token() {
                    Token::FlushTimer => {
                        if let Err(e) = self.disk_image.flush() {
                            error!("Failed to flush the disk: {:?}", e);
                            break 'poll;
                        }
                    }
                    Token::QueueAvailable => {
                        if let Err(e) = queue_evt.read() {
                            error!("failed reading queue EventFd: {:?}", e);
                            break 'poll;
                        }
                        needs_interrupt |= self.process_queue(0, &mut flush_timer);
                    }
                    Token::Kill => break 'poll,
                }
            }
            if needs_interrupt {
                self.signal_used_queue();
            }
        }
    }
}

/// Virtio device for exposing block level read/write operations on a host file.
pub struct Block<T: DiskFile> {
    kill_evt: Option<EventFd>,
    disk_image: Option<T>,
    config_space: virtio_blk_config,
    avail_features: u64,
    read_only: bool,
}

fn build_config_space(disk_size: u64) -> virtio_blk_config {
    virtio_blk_config {
        // If the image is not a multiple of the sector size, the tail bits are not exposed.
        capacity: Le64::from(disk_size >> SECTOR_SHIFT),
        max_discard_sectors: Le32::from(MAX_DISCARD_SECTORS),
        discard_sector_alignment: Le32::from(DISCARD_SECTOR_ALIGNMENT),
        max_write_zeroes_sectors: Le32::from(MAX_WRITE_ZEROES_SECTORS),
        write_zeroes_may_unmap: 1,
        // Limit number of segments to 1 - see parse_discard_write_zeroes()
        max_discard_seg: Le32::from(1),
        max_write_zeroes_seg: Le32::from(1),
        ..Default::default()
    }
}

impl<T: DiskFile> Block<T> {
    /// Create a new virtio block device that operates on the given file.
    ///
    /// The given file must be seekable and sizable.
    pub fn new(mut disk_image: T, read_only: bool) -> SysResult<Block<T>> {
        let disk_size = disk_image.seek(SeekFrom::End(0))? as u64;
        if disk_size % SECTOR_SIZE != 0 {
            warn!("Disk size {} is not a multiple of sector size {}; \
                         the remainder will not be visible to the guest.",
                  disk_size,
                  SECTOR_SIZE);
        }

        let mut avail_features: u64 = 1 << VIRTIO_BLK_F_FLUSH;
        if read_only {
            avail_features |= 1 << VIRTIO_BLK_F_RO;
        } else {
            avail_features |= 1 << VIRTIO_BLK_F_DISCARD;
            avail_features |= 1 << VIRTIO_BLK_F_WRITE_ZEROES;
        }

        Ok(Block {
               kill_evt: None,
               disk_image: Some(disk_image),
               config_space: build_config_space(disk_size),
               avail_features,
               read_only,
           })
    }
}

impl<T: DiskFile> Drop for Block<T> {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }
    }
}

impl<T: 'static + AsRawFd + DiskFile + Send> VirtioDevice for Block<T> {
    fn keep_fds(&self) -> Vec<RawFd> {
        let mut keep_fds = Vec::new();

        if let Some(ref disk_image) = self.disk_image {
            keep_fds.push(disk_image.as_raw_fd());
        }

        keep_fds
    }

    fn features(&self, page: u32) -> u32 {
        match page {
            0 => self.avail_features as u32,
            1 => (self.avail_features >> 32) as u32,
            _ => 0,
        }
    }

    fn device_type(&self) -> u32 {
        TYPE_BLOCK
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_len = size_of_val(&self.config_space) as u64;
        if offset >= config_len {
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            let offset = offset as usize;
            let end = cmp::min(end, config_len) as usize;
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&self.config_space.as_slice()[offset..end])
                .unwrap();
        }
    }

    fn activate(&mut self,
                mem: GuestMemory,
                interrupt_evt: EventFd,
                status: Arc<AtomicUsize>,
                queues: Vec<Queue>,
                mut queue_evts: Vec<EventFd>) {
        if queues.len() != 1 || queue_evts.len() != 1 {
            return;
        }

        let (self_kill_evt, kill_evt) =
            match EventFd::new().and_then(|e| Ok((e.try_clone()?, e))) {
                Ok(v) => v,
                Err(e) => {
                    error!("failed creating kill EventFd pair: {:?}", e);
                    return;
                }
            };
        self.kill_evt = Some(self_kill_evt);

        if let Some(disk_image) = self.disk_image.take() {
            let worker_result = thread::Builder::new()
                .name("virtio_blk".to_string())
                .spawn(move || {
                    let mut worker = Worker {
                        queues: queues,
                        mem: mem,
                        disk_image: disk_image,
                        interrupt_status: status,
                        interrupt_evt: interrupt_evt,
                    };
                    worker.run(queue_evts.remove(0), kill_evt);
                });

            if let Err(e) = worker_result {
                error!("failed to spawn virtio_blk worker: {}", e);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::PathBuf;
    use sys_util::TempDir;

    use super::*;

    #[test]
    fn read_size() {
        let tempdir = TempDir::new("/tmp/block_read_test").unwrap();
        let mut path = PathBuf::from(tempdir.as_path().unwrap());
        path.push("disk_image");
        let f = File::create(&path).unwrap();
        f.set_len(0x1000).unwrap();

        let b = Block::new(f, true).unwrap();
        let mut num_sectors = [0u8; 4];
        b.read_config(0, &mut num_sectors);
        // size is 0x1000, so num_sectors is 8 (4096/512).
        assert_eq!([0x08, 0x00, 0x00, 0x00], num_sectors);
        let mut msw_sectors = [0u8; 4];
        b.read_config(4, &mut msw_sectors);
        // size is 0x1000, so msw_sectors is 0.
        assert_eq!([0x00, 0x00, 0x00, 0x00], msw_sectors);
    }

    #[test]
    fn read_features() {
        let tempdir = TempDir::new("/tmp/block_read_test").unwrap();
        let mut path = PathBuf::from(tempdir.as_path().unwrap());
        path.push("disk_image");

        // read-write block device
        {
            let f = File::create(&path).unwrap();
            let b = Block::new(f, false).unwrap();
            // writable device should set VIRTIO_BLK_F_FLUSH + VIRTIO_BLK_F_DISCARD
            // + VIRTIO_BLK_F_WRITE_ZEROES
            assert_eq!(0x6200, b.features(0));
        }

        // read-only block device
        {
            let f = File::create(&path).unwrap();
            let b = Block::new(f, true).unwrap();
            // read-only device should set VIRTIO_BLK_F_FLUSH and VIRTIO_BLK_F_RO
            assert_eq!(0x220, b.features(0));
        }
    }
}
