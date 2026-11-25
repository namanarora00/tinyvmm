//! Milestones 3 and 4: boot a real Linux kernel (`tinyvmm boot`), and fork
//! it while it runs.
//!
//! What Linux on arm64 needs from us (Documentation/arch/arm64/booting.rst):
//!   1. RAM, with the kernel Image at a 2 MiB-aligned address (+ text_offset).
//!   2. A device tree blob describing the machine, address in x0.
//!   3. The vCPU at EL1, MMU off, interrupts masked, PC at the Image start.
//!      (KVM's vCPU reset state is already "EL1, MMU off, masked".)
//!   4. An interrupt controller and a timer. Both are emulated inside KVM
//!      (vGICv3, arch timer); we only create the GIC and say where it lives.
//!   5. A console: our 16550 UART (serial.rs), the one device we emulate.
//!
//! Fork (milestone 4) is the toy-guest design applied to Linux: Unix fork()
//! of the whole VMM process copies guest RAM copy-on-write, and our device
//! state (the UART) for free, because both live in our memory. What lives
//! inside KVM is saved first and loaded into a fresh VM in each child: every
//! vCPU register (including the timer) and the GIC.
//!
//! Agent mode (`--agent`, milestone 6, RL environments) adds a virtio console
//! (virtio.rs) and two Unix sockets per VM (agent.rs): a harness sends files
//! and commands to an agent inside the guest, and asks for forks with
//! "fork N" on the control socket. Each fork child gets its own sockets.

use crate::agent;
use crate::fdt::Fdt;
use crate::gic;
use crate::kvm::{self, Device, Exit, Kvm, REG_PC, Vcpu, VcpuInit, Vm};
use crate::serial::Serial;
use crate::virtio::{GuestMem, VirtioConsole};
use std::fs::File;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Instant;

// Guest physical memory map. Devices low, RAM from 2 GiB (QEMU "virt" layout).
const GIC_DIST: u64 = 0x0800_0000;
const GIC_DIST_SIZE: u64 = 0x1_0000;
const GIC_REDIST: u64 = 0x080A_0000;
const GIC_REDIST_SIZE_PER_CPU: u64 = 0x2_0000;
const UART_BASE: u64 = 0x0900_0000;
const UART_SIZE: u64 = 0x1000;
/// Not in the device tree: guest init writes here once, to timestamp boot.
const BOOT_MARKER: u64 = 0x0904_0000;
/// Not in the device tree either: tinyvmm's control page, used by the guest's
/// `forkme` program through /dev/mem. Write offset 0 = "fork me"; read
/// offset 4 = "which VM am I": 0 in the one that asked, i in child i.
const CTRL_BASE: u64 = 0x0905_0000;
const CTRL_FORK: u64 = CTRL_BASE;
const CTRL_ROLE: u64 = CTRL_BASE + 4;
/// virtio-mmio console (agent mode): registers here, interrupt SPI 2 (INTID 34).
const VIRTIO_BASE: u64 = 0x0A00_0000;
const VIRTIO_SIZE: u64 = 0x200;
const VIRTIO_SPI: u32 = 2;
const RAM_BASE: u64 = 0x8000_0000;

/// The UART's interrupt: SPI 1 in device-tree numbering = GIC INTID 33.
const UART_SPI: u32 = 1;
const NR_IRQS: u32 = 128; // 32 private (SGIs + PPIs) + 96 shared
const PHANDLE_GIC: u32 = 1;

const MIB: u64 = 1 << 20;
const REG_X0: u64 = 0x6030_0000_0010_0000; // core register, offset 0
/// MPIDR_EL1: the CPU's affinity id, which the device tree's cpu node must match.
/// sysreg encoding op0=3 op1=0 CRn=0 CRm=0 op2=5.
const REG_MPIDR: u64 = 0x6030_0000_0013_0000 | (3 << 14) | 5;

struct Args {
    kernel: String,
    initrd: Option<String>,
    mem_mib: u64,
    cmdline: String,
    dump_dtb: Option<String>,
    thp: bool,
    forks: u64,
    log_dir: String,
    agent: bool,
}

fn parse_args(argv: &[String]) -> Args {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut a = Args {
        kernel: format!("{home}/guest/Image"),
        initrd: Some(format!("{home}/guest/initramfs.cpio")),
        mem_mib: 512,
        cmdline: "console=ttyS0 earlycon=uart8250,mmio,0x9000000 rdinit=/init panic=-1".into(),
        dump_dtb: None,
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
            "--dump-dtb" => a.dump_dtb = Some(val()),
            "--thp" => a.thp = true,
            "--forks" => a.forks = val().parse().expect("--forks N"),
            "--log-dir" => a.log_dir = val(),
            "--agent" => a.agent = true,
            _ => panic!(
                "usage: tinyvmm boot [--kernel Image] [--initrd cpio | --no-initrd] [--mem MiB] [--cmdline S | --append S] [--dump-dtb file] [--thp] [--forks N] [--log-dir D] [--agent]"
            ),
        }
    }
    a
}

