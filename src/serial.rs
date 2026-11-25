//! A 16550A UART, enough for Linux's 8250 driver: console out, keyboard in.
//!
//! The guest sees eight one-byte registers. Every guest access is an MMIO exit
//! that lands here. Transmit is instant (we write the byte to stdout), so the
//! transmit register is always empty. The interesting part is the interrupt
//! logic, because the driver's TX path is interrupt-driven: it enables the
//! "transmitter empty" interrupt, gets interrupted, writes bytes, repeats.

use std::collections::VecDeque;
use std::io::Write;

// Register offsets (with the divisor latch bit, LCR.DLAB, clear).
const RBR_THR: u64 = 0; // read: received byte; write: byte to send
const IER: u64 = 1; // interrupt enable
const IIR_FCR: u64 = 2; // read: which interrupt is pending; write: FIFO control
const LCR: u64 = 3; // line control (bit 7 = DLAB)
const MCR: u64 = 4; // modem control
const LSR: u64 = 5; // line status
const MSR: u64 = 6; // modem status
const SCR: u64 = 7; // scratch

const IER_RDI: u8 = 0x01; // interrupt when a received byte is waiting
const IER_THRI: u8 = 0x02; // interrupt when the transmit register is empty
const IIR_NONE: u8 = 0x01;
const IIR_THRI: u8 = 0x02;
const IIR_RDI: u8 = 0x04;
const IIR_FIFO_ON: u8 = 0xc0;
const LCR_DLAB: u8 = 0x80;
const LSR_DR: u8 = 0x01; // data ready
const LSR_THRE: u8 = 0x20; // transmit holding register empty
const LSR_TEMT: u8 = 0x40; // transmitter empty
const MSR_DCD_DSR_CTS: u8 = 0xb0; // "a cable is plugged in and ready"

pub struct Serial {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    fcr: u8,
    divisor: [u8; 2],
    rx: VecDeque<u8>,
    /// A "transmitter empty" interrupt is owed. Set when the guest sends a
    /// byte or enables THRI; cleared when the guest reads IIR and sees it.
    thr_pending: bool,
    irq_level: bool,
    raise_irq: Box<dyn FnMut(bool) + Send>,
    out: Box<dyn Write + Send>,
}

impl Serial {
    pub fn new(out: Box<dyn Write + Send>, raise_irq: Box<dyn FnMut(bool) + Send>) -> Self {
        Serial {
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            fcr: 0,
            divisor: [0; 2],
            rx: VecDeque::new(),
            thr_pending: false,
            irq_level: false,
            raise_irq,
            out,
        }
    }

    fn dlab(&self) -> bool {
        self.lcr & LCR_DLAB != 0
    }

    pub fn read(&mut self, off: u64) -> u8 {
        let v = match off {
            RBR_THR if self.dlab() => self.divisor[0],
            RBR_THR => self.rx.pop_front().unwrap_or(0),
            IER if self.dlab() => self.divisor[1],
            IER => self.ier,
            IIR_FCR => {
                let fifo = if self.fcr & 1 != 0 { IIR_FIFO_ON } else { 0 };
                fifo | if self.ier & IER_RDI != 0 && !self.rx.is_empty() {
                    IIR_RDI
                } else if self.ier & IER_THRI != 0 && self.thr_pending {
                    self.thr_pending = false; // reading IIR acknowledges THRE
                    IIR_THRI
                } else {
                    IIR_NONE
                }
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => LSR_THRE | LSR_TEMT | if self.rx.is_empty() { 0 } else { LSR_DR },
            MSR => MSR_DCD_DSR_CTS,
            SCR => self.scr,
            _ => 0,
        };
        self.update_irq();
        v
    }

    pub fn write(&mut self, off: u64, v: u8) {
        match off {
            RBR_THR if self.dlab() => self.divisor[0] = v,
            RBR_THR => {
                let _ = self.out.write_all(&[v]);
                let _ = self.out.flush();
                self.thr_pending = true; // sent instantly: empty again
            }
            IER if self.dlab() => self.divisor[1] = v,
            IER => {
                // Turning THRI on while the transmitter is empty raises it at once.
                if v & IER_THRI != 0 && self.ier & IER_THRI == 0 {
                    self.thr_pending = true;
                }
                self.ier = v & 0x0f;
            }
            IIR_FCR => {
                self.fcr = v;
                if v & 0x02 != 0 {
                    self.rx.clear(); // "clear receive FIFO"
                }
            }
            LCR => self.lcr = v,
            MCR => self.mcr = v,
            SCR => self.scr = v,
            _ => {}
        }
        self.update_irq();
    }

    /// After a VM fork: this struct's state (registers, queued input) was
    /// copied by fork(), but its output and interrupt line still point at the
    /// parent. Re-point them, then re-assert the line, because the child's
    /// fresh GIC has never seen our current level.
    pub fn rewire(&mut self, out: Box<dyn Write + Send>, raise_irq: Box<dyn FnMut(bool) + Send>) {
        self.out = out;
        self.raise_irq = raise_irq;
        let level = self.irq_level;
        (self.raise_irq)(level);
    }

    /// Bytes typed on the host keyboard.
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))] // x86 path has no keyboard thread yet
    pub fn push_input(&mut self, bytes: &[u8]) {
        self.rx.extend(bytes);
        self.update_irq();
    }

    /// The interrupt line is level-triggered: high while any enabled cause holds.
    /// Only tell KVM when the level actually changes (each call is a syscall).
    fn update_irq(&mut self) {
        let level = (self.ier & IER_RDI != 0 && !self.rx.is_empty()) || (self.ier & IER_THRI != 0 && self.thr_pending);
        if level != self.irq_level {
            self.irq_level = level;
            (self.raise_irq)(level);
        }
    }
}
