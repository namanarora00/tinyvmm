#!/bin/sh
# One-shot setup of a throwaway x86 GCP VM for tinyvmm: tools, Rust, Docker,
# the host's own kernel as the guest kernel, and the guest's Python image.
set -e
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential docker.io cpio >/dev/null
sudo chmod 666 /dev/kvm /var/run/docker.sock   # throwaway VM: skip group re-login
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -q >/dev/null 2>&1
mkdir -p ~/guest
K=/boot/vmlinuz-$(uname -r)
sudo cp "$K" ~/guest/bzImage && sudo chown "$USER" ~/guest/bzImage
echo "guest kernel: $(uname -r)"
grep -E "^CONFIG_(VIRTIO|VIRTIO_MMIO|VIRTIO_MMIO_CMDLINE_DEVICES|VIRTIO_CONSOLE|SERIAL_8250|SERIAL_8250_CONSOLE|DEVTMPFS|BLK_DEV_INITRD|MODULE_DECOMPRESS)=" /boot/config-$(uname -r) || true
docker pull -q python:3.12-slim >/dev/null && echo "python:3.12-slim pulled"