fn align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) & !(a - 1)
}

/// Parsed arm64 Image header (first 64 bytes of the file).
struct ImageHeader {
    text_offset: u64,
    image_size: u64, // includes .bss, so it is larger than the file
}

fn parse_image(img: &[u8]) -> io::Result<ImageHeader> {
    let u64_at = |o: usize| u64::from_le_bytes(img[o..o + 8].try_into().unwrap());
    if img.len() < 64 || &img[56..60] != b"ARM\x64" {
        return Err(io::Error::other("not an arm64 Image (magic \"ARM\\x64\" missing at offset 56)"));
    }
    Ok(ImageHeader { text_offset: u64_at(8), image_size: u64_at(16) })
}

fn device_tree(ram_size: u64, cmdline: &str, initrd: Option<(u64, u64)>, mpidr: u64, virtio: bool) -> Vec<u8> {
    let uart = format!("/serial@{UART_BASE:x}");
    let mut f = Fdt::new();
    f.begin_node("");
    f.prop_str("compatible", "linux,dummy-virt");
    f.prop_str("model", "tinyvmm");
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_u32("interrupt-parent", PHANDLE_GIC);

    f.begin_node("chosen");
    f.prop_str("bootargs", cmdline);
    f.prop_str("stdout-path", &uart);
    if let Some((start, end)) = initrd {
        f.prop_u64("linux,initrd-start", start);
        f.prop_u64("linux,initrd-end", end);
    }
    f.end_node();

    f.begin_node("aliases");
    f.prop_str("serial0", &uart); // makes our UART ttyS0
    f.end_node();

    f.begin_node(&format!("memory@{RAM_BASE:x}"));
    f.prop_str("device_type", "memory");
    f.prop_regs("reg", &[(RAM_BASE, ram_size)]);
    f.end_node();

    f.begin_node("cpus");
    f.prop_u32("#address-cells", 1);
    f.prop_u32("#size-cells", 0);
    f.begin_node("cpu@0");
    f.prop_str("device_type", "cpu");
    f.prop_str("compatible", "arm,arm-v8");
    f.prop_str("enable-method", "psci");
    f.prop_u32("reg", (mpidr & 0xff_ffff) as u32);
    f.end_node();
    f.end_node();

    // Power management firmware calls (CPU on/off, system off). KVM answers
    // them itself when the guest executes `hvc`.
    f.begin_node("psci");
    f.prop_strs("compatible", &["arm,psci-1.0", "arm,psci-0.2"]);
    f.prop_str("method", "hvc");
    f.end_node();

    f.begin_node(&format!("intc@{GIC_DIST:x}"));
    f.prop_str("compatible", "arm,gic-v3");
    f.prop_u32("#interrupt-cells", 3); // <type number flags>, type 0 = SPI, 1 = PPI
    f.prop_empty("interrupt-controller");
    f.prop_regs("reg", &[(GIC_DIST, GIC_DIST_SIZE), (GIC_REDIST, GIC_REDIST_SIZE_PER_CPU)]);
    f.prop_u32("phandle", PHANDLE_GIC);
    f.end_node();

    // Per-CPU timer interrupts (PPIs): secure, non-secure, virtual, hypervisor.
    // Flags 4 = level-triggered, active high.
    f.begin_node("timer");
    f.prop_str("compatible", "arm,armv8-timer");
    f.prop_cells("interrupts", &[1, 13, 4, 1, 14, 4, 1, 11, 4, 1, 10, 4]);
    f.prop_empty("always-on");
    f.end_node();

    f.begin_node(&format!("serial@{UART_BASE:x}"));
    f.prop_str("compatible", "ns16550a");
    f.prop_regs("reg", &[(UART_BASE, UART_SIZE)]);
    f.prop_u32("clock-frequency", 1_843_200);
    f.prop_cells("interrupts", &[0, UART_SPI, 4]);
    f.end_node();

    if virtio {
        f.begin_node(&format!("virtio_mmio@{VIRTIO_BASE:x}"));
        f.prop_str("compatible", "virtio,mmio");
        f.prop_regs("reg", &[(VIRTIO_BASE, VIRTIO_SIZE)]);
        f.prop_cells("interrupts", &[0, VIRTIO_SPI, 4]);
        f.prop_empty("dma-coherent"); // guest RAM is coherent with us: no cache flushes needed
        f.end_node();
    }

    f.end_node();
    f.finish()
}

