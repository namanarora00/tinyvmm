//! tinyvmm: a tiny virtual machine monitor on raw KVM, for arm64 and x86_64.
//!
//!   tinyvmm boot [...]       boot a Linux kernel (arm64: boot.rs, x86_64: x86.rs)
//!   tinyvmm fork-demo [...]  fork a running toy VM (arm64 only)

mod agent;
#[cfg(target_arch = "aarch64")]
mod boot;
#[cfg(target_arch = "aarch64")]
mod fdt;
#[cfg(target_arch = "aarch64")]
mod forkdemo;
#[cfg(target_arch = "aarch64")]
mod gic;
#[allow(dead_code)] // each architecture uses only part of it
mod kvm;
mod serial;
mod virtio;
#[cfg(target_arch = "x86_64")]
mod x86;

use std::time::Instant;

fn main() {
    let process_start = Instant::now();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let result = match argv.first().map(String::as_str) {
        #[cfg(target_arch = "aarch64")]
        Some("boot") => boot::main(&argv[1..], process_start),
        #[cfg(target_arch = "aarch64")]
        Some("fork-demo") => forkdemo::main(&argv[1..]),
        #[cfg(target_arch = "x86_64")]
        Some("boot") => x86::main(&argv[1..], process_start),
        _ => {
            eprintln!("usage: tinyvmm boot [...] | tinyvmm fork-demo [...]");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("tinyvmm: {e}");
        std::process::exit(1);
    }
}
