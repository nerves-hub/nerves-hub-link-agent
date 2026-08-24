#!/bin/bash
# Demonstrates that fwup does real A/B against a plain file, no privileges.
set -euo pipefail

work=$(mktemp -d)
cd "$work"

# Which slot is live, read straight out of the MBR.
#
# Partition entry 0 lives at byte 446; its start-LBA is the little-endian u32 at
# offset 8 within it. Read directly rather than shelling out to fdisk, so this
# needs nothing installed and works as an unprivileged user.
live() {
    local lba
    lba=$(od -An -tu4 -j454 -N4 disk.img | tr -d ' ')

    case "$lba" in
        2048) echo "A" ;;
        4096) echo "B" ;;
        *)    echo "? (start lba $lba)" ;;
    esac
}

build() {
    printf 'rootfs version %s\n' "$1" > rootfs.bin
    # Pad so the payload is a whole number of blocks.
    truncate -s 1M rootfs.bin
    # Absolute: fwup resolves host-path relative to the config file, not to cwd.
    ROOTFS_PATH="$work/rootfs.bin" \
    NH_PRODUCT="${NH_PRODUCT:-SmartKiosk}" \
    NH_VERSION="0.$1.0" \
    NH_PLATFORM="${NH_PLATFORM:-platform}" \
    NH_ARCHITECTURE="${NH_ARCHITECTURE:-x86_64}" \
        fwup -c -f /work/test/ab/fwup.conf -o "v$1.fw"
}

echo "=== building three firmware archives ==="

build 1; build 2; build 3

echo
echo "=== factory write (task complete) ==="
fwup -a -d disk.img -i v1.fw -t complete --quiet
echo "live slot: $(live)   rootfs says: $(dd if=disk.img bs=512 skip=2048 count=1 2>/dev/null | head -1)"

echo
echo "=== upgrade to v2 (task upgrade — fwup chooses the free slot) ==="
fwup -a -d disk.img -i v2.fw -t upgrade --quiet
echo "live slot: $(live)   slot B holds: $(dd if=disk.img bs=512 skip=4096 count=1 2>/dev/null | head -1)"
echo "                     slot A still holds: $(dd if=disk.img bs=512 skip=2048 count=1 2>/dev/null | head -1)"

echo
echo "=== upgrade to v3 (should go back to A) ==="
fwup -a -d disk.img -i v3.fw -t upgrade --quiet
echo "live slot: $(live)   slot A holds: $(dd if=disk.img bs=512 skip=2048 count=1 2>/dev/null | head -1)"
echo "                     slot B still holds: $(dd if=disk.img bs=512 skip=4096 count=1 2>/dev/null | head -1)"

echo
echo "=== metadata NervesHub will read ==="
fwup -m -i v3.fw

echo
echo "=== both slots, side by side ==="
echo "  A (lba 2048): $(dd if=disk.img bs=512 skip=2048 count=1 2>/dev/null | head -1)"
echo "  B (lba 4096): $(dd if=disk.img bs=512 skip=4096 count=1 2>/dev/null | head -1)"
echo "  live slot:    $(live)"