/// Puts the host terminal in raw mode (keys go straight to the guest) and
/// restores it on drop.
struct RawTerminal(Option<libc::termios>);

impl RawTerminal {
    fn enable() -> Self {
        unsafe {
            if libc::isatty(0) == 0 {
                return RawTerminal(None);
            }
            let mut t: libc::termios = std::mem::zeroed();
            libc::tcgetattr(0, &mut t);
            let saved = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            RawTerminal(Some(saved))
        }
    }
    fn restore(saved: &Option<libc::termios>) {
        if let Some(t) = saved {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        Self::restore(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Kicking the vCPU out of KVM_RUN from another thread.
//
// To fork on the host's command (Ctrl-A f), the keyboard thread must stop a
// vCPU that may be deep inside KVM_RUN, even halted in the guest's idle loop.
// It sends SIGUSR1 to the vCPU thread; the handler sets kvm_run.immediate_exit.
// The signal makes a running KVM_RUN return EINTR; immediate_exit covers the
// race where the signal lands just before the next KVM_RUN starts.
// ---------------------------------------------------------------------------

static RUN_PAGE: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static VCPU_THREAD: AtomicU64 = AtomicU64::new(0);
static FORK_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Forks asked for on a control socket: how many, and where the vCPU thread
/// sends the children's names once they exist.
static FORK_COUNT: AtomicU64 = AtomicU64::new(0);
static FORK_REPLY: Mutex<Option<mpsc::Sender<Vec<String>>>> = Mutex::new(None);
static SOCK_DIR: OnceLock<String> = OnceLock::new();

/// Runs on the control-socket thread: fork this VM into n children and
/// return their names once each one is listening on its own sockets.
fn host_fork(n: u64) -> Vec<String> {
    // Reap children that have exited since the last fork, so finished
    // episodes don't pile up as zombie processes over a long RL run.
    while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
    let (tx, rx) = mpsc::channel();
    *FORK_REPLY.lock().unwrap() = Some(tx);
    FORK_COUNT.store(n, Ordering::SeqCst);
    request_fork_from_host();
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

fn virtio_irq(vm: Arc<Vm>) -> Box<dyn FnMut(bool) + Send> {
    Box::new(move |level| vm.set_spi(32 + VIRTIO_SPI, level).expect("KVM_IRQ_LINE"))
}
/// Debug (TINYVMM_TRACE): a fork child samples its guest's PC every 200 us
/// until the guest reads its role, to show what the guest spends that time on.
static SAMPLING: AtomicBool = AtomicBool::new(false);
/// PSTATE, right after PC in struct kvm_regs: offset 264 / 4 = 0x42.
const REG_PSTATE: u64 = 0x6030_0000_0010_0042;

fn start_sampler() {
    SAMPLING.store(true, Ordering::SeqCst);
    std::thread::spawn(|| {
        while SAMPLING.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_micros(200));
            unsafe { libc::pthread_kill(VCPU_THREAD.load(Ordering::SeqCst) as libc::pthread_t, libc::SIGUSR1) };
        }
    });
}

fn print_samples(name: &str, samples: &[(u64, u64)]) {
    let mut counts = std::collections::HashMap::new();
    for &(pc, el) in samples {
        *counts.entry((pc, el)).or_insert(0u32) += 1;
    }
    let mut top: Vec<_> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    let el0 = samples.iter().filter(|s| s.1 == 0).count();
    eprint!("[tinyvmm vm {name}] {} PC samples ({} in guest user space, {} in guest kernel):\r\n", samples.len(), el0, samples.len() - el0);
    for ((pc, el), n) in top.iter().take(12) {
        eprint!("[tinyvmm vm {name}] sample {n:4} EL{el} {pc:#018x}\r\n");
    }
}

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
        sa.sa_flags = 0; // no SA_RESTART: KVM_RUN must return EINTR
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
    }
}

fn request_fork_from_host() {
    FORK_REQUESTED.store(true, Ordering::SeqCst);
    unsafe { libc::pthread_kill(VCPU_THREAD.load(Ordering::SeqCst) as libc::pthread_t, libc::SIGUSR1) };
}

// ---------------------------------------------------------------------------
// The machine: what lives in KVM, and how to rebuild it.
// ---------------------------------------------------------------------------

/// Everything needed to (re)build a VM over the same guest RAM.
struct Template {
    kvm: Kvm,
    ram: *mut u8,
    ram_size: u64,
    init: VcpuInit,
    reg_ids: Vec<u64>,
    mpidr: u64,
}

/// The KVM objects of one running VM. A fork child cannot use its parent's
/// (KVM ties them to the creating process), so it builds its own.
struct Machine {
    vm: Arc<Vm>,
    vcpu: Vcpu,
    gic: Device,
}

fn build_machine(t: &Template) -> io::Result<Machine> {
    let vm = Arc::new(t.kvm.create_vm()?);
    vm.set_memory(0, RAM_BASE, t.ram, t.ram_size as usize)?;
    let vcpu = vm.create_vcpu(0)?;
    vcpu.init(&t.init)?;
    let gic = create_gic(&vm, 1)?;
    Ok(Machine { vm, vcpu, gic })
}

/// Create the in-kernel GICv3 and tell it where it lives in guest memory.
/// Must happen after all vCPUs exist (the redistributor region is per vCPU).
fn create_gic(vm: &Vm, ncpus: u64) -> io::Result<Device> {
    let gic = vm.create_device(kvm::KVM_DEV_TYPE_ARM_VGIC_V3)?;
    let dist = GIC_DIST;
    let redist = GIC_REDIST;
    let nr_irqs = NR_IRQS;
    gic.set_attr(kvm::VGIC_GRP_ADDR, kvm::VGIC_V3_ADDR_TYPE_DIST, &dist as *const u64 as u64)?;
    gic.set_attr(kvm::VGIC_GRP_ADDR, kvm::VGIC_V3_ADDR_TYPE_REDIST, &redist as *const u64 as u64)?;
    gic.set_attr(kvm::VGIC_GRP_NR_IRQS, 0, &nr_irqs as *const u32 as u64)?;
    gic.set_attr(kvm::VGIC_GRP_CTRL, kvm::VGIC_CTRL_INIT, 0)?;
    assert!(GIC_REDIST + GIC_REDIST_SIZE_PER_CPU * ncpus <= UART_BASE);
    Ok(gic) // kept: the parent needs it to save GIC state at fork time
}

fn uart_irq(vm: Arc<Vm>) -> Box<dyn FnMut(bool) + Send> {
    Box::new(move |level| vm.set_spi(32 + UART_SPI, level).expect("KVM_IRQ_LINE"))
}

// ---------------------------------------------------------------------------
// Fork
// ---------------------------------------------------------------------------

/// Timestamps a fork child collects on its way back to running guest code.
#[derive(Clone, Copy)]
struct ChildTimes {
    idx: u64,
    guest_asked: bool, // forked by the guest's `forkme` (so it will read its role)
    t_request: Instant,
    t_fork_call: Instant,
    t_start: Instant,
    t_built: Instant,
    t_regs: Instant,
    t_gic: Instant,
    t_devices: Instant,
    t_first_exit: Option<Instant>,
    mono_resume_ns: u64, // CLOCK_MONOTONIC, to line up with kernel traces
}

fn mono_ns() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

const RECORD_WORDS: usize = 10;

struct ForkCtx<'a> {
    t: &'a Template,
    serial: &'a Arc<Mutex<Serial>>,
    virtio: Option<&'a Arc<Mutex<VirtioConsole>>>,
    channel: Option<&'a agent::Channel>,
    log_dir: &'a str,
    path: &'a str, // this VM's name in the fork tree: "root", "2", "2.1", ...
}

