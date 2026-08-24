#!/bin/bash
# Boot the A/B image under QEMU.
#
#   ./test/device/run-qemu.sh            # interactive, ctrl-a x to quit
#   ./test/device/run-qemu.sh --headless # log to tmp/device/console.log
#
# hvf, so an arm64 guest on an arm64 host runs at close to native speed rather
# than being emulated instruction by instruction.
#
# u-boot is built from source with CONFIG_BOOTCOUNT_LIMIT — Debian's package has
# it off, which means no counted rollback. See test/device/uboot.config.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="$root/tmp/device"

# The board's flash, seeded once. Not part of the firmware image: a real board
# has its bootloader in flash before any firmware is written, and wiping it is
# a bench operation rather than something an update does.
if [ ! -f "$out/flash0.img" ]; then
    echo "seeding flash from the device image"
    container=$(docker create "${DEVICE_IMAGE:-nerves-hub-link-agent:device}")
    # The rig's own u-boot: they differ in where the environment lives.
    if [ "${BOOT_SCHEME:-fwup}" = "rauc" ]; then
        docker cp "$container:/usr/lib/nerves-hub/u-boot-rauc.bin" "$out/u-boot.bin" > /dev/null
    else
        docker cp "$container:/usr/lib/nerves-hub/u-boot.bin" "$out/u-boot.bin" > /dev/null
    fi
    docker rm "$container" > /dev/null

    dd if=/dev/zero of="$out/flash0.img" bs=1m count=64 2>/dev/null
    dd if="$out/u-boot.bin" of="$out/flash0.img" conv=notrunc 2>/dev/null
    dd if=/dev/zero of="$out/flash1.img" bs=1m count=64 2>/dev/null
fi

# The RAUC rig keeps u-boot's environment on an SD card, because that is the
# only store both u-boot and Linux can reach on this machine. 16 MB is far more
# than the environment needs and the smallest size that is not a fiddle.
if [ "${BOOT_SCHEME:-fwup}" = "rauc" ] && [ ! -f "$out/sd.img" ]; then
    dd if=/dev/zero of="$out/sd.img" bs=1m count=16 2>/dev/null
fi

args=(
    -M virt
    -accel hvf
    -cpu host
    -smp 2
    -m 2048
    # Real u-boot, which reads the environment off the disk and decides which
    # slot to boot. Nothing here names a kernel or a slot — that is the whole
    # difference from booting `-kernel` directly, and it is what makes rollback
    # possible at all.
    # Two pflash banks: u-boot in the first, its environment in the second.
    # The environment bank is a file so it persists, which is what lets u-boot
    # keep a boot counter across resets — the whole basis of rollback.
    -drive "if=pflash,format=raw,index=0,file=$out/flash0.img"
    -drive "if=pflash,format=raw,index=1,file=$out/flash1.img"
    -drive "file=$out/disk.img,format=raw,if=virtio"
    # User-mode networking: the guest reaches the host's LAN address directly,
    # so the agent config can name the same IP as everywhere else.
    # 2222 on the host reaches sshd in the guest, which is what makes the rig
    # scriptable rather than something to be typed at.
    -netdev user,id=net0,hostfwd=tcp::2222-:22
    -device virtio-net-pci,netdev=net0
    -nographic
)

# An SD card for the RAUC rig's u-boot environment. Linux sees the same card as
# /dev/mmcblk0, which is how `fw_setenv` and u-boot end up writing one store.
if [ "${BOOT_SCHEME:-fwup}" = "rauc" ]; then
    args+=(
        -device sdhci-pci,id=sdhci
        -drive "if=none,id=sd0,file=$out/sd.img,format=raw"
        -device sd-card,drive=sd0
    )
fi

if [ "${1:-}" = "--headless" ]; then
    exec qemu-system-aarch64 "${args[@]}" < /dev/null > "$out/console.log" 2>&1
fi

exec qemu-system-aarch64 "${args[@]}"
