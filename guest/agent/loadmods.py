"""Load kernel modules listed in /mods/order ("file [params]" per line) with
the finit_module syscall: the Python image has no insmod. Used when the guest
kernel builds virtio_mmio / virtio_console as modules instead of built in."""
import ctypes, os
libc = ctypes.CDLL(None, use_errno=True)
SYS_FINIT_MODULE = 313 if os.uname().machine == "x86_64" else 273
for line in open("/mods/order"):
    name, _, params = line.strip().partition(" ")
    if not name:
        continue
    fd = os.open(f"/mods/{name}", os.O_RDONLY)
    flags = 4 if name.endswith((".zst", ".xz", ".gz")) else 0  # MODULE_INIT_COMPRESSED_FILE
    if libc.syscall(SYS_FINIT_MODULE, fd, params.encode(), flags) != 0:
        err = ctypes.get_errno()
        if err != 17:  # EEXIST: already loaded / built in
            print(f"loadmods: {name}: {os.strerror(err)}")
    os.close(fd)