/// Fork the running VM into `n` children. Returns Some((machine, times,
/// name)) in a child process, None in the parent.
type Forked = (Machine, ChildTimes, String, Option<agent::Channel>);

fn fork_vm(ctx: &ForkCtx, m: &Machine, n: u64, t_request: Instant, guest_asked: bool, children: &mut Vec<i32>) -> io::Result<Option<Forked>> {
    // 1. Snapshot what lives in KVM. The vCPU is stopped at an instruction
    //    boundary (the caller ran it once with immediate_exit set).
    let t0 = Instant::now();
    let cpu = m.vcpu.save(&ctx.t.reg_ids)?;
    let t_cpu = Instant::now();
    let gic_state = gic::save(&m.gic, NR_IRQS, ctx.t.mpidr)?;
    let t_gic_saved = Instant::now();

    // 2. Only the calling thread survives fork(). If the keyboard thread held
    //    the UART lock at that moment, the child's copy would stay locked
    //    forever. Holding it ourselves across fork() rules that out.
    let mut serial = ctx.serial.lock().unwrap();
    let mut virtio = ctx.virtio.map(|v| v.lock().unwrap()); // same reason: socket threads use it
    io::stdout().flush()?;
    std::fs::create_dir_all(ctx.log_dir)?;

    let mut parent_fork_us = Vec::new();
    for idx in 1..=n {
        let t_fork_call = Instant::now();
        let pid = unsafe { libc::fork() };
        if pid > 0 {
            parent_fork_us.push(t_fork_call.elapsed().as_micros());
        }
        match pid {
            -1 => return Err(io::Error::last_os_error()),
            0 => {
                // ---- child process ----
                let t_start = Instant::now();
                unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) }; // die with the parent
                let name = if ctx.path == "root" { idx.to_string() } else { format!("{}.{idx}", ctx.path) };
                let fresh = build_machine(ctx.t)?;
                let t_built = Instant::now();
                for (id, e) in fresh.vcpu.restore(&cpu) {
                    eprint!("[tinyvmm vm {name}] register {id:#x} not restored: {e}\r\n");
                }
                let t_regs = Instant::now();
                for (group, attr, e) in gic::restore(&fresh.gic, &gic_state) {
                    eprint!("[tinyvmm vm {name}] GIC group {group} attr {attr:#x} not restored: {e}\r\n");
                }
                let t_gic = Instant::now();
                // Devices: the UART's registers and queued input were copied
                // by fork(); point its output at a log file and its interrupt
                // at our GIC.
                let log = File::create(format!("{}/vm-{name}.log", ctx.log_dir))?;
                serial.rewire(Box::new(log), uart_irq(fresh.vm.clone()));
                drop(serial);
                // Agent channel: close the parent's sockets, open our own, and
                // point the virtio console at them and at our GIC.
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
                let t_devices = Instant::now();
                let times = ChildTimes {
                    idx,
                    guest_asked,
                    t_request,
                    t_fork_call,
                    t_start,
                    t_built,
                    t_regs,
                    t_gic,
                    t_devices,
                    t_first_exit: None,
                    mono_resume_ns: mono_ns(),
                };
                if std::env::var_os("TINYVMM_TRACE").is_some() {
                    start_sampler();
                }
                return Ok(Some((fresh, times, name, channel)));
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
    eprint!(
        "[tinyvmm vm {}] forked {n}: guest paused {:.2} ms (save {} regs {:.2} ms, save GIC {:.2} ms, {n} x fork() {:.2} ms), VMM resident memory {} MiB\r\n[tinyvmm vm {}] each fork() call in the parent, us: {:?}\r\n",
        ctx.path,
        ms(t_request, done),
        ctx.t.reg_ids.len(),
        ms(t0, t_cpu),
        ms(t_cpu, t_gic_saved),
        ms(t_gic_saved, done),
        resident_mib(),
        ctx.path,
        parent_fork_us
    );
    Ok(None)
}

fn resident_mib() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    pages * 4096 / (1 << 20)
}

