# tinyvmm

A small virtual machine monitor written from scratch on raw KVM ioctls, for
**arm64 and x86_64**, that can **fork a running Linux VM**: processes, files
and all. It was built to answer one question: is forking a prepared VM a
good way to run RL environments, where a trainer needs many identical,
isolated copies of the same starting state (a GRPO group) over and over?

No KVM crates, no QEMU, no firmware: about 3,200 lines of Rust (2,600
without comments and blank lines), plus a few small scripts.

## What it does

- **Boots Linux** on arm64 (device tree, GICv3, PSCI) and x86_64 (bzImage
  64-bit boot protocol, in-kernel PIC/IOAPIC/LAPIC and PIT).
- **Forks a running VM** in tens of milliseconds (18–95 ms in our runs,
  depending on the host and how much guest memory is in use). Each child is a
  full VM with its own kernel, and inherits the parent's memory
  (copy-on-write), running processes, open files and device state. Writes
  stay private to each child.
- **Agent mode**: a virtio console plus an agent inside the guest, so a
  harness can send files and commands to any VM and ask for `fork N` over a
  Unix socket. Every fork child gets its own sockets.
- **Benchmarks** against Docker (fresh container per episode, and a warm
  container pool) and against Firecracker.

## Results

### Grading RL rollout groups (x86_64)

