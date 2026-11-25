//! x86_64 backend: boot a Linux bzImage under KVM, run the agent, fork.
//!
//! Same design as the arm64 path (boot.rs): one VM per process, fork() of
//! the whole VMM process gives children copy-on-write guest RAM and our
//! device state for free, and what lives inside KVM is saved and loaded into
//! a fresh VM in each child. What differs is everything that touches the CPU:
//!
//!   boot      Linux's 64-bit boot protocol (Documentation/arch/x86/boot.rst):
//!             we load the bzImage's protected-mode part at 1 MiB, fill in
//!             the "zero page" (boot_params: memory map, command line, initrd),
//!             build identity page tables and a GDT, and start the vCPU
//!             already in 64-bit mode at the kernel's 64-bit entry point.
//!   devices   the 16550 UART sits on I/O port 0x3f8 (port I/O exits instead
//!             of MMIO). The interrupt controllers (PIC, IOAPIC, local APIC)
//!             and the PIT timer are emulated inside KVM. There is no device
//!             tree on x86: the virtio console is announced on the kernel
//!             command line (virtio_mmio.device=4K@0xd0000000:5).
//!   fork      a child needs: general, segment/control, FPU/AVX (xsave) and
//!             debug registers, MSRs (incl. the TSC), local APIC, pending
//!             events, plus VM-wide PIC/IOAPIC, PIT and kvmclock state.

use crate::agent;
use crate::kvm::{Exit, Kvm, Vcpu, Vm};
use crate::serial::Serial;
use crate::virtio::{GuestMem, VirtioConsole};
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Instant;

// ---- x86 ioctls from linux/kvm.h (checked against the C header by tools/x86_consts.c)
const KVM_GET_MSR_INDEX_LIST: u64 = 0xC004_AE02;
const KVM_GET_SUPPORTED_CPUID: u64 = 0xC008_AE05;
const KVM_SET_TSS_ADDR: u64 = 0xAE47;
const KVM_SET_IDENTITY_MAP_ADDR: u64 = 0x4008_AE48;
const KVM_CREATE_IRQCHIP: u64 = 0xAE60;
const KVM_GET_IRQCHIP: u64 = 0xC208_AE62;
const KVM_SET_IRQCHIP: u64 = 0x8208_AE63;
const KVM_CREATE_PIT2: u64 = 0x4040_AE77;
const KVM_GET_PIT2: u64 = 0x8070_AE9F;
const KVM_SET_PIT2: u64 = 0x4070_AEA0;
const KVM_SET_CLOCK: u64 = 0x4030_AE7B;
const KVM_GET_CLOCK: u64 = 0x8030_AE7C;
const KVM_GET_REGS: u64 = 0x8090_AE81;
const KVM_SET_REGS: u64 = 0x4090_AE82;
const KVM_GET_SREGS: u64 = 0x8138_AE83;
const KVM_SET_SREGS: u64 = 0x4138_AE84;
const KVM_GET_MSRS: u64 = 0xC008_AE88;
const KVM_SET_MSRS: u64 = 0x4008_AE89;
const KVM_GET_LAPIC: u64 = 0x8400_AE8E;
const KVM_SET_LAPIC: u64 = 0x4400_AE8F;
const KVM_SET_CPUID2: u64 = 0x4008_AE90;
const KVM_GET_MP_STATE: u64 = 0x8004_AE98;
const KVM_SET_MP_STATE: u64 = 0x4004_AE99;
const KVM_GET_VCPU_EVENTS: u64 = 0x8040_AE9F;
const KVM_SET_VCPU_EVENTS: u64 = 0x4040_AEA0;
const KVM_GET_DEBUGREGS: u64 = 0x8080_AEA1;
const KVM_SET_DEBUGREGS: u64 = 0x4080_AEA2;
const KVM_GET_XSAVE: u64 = 0x9000_AEA4;
const KVM_SET_XSAVE: u64 = 0x5000_AEA5;
const KVM_GET_XCRS: u64 = 0x8188_AEA6;
const KVM_SET_XCRS: u64 = 0x4188_AEA7;

