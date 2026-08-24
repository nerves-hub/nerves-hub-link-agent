#!/bin/bash
# Build a RAUC bundle carrying the same rootfs the fwup image does.
#
#   ./test/device/build-rauc-bundle.sh 11.0.1
#
# Needs test/device/build.sh to have run first, for tmp/device/rootfs.ext4.
#
# The bundle is signed with tmp/keys/rauc-key.pem, whose certificate is baked
# into the image as RAUC's keyring — RAUC verifies bundles itself and refuses
# an unsigned one, so NervesHub never holds this key.
set -euo pipefail

version="${1:-11.0.1}"
product="${NH_PRODUCT:-SmartKiosk}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
out="$root/tmp/device"

test -f "$out/rootfs.ext4" || { echo "run test/device/build.sh first"; exit 1; }

docker run --rm \
    -v "$out:/out" \
    -v "$root/tmp/keys:/keys:ro" \
    --platform linux/arm64 \
    debian:trixie-slim bash -c "
        set -e
        apt-get update -qq >/dev/null 2>&1
        apt-get install -y -qq rauc squashfs-tools >/dev/null 2>&1

        mkdir -p /tmp/bundle
        cp /out/rootfs.ext4 /tmp/bundle/rootfs.ext4

        cat > /tmp/bundle/manifest.raucm <<CONF
[update]
compatible=nerves-hub-agent-qemu
version=$version
description=agent test image

[bundle]
format=verity

[meta.nerveshub]
product=$product
architecture=arm64
platform=qemu-arm64

[image.rootfs]
filename=rootfs.ext4
CONF

        rm -f /out/$product-$version.raucb
        rauc bundle --cert=/keys/rauc-cert.pem --key=/keys/rauc-key.pem \
            /tmp/bundle/ /out/$product-$version.raucb 2>&1 | tail -1

        echo '==> hash RAUC reports (which is the NervesHub uuid)'
        rauc info --keyring=/keys/rauc-cert.pem --output-format=json \
            /out/$product-$version.raucb 2>/dev/null | tr ',' '\n' | grep '\"hash\"'
        chmod a+rw /out/$product-$version.raucb
    "

ls -lh "$out/$product-$version.raucb" | awk '{print "   ", $5, $9}'
