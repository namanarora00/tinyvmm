//! Milestone 2 demo: fork a running toy VM (`tinyvmm fork-demo`).
//!
//! Design: one VM per process, and fork = Unix fork() of the VMM process.
//!   * fork() gives the child a copy-on-write copy of guest RAM for free: the
//!     kernel shares every page and copies one only when either side writes.
//!   * KVM VM/vCPU fds only work in the process that created them, so the
//!     child builds a fresh VM + vCPU over its (CoW) copy of RAM and loads the
//!     parent's saved registers into it.
//!   * The parent keeps running its original VM untouched.

use crate::kvm::{self, CpuState, Exit, Kvm, REG_PC, Vcpu, VcpuInit, Vm};
use std::io::{self, Write};
use std::time::Instant;

const RAM_BASE: u64 = 0x8000_0000;
// MMIO "devices": no RAM at these addresses, so guest accesses exit to us.
const UART: u64 = 0x0900_0000;
const REPORT: u64 = 0x0900_1000;
const FORK: u64 = 0x0900_2000;
const ROLE: u64 = 0x0900_3000;

const GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fork.bin"));
const MIB: usize = 1 << 20;

struct Args {
    forks: u32,
    mem_mib: usize,
    touch_mib: usize,
    thp: bool,
    quiet: bool,
}

fn parse_args(argv: &[String]) -> Args {
    let mut a = Args { forks: 1, mem_mib: 128, touch_mib: 0, thp: false, quiet: false };
    let mut it = argv.iter().cloned();
    while let Some(flag) = it.next() {
        let mut num = || -> usize { it.next().and_then(|v| v.parse().ok()).expect("flag needs a number") };
        match flag.as_str() {
            "--forks" => a.forks = num() as u32,
            "--mem" => a.mem_mib = num(),
            "--touch" => a.touch_mib = num(),
            "--thp" => a.thp = true,
            "--quiet" => a.quiet = true,
            _ => panic!("usage: tinyvmm fork-demo [--forks N] [--mem MiB] [--touch MiB] [--thp] [--quiet]"),
        }
    }
    assert!(a.touch_mib < a.mem_mib, "--touch must be smaller than --mem");
    a
}

/// Timestamps a child collects on its way to running; reported at its first exit.
#[derive(Clone, Copy)]
struct ChildTimes {
    idx: u64,
    t_request: Instant,   // parent saw the guest's fork request
    t_fork_call: Instant, // parent called fork() for this child
    t_start: Instant,     // fork() returned in the child
    t_vm: Instant,        // new VM created, memory registered
    t_vcpu: Instant,      // vCPU created and initialised
    t_regs: Instant,      // registers loaded
}

/// One running VM: what this process drives.
struct Machine {
    #[allow(dead_code)] // never used after setup, but owns the VM fd for the machine's lifetime
    vm: Vm,
    vcpu: Vcpu,
    role: u64, // 0 = the original VM, i = child i
    pending: Option<ChildTimes>,
}

/// Everything a child needs to rebuild a VM, captured before fork().
struct Template<'a> {
    kvm: &'a Kvm,
    ram: *mut u8,
    ram_size: usize,
    init: VcpuInit,
}

fn build_vm(t: &Template) -> io::Result<Vm> {
    let vm = t.kvm.create_vm()?;
    vm.set_memory(0, RAM_BASE, t.ram, t.ram_size)?;
    Ok(vm)
}

fn build_vcpu(t: &Template, vm: &Vm) -> io::Result<Vcpu> {
    let vcpu = vm.create_vcpu(0)?;
    vcpu.init(&t.init)?;
    Ok(vcpu)
}

/// Fork the running VM into `n` children. Returns Some(child machine) in a
/// child process and None in the parent.
fn fork_vm(t: &Template, m: &Machine, reg_ids: &[u64], n: u64, children: &mut Vec<i32>, parent_stats: &mut [u64; 2]) -> io::Result<Option<Machine>> {
    let t_request = Instant::now();

    // 1. Save the vCPU. It is parked on the `str` to the FORK address: the
    //    parent's next KVM_RUN completes that instruction, but a brand-new
    //    vCPU knows nothing about it and would execute it again (and fork
    //    again, forever). So children start one instruction later.
    let mut cpu: CpuState = m.vcpu.save(reg_ids)?;
    cpu.set_u64(REG_PC, cpu.get_u64(REG_PC) + 4);
    let t_saved = Instant::now();

    // Anything buffered in stdout would be printed twice after fork().
    io::stdout().flush()?;

    for idx in 1..=n {
        let t_fork_call = Instant::now();
        match unsafe { libc::fork() } {
            -1 => return Err(io::Error::last_os_error()),
            0 => {
                // 2. Child: same memory (CoW), but the parent's KVM fds are
                //    dead weight here. Build our own VM over our copy of RAM.
                let t_start = Instant::now();
                let vm = build_vm(t)?;
                let t_vm = Instant::now();
                let vcpu = build_vcpu(t, &vm)?;
                let t_vcpu = Instant::now();
                let failed = vcpu.restore(&cpu);
                for (id, e) in &failed {
                    eprintln!("[child {idx}] could not restore register {id:#x}: {e}");
                }
                let t_regs = Instant::now();
                let times = ChildTimes { idx, t_request, t_fork_call, t_start, t_vm, t_vcpu, t_regs };
                return Ok(Some(Machine { vm, vcpu, role: idx, pending: Some(times) }));
            }
            pid => children.push(pid),
        }
    }
    parent_stats[0] = (t_saved - t_request).as_nanos() as u64;
    parent_stats[1] = t_request.elapsed().as_nanos() as u64;
    Ok(None)
}

