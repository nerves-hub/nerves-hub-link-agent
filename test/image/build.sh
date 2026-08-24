#!/bin/bash
# Build a firmware image whose rootfs comes out of the agent's own container.
#
#   ./test/image/build.sh 1.0.0
#
# The container image is already a Linux userland with the agent, fwup, bash and
# CA certificates in it — which is most of what a device rootfs needs. Exporting
# its filesystem and squashing it gives a rootfs in about a minute, with no
# Buildroot or Yocto build to sit through.
#
# This is a test rig, not a product. What it is missing is a kernel, an init
# that expects to be PID 1 on real hardware, and a bootloader — all of which
# matter the moment you want to boot it rather than just write it. See the
# README for what to use when this stops being enough.
set -euo pipefail

version="${1:-0.1.0}"
product="${NH_PRODUCT:-SmartKiosk}"
platform="${NH_PLATFORM:-container}"
# The container's own architecture, not the host's. Labelling an arm64 rootfs
# x86_64 would be a lie the moment anything tried to boot it, and NervesHub
# matches deployments on this field.
architecture="${NH_ARCHITECTURE:-$(docker version --format '{{.Server.Arch}}' 2>/dev/null || echo unknown)}"
image="${AGENT_IMAGE:-nerves-hub-link-agent:test}"
# Sign if a key is there. NervesHub rejects an unsigned archive unless the org
# has no keys at all, and a device configured with a public key rejects one too.
key="${FWUP_PRIVATE_KEY:-$(cd "$(dirname "$0")/../.." && pwd)/tmp/keys/agent-test.priv}"

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="$root/tmp/image"
mkdir -p "$out"

echo "==> exporting the container filesystem"
container=$(docker create "$image")
trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
docker export "$container" -o "$out/rootfs.tar"

echo "==> squashing, and packaging with fwup"
# Done inside a container so the host needs neither mksquashfs nor fwup, and so
# the squashfs is built by the same Linux that will read it.
docker run --rm \
    -v "$out:/out" \
    -v "$root/test:/test" \
    -v "$(dirname "$key"):/keys:ro" \
    --user root \
    -e NH_PRODUCT="$product" \
    -e NH_VERSION="$version" \
    -e NH_PLATFORM="$platform" \
    -e NH_ARCHITECTURE="$architecture" \
    --entrypoint /test/image/package.sh \
    "$image"

echo
echo "==> sizes"
ls -lh "$out/rootfs.tar" "$out/rootfs.squashfs" "$out/$product-$version.fw" | awk '{print "   ", $5, $9}'
