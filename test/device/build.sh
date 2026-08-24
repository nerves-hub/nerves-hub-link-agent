#!/bin/bash
# Build a bootable A/B firmware image, and pull out the kernel QEMU needs.
#
#   ./test/device/build.sh 3.0.0
#
# Produces, in tmp/device/:
#   SmartKiosk-<version>.fw   the firmware, signed if a key is present
#
# The kernel is *not* pulled out: u-boot loads it from whichever slot it decides
# to boot, which is the only arrangement where A/B actually replaces the kernel
# too.
set -euo pipefail

version="${1:-3.0.0}"
product="${NH_PRODUCT:-SmartKiosk}"
image="${DEVICE_IMAGE:-nerves-hub-link-agent:device}"

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="$root/tmp/device"
key="$root/tmp/keys"
mkdir -p "$out"

# u-boot, from Debian's package. Fetched once and cached.
if [ ! -f "$root/tmp/uboot/u-boot.bin" ]; then
    echo "==> fetching u-boot"
    mkdir -p "$root/tmp/uboot"
    docker run --rm -v "$root/tmp/uboot:/out" --entrypoint bash debian:bookworm-slim -c '
        apt-get update -qq >/dev/null 2>&1
        apt-get install -y -qq u-boot-qemu >/dev/null 2>&1
        cp /usr/lib/u-boot/qemu_arm64/u-boot.bin /out/ && chmod a+r /out/u-boot.bin'
fi

echo "==> exporting the device filesystem"
container=$(docker create "$image")
trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
docker export "$container" -o "$out/rootfs.tar"

echo "==> building the ext4 rootfs and packaging"
docker run --rm \
    -v "$out:/out" \
    -v "$root/test:/test" \
    -v "$key:/keys:ro" \
    --user root \
    -e NH_PRODUCT="$product" \
    -e NH_VERSION="$version" \
    -e NH_PLATFORM="${NH_PLATFORM:-qemu-arm64}" \
    -e NH_ARCHITECTURE="${NH_ARCHITECTURE:-arm64}" \
    -e NH_IDENTIFIER="${NH_IDENTIFIER:-qemu-device-01}" \
    --entrypoint /test/device/package.sh \
    "$image"

echo
echo "==> sizes"
ls -lh "$out"/*.fw "$out/rootfs.ext4" "$out/vmlinuz" "$out/initrd.img" | awk '{print "   ", $5, $9}'
