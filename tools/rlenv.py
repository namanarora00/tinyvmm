"""Client for tinyvmm agent-mode VMs, as an RL harness would use them.
Runs inside the Colima VM (needs KVM).

    vm = TinyVM.boot("/tmp/rlenv")          # boot once: kernel + Python rootfs + agent
    vm.exec("python3 -c 'print(1)'")         # files + command -> output
    kids = vm.fork(8)                        # 8 copies of this exact running VM
    kids[0].exec(...)                        # each one independent from here on
    for k in kids: k.quit()
"""

import json
import os
import platform
import socket
import struct
import subprocess
import time

HOME = os.environ["HOME"]
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ARGS = "console=ttyS0 reboot=k panic=-1 rdinit=/init quiet earlycon=uart8250,mmio,0x9000000"


def find_tinyvmm():
    """The tinyvmm binary: $TINYVMM if set, else the first release build found
    in $CARGO_TARGET_DIR, this repo's target/ (plain `cargo build --release`),
    or ~/tinyvmm-target (where run.sh builds inside Colima)."""
    if os.environ.get("TINYVMM"):
        return os.environ["TINYVMM"]
    dirs = [os.environ.get("CARGO_TARGET_DIR"), os.path.join(REPO, "target"), f"{HOME}/tinyvmm-target"]
    paths = [os.path.join(d, "release", "tinyvmm") for d in dirs if d]
    for p in paths:
        if os.access(p, os.X_OK):
            return p
    raise FileNotFoundError(f"no tinyvmm binary (looked in {', '.join(paths)}): run `cargo build --release` or set TINYVMM")


def connect(path, timeout=30):
    deadline = time.time() + timeout
    while True:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(path)
            return s
        except OSError:
            if time.time() > deadline:
                raise
            time.sleep(0.001)


class TinyVM:
    def __init__(self, sock_dir, name="root"):
        self.dir, self.name = sock_dir, name
        self.conn = connect(f"{sock_dir}/vm-{name}.sock")
        self.ctl = connect(f"{sock_dir}/vm-{name}.ctl")
        self.ctl_lines = self.ctl.makefile("r")
        self.next_id = 0
        self.proc = None

    @classmethod
    def boot(cls, sock_dir, mem=None, thp=True, initrd=f"{HOME}/guest/initramfs-py.cpio", log=None):
        os.makedirs(sock_dir, exist_ok=True)
        if mem is None:
            # x86: the kernel defers setting up half of RAM until later in
            # boot, and a 117 MB initramfs doesn't unpack in 1 GB.
            mem = 2048 if platform.machine() == "x86_64" else 1024
        argv = [find_tinyvmm(), "boot", "--agent", "--mem", str(mem), "--initrd", initrd, "--log-dir", sock_dir]
        if platform.machine() != "x86_64":
            argv += ["--cmdline", ARGS]  # arm64; on x86 tinyvmm's default command line is right
        if thp:
            argv.append("--thp")
        t0 = time.monotonic()
        proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log or subprocess.DEVNULL, stderr=subprocess.STDOUT)
        vm = cls(sock_dir, "root")
        vm.proc = proc
        hello = vm.recv()  # the agent's first frame, sent once it is up
        assert hello.get("ready"), hello
        vm.boot_ms = (time.monotonic() - t0) * 1000
        return vm

    def send(self, obj):
        data = json.dumps(obj).encode()
        self.conn.sendall(struct.pack(">I", len(data)) + data)

    def _recv_exact(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.conn.recv(n - len(buf))
            if not chunk:
                raise ConnectionError(f"vm {self.name}: console closed")
            buf += chunk
        return buf

    def recv(self):
        (n,) = struct.unpack(">I", self._recv_exact(4))
        return json.loads(self._recv_exact(n))

    def exec(self, cmd=None, files=None, cwd="/work", timeout=10):
        self.next_id += 1
        self.send({"id": self.next_id, "cmd": cmd, "files": files or {}, "cwd": cwd, "timeout": timeout})
        reply = self.recv()
        assert reply.get("id") == self.next_id, reply
        return reply

    def fork(self, n):
        """n copies of this VM, running from this exact moment. Don't fork
        with a request in flight: the children would inherit it."""
        self.ctl.sendall(f"fork {n}\n".encode())
        line = self.ctl_lines.readline().split()
        assert line and line[0] == "ok", line
        return [TinyVM(self.dir, name) for name in line[1:]]

    def quit(self):
        try:
            self.ctl.sendall(b"quit\n")
            self.ctl_lines.readline()
        except OSError:
            pass
        for s in (self.conn, self.ctl):
            s.close()
        if self.proc:
            self.proc.wait(timeout=10)
