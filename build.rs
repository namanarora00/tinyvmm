// Assemble each guest program in guest/*.S into a flat binary in OUT_DIR,
// which src/main.rs embeds with include_bytes!. Flat = raw instructions,
// no ELF headers: the vCPU just starts executing the first byte.
use std::process::Command;

fn run(cmd: &mut Command) {
    let status = cmd.status().expect("failed to run assembler tools (need binutils)");
    assert!(status.success(), "{cmd:?} failed");
}

fn main() {
    // The toy guests are arm64 assembly; only the arm64 build embeds them.
    if std::env::var("CARGO_CFG_TARGET_ARCH").unwrap() != "aarch64" {
        return;
    }
    let out = std::env::var("OUT_DIR").unwrap();
    for name in ["fork"] {
        let src = format!("guest/{name}.S");
        println!("cargo:rerun-if-changed={src}");
        run(Command::new("as").args([&src, "-o", &format!("{out}/{name}.o")]));
        run(Command::new("objcopy").args([
            "-O",
            "binary",
            &format!("{out}/{name}.o"),
            &format!("{out}/{name}.bin"),
        ]));
    }
}
