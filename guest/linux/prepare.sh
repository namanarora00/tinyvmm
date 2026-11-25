#!/bin/sh
# Run inside the Colima VM. Produces ~/guest/Image and ~/guest/initramfs.cpio.
#
# Kernel: Ubuntu's own arm64 kernel from /boot (no download). Ubuntu ships it
# gzip-compressed; arm64 kernels can't decompress themselves without EFI, so we
# unpack it to the raw Image the arm64 boot protocol expects.
# Initramfs: busybox (static) + our /init, packed as a newc cpio.
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HOME/guest
mkdir -p "$OUT"
KVER=$(uname -r)
sudo cat "/boot/vmlinuz-$KVER" | gunzip > "$OUT/Image"

STAGE=$(mktemp -d)
mkdir -p "$STAGE/bin" "$STAGE/dev" "$STAGE/proc" "$STAGE/sys" "$STAGE/tmp"
cp /usr/bin/busybox "$STAGE/bin/busybox"
install -m 755 "$HERE/init" "$STAGE/init"
install -m 755 "$HERE/fork-demo.sh" "$STAGE/fork-demo.sh"
install -m 755 "$HERE/heartbeat.sh" "$STAGE/heartbeat.sh"
gcc -static -O2 -o "$STAGE/bin/forkme" "$HERE/forkme.c"
# The kernel opens /dev/console for init's stdio before devtmpfs is mounted,
# so the node must exist inside the archive. mknod needs root.
sudo mknod -m 600 "$STAGE/dev/console" c 5 1
(cd "$STAGE" && sudo find . | sudo cpio -o -H newc --quiet) > "$OUT/initramfs.cpio"
sudo rm -rf "$STAGE"
ls -la "$OUT"
