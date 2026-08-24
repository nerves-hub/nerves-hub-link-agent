#!/bin/bash
# Runs inside the device container: turn the exported filesystem into an ext4
# image, package it, and leave the kernel where QEMU can find it.
set -euo pipefail

: "${NH_PRODUCT:?}" "${NH_VERSION:?}" "${NH_PLATFORM:?}" "${NH_ARCHITECTURE:?}"

apt-get update -qq >/dev/null
apt-get install -y -qq e2fsprogs >/dev/null

mkdir -p /tmp/root
tar -xf /out/rootfs.tar -C /tmp/root

# The ops archive ships *inside* the firmware. `fwup -t validate` is how the
# agent marks a boot good, and a device keeps no copy of its own firmware to run
# that task from, so the task has to arrive as its own small archive. Built
# before mke2fs so it lands in the rootfs image rather than beside it.
#
# Both configs state where the environment lives, and a disagreement would show
# up as a bad CRC rather than as a mistake, so check it instead of trusting the
# comment in ops.conf that says to.
for var in UBOOT_ENV_OFFSET UBOOT_ENV_COUNT; do
    from_fwup=$(sed -n "s/^define($var, \([0-9]*\))/\1/p" /test/device/fwup.conf)
    from_ops=$(sed -n "s/^define($var, \([0-9]*\))/\1/p" /test/device/ops.conf)

    if [ -z "$from_fwup" ] || [ "$from_fwup" != "$from_ops" ]; then
        echo "ops.conf $var=$from_ops does not match fwup.conf $var=$from_fwup" >&2
        exit 1
    fi
done

mkdir -p /tmp/root/usr/share/fwup
fwup -c -f /test/device/ops.conf -o /tmp/root/usr/share/fwup/ops.fw

# The kernel and initrd travel with the rootfs *and* come out separately: they
# are in the image because a real bootloader would read them from it, and they
# are on the host because QEMU is standing in for that bootloader.
cp /tmp/root/boot/vmlinuz-* /out/vmlinuz
cp /tmp/root/boot/initrd.img-* /out/initrd.img

# `mke2fs -d` populates the filesystem from a directory without a loop mount and
# without root on the host — the thing that makes this buildable in a container
# at all. 1000 MB, inside the 1 GB slot the fwup.conf reserves.
rm -f /out/rootfs.ext4
mke2fs -q -t ext4 -b 4096 -d /tmp/root -F /out/rootfs.ext4 1000M

# /data, seeded with the device identifier.
#
# On a real product this is written at manufacture, not baked into an image —
# otherwise every device shipped is the same device. It is baked here because
# the rig is one VM and provisioning is not what this is testing.
#
# The product key and secret are *not* here: a product shared secret is shared
# by design, so it belongs in the firmware. A per-device certificate would not.
rm -rf /tmp/data /out/data.ext4
mkdir -p /tmp/data/nerves-hub
echo "${NH_IDENTIFIER:-qemu-device-01}" > /tmp/data/nerves-hub/identifier
mke2fs -q -t ext4 -b 4096 -d /tmp/data -F /out/data.ext4 256M

signing=()
if [ -f /keys/agent-test.priv ]; then
    echo "==> signing with agent-test"
    signing=(--private-key-file /keys/agent-test.priv)
fi

out="/out/${NH_PRODUCT}-${NH_VERSION}.fw"

ROOTFS_PATH=/out/rootfs.ext4 \
DATA_PATH=/out/data.ext4 \
    fwup -c -f /test/device/fwup.conf "${signing[@]}" -o "$out"

echo
echo "==> metadata"
fwup -m -i "$out"
