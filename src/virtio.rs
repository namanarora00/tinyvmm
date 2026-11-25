//! virtio-mmio transport + a virtio console: a fast byte pipe to the guest.
//!
//! Why not the UART: every byte through the 16550 is an MMIO exit (~30-60 us
//! here). virtio moves data through shared memory instead. The guest driver
//! puts buffers in guest RAM and lists them in "virtqueues"; it writes one
//! register ("queue notify") to say "I added some"; we read or fill them
//! straight out of guest memory, mark them used, and raise one interrupt.
//! One exit per buffer instead of one per byte.
//!
//! A split virtqueue lives in guest memory, at addresses the driver gives us:
//!   descriptor table  N x {addr u64, len u32, flags u16, next u16}; a buffer
//!                     can be a chain (flags NEXT), and WRITE marks buffers
//!                     the device fills (vs ones it reads)
//!   available ring    {flags u16, idx u16, ring[N] u16}: heads the driver offers
//!   used ring         {flags u16, idx u16, ring[N] {id u32, len u32}}: heads we
//!                     are done with, and how many bytes we wrote
//! The console has two queues: 0 = receive (host -> guest, buffers the guest
//! offers for us to fill), 1 = transmit (guest -> host, buffers to read).
//!
//! Register layout: virtio spec 1.x, section 4.2.2 (MMIO, version 2).

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{Ordering, fence};

const MAGIC: u32 = 0x7472_6976; // "virt"
const DEVICE_ID_CONSOLE: u32 = 3;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const QUEUE_MAX: u16 = 256;
const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const RX: usize = 0;
const TX: usize = 1;

/// Guest RAM as seen from the VMM: guest physical address -> our pointer.
#[derive(Clone, Copy)]
pub struct GuestMem {
    pub host: *mut u8,
    pub gpa_base: u64,
    pub size: u64,
}
unsafe impl Send for GuestMem {}

impl GuestMem {
    fn at(&self, gpa: u64, len: u64) -> *mut u8 {
        assert!(gpa >= self.gpa_base && gpa + len <= self.gpa_base + self.size, "guest address {gpa:#x}+{len} outside RAM");
        unsafe { self.host.add((gpa - self.gpa_base) as usize) }
    }
    fn u16(&self, gpa: u64) -> u16 {
        unsafe { (self.at(gpa, 2) as *const u16).read_volatile() }
    }
    fn u32(&self, gpa: u64) -> u32 {
        unsafe { (self.at(gpa, 4) as *const u32).read_volatile() }
    }
    fn u64(&self, gpa: u64) -> u64 {
        unsafe { (self.at(gpa, 8) as *const u64).read_volatile() }
    }
    fn set_u16(&self, gpa: u64, v: u16) {
        unsafe { (self.at(gpa, 2) as *mut u16).write_volatile(v) }
    }
    fn set_u32(&self, gpa: u64, v: u32) {
        unsafe { (self.at(gpa, 4) as *mut u32).write_volatile(v) }
    }
    fn read(&self, gpa: u64, len: u32) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.at(gpa, len as u64), len as usize) }
    }
    fn write(&self, gpa: u64, data: &[u8]) {
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.at(gpa, data.len() as u64), data.len()) }
    }
}

#[derive(Clone, Copy, Default)]
struct Queue {
    num: u16,
    ready: bool,
    desc: u64,
    avail: u64,
    used: u64,
    next_avail: u16, // next available-ring entry we haven't consumed
}

struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

pub struct VirtioConsole {
    mem: GuestMem,
    status: u32,
    dev_features_sel: u32,
    drv_features_sel: u32,
    drv_features: u64,
    queue_sel: usize,
    queues: [Queue; 2],
    interrupt_status: u32,
    irq_level: bool,
    rx_pending: VecDeque<u8>,
    out: Box<dyn Write + Send>,
    raise_irq: Box<dyn FnMut(bool) + Send>,
}