// ---- guest physical memory layout (RAM starts at 0 on x86)
const GDT_ADDR: u64 = 0x500;
const BOOT_PARAMS: u64 = 0x7000; // the "zero page"
const STACK_TOP: u64 = 0x8ff0;
const PML4: u64 = 0x9000; // identity page tables: PML4 -> PDPT -> PD with 2 MiB pages
const PDPT: u64 = 0xa000;
const PD: u64 = 0xb000;
const CMDLINE: u64 = 0x20000;
const KERNEL: u64 = 0x10_0000; // 1 MiB: where the bzImage's protected-mode code goes
const COM1: u16 = 0x3f8;
const COM1_GSI: u32 = 4;
const VIRTIO_BASE: u64 = 0xd000_0000;
const VIRTIO_SIZE: u64 = 0x1000;
const VIRTIO_GSI: u32 = 5;
const MIB: u64 = 1 << 20;

const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=k panic=-1 pci=off nokaslr noapic 8250.nr_uarts=1 \
     i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd tsc=reliable quiet rdinit=/init";

fn ioctl(fd: RawFd, req: u64, arg: u64) -> io::Result<i32> {
    let ret = unsafe { libc::ioctl(fd, req as _, arg) };
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ret) }
}

fn get<const N: usize>(fd: RawFd, req: u64) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; N];
    ioctl(fd, req, buf.as_mut_ptr() as u64)?;
    Ok(buf)
}

fn set(fd: RawFd, req: u64, buf: &[u8]) -> io::Result<()> {
    ioctl(fd, req, buf.as_ptr() as u64).map(|_| ())
}

fn w16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn w32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn w64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn r32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn r64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

struct Args {
    kernel: String,
    initrd: Option<String>,
    mem_mib: u64,
    cmdline: String,
    thp: bool,
    forks: u64,
    log_dir: String,
    agent: bool,
}

fn parse_args(argv: &[String]) -> Args {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut a = Args {
        kernel: format!("{home}/guest/bzImage"),
        initrd: Some(format!("{home}/guest/initramfs.cpio")),
        mem_mib: 1024,
        cmdline: DEFAULT_CMDLINE.into(),
        thp: false,
        forks: 1,
        log_dir: "/tmp/tinyvmm".into(),
        agent: false,
    };
    let mut it = argv.iter().cloned();
    while let Some(flag) = it.next() {
        let mut val = || it.next().expect("flag needs a value");
        match flag.as_str() {
            "--kernel" => a.kernel = val(),
            "--initrd" => a.initrd = Some(val()),
            "--no-initrd" => a.initrd = None,
            "--mem" => a.mem_mib = val().parse().expect("--mem MiB"),
            "--cmdline" => a.cmdline = val(),
            "--append" => a.cmdline = format!("{} {}", a.cmdline, val()),
            "--thp" => a.thp = true,
            "--forks" => a.forks = val().parse().expect("--forks N"),
            "--log-dir" => a.log_dir = val(),
            "--agent" => a.agent = true,
            _ => panic!("usage: tinyvmm boot [--kernel bzImage] [--initrd cpio | --no-initrd] [--mem MiB] [--cmdline S | --append S] [--thp] [--forks N] [--log-dir D] [--agent]"),
        }
    }
    if a.agent {
        a.cmdline = format!("{} virtio_mmio.device=4K@{VIRTIO_BASE:#x}:{VIRTIO_GSI}", a.cmdline);
    }
    a
}

// ---------------------------------------------------------------------------
// The machine: KVM objects of one VM, and how to (re)build them
// ---------------------------------------------------------------------------

struct Template {
    kvm: Kvm,
    ram: *mut u8,
    ram_size: u64,
    cpuid: Vec<u8>,     // struct kvm_cpuid2, as KVM supports it on this host
    msr_list: Vec<u32>, // every MSR KVM can save/restore
}

struct Machine {
    vm: Arc<Vm>,
    vcpu: Vcpu,
}

fn supported_cpuid(kvm: &Kvm) -> io::Result<Vec<u8>> {
    let n = 256usize;
    let mut buf = vec![0u8; 8 + 40 * n]; // { nent u32, pad u32, entries[n] of 40 bytes }
    w32(&mut buf, 0, n as u32);
    ioctl(kvm.raw_fd(), KVM_GET_SUPPORTED_CPUID, buf.as_mut_ptr() as u64)?;
    let nent = r32(&buf, 0) as usize;
    buf.truncate(8 + 40 * nent);
    for i in 0..nent {
        let e = 8 + 40 * i; // { function, index, flags, eax, ebx, ecx, edx, pad[3] }
        if r32(&buf, e) == 1 {
            let ebx = r32(&buf, e + 16) & 0x00ff_ffff; // initial APIC id (bits 31:24) = 0
            w32(&mut buf, e + 16, ebx);
        }
    }
    Ok(buf)
}

