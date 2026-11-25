# tinyvmm

A small VMM on raw KVM (arm64 + x86_64) that can **fork a running Linux VM**,
processes and all. Built to test one idea: is forking a prepared VM a good
way to give an RL trainer many identical, isolated environments (a GRPO
group)?

About 3,200 lines of Rust. No KVM crates, no QEMU, no firmware.

## Results

Grading one GRPO group: 8 candidate fixes for a bug-fixing task from
[minirl](https://github.com/namanarora00/minirl), each run against hidden
tests in its own environment. Median over 8 tasks on a GCP `n1-highcpu-4`
with nested KVM (`tools/rl_bench.py`):

| Backend | Time per group | Isolation between episodes |
|---|---|---|
| **tinyvmm**: fork 8 children of a prepared VM | **704 ms** | separate VMs, separate kernels |
| Docker: a fresh container per candidate | 2,144 ms | containers sharing the host kernel |
| Docker: 8 warm containers reused | 353 ms | none within a container |

All three got every reward right (64/64). Each fork child used ~7.4 MiB of
private memory; everything else is shared with the template.

On an Apple M4, where KVM runs nested inside a Colima VM, the same children
were 8–28x slower, and the slowdown grew quadratically with concurrent VMs.
That comes from Apple's nested virtualization, not from KVM. Against
Firecracker on the Mac, forking 4 clones paused the original VM for 8 ms
(vs 318 ms to write a snapshot) and used 156 MiB of host RAM (vs 608 MiB).

## How it works

- **Fork is Unix `fork()`.** Guest RAM is private memory in the VMM process,
  so children get it copy-on-write, along with the emulated devices. Each
  child builds a fresh KVM VM and loads the state that lives in KVM: vCPU
  registers, timer and interrupt controller (GICv3 on arm64; MSRs, LAPIC,
  PIC/IOAPIC, PIT and kvmclock on x86).
- **Agent mode.** A virtio console connects to an agent inside the guest,
  which runs on a `python:3.12-slim` filesystem held in RAM. Each VM listens
  on two Unix sockets: one for files and commands, one for `fork N`.

```python
from rlenv import TinyVM                  # tools/rlenv.py

vm = TinyVM.boot("/tmp/vms")              # kernel + Python rootfs + agent
kids = vm.fork(8)                         # 8 copies of this running VM
kids[0].exec("pytest -q", files={"solution.py": code})
```

## Running

Needs Linux with `/dev/kvm`, Rust and Docker.

```bash
cargo build --release
guest/x86/bootstrap.sh          # x86_64: tools + host kernel as ~/guest/bzImage
guest/linux/prepare.sh          # arm64: Ubuntu kernel as ~/guest/Image
guest/agent/prepare.sh          # both: ~/guest/initramfs-py.cpio
python3 tools/agent_smoke.py    # boot, exec, fork, isolation
python3 tools/rl_bench.py 8     # vs Docker (MINIRL=path to minirl, default ../minirl)
```

## Limitations

- All numbers come from nested virtualization. Bare metal is unmeasured.
- One vCPU, no disk or network devices.
- Fork children share random-number state (no vmgenid yet).
- x86: guest userspace output to the console doesn't show up yet.
- Not hardened (no seccomp, no jailer).

## License

MIT, see [LICENSE](LICENSE).