impl VirtioConsole {
    pub fn new(mem: GuestMem, out: Box<dyn Write + Send>, raise_irq: Box<dyn FnMut(bool) + Send>) -> Self {
        VirtioConsole {
            mem,
            status: 0,
            dev_features_sel: 0,
            drv_features_sel: 0,
            drv_features: 0,
            queue_sel: 0,
            queues: [Queue::default(); 2],
            interrupt_status: 0,
            irq_level: false,
            rx_pending: VecDeque::new(),
            out,
            raise_irq,
        }
    }

    pub fn read(&mut self, off: u64) -> u32 {
        let q = self.queues.get(self.queue_sel).copied().unwrap_or_default();
        match off {
            0x000 => MAGIC,
            0x004 => 2, // version 2 = "modern" virtio-mmio
            0x008 => DEVICE_ID_CONSOLE,
            0x00c => 0x554d_4551, // vendor id; any value works
            0x010 => (VIRTIO_F_VERSION_1 >> (32 * self.dev_features_sel)) as u32,
            0x034 => {
                if self.queue_sel < 2 {
                    QUEUE_MAX as u32
                } else {
                    0
                }
            }
            0x044 => q.ready as u32,
            0x060 => self.interrupt_status,
            0x070 => self.status,
            0x0fc => 0, // config generation: our config never changes
            _ => 0,     // config space (0x100+): no size/multiport features, so all zero
        }
    }