fn msr_index_list(kvm: &Kvm) -> io::Result<Vec<u32>> {
    let mut hdr = [0u32; 1];
    let _ = ioctl(kvm.raw_fd(), KVM_GET_MSR_INDEX_LIST, hdr.as_mut_ptr() as u64); // E2BIG, sets the count
    let n = hdr[0] as usize;
    let mut buf = vec![0u32; n + 1];
    buf[0] = n as u32;
    ioctl(kvm.raw_fd(), KVM_GET_MSR_INDEX_LIST, buf.as_mut_ptr() as u64)?;
    let mut list = buf[1..].to_vec();
    // The TSC first and the TSC deadline last: the deadline is relative to the TSC.
    list.sort_by_key(|&m| match m {
        0x10 => 0,
        0x6e0 => 2,
        _ => 1,
    });
    Ok(list)
}

fn build_machine(t: &Template) -> io::Result<Machine> {
    let vm = Arc::new(t.kvm.create_vm()?);
    let fd = vm.raw_fd();
    ioctl(fd, KVM_SET_TSS_ADDR, 0xfffb_d000)?; // 3 pages KVM needs for real-mode emulation
    let idmap: u64 = 0xfffb_c000;
    ioctl(fd, KVM_SET_IDENTITY_MAP_ADDR, &idmap as *const u64 as u64)?;
    ioctl(fd, KVM_CREATE_IRQCHIP, 0)?; // PIC + IOAPIC (+ a local APIC per vCPU) inside KVM
    let mut pit = [0u32; 16];
    pit[0] = 1; // KVM_PIT_SPEAKER_DUMMY
    ioctl(fd, KVM_CREATE_PIT2, pit.as_ptr() as u64)?;
    vm.set_memory(0, 0, t.ram, t.ram_size as usize)?;
    let vcpu = vm.create_vcpu(0)?;
    set(vcpu.raw_fd(), KVM_SET_CPUID2, &t.cpuid)?;
    Ok(Machine { vm, vcpu })
}

fn uart_irq(vm: Arc<Vm>) -> Box<dyn FnMut(bool) + Send> {
    Box::new(move |level| vm.set_irq_line(COM1_GSI, level).expect("KVM_IRQ_LINE"))
}

fn virtio_irq(vm: Arc<Vm>) -> Box<dyn FnMut(bool) + Send> {
    Box::new(move |level| vm.set_irq_line(VIRTIO_GSI, level).expect("KVM_IRQ_LINE"))
}

// ---------------------------------------------------------------------------
// Boot: bzImage + zero page + long mode
// ---------------------------------------------------------------------------

/// kvm_segment: base u64, limit u32, selector u16, type, present, dpl, db, s, l, g, avl, unusable, pad
fn segment(selector: u16, kind: u8, s: u8, db: u8, l: u8) -> [u8; 24] {
    let mut b = [0u8; 24];
    w32(&mut b, 8, 0xfffff); // limit, in 4 KiB units (g = 1)
    w16(&mut b, 12, selector);
    b[14] = kind;
    b[15] = 1; // present
    b[17] = db;
    b[18] = s;
    b[19] = l;
    b[20] = 1; // g
    b
}

