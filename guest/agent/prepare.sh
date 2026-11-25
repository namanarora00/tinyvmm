#!/bin/sh
# Run inside the Colima VM. Builds ~/guest/initramfs-py.cpio: the whole
# python:3.12-slim filesystem plus the agent, as an initramfs. The guest runs
# entirely from RAM, so after a VM fork every child shares this filesystem
# copy-on-write: a fresh environment per episode costs no disk and no copying.
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HOME/guest
mkdir -p "$OUT"
STAGE=$(mktemp -d)
CID=$(docker create python:3.12-slim)
docker export "$CID" | sudo tar -x -C "$STAGE"
docker rm "$CID" >/dev/null
sudo install -m 755 "$HERE/init" "$STAGE/init"
sudo install -m 755 "$HERE/agent.py" "$STAGE/agent.py"
sudo install -m 644 "$HERE/loadmods.py" "$STAGE/loadmods.py"
# Optional kernel modules: MODS_DIR holds the .ko files and an "order" file.
if [ -n "$MODS_DIR" ] && [ -f "$MODS_DIR/order" ]; then sudo mkdir -p "$STAGE/mods" && sudo cp "$MODS_DIR"/* "$STAGE/mods/"; fi
[ -e "$STAGE/dev/console" ] || sudo mknod -m 600 "$STAGE/dev/console" c 5 1
(cd "$STAGE" && sudo find . | sudo cpio -o -H newc --quiet) > "$OUT/initramfs-py.cpio"
sudo rm -rf "$STAGE"
ls -la "$OUT/initramfs-py.cpio"
