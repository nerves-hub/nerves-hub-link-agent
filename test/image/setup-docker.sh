#!/bin/bash
# Prepare a container-sized "device": an A/B disk image with the factory
# firmware written to slot A, and the metadata file the agent reads to say what
# it is running.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
version="${1:-2.0.0}"
product="${NH_PRODUCT:-SmartKiosk}"
data="$root/tmp/docker/data"

mkdir -p "$data"
rm -f "$data/disk.img"

docker run --rm \
    -v "$root/tmp/image:/image:ro" \
    -v "$data:/data" \
    --user root --entrypoint bash \
    nerves-hub-link-agent:test -c "
        set -euo pipefail
        fwup -a -d /data/disk.img -i /image/$product-$version.fw -t complete -U --quiet
        uuid=\$(fwup -m -i /image/$product-$version.fw --metadata-key meta-uuid)

        # What the agent reads as its running firmware. On a real device the
        # build writes this into the rootfs, or it comes from fw_printenv.
        cat > /data/firmware.env <<ENV
nerves_fw_uuid=\$uuid
nerves_fw_version=$version
nerves_fw_product=$product
nerves_fw_platform=container
nerves_fw_architecture=arm64
ENV
        chmod -R a+rwX /data
        echo 'factory image written, running:'
        cat /data/firmware.env
    "
