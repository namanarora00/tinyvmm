#!/usr/bin/env python3
"""Command agent inside the guest. Talks to the harness over the virtio console.

Protocol, both directions: a 4-byte big-endian length, then that many bytes
of JSON. Requests:
    {"id": 1, "files": {"solution.py": "..."}, "cmd": "python run_tests.py tests",
     "cwd": "/work/task", "timeout": 10}
Each file is written (relative to cwd), then cmd runs in a shell, and the
reply is {"id": 1, "exit": 0, "output": "...", "ms": 12.3}. On start-up the
agent sends {"ready": true}.

It runs as a child of PID 1 and lives on across VM forks: a fork child's
agent is this same process, blocked on the same read, in a copy of memory.
"""
import json
import os
import struct
import subprocess
import time
import tty

fd = os.open("/dev/hvc0", os.O_RDWR | os.O_NOCTTY)
tty.setraw(fd)  # no echo, no line editing, no newline translation


def read_exact(n):
    buf = b""
    while len(buf) < n:
        chunk = os.read(fd, n - len(buf))
        if not chunk:
            raise SystemExit("console closed")
        buf += chunk
    return buf


def send(obj):
    data = json.dumps(obj).encode()
    frame = struct.pack(">I", len(data)) + data
    while frame:
        frame = frame[os.write(fd, frame):]


send({"ready": True, "pid": os.getpid()})
while True:
    (n,) = struct.unpack(">I", read_exact(4))
    req = json.loads(read_exact(n))
    t0 = time.monotonic()
    cwd = req.get("cwd", "/work")
    os.makedirs(cwd, exist_ok=True)
    for path, text in req.get("files", {}).items():
        with open(os.path.join(cwd, path), "w") as f:
            f.write(text)
    out, code = "", 0
    if req.get("cmd"):
        try:
            p = subprocess.run(req["cmd"], shell=True, cwd=cwd, capture_output=True, timeout=req.get("timeout", 10))
            out, code = (p.stdout + p.stderr).decode(errors="replace"), p.returncode
        except subprocess.TimeoutExpired:
            out, code = "[timed out]", 124
    send({"id": req.get("id"), "exit": code, "output": out[-20000:], "ms": (time.monotonic() - t0) * 1000})