/// Child: send its timings to the root VM, which prints a summary at the end.
fn report_child(fd: i32, name: &str, c: &ChildTimes, done: Instant) {
    let ns = |a: Instant, b: Instant| (b - a).as_nanos() as u64;
    let first = c.t_first_exit.unwrap_or(done);
    let rec: [u64; RECORD_WORDS] = [
        c.idx,
        ns(c.t_request, done),       // request -> guest running again
        ns(c.t_request, c.t_fork_call), // waiting: snapshot + earlier siblings
        ns(c.t_fork_call, c.t_start),   // fork() syscall
        ns(c.t_start, c.t_built),       // new VM + vCPU + GIC
        ns(c.t_built, c.t_regs),        // load vCPU registers
        ns(c.t_regs, c.t_gic),          // load GIC state
        ns(c.t_gic, c.t_devices),       // re-wire devices
        ns(c.t_devices, first),         // first KVM_RUN until first exit
        ns(first, done),                // first exit until role read
    ];
    unsafe { libc::write(fd, rec.as_ptr() as *const _, std::mem::size_of_val(&rec)) };
    eprint!(
        "[tinyvmm vm {name}] running {:.2} ms after the fork request ({})\r\n",
        rec[1] as f64 / 1e6,
        if c.guest_asked { "guest read its role" } else { "first exit" }
    );
    if std::env::var_os("TINYVMM_TRACE").is_some() {
        eprint!(
            "[tinyvmm vm {name}] trace window pid {} mono {:.6} {:.6}\r\n",
            std::process::id(),
            c.mono_resume_ns as f64 / 1e9,
            mono_ns() as f64 / 1e9
        );
    }
}