Eight bug-fixing tasks from [minirl](https://github.com/namanarora00/minirl),
eight candidate fixes per task (one correct, seven wrong, so the right
rewards are known in advance). Each candidate runs the task's hidden tests in
an isolated environment. Measured on a GCP `n1-highcpu-4` spot VM with nested
KVM, all three backends on the same machine (`tools/rl_bench.py`):

| Grade one group of 8 | Median time | Isolation between episodes |
|---|---|---|
| **tinyvmm**: fork 8 children of a prepared VM, one candidate each | **704 ms** (fork 54 ms + run 654 ms) | a full VM with its own kernel |
| Docker, a fresh container per candidate | 2,144 ms | a container on the shared host kernel |
| Docker, 8 warm containers reused across candidates | 353 ms | none within a container |

All three backends returned the correct reward for all 64 candidates. Each
fork child used about 7.4 MiB of private memory (the rest is shared with the
template VM); a fresh container peaked at about 4.9 MiB.

So forking a prepared VM was **about 3x faster than a fresh container per
episode**, with stronger isolation. A shared container pool is faster still,
but one episode can leave state behind for the next.

### Fork children under load

Time for the first command in fresh fork children (`tools/debug_scaling.py`):

| Children at once | x86_64, GCP nested KVM (4 cores) | arm64, KVM nested inside a VM on an Apple M4 |
|---|---|---|
| 1 | 190 ms | 1,600 ms |
| 8 | 534 ms each | 14,800 ms each |

A fresh child faults in every page it touches. On GCP this cost grows
linearly with the number of children. Under Apple's nested virtualization it
grew roughly quadratically (the time shows up as guest time, in the shadow
memory mappings the host hypervisor keeps for nested VMs), which made the
Mac unusable for this workload. That is a property of the nested setup, not
of KVM on real hardware.

### Against Firecracker (arm64, on the Mac)

Same kernel and initramfs, 1 vCPU, 512 MiB, fork vs snapshot restore
(`tools/bench.py`, 2–3 runs each, so treat as rough):

| | tinyvmm (fork, huge pages) | Firecracker (snapshot + restore) |
|---|---|---|
| Original VM paused, 4 clones | 8 ms | 318 ms (writing a 512 MiB snapshot) |
| Request → 4 clones running (median) | 57 ms | 541 ms |
| Host RAM, original + 4 clones | 156 MiB | 608 MiB (incl. the snapshot file in tmpfs) |

Firecracker is faster at starting clones from a snapshot that already
exists (10–31 ms for one clone), which tinyvmm has no equivalent of yet.

## How it works

**One VM per process, and fork is Unix `fork()`.** Guest RAM is private
anonymous memory in the VMM process, so `fork()` gives each child a
copy-on-write copy for free, along with all device state the VMM emulates
(the UART, the virtio console). KVM objects can't be shared across
processes, so each child creates a fresh VM and vCPU and loads the state
that lives inside KVM, saved by the parent just before the fork:

| | arm64 | x86_64 |
|---|---|---|
| vCPU | all 259 registers from `KVM_GET_REG_LIST` (incl. the timer) | general, segment/control, xsave, xcrs, debug registers, MSRs, local APIC, pending events, mp state |
| VM-wide | GICv3 distributor, redistributor, CPU interface | PIC master/slave, IOAPIC, PIT, kvmclock |

Before saving, the parent runs the vCPU once with `immediate_exit` set, so
any half-finished MMIO or port I/O instruction completes and nothing else
runs. The UART and virtio locks are held across `fork()` so no other thread
can be mid-update in the child's copy.

**Devices.** A 16550 UART (console) and a virtio-mmio console with split
virtqueues. The virtio console carries the agent protocol: each request
crosses as one buffer per exit, instead of one exit per byte as on the
UART. The guest finds the devices through the device tree on arm64, and
through the kernel command line on x86_64.

**Agent mode.** The guest is the `python:3.12-slim` filesystem, unpacked
into RAM as an initramfs, with a small agent on `/dev/hvc0`. The protocol is
length-prefixed JSON both ways. Each VM listens on two sockets:

```
<dir>/vm-<name>.sock   the stream to the agent: {"files": {...}, "cmd": "...", "cwd": "..."}
<dir>/vm-<name>.ctl    control: "fork N" -> "ok <child names>", "quit"
```

The guest filesystem lives in memory, so every fork child shares it
copy-on-write. A fresh environment per episode copies nothing up front.

## Layout

```
src/kvm.rs        KVM ioctls and struct layouts, by hand (shared)
src/boot.rs       arm64: boot, device tree, GIC, fork, agent mode
src/x86.rs        x86_64: bzImage boot, irqchip, fork, agent mode
src/gic.rs        arm64 GICv3 save/restore
src/fdt.rs        device tree writer
src/serial.rs     16550 UART
src/virtio.rs     virtio-mmio transport + console
src/agent.rs      per-VM Unix sockets (agent stream + control)
src/forkdemo.rs   arm64 toy guest that forks itself (milestone 2)
guest/            guest init scripts, agent, rootfs/kernel build scripts
tools/            Python client (rlenv.py), benchmarks, debugging tools
```

## Running it

You need Linux with `/dev/kvm` (arm64 or x86_64), Rust, and Docker (to
build the guest filesystem and for the Docker baselines).

```bash
cargo build --release
```

Guest kernel and filesystem:

- **x86_64:** `guest/x86/bootstrap.sh` installs the tools, copies the host's
  own kernel to `~/guest/bzImage`, and pulls `python:3.12-slim`. It needs a
  kernel with virtio-mmio and its command-line devices built in, as Ubuntu's
  GCP kernels have.
- **arm64:** `guest/linux/prepare.sh` unpacks Ubuntu's arm64 kernel into
  `~/guest/Image` (plus a busybox initramfs for the non-agent demos).
- **Both:** `guest/agent/prepare.sh` builds `~/guest/initramfs-py.cpio`.

Then, from Python (`tools/rlenv.py`):

```python
from rlenv import TinyVM

vm = TinyVM.boot("/tmp/vms")           # boot once: kernel + Python rootfs + agent
vm.exec("python3 -c 'print(6*7)'")     # -> {"exit": 0, "output": "42\n", ...}
kids = vm.fork(8)                      # 8 copies of this exact running VM
kids[0].exec("pytest -q", files={"solution.py": code})
for k in kids:
    k.quit()
```

Benchmarks (`MINIRL` points at a minirl checkout; default `../minirl`):

```bash
python3 tools/agent_smoke.py       # boot, exec, fork, isolation checks
python3 tools/debug_scaling.py     # first command in 1/2/4/8 fresh children
python3 tools/rl_bench.py 8        # tinyvmm vs fresh Docker vs Docker pool
```

On a Mac, `run.sh` builds and runs inside a Colima VM. That needs an M3 or
newer chip and Colima's `nestedVirtualization: true`. As shown above, the
nested setup makes fork children very slow there.

## Limitations

- Every number above comes from nested virtualization (KVM inside a cloud VM
  or a Colima VM). Bare-metal numbers are not measured.
- One vCPU per VM. No block or network devices yet: the whole environment
  lives in an initramfs.
- The parent pauses for the first `fork()`, roughly in proportion to its
  resident memory (tens of ms for a ~450 MB guest).
- Fork children start with the same random-number state. Nothing tells the
  guest it was cloned yet (no vmgenid), so "independent" rollouts can draw
  the same random numbers.
- x86_64: userspace output to the guest console doesn't show up yet (the
  UART's interrupt path), the 8250 driver's loopback probe prints junk, and
  guests need 2 GB of RAM to unpack the 117 MB filesystem.
- The VMM itself is not hardened: no seccomp filters, no jailer, no rate
  limits. Don't run untrusted guests with it outside a throwaway machine.
