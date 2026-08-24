#!/bin/bash
# Runs inside the container: squash the exported rootfs and package it with
# fwup. Split out of build.sh rather than embedded in a `docker run bash -c`,
# because quoting a shell script inside a shell script inside a shell script is
# how you get an afternoon back only if you don't.
set -euo pipefail

: "${NH_PRODUCT:?}" "${NH_VERSION:?}" "${NH_PLATFORM:?}" "${NH_ARCHITECTURE:?}"

apt-get update -qq >/dev/null
apt-get install -y -qq squashfs-tools >/dev/null

mkdir -p /tmp/root
tar -xf /out/rootfs.tar -C /tmp/root

# A read-only rootfs is the point: an upgrade overwrites the whole partition, so
# nothing that must survive one can live here. Squashfs makes that a property of
# the filesystem rather than a convention someone has to remember.
mksquashfs /tmp/root /out/rootfs.squashfs -comp zstd -noappend -quiet

signing=()
if [ -f /keys/agent-test.priv ]; then
    echo "==> signing with agent-test"
    signing=(--private-key-file /keys/agent-test.priv)
else
    echo "==> WARNING: unsigned. NervesHub will reject this if the org has keys,"
    echo "    and so will a device configured with a public key."
fi

out="/out/${NH_PRODUCT}-${NH_VERSION}.fw"

ROOTFS_PATH=/out/rootfs.squashfs \
    fwup -c -f /test/image/fwup.conf "${signing[@]}" -o "$out"

echo
echo "==> metadata"
fwup -m -i "$out"