fn setup_boot(m: &Machine, ram: &mut [u8], kernel: &[u8], initrd: Option<&[u8]>, cmdline: &str) -> io::Result<()> {
    if kernel.len() < 0x300 || &kernel[0x202..0x206] != b"HdrS" {
        return Err(io::Error::other("not a bzImage (no HdrS header)"));
    }
    let version = u16::from_le_bytes([kernel[0x206], kernel[0x207]]);
    let xloadflags = u16::from_le_bytes([kernel[0x236], kernel[0x237]]);
    if version < 0x020c || xloadflags & 1 == 0 {
        return Err(io::Error::other("bzImage too old for the 64-bit boot protocol"));
    }
    let setup_sects = match kernel[0x1f1] {
        0 => 4,
        n => n as usize,
    };
    let pm = &kernel[(setup_sects + 1) * 512..]; // the protected-mode kernel
    let put = |ram: &mut [u8], gpa: u64, data: &[u8]| {
        let a = gpa as usize;
        assert!(a + data.len() <= ram.len(), "does not fit in guest RAM");
        ram[a..a + data.len()].copy_from_slice(data);
    };
    put(ram, KERNEL, pm);
    put(ram, CMDLINE, cmdline.as_bytes());
    put(ram, CMDLINE + cmdline.len() as u64, &[0]);

    // The zero page: the kernel's own setup header, plus what a boot loader fills in.
    let mut zp = vec![0u8; 4096];
    let hdr_end = 0x202 + kernel[0x201] as usize;
    zp[0x1f1..hdr_end].copy_from_slice(&kernel[0x1f1..hdr_end]);
    w16(&mut zp, 0x1fa, 0xffff); // vid_mode: "normal"
    zp[0x210] = 0xff; // type_of_loader: undefined
    zp[0x211] |= 0x01 | 0x80; // loadflags: LOADED_HIGH | CAN_USE_HEAP
    w16(&mut zp, 0x224, 0xfe00); // heap_end_ptr
    w32(&mut zp, 0x228, CMDLINE as u32);
    let ram_size = ram.len() as u64;
    if let Some(rd) = initrd {
        let addr = (ram_size - rd.len() as u64 - MIB) & !0xfff; // near the top of RAM
        put(ram, addr, rd);
        w32(&mut zp, 0x218, addr as u32);
        w32(&mut zp, 0x21c, rd.len() as u32);
    }
    // e820 memory map: low RAM, the legacy hole (BIOS/VGA), RAM from 1 MiB.
    let e820 = [(0u64, 0x9fc00u64, 1u32), (0x9fc00, 0x10_0000 - 0x9fc00, 2), (0x10_0000, ram_size - 0x10_0000, 1)];
    zp[0x1e8] = e820.len() as u8;
    for (i, (addr, size, kind)) in e820.iter().enumerate() {
        let off = 0x2d0 + 20 * i;
        w64(&mut zp, off, *addr);
        w64(&mut zp, off + 8, *size);
        w32(&mut zp, off + 16, *kind);
    }
    put(ram, BOOT_PARAMS, &zp);

    // Page tables identity-mapping the first 1 GiB with 2 MiB pages.
    let mut pt = vec![0u8; 3 * 4096];
    w64(&mut pt, 0, PDPT | 3); // present | writable
    w64(&mut pt, 4096, PD | 3);
    for i in 0..512u64 {
        w64(&mut pt, 8192 + 8 * i as usize, (i << 21) | 0x83); // present | writable | 2 MiB page
    }
    put(ram, PML4, &pt);
    // GDT: null, 64-bit code, data, TSS.
    let gdt: [u64; 4] = [0, 0x00af_9b00_0000_ffff, 0x00cf_9300_0000_ffff, 0x008f_8b00_0000_ffff];
    let gdt_bytes: Vec<u8> = gdt.iter().flat_map(|e| e.to_le_bytes()).collect();
    put(ram, GDT_ADDR, &gdt_bytes);

    // Special registers: segments, GDT, paging on, long mode on.
    let fd = m.vcpu.raw_fd();
    let mut sregs = get::<312>(fd, KVM_GET_SREGS)?;
    let code = segment(0x08, 0xb, 1, 0, 1);
    let data = segment(0x10, 0x3, 1, 1, 0);
    let tss = segment(0x18, 0xb, 0, 0, 0);
    sregs[0..24].copy_from_slice(&code); // cs
    for off in [24, 48, 72, 96, 120] {
        sregs[off..off + 24].copy_from_slice(&data); // ds es fs gs ss
    }
    sregs[144..168].copy_from_slice(&tss); // tr
    w64(&mut sregs, 192, GDT_ADDR);
    w16(&mut sregs, 200, (gdt.len() * 8 - 1) as u16);
    w64(&mut sregs, 208, 0); // idt: none yet
    w16(&mut sregs, 216, 0);
    let cr0 = r64(&sregs, 224) | 0x1 | 0x8000_0000; // PE | PG
    w64(&mut sregs, 224, cr0);
    w64(&mut sregs, 240, PML4); // cr3
    let cr4 = r64(&sregs, 248) | 0x20; // PAE
    w64(&mut sregs, 248, cr4);
    let efer = r64(&sregs, 264) | 0x100 | 0x400; // LME | LMA
    w64(&mut sregs, 264, efer);
    set(fd, KVM_SET_SREGS, &sregs)?;

    // General registers: the 64-bit entry is 0x200 bytes into the
    // protected-mode kernel; rsi points at the zero page.
    let mut regs = vec![0u8; 144]; // rax rbx rcx rdx rsi rdi rsp rbp r8..r15 rip rflags
    w64(&mut regs, 32, BOOT_PARAMS);
    w64(&mut regs, 48, STACK_TOP);
    w64(&mut regs, 56, STACK_TOP);
    w64(&mut regs, 128, KERNEL + 0x200);
    w64(&mut regs, 136, 0x2); // rflags: bit 1 is always set
    set(fd, KVM_SET_REGS, &regs)?;

    // Local APIC: LINT0 delivers the PIC's interrupts (ExtINT), LINT1 is NMI,
    // the wiring a PC BIOS leaves behind.
    let mut lapic = get::<1024>(fd, KVM_GET_LAPIC)?;
    let lint0 = (r32(&lapic, 0x350) & !0x700) | 0x700;
    let lint1 = (r32(&lapic, 0x360) & !0x700) | 0x400;
    w32(&mut lapic, 0x350, lint0);
    w32(&mut lapic, 0x360, lint1);
    set(fd, KVM_SET_LAPIC, &lapic)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fork: what lives in KVM, saved in the parent and loaded in each child
// ---------------------------------------------------------------------------

struct Snapshot {
    regs: Vec<u8>,
    sregs: Vec<u8>,
    xsave: Vec<u8>,
    xcrs: Vec<u8>,
    debugregs: Vec<u8>,
    lapic: Vec<u8>,
    events: Vec<u8>,
    mp_state: Vec<u8>,
    msrs: Vec<(u32, u64)>,
    irqchips: Vec<Vec<u8>>, // PIC master, PIC slave, IOAPIC
    pit: Vec<u8>,
    clock: Vec<u8>,
}

/// Read MSRs. KVM_GET_MSRS stops at the first one it can't read and returns
/// how many it did, so skip that one and go on.
fn get_msrs(fd: RawFd, list: &[u32]) -> io::Result<Vec<(u32, u64)>> {
    let mut out = Vec::new();
    let mut rest = list;
    while !rest.is_empty() {
        let mut buf = vec![0u8; 8 + 16 * rest.len()]; // { nmsrs u32, pad u32, entries { index u32, pad u32, data u64 } }
        w32(&mut buf, 0, rest.len() as u32);
        for (i, &m) in rest.iter().enumerate() {
            w32(&mut buf, 8 + 16 * i, m);
        }
        let n = ioctl(fd, KVM_GET_MSRS, buf.as_mut_ptr() as u64)? as usize;
        for i in 0..n {
            out.push((rest[i], r64(&buf, 8 + 16 * i + 8)));
        }
        rest = if n < rest.len() { &rest[n + 1..] } else { &[] };
    }
    Ok(out)
}

/// Write MSRs, skipping any KVM refuses. Returns the refused ones.
fn set_msrs(fd: RawFd, msrs: &[(u32, u64)]) -> Vec<u32> {
    let mut refused = Vec::new();
    let mut rest = msrs;
    while !rest.is_empty() {
        let mut buf = vec![0u8; 8 + 16 * rest.len()];
        w32(&mut buf, 0, rest.len() as u32);
        for (i, &(m, v)) in rest.iter().enumerate() {
            w32(&mut buf, 8 + 16 * i, m);
            w64(&mut buf, 8 + 16 * i + 8, v);
        }
        let n = ioctl(fd, KVM_SET_MSRS, buf.as_mut_ptr() as u64).unwrap_or(0) as usize;
        if n < rest.len() {
            refused.push(rest[n].0);
            rest = &rest[n + 1..];
        } else {
            rest = &[];
        }
    }
    refused
}

fn snapshot(m: &Machine, t: &Template) -> io::Result<Snapshot> {
    let c = m.vcpu.raw_fd();
    let v = m.vm.raw_fd();
    let mut irqchips = Vec::new();
    for chip in 0..3u32 {
        let mut buf = vec![0u8; 520]; // { chip_id u32, pad u32, union[512] }
        w32(&mut buf, 0, chip);
        ioctl(v, KVM_GET_IRQCHIP, buf.as_mut_ptr() as u64)?;
        irqchips.push(buf);
    }
    Ok(Snapshot {
        regs: get::<144>(c, KVM_GET_REGS)?,
        sregs: get::<312>(c, KVM_GET_SREGS)?,
        xsave: get::<4096>(c, KVM_GET_XSAVE)?,
        xcrs: get::<392>(c, KVM_GET_XCRS)?,
        debugregs: get::<128>(c, KVM_GET_DEBUGREGS)?,
        lapic: get::<1024>(c, KVM_GET_LAPIC)?,
        events: get::<64>(c, KVM_GET_VCPU_EVENTS)?,
        mp_state: get::<4>(c, KVM_GET_MP_STATE)?,
        msrs: get_msrs(c, &t.msr_list)?,
        irqchips,
        pit: get::<112>(v, KVM_GET_PIT2)?,
        clock: get::<48>(v, KVM_GET_CLOCK)?,
    })
}

/// Load a snapshot into a freshly built machine. Returns what KVM refused.
/// Order follows Firecracker's x86 restore: VM-wide devices, then the vCPU
/// (mp state, registers, FPU, debug, LAPIC, MSRs, pending events last).
fn restore(m: &Machine, s: &Snapshot) -> Vec<String> {
    let c = m.vcpu.raw_fd();
    let v = m.vm.raw_fd();
    let mut errors = Vec::new();
    fn check(errors: &mut Vec<String>, what: &str, r: io::Result<()>) {
        if let Err(e) = r {
            errors.push(format!("{what}: {e}"));
        }
    }
    let mut clock = s.clock.clone();
    w32(&mut clock, 8, 0); // flags: set the clock to exactly the saved value
    check(&mut errors, "pit", set(v, KVM_SET_PIT2, &s.pit));
    check(&mut errors, "clock", set(v, KVM_SET_CLOCK, &clock));
    for (i, chip) in s.irqchips.iter().enumerate() {
        check(&mut errors, &format!("irqchip {i}"), set(v, KVM_SET_IRQCHIP, chip));
    }
    check(&mut errors, "mp_state", set(c, KVM_SET_MP_STATE, &s.mp_state));
    check(&mut errors, "regs", set(c, KVM_SET_REGS, &s.regs));
    check(&mut errors, "sregs", set(c, KVM_SET_SREGS, &s.sregs));
    check(&mut errors, "xsave", set(c, KVM_SET_XSAVE, &s.xsave));
    check(&mut errors, "xcrs", set(c, KVM_SET_XCRS, &s.xcrs));
    check(&mut errors, "debugregs", set(c, KVM_SET_DEBUGREGS, &s.debugregs));
    check(&mut errors, "lapic", set(c, KVM_SET_LAPIC, &s.lapic));
    let refused = set_msrs(c, &s.msrs);
    if !refused.is_empty() {
        errors.push(format!("{} MSRs refused: {:x?}", refused.len(), refused));
    }
    check(&mut errors, "vcpu_events", set(c, KVM_SET_VCPU_EVENTS, &s.events));
    errors
}

// ---------------------------------------------------------------------------
// Kicking the vCPU, and forks asked for on a control socket (as in boot.rs)
// ---------------------------------------------------------------------------

static RUN_PAGE: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static VCPU_THREAD: AtomicU64 = AtomicU64::new(0);
static FORK_REQUESTED: AtomicBool = AtomicBool::new(false);
static FORK_COUNT: AtomicU64 = AtomicU64::new(0);
static FORK_REPLY: Mutex<Option<mpsc::Sender<Vec<String>>>> = Mutex::new(None);
static SOCK_DIR: OnceLock<String> = OnceLock::new();

extern "C" fn on_kick(_sig: libc::c_int) {
    let page = RUN_PAGE.load(Ordering::SeqCst);
    if !page.is_null() {
        unsafe { page.add(1).write_volatile(1) }; // kvm_run.immediate_exit
    }
}

fn install_kick_handler() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_kick as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
    }
}