pub fn main(argv: &[String]) -> io::Result<()> {
    let args = parse_args(argv);
    let kvm = Kvm::open()?;

    // Guest RAM: private anonymous memory. MAP_PRIVATE is what makes fork()
    // copy-on-write; MAP_SHARED would make parent and child share writes.
    let ram_size = args.mem_mib * MIB;
    let ram = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            ram_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    assert_ne!(ram, libc::MAP_FAILED, "mmap guest RAM");
    let ram = ram as *mut u8;
    if args.thp {
        unsafe { libc::madvise(ram as *mut _, ram_size, libc::MADV_HUGEPAGE) };
    }
    unsafe { std::ptr::copy_nonoverlapping(GUEST.as_ptr(), ram, GUEST.len()) };
    // Simulate a guest that has already used memory (a booted Linux would
    // have tens or hundreds of MiB resident): fork cost grows with it.
    for off in (MIB..args.touch_mib * MIB + MIB).step_by(4096) {
        unsafe { *ram.add(off) = 1 };
    }

    // Build the original VM.
    let probe = kvm.create_vm()?;
    let mut init = probe.preferred_target()?;
    drop(probe);
    init.features[0] |= 1 << kvm::KVM_ARM_VCPU_PSCI_0_2;
    let t = Template { kvm: &kvm, ram, ram_size, init };
    let vm = build_vm(&t)?;
    let vcpu = build_vcpu(&t, &vm)?;
    vcpu.set_u64(REG_PC, RAM_BASE)?;
    let reg_ids = vcpu.reg_list()?;
    let mut m = Machine { vm, vcpu, role: 0, pending: None };

    // Children report their timings through this pipe.
    let mut pipe = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let mut children = Vec::new();
    let mut parent_stats = [0u64; 2];
    let mut line = Vec::new();

    loop {
        let exit = m.vcpu.run()?;
        if let Some(c) = m.pending.take() {
            report_child(pipe[1], &c);
        }
        let who = if m.role == 0 { "parent".to_string() } else { format!("child {}", m.role) };
        match exit {
            Exit::MmioWrite { addr: UART, data, .. } => {
                line.push(data as u8);
                if data as u8 == b'\n' && !args.quiet {
                    print!("[{who}] {}", String::from_utf8_lossy(&line));
                }
                if data as u8 == b'\n' {
                    line.clear();
                }
            }
            Exit::MmioWrite { addr: REPORT, data, .. } => {
                if !args.quiet {
                    println!("[{who}] counter = {data}");
                }
            }
            Exit::MmioWrite { addr: FORK, .. } => {
                if let Some(child) = fork_vm(&t, &m, &reg_ids, args.forks as u64, &mut children, &mut parent_stats)? {
                    m = child; // drops (closes) the parent's VM and vCPU fds in this process
                }
            }
            Exit::MmioRead { addr: ROLE, len } => m.vcpu.set_mmio_read(m.role, len),
            Exit::SystemEvent(_kind) => break, // 1 = power off, 2 = reset
            other => panic!("[{who}] unhandled exit {other:?}"),
        }
    }

    if m.role != 0 {
        return Ok(()); // a child: done
    }
    // Parent: collect every child's timings, then reap them.
    unsafe { libc::close(pipe[1]) };
    let mut records = Vec::new();
    let mut rec = [0u64; 8];
    while unsafe { libc::read(pipe[0], rec.as_mut_ptr() as *mut _, 64) } == 64 {
        records.push(rec);
    }
    for pid in &children {
        let mut status = 0;
        unsafe { libc::waitpid(*pid, &mut status, 0) };
    }
    summarize(&args, &records, &parent_stats, reg_ids.len());
    Ok(())
}

/// Child: send [idx, total, own, fork, vm, vcpu, regs, first_run] in ns.
fn report_child(fd: i32, c: &ChildTimes) {
    let now = Instant::now();
    let ns = |a: Instant, b: Instant| (b - a).as_nanos() as u64;
    let rec = [
        c.idx,
        ns(c.t_request, now),
        ns(c.t_fork_call, now),
        ns(c.t_fork_call, c.t_start),
        ns(c.t_start, c.t_vm),
        ns(c.t_vm, c.t_vcpu),
        ns(c.t_vcpu, c.t_regs),
        ns(c.t_regs, now),
    ];
    unsafe { libc::write(fd, rec.as_ptr() as *const _, 64) };
}

fn summarize(args: &Args, records: &[[u64; 8]], parent: &[u64; 2], nregs: usize) {
    let us = |ns: u64| ns as f64 / 1000.0;
    println!();
    println!(
        "forks: {}  guest RAM: {} MiB  touched: {} MiB  THP: {}",
        records.len(),
        args.mem_mib,
        args.touch_mib,
        if args.thp { "on" } else { "off" }
    );
    println!("parent: save {nregs} registers {:.0} us, paused {:.0} us in total", us(parent[0]), us(parent[1]));
    let names = [
        "fork request -> child running (fan-out)",
        "own fork() call -> child running",
        "  fork() syscall",
        "  create VM + map RAM",
        "  create + init vCPU",
        "  load registers",
        "  first KVM_RUN until first exit",
    ];
    println!("{:<42} {:>9} {:>9} {:>9}", "per child (us)", "min", "median", "max");
    for (i, name) in names.iter().enumerate() {
        let mut v: Vec<u64> = records.iter().map(|r| r[i + 1]).collect();
        v.sort();
        if v.is_empty() {
            continue;
        }
        println!("{:<42} {:>9.0} {:>9.0} {:>9.0}", name, us(v[0]), us(v[v.len() / 2]), us(v[v.len() - 1]));
    }
}