    pub fn write(&mut self, off: u64, v: u32) {
        let sel = self.queue_sel;
        match off {
            0x014 => self.dev_features_sel = v,
            0x020 => {
                let shift = 32 * self.drv_features_sel;
                self.drv_features = (self.drv_features & !(0xffff_ffffu64 << shift)) | ((v as u64) << shift);
            }
            0x024 => self.drv_features_sel = v,
            0x030 => self.queue_sel = v as usize,
            0x038 if sel < 2 => self.queues[sel].num = v as u16,
            0x044 if sel < 2 => self.queues[sel].ready = v == 1,
            0x050 => match v as usize {
                RX => self.fill_rx(), // guest offered fresh receive buffers
                TX => self.drain_tx(),
                _ => {}
            },
            0x064 => {
                self.interrupt_status &= !v; // guest acknowledged the interrupt
                self.update_irq();
            }
            0x070 => {
                if v == 0 {
                    self.reset();
                } else {
                    self.status = v;
                }
            }
            0x080 if sel < 2 => self.queues[sel].desc = (self.queues[sel].desc & !0xffff_ffff) | v as u64,
            0x084 if sel < 2 => self.queues[sel].desc = (self.queues[sel].desc & 0xffff_ffff) | ((v as u64) << 32),
            0x090 if sel < 2 => self.queues[sel].avail = (self.queues[sel].avail & !0xffff_ffff) | v as u64,
            0x094 if sel < 2 => self.queues[sel].avail = (self.queues[sel].avail & 0xffff_ffff) | ((v as u64) << 32),
            0x0a0 if sel < 2 => self.queues[sel].used = (self.queues[sel].used & !0xffff_ffff) | v as u64,
            0x0a4 if sel < 2 => self.queues[sel].used = (self.queues[sel].used & 0xffff_ffff) | ((v as u64) << 32),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.status = 0;
        self.drv_features = 0;
        self.queues = [Queue::default(); 2];
        self.interrupt_status = 0;
        self.update_irq();
    }

    fn desc(&self, q: &Queue, i: u16) -> Desc {
        let d = q.desc + 16 * i as u64;
        Desc { addr: self.mem.u64(d), len: self.mem.u32(d + 8), flags: self.mem.u16(d + 12), next: self.mem.u16(d + 14) }
    }

    /// Next buffer chain the driver offered, if any.
    fn pop_avail(&mut self, qi: usize) -> Option<u16> {
        let q = self.queues[qi];
        if !q.ready || q.num == 0 {
            return None;
        }
        let avail_idx = self.mem.u16(q.avail + 2);
        fence(Ordering::Acquire); // read the ring entry only after seeing idx
        if q.next_avail == avail_idx {
            return None;
        }
        let head = self.mem.u16(q.avail + 4 + 2 * (q.next_avail % q.num) as u64);
        self.queues[qi].next_avail = q.next_avail.wrapping_add(1);
        Some(head)
    }

    /// Hand a chain back to the driver, saying how many bytes we wrote into it.
    fn push_used(&mut self, qi: usize, head: u16, written: u32) {
        let q = self.queues[qi];
        let idx = self.mem.u16(q.used + 2);
        let slot = q.used + 4 + 8 * (idx % q.num) as u64;
        self.mem.set_u32(slot, head as u32);
        self.mem.set_u32(slot + 4, written);
        fence(Ordering::Release); // the entry must be visible before the index moves
        self.mem.set_u16(q.used + 2, idx.wrapping_add(1));
    }

    /// Guest -> host: read every offered transmit buffer.
    fn drain_tx(&mut self) {
        let mut any = false;
        while let Some(head) = self.pop_avail(TX) {
            let q = self.queues[TX];
            let mut i = head;
            loop {
                let d = self.desc(&q, i);
                if d.flags & DESC_F_WRITE == 0 {
                    let data = self.mem.read(d.addr, d.len).to_vec();
                    let _ = self.out.write_all(&data);
                }
                if d.flags & DESC_F_NEXT == 0 {
                    break;
                }
                i = d.next;
            }
            self.push_used(TX, head, 0);
            any = true;
        }
        if any {
            let _ = self.out.flush();
            self.interrupt_status |= 1;
            self.update_irq();
        }
    }

    /// Host -> guest: copy pending bytes into offered receive buffers.
    fn fill_rx(&mut self) {
        let mut any = false;
        while !self.rx_pending.is_empty() {
            let Some(head) = self.pop_avail(RX) else { break };
            let q = self.queues[RX];
            let (mut i, mut written) = (head, 0u32);
            loop {
                let d = self.desc(&q, i);
                if d.flags & DESC_F_WRITE != 0 {
                    let n = (d.len as usize).min(self.rx_pending.len());
                    let chunk: Vec<u8> = self.rx_pending.drain(..n).collect();
                    self.mem.write(d.addr, &chunk);
                    written += n as u32;
                }
                if d.flags & DESC_F_NEXT == 0 || self.rx_pending.is_empty() {
                    break;
                }
                i = d.next;
            }
            self.push_used(RX, head, written);
            any = true;
        }
        if any {
            self.interrupt_status |= 1;
            self.update_irq();
        }
    }

    /// Bytes from the host side (the harness) for the guest.
    pub fn push_input(&mut self, bytes: &[u8]) {
        self.rx_pending.extend(bytes);
        self.fill_rx();
    }

    /// Level-triggered: the line is high while an interrupt is unacknowledged.
    fn update_irq(&mut self) {
        let level = self.interrupt_status != 0;
        if level != self.irq_level {
            self.irq_level = level;
            (self.raise_irq)(level);
        }
    }

    /// After a VM fork: device state was copied by fork(); point output and
    /// the interrupt at the child, and re-assert the line for its fresh GIC.
    /// Returns the old sinks so the caller can leak them: they hold the
    /// parent's file descriptors, which must not be closed twice.
    pub fn rewire(
        &mut self,
        out: Box<dyn Write + Send>,
        raise_irq: Box<dyn FnMut(bool) + Send>,
    ) -> (Box<dyn Write + Send>, Box<dyn FnMut(bool) + Send>) {
        let old_out = std::mem::replace(&mut self.out, out);
        let old_irq = std::mem::replace(&mut self.raise_irq, raise_irq);
        let level = self.irq_level;
        (self.raise_irq)(level);
        (old_out, old_irq)
    }
}