fn host_fork(n: u64) -> Vec<String> {
    while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
    let (tx, rx) = mpsc::channel();
    *FORK_REPLY.lock().unwrap() = Some(tx);
    FORK_COUNT.store(n, Ordering::SeqCst);
    FORK_REQUESTED.store(true, Ordering::SeqCst);
    unsafe { libc::pthread_kill(VCPU_THREAD.load(Ordering::SeqCst) as libc::pthread_t, libc::SIGUSR1) };
    let names = rx.recv().unwrap_or_default();
    let dir = SOCK_DIR.get().cloned().unwrap_or_default();
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    for name in &names {
        while !std::path::Path::new(&format!("{dir}/vm-{name}.ctl")).exists() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
    }
    names
}

struct ForkCtx<'a> {
    t: &'a Template,
    serial: &'a Arc<Mutex<Serial>>,
    virtio: Option<&'a Arc<Mutex<VirtioConsole>>>,
    channel: Option<&'a agent::Channel>,
    log_dir: &'a str,
    path: &'a str,
}

/// Fork into n children. Some((machine, name, channel)) in a child, None in the parent.
fn fork_vm(ctx: &ForkCtx, m: &Machine, n: u64, children: &mut Vec<i32>) -> io::Result<Option<(Machine, String, Option<agent::Channel>)>> {
    let t_request = Instant::now();
    let snap = snapshot(m, ctx.t)?;
    let t_saved = Instant::now();
    let mut serial = ctx.serial.lock().unwrap(); // no other thread may hold these across fork()
    let mut virtio = ctx.virtio.map(|v| v.lock().unwrap());
    io::stdout().flush()?;
    std::fs::create_dir_all(ctx.log_dir)?;
    for idx in 1..=n {
        match unsafe { libc::fork() } {
            -1 => return Err(io::Error::last_os_error()),
            0 => {
                unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
                let name = if ctx.path == "root" { idx.to_string() } else { format!("{}.{idx}", ctx.path) };
                let fresh = build_machine(ctx.t)?;
                for e in restore(&fresh, &snap) {
                    eprintln!("[tinyvmm vm {name}] not restored: {e}");
                }
                let log = std::fs::File::create(format!("{}/vm-{name}.log", ctx.log_dir))?;
                serial.rewire(Box::new(log), uart_irq(fresh.vm.clone()));
                drop(serial);
                let mut channel = None;
                if let (Some(v), Some(dev)) = (ctx.virtio, virtio.as_mut()) {
                    if let Some(ch) = ctx.channel {
                        ch.close_inherited();
                    }
                    let ch = agent::start(ctx.log_dir, &name, v.clone(), host_fork)?;
                    drop(dev.rewire(Box::new(ch.sink.clone()), virtio_irq(fresh.vm.clone())));
                    channel = Some(ch);
                }
                drop(virtio);
                RUN_PAGE.store(fresh.vcpu.run_page(), Ordering::SeqCst);
                VCPU_THREAD.store(unsafe { libc::pthread_self() } as u64, Ordering::SeqCst);
                children.clear();
                return Ok(Some((fresh, name, channel)));
            }
            pid => children.push(pid),
        }
    }
    drop(serial);
    drop(virtio);
    let done = Instant::now();
    if let Some(tx) = FORK_REPLY.lock().unwrap().take() {
        let names = (1..=n).map(|i| if ctx.path == "root" { i.to_string() } else { format!("{}.{i}", ctx.path) }).collect();
        let _ = tx.send(names);
    }
    eprintln!(
        "[tinyvmm vm {}] forked {n}: guest paused {:.2} ms (save state {:.2} ms, {} MSRs)",
        ctx.path,
        (done - t_request).as_secs_f64() * 1e3,
        (t_saved - t_request).as_secs_f64() * 1e3,
        snap.msrs.len()
    );
    Ok(None)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

pub fn main(argv: &[String], process_start: Instant) -> io::Result<()> {
    let args = parse_args(argv);
    let kvm = Kvm::open()?;
    let ram_size = args.mem_mib * MIB;
    let ram = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            ram_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if ram == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let ram = ram as *mut u8;
    if args.thp {
        unsafe { libc::madvise(ram as *mut _, ram_size as usize, libc::MADV_HUGEPAGE) };
    }
    let cpuid = supported_cpuid(&kvm)?;
    let msr_list = msr_index_list(&kvm)?;
    let t = Template { kvm, ram, ram_size, cpuid, msr_list };
    let mut m = build_machine(&t)?;

    let kernel = std::fs::read(&args.kernel)?;
    let initrd = match &args.initrd {
        Some(p) => Some(std::fs::read(p)?),
        None => None,
    };
    let ram_slice = unsafe { std::slice::from_raw_parts_mut(ram, ram_size as usize) };
    setup_boot(&m, ram_slice, &kernel, initrd.as_deref(), &args.cmdline)?;

    let serial = Arc::new(Mutex::new(Serial::new(Box::new(io::stdout()), uart_irq(m.vm.clone()))));
    let _ = SOCK_DIR.set(args.log_dir.clone());
    let mut channel = None;
    let virtio = if args.agent {
        if let Ok(entries) = std::fs::read_dir(&args.log_dir) {
            for e in entries.flatten() {
                let f = e.file_name().to_string_lossy().to_string();
                if f.starts_with("vm-") && (f.ends_with(".sock") || f.ends_with(".ctl")) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let mem = GuestMem { host: ram, gpa_base: 0, size: ram_size };
        let dev = Arc::new(Mutex::new(VirtioConsole::new(mem, Box::new(io::sink()), virtio_irq(m.vm.clone()))));
        let ch = agent::start(&args.log_dir, "root", dev.clone(), host_fork)?;
        drop(dev.lock().unwrap().rewire(Box::new(ch.sink.clone()), virtio_irq(m.vm.clone())));
        channel = Some(ch);
        Some(dev)
    } else {
        None
    };
    install_kick_handler();
    RUN_PAGE.store(m.vcpu.run_page(), Ordering::SeqCst);
    VCPU_THREAD.store(unsafe { libc::pthread_self() } as u64, Ordering::SeqCst);
    eprintln!("[tinyvmm] VMM ready in {:.1} ms, booting {} (x86_64)", process_start.elapsed().as_secs_f64() * 1e3, args.kernel);

    let mut path = "root".to_string();
    let mut children: Vec<i32> = Vec::new();
    let mut warned = std::collections::HashSet::new();
    loop {
        if FORK_REQUESTED.swap(false, Ordering::SeqCst) {
            // Stop at an instruction boundary: finish any half-done I/O, run nothing more.
            m.vcpu.set_immediate_exit(true);
            let _ = m.vcpu.run()?;
            m.vcpu.set_immediate_exit(false);
            let n = match FORK_COUNT.swap(0, Ordering::SeqCst) {
                0 => args.forks,
                n => n,
            };
            let ctx = ForkCtx { t: &t, serial: &serial, virtio: virtio.as_ref(), channel: channel.as_ref(), log_dir: &args.log_dir, path: &path };
            if let Some((child, name, ch)) = fork_vm(&ctx, &m, n, &mut children)? {
                m = child;
                path = name;
                if ch.is_some() {
                    channel = ch;
                }
            }
            continue;
        }
        match m.vcpu.run()? {
            Exit::IoOut { port, data, .. } if (COM1..COM1 + 8).contains(&port) => {
                let mut s = serial.lock().unwrap();
                for b in data {
                    s.write((port - COM1) as u64, b);
                }
            }
            Exit::IoIn { port, size, count } if (COM1..COM1 + 8).contains(&port) => {
                let v = serial.lock().unwrap().read((port - COM1) as u64);
                m.vcpu.set_io_in(&vec![v; size * count]);
            }
            Exit::IoOut { port: 0x64, data, .. } if data.first() == Some(&0xfe) => {
                eprintln!("[tinyvmm vm {path}] guest reset (keyboard controller)");
                break; // reboot=k: the guest's way to reboot/power off
            }
            Exit::IoIn { size, count, .. } => m.vcpu.set_io_in(&vec![0; size * count]), // nothing there
            Exit::IoOut { .. } => {}
            Exit::MmioRead { addr, len } if virtio.is_some() && (VIRTIO_BASE..VIRTIO_BASE + VIRTIO_SIZE).contains(&addr) => {
                let v = virtio.as_ref().unwrap().lock().unwrap().read(addr - VIRTIO_BASE);
                m.vcpu.set_mmio_read(v as u64, len);
            }
            Exit::MmioWrite { addr, data, .. } if virtio.is_some() && (VIRTIO_BASE..VIRTIO_BASE + VIRTIO_SIZE).contains(&addr) => {
                virtio.as_ref().unwrap().lock().unwrap().write(addr - VIRTIO_BASE, data as u32);
            }
            Exit::MmioRead { addr, len } => {
                if warned.insert(addr) {
                    eprintln!("[tinyvmm] unhandled MMIO read at {addr:#x}");
                }
                m.vcpu.set_mmio_read(0, len);
            }
            Exit::MmioWrite { addr, .. } => {
                if warned.insert(addr) {
                    eprintln!("[tinyvmm] unhandled MMIO write at {addr:#x}");
                }
            }
            Exit::SystemEvent(_) => break,
            Exit::Shutdown => return Err(io::Error::other(format!("vm {path}: triple fault"))),
            Exit::Hlt => {} // with KVM's irqchip, HLT is handled in the kernel
            Exit::Interrupted => m.vcpu.set_immediate_exit(false),
            Exit::Other(r) => return Err(io::Error::other(format!("vm {path}: unexpected exit reason {r}"))),
        }
    }
    for pid in children {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    Ok(())
}
