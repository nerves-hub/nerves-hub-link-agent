#!/bin/bash
# Build the nerves-hub-link-agent package with Buildroot, in a container.
#
#   ./support/buildroot/build-test.sh            # build just the package
#   ./support/buildroot/build-test.sh image      # and the whole rootfs
#
# Buildroot needs Linux and refuses to run as root, so this runs as an ordinary
# user inside a Debian container.
#
# The build lives in a named Docker volume rather than a bind mount from the
# host, because glibc does not build on a case-insensitive filesystem and a
# macOS bind mount is case-insensitive. It fails deep into the glibc build with
# "No rule to make target .../stdlib/stamp.os", which says nothing about the
# cause. Yocto refuses upfront for the same reason; Buildroot does not check.
#
# `docker volume rm nhla-buildroot` reclaims the space.
set -euo pipefail

# `image` is spelled `all` to make: Buildroot has no target by that name, and
# passing one through verbatim fails with "No rule to make target" after the
# container has already installed its build dependencies.
case "${1:-package}" in
    image)   target=all ;;
    package) target=nerves-hub-link-agent ;;
    *)       target="$1" ;;
esac
# 2025.08 is the first release shipping Rust 1.88, which is the agent minimum.
# 2025.02 ships 1.82 and cannot even vendor the lock: crates in it use edition
# 2024, which that cargo does not understand.
version="${BUILDROOT_VERSION:-2025.08.1}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
volume="${BUILDROOT_VOLUME:-nhla-buildroot}"

docker volume create "$volume" >/dev/null

# `qemu_aarch64_virt_defconfig` is the same board the QEMU rigs emulate, so a
# problem here is a problem with the package rather than with an unfamiliar
# target. systemd because the package installs a unit and that path is worth
# exercising; glibc because systemd requires it.
docker run --rm \
    -v "$volume:/out" \
    -v "$root/support/buildroot:/external:ro" \
    -e "BR_TARGET=$target" \
    -e "BR_VERSION=$version" \
    debian:bookworm bash -euxc '
        apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
            build-essential git wget cpio unzip rsync bc file python3 perl \
            libncurses-dev ca-certificates sed make binutils diffutils findutils \
            gzip tar xz-utils patch texinfo gettext >/dev/null

        # Buildroot refuses to build as root, with reason.
        useradd -m -u 1000 br
        mkdir -p /out/dl /out/build
        chown -R br /out

        su br -c "
            set -eux
            cd /out
            if [ ! -d buildroot-\${BR_VERSION} ]; then
                wget -q https://buildroot.org/downloads/buildroot-\${BR_VERSION}.tar.gz
                tar -xzf buildroot-\${BR_VERSION}.tar.gz
                rm buildroot-\${BR_VERSION}.tar.gz
            fi
            cd buildroot-\${BR_VERSION}

            make BR2_EXTERNAL=/external O=/out/build qemu_aarch64_virt_defconfig

            cd /out/build
            cat >> .config <<CFG
BR2_INIT_SYSTEMD=y
BR2_TOOLCHAIN_BUILDROOT_GLIBC=y
BR2_PACKAGE_NERVES_HUB_LINK_AGENT=y
BR2_DL_DIR=\"/out/dl\"
# A read timeout, not just a connect timeout. The default has none, so a
# download that stalls mid-transfer hangs the build indefinitely rather than
# retrying -- which is exactly what happened, at 10% of a gcc tarball.
BR2_WGET=\"wget --passive-ftp -nd -t 3 -T 30\"
CFG
            make olddefconfig

            grep -E \"^BR2_(PACKAGE_NERVES_HUB_LINK_AGENT|INIT_SYSTEMD)=\" .config

            make -j\$(nproc) \${BR_TARGET}
        "
    '