fn summarize(records: &[[u64; RECORD_WORDS]]) {
    if records.is_empty() {
        return;
    }
    let names = [
        "fork request -> child running guest code",
        "  waiting (snapshot + earlier siblings)",
        "  fork() syscall",
        "  create VM + vCPU + GIC",
        "  load vCPU registers",
        "  load GIC state",
        "  re-wire devices",
        "  first KVM_RUN until first exit",
        "  first exit until role read",
    ];
    eprint!("\r\n[tinyvmm] {} fork children, times in ms:\r\n", records.len());
    eprint!("{:<44} {:>8} {:>8} {:>8}\r\n", "", "min", "median", "max");
    for (i, name) in names.iter().enumerate() {
        let mut v: Vec<u64> = records.iter().map(|r| r[i + 1]).collect();
        v.sort();
        let f = |x: u64| x as f64 / 1e6;
        eprint!("{:<44} {:>8.2} {:>8.2} {:>8.2}\r\n", name, f(v[0]), f(v[v.len() / 2]), f(v[v.len() - 1]));
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

pub fn main(argv: &[String], process_start: Instant) -> io::Result<()> {
    let args = parse_args(argv);
    let kvm = Kvm::open()?;

    // --- RAM ---
    // MAP_PRIVATE: what makes fork() copy-on-write. MAP_SHARED would let a
    // parent and its children see each other's writes.
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
        // Back guest RAM with 2 MiB pages where possible: KVM can then map
        // guest memory in 2 MiB blocks, so one fault covers 512x more memory.
        unsafe { libc::madvise(ram as *mut _, ram_size as usize, libc::MADV_HUGEPAGE) };
    }
    let load = |gpa: u64, bytes: &[u8]| {
        let off = gpa - RAM_BASE;
        assert!(off + bytes.len() as u64 <= ram_size, "image does not fit in guest RAM");
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ram.add(off as usize), bytes.len()) };
    };

    // --- Kernel, initrd, device tree placement ---
    //   RAM_BASE + text_offset : kernel (image_size bytes, incl. bss)
    //   next 2 MiB boundary    : initrd
    //   last 2 MiB of RAM      : device tree (must not share a 2 MiB block)
    let image = std::fs::read(&args.kernel)?;
    let hdr = parse_image(&image)?;
    let kernel_addr = RAM_BASE + hdr.text_offset;
    load(kernel_addr, &image);
    let mut next = align_up(kernel_addr + hdr.image_size, 2 * MIB);
    let initrd = match &args.initrd {
        Some(path) => {
            let data = std::fs::read(path)?;
            load(next, &data);
            let range = (next, next + data.len() as u64);
            next = align_up(range.1, 2 * MIB);
            Some(range)
        }
        None => None,
    };
    let dtb_addr = RAM_BASE + ram_size - 2 * MIB;
    assert!(next <= dtb_addr, "kernel + initrd overlap the device tree: use more --mem");

    // --- VM, vCPU, GIC ---
    let probe = kvm.create_vm()?;
    let mut init = probe.preferred_target()?;
    drop(probe);
    init.features[0] |= 1 << kvm::KVM_ARM_VCPU_PSCI_0_2;
    let mut t = Template { kvm, ram, ram_size, init, reg_ids: Vec::new(), mpidr: 0 };
    let mut m = build_machine(&t)?;
    t.reg_ids = m.vcpu.reg_list()?;
    t.mpidr = u64::from_le_bytes(m.vcpu.get_reg(REG_MPIDR)?[..8].try_into().unwrap());

    let dtb = device_tree(ram_size, &args.cmdline, initrd, t.mpidr, args.agent);
    assert!(dtb.len() as u64 <= 2 * MIB);
    if let Some(path) = &args.dump_dtb {
        std::fs::write(path, &dtb)?;
    }
    load(dtb_addr, &dtb);
    m.vcpu.set_u64(REG_PC, kernel_addr)?;
    m.vcpu.set_u64(REG_X0, dtb_addr)?;

    // --- Devices and the keyboard thread ---
    let serial = Arc::new(Mutex::new(Serial::new(Box::new(io::stdout()), uart_irq(m.vm.clone()))));
    // Agent mode: the virtio console and this VM's sockets.
    let _ = SOCK_DIR.set(args.log_dir.clone());
    let mut channel = None;
    let virtio = if args.agent {
        if let Ok(entries) = std::fs::read_dir(&args.log_dir) {
            for e in entries.flatten() {
                let f = e.file_name().to_string_lossy().to_string();
                if f.starts_with("vm-") && (f.ends_with(".sock") || f.ends_with(".ctl")) {
                    let _ = std::fs::remove_file(e.path()); // stale, from an earlier run
                }
            }
        }
        let mem = GuestMem { host: ram, gpa_base: RAM_BASE, size: ram_size };
        let dev = Arc::new(Mutex::new(VirtioConsole::new(mem, Box::new(io::sink()), virtio_irq(m.vm.clone()))));
        let ch = agent::start(&args.log_dir, "root", dev.clone(), host_fork)?;
        drop(dev.lock().unwrap().rewire(Box::new(ch.sink.clone()), virtio_irq(m.vm.clone())));
        channel = Some(ch);
        Some(dev)
    } else {
        None
    };
    let term = RawTerminal::enable();
    let interactive = term.0.is_some();
    install_kick_handler();
    RUN_PAGE.store(m.vcpu.run_page(), Ordering::SeqCst);
    VCPU_THREAD.store(unsafe { libc::pthread_self() } as u64, Ordering::SeqCst);
    {
        let serial = serial.clone();
        let saved_term = term.0;
        std::thread::spawn(move || stdin_loop(serial, saved_term));
    }
    // Children report their fork timings to the root VM through this pipe.
    let mut pipe = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);

    // --- Run ---
    let mut path = "root".to_string();
    let mut children: Vec<i32> = Vec::new();
    let mut pending: Option<ChildTimes> = None;
    let mut guest_fork_request: Option<Instant> = None;
    let t_first_run = Instant::now();
    eprint!("[tinyvmm] VMM ready in {:.1} ms, booting {}\r\n", ms(process_start, t_first_run), args.kernel);
    if interactive {
        eprint!("[tinyvmm] Ctrl-A f forks this VM ({} children, consoles in {}), Ctrl-A x quits\r\n", args.forks, args.log_dir);
    }
    let mut exits: u64 = 0;
    let mut uart_exits: u64 = 0;
    let mut samples: Vec<(u64, u64)> = Vec::new();
    let mut warned = std::collections::HashSet::new();
    loop {
        if FORK_REQUESTED.swap(false, Ordering::SeqCst) {
            let guest_request = guest_fork_request.take();
            let guest_asked = guest_request.is_some(); // else Ctrl-A f on the host
            let t_request = guest_request.unwrap_or_else(Instant::now);
            // Stop at an instruction boundary. If the last exit was an MMIO
            // access (the guest's fork request is one), KVM finishes it on
            // this KVM_RUN and then returns EINTR without running anything
            // else. Saving state between an MMIO exit and its completion
            // would give the child a half-executed instruction.
            m.vcpu.set_immediate_exit(true);
            let _ = m.vcpu.run()?;
            m.vcpu.set_immediate_exit(false);
            let n = match FORK_COUNT.swap(0, Ordering::SeqCst) {
                0 => args.forks,
                n => n,
            };
            let ctx = ForkCtx { t: &t, serial: &serial, virtio: virtio.as_ref(), channel: channel.as_ref(), log_dir: &args.log_dir, path: &path };
            if let Some((child, times, name, ch)) = fork_vm(&ctx, &m, n, t_request, guest_asked, &mut children)? {
                m = child; // drops (closes) the parent's KVM objects in this process
                pending = Some(times);
                path = name;
                if ch.is_some() {
                    channel = ch;
                }
            }
            continue;
        }

        let exit = m.vcpu.run()?;
        exits += 1;
        let now = Instant::now();
        if let Some(c) = pending.as_mut() {
            c.t_first_exit.get_or_insert(now);
            let is_role_read = matches!(exit, Exit::MmioRead { addr: CTRL_ROLE, .. });
            if is_role_read || !c.guest_asked {
                report_child(pipe[1], &path, c, now);
                pending = None;
                if SAMPLING.swap(false, Ordering::SeqCst) {
                    print_samples(&path, &samples);
                }
            }
        }
        match exit {
            Exit::MmioWrite { addr, data, .. } if (UART_BASE..UART_BASE + UART_SIZE).contains(&addr) => {
                uart_exits += 1;
                serial.lock().unwrap().write(addr - UART_BASE, data as u8);
            }
            Exit::MmioRead { addr, len } if (UART_BASE..UART_BASE + UART_SIZE).contains(&addr) => {
                uart_exits += 1;
                let v = serial.lock().unwrap().read(addr - UART_BASE);
                m.vcpu.set_mmio_read(v as u64, len);
            }
            Exit::MmioRead { addr, len } if virtio.is_some() && (VIRTIO_BASE..VIRTIO_BASE + VIRTIO_SIZE).contains(&addr) => {
                let v = virtio.as_ref().unwrap().lock().unwrap().read(addr - VIRTIO_BASE);
                m.vcpu.set_mmio_read(v as u64, len);
            }
            Exit::MmioWrite { addr, data, .. } if virtio.is_some() && (VIRTIO_BASE..VIRTIO_BASE + VIRTIO_SIZE).contains(&addr) => {
                virtio.as_ref().unwrap().lock().unwrap().write(addr - VIRTIO_BASE, data as u32);
            }
            Exit::MmioWrite { addr: BOOT_MARKER, .. } => {
                eprint!(
                    "[tinyvmm] guest init reached: {:.1} ms after the first KVM_RUN, {:.1} ms after process start ({exits} exits so far, {uart_exits} of them UART)\r\n",
                    ms(t_first_run, now),
                    ms(process_start, now),
                );
            }
            Exit::MmioWrite { addr: CTRL_FORK, .. } => {
                guest_fork_request = Some(now);
                FORK_REQUESTED.store(true, Ordering::SeqCst);
            }
            Exit::MmioRead { addr: CTRL_ROLE, len } => {
                let role = if path == "root" { 0 } else { path.rsplit('.').next().unwrap().parse().unwrap_or(0) };
                m.vcpu.set_mmio_read(role, len);
            }
            Exit::MmioRead { addr, len } => {
                if warned.insert(addr) {
                    eprint!("[tinyvmm] unhandled MMIO read at {addr:#x}\r\n");
                }
                m.vcpu.set_mmio_read(0, len);
            }
            Exit::MmioWrite { addr, .. } => {
                if warned.insert(addr) {
                    eprint!("[tinyvmm] unhandled MMIO write at {addr:#x}\r\n");
                }
            }
            Exit::SystemEvent(kind) => {
                let what = if kind == 2 { "reset" } else { "power-off" };
                io::stdout().flush()?;
                eprint!(
                    "[tinyvmm vm {path}] guest requested {what} after {:.1} ms, {exits} exits ({uart_exits} UART)\r\n",
                    ms(t_first_run, now)
                );
                break;
            }
            Exit::Interrupted => {
                m.vcpu.set_immediate_exit(false); // a kick: loop top checks why
                if SAMPLING.load(Ordering::SeqCst) {
                    let pc = u64::from_le_bytes(m.vcpu.get_reg(REG_PC)?[..8].try_into().unwrap());
                    let pstate = u64::from_le_bytes(m.vcpu.get_reg(REG_PSTATE)?[..8].try_into().unwrap());
                    samples.push((pc, (pstate >> 2) & 3));
                }
            }
            Exit::IoOut { .. } | Exit::IoIn { .. } | Exit::Hlt | Exit::Shutdown => {
                return Err(io::Error::other("x86-only exit on arm64"));
            }
            Exit::Other(reason) => {
                return Err(io::Error::other(format!("unexpected KVM exit reason {reason}")));
            }
        }
    }

    // --- Shut down: deal with the VMs we forked ---
    if interactive {
        // Their consoles have no keyboard, so they would idle forever.
        for &pid in &children {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    let mut records = Vec::new();
    if path == "root" {
        // Only the root collects timings: read until every child has exited.
        unsafe { libc::close(pipe[1]) };
        let mut rec = [0u64; RECORD_WORDS];
        let size = std::mem::size_of_val(&rec);
        while unsafe { libc::read(pipe[0], rec.as_mut_ptr() as *mut _, size) } == size as isize {
            records.push(rec);
        }
    }
    for &pid in &children {
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }
    summarize(&records);
    drop(term);
    Ok(())
}

/// Host keyboard -> guest UART. Ctrl-A x quits, Ctrl-A f forks the VM.
fn stdin_loop(serial: Arc<Mutex<Serial>>, saved_term: Option<libc::termios>) {
    let mut buf = [0u8; 256];
    let mut ctrl_a = false;
    loop {
        let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n <= 0 {
            return; // EOF: piped input finished
        }
        let mut forward = Vec::with_capacity(n as usize);
        for &b in &buf[..n as usize] {
            if ctrl_a {
                ctrl_a = false;
                match b {
                    b'x' => {
                        RawTerminal::restore(&saved_term);
                        eprintln!("\n[tinyvmm] quit (Ctrl-A x)");
                        std::process::exit(0);
                    }
                    b'f' => request_fork_from_host(),
                    _ => forward.push(b), // Ctrl-A Ctrl-A sends one Ctrl-A
                }
            } else if b == 0x01 {
                ctrl_a = true;
            } else {
                forward.push(b);
            }
        }
        if !forward.is_empty() {
            serial.lock().unwrap().push_input(&forward);
        }
    }
}

fn ms(a: Instant, b: Instant) -> f64 {
    (b - a).as_secs_f64() * 1000.0
}
