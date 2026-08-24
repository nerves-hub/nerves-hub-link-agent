# The agent on Linux, with a real fwup.
#
# Running natively is the faster loop and is safe while the sandbox update tool
# is in use. This image is for everything the sandbox cannot tell you: the
# health extension needs /proc, journald needs systemd, and fwup needs to
# actually write an image. It still cannot reboot — that is QEMU's job.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Every tool, because the test images are one binary used as several devices.
# A real device builds only what it has: the features exist so an image that
# will never see a RAUC bundle does not carry the code to install one, and so
# that a build without `local-shell` cannot be talked into serving a shell.
RUN cargo build --release --locked --all-features

# fwup is not in Debian, and its releases carry .deb for amd64 and armhf only —
# no arm64, which is what this builds as on an Apple Silicon host. So it is
# built from source, which also means the container runs the same fwup version
# on every architecture rather than whichever one had a package.
FROM debian:bookworm-slim AS fwup
ARG FWUP_VERSION=1.16.0
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl \
        libarchive-dev libconfuse-dev libsodium-dev pkg-config \
    && curl -fsSL -o /tmp/fwup.tar.gz \
        "https://github.com/fwup-home/fwup/releases/download/v${FWUP_VERSION}/fwup-${FWUP_VERSION}.tar.gz" \
    && tar -xzf /tmp/fwup.tar.gz -C /tmp \
    && cd "/tmp/fwup-${FWUP_VERSION}" \
    && ./configure --prefix=/usr/local \
    && make -j"$(nproc)" \
    && make install

# `docker build --target test` gives a toolchain with fwup on PATH, which is
# what tests/fwup_install.rs needs and a laptop does not have.
FROM rust:1-bookworm AS test
RUN apt-get update \
    && apt-get install -y --no-install-recommends libarchive13 libconfuse2 libsodium23 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=fwup /usr/local/bin/fwup /usr/local/bin/fwup
WORKDIR /work

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates sudo \
        libarchive13 libconfuse2 libsodium23 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=fwup /usr/local/bin/fwup /usr/local/bin/fwup

# Not root. The agent downloads from the network and runs arbitrary support
# scripts; neither is work for a privileged process. Rebooting is the one thing
# it cannot do unprivileged, so it gets sudo for exactly that — which is also
# what `reboot.command` defaults to.
RUN useradd --system --create-home --uid 10001 agent \
    && mkdir -p /var/lib/nerves-hub-link-agent /run/nerves-hub-link-agent \
    && chown -R agent:agent /var/lib/nerves-hub-link-agent /run/nerves-hub-link-agent \
    && echo 'agent ALL=(root) NOPASSWD: /sbin/reboot' > /etc/sudoers.d/agent-reboot \
    && chmod 0440 /etc/sudoers.d/agent-reboot

USER agent

COPY --from=build /src/target/release/nerves-hub-link-agent /usr/local/bin/
COPY --from=build /src/target/release/agent-ctl /usr/local/bin/

ENTRYPOINT ["/usr/local/bin/nerves-hub-link-agent"]
CMD ["--config", "/etc/nerves-hub-link-agent.toml"]


# u-boot, built from source.
#
# Debian's package cannot do counted rollback: `CONFIG_BOOTCOUNT_LIMIT` is off,
# so u-boot has no idea a boot was ever attempted. Building it is also the only
# way to be sure of where the environment lives, which decides whether fwup,
# Linux and the bootloader are looking at the same bytes.
FROM debian:bookworm-slim AS uboot
ARG UBOOT_VERSION=2025.01
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        bc bison build-essential ca-certificates curl flex \
        libssl-dev python3 python3-dev python3-setuptools swig \
        # u-boot builds its host tools unconditionally, and mkeficapsule wants
        # gnutls whether or not the target uses EFI capsules.
        libgnutls28-dev uuid-dev \
    && rm -rf /var/lib/apt/lists/*

COPY test/device/uboot.config /tmp/uboot.config

RUN curl -fsSL -o /tmp/u-boot.tar.bz2 \
        "https://ftp.denx.de/pub/u-boot/u-boot-${UBOOT_VERSION}.tar.bz2" \
    && tar -xjf /tmp/u-boot.tar.bz2 -C /tmp \
    && cd "/tmp/u-boot-${UBOOT_VERSION}" \
    && make qemu_arm64_defconfig \
    && cat /tmp/uboot.config >> .config \
    && make olddefconfig \
    && make -j"$(nproc)" \
    && mkdir -p /out && cp u-boot.bin /out/ \
    && grep -E "^CONFIG_(BOOTCOUNT|ENV_IS)" .config > /out/config.summary

# The RAUC rig's u-boot: same source, environment in a file on the data
# partition so that `fw_setenv` and u-boot share one store. See
# test/device/uboot-rauc.config for why nothing simpler works here.
FROM uboot AS uboot-rauc
# Re-declared: a build argument does not cross a FROM.
ARG UBOOT_VERSION=2025.01
COPY test/device/uboot-rauc.config /tmp/uboot-rauc.config
RUN cd "/tmp/u-boot-${UBOOT_VERSION}" \
    && make qemu_arm64_defconfig \
    && cat /tmp/uboot-rauc.config >> .config \
    && make olddefconfig \
    && make -j"$(nproc)" \
    && cp u-boot.bin /out/u-boot-rauc.bin \
    && grep -E "^CONFIG_(ENV_IS|ENV_OFFSET|ENV_SIZE|MMC_PCI)" .config > /out/config-rauc.summary

# A rootfs that can boot, as opposed to one that can only be written.
#
# The runtime stage above is a container filesystem: no kernel, no init that
# expects to be PID 1 on hardware, no journald. This adds those, which is what
# separates "fwup wrote the slot" from "the device came up on the new slot" —
# and the second is the only way to test rollback.
#
# Deliberately Debian rather than Buildroot. It is bigger than anything you
# would ship, and it boots today with a stock kernel and a real systemd, which
# is the trade a test rig should make.
# Trixie, not bookworm: bookworm ships RAUC 1.8, which installs bundles fine and
# then cannot say what it installed — `bundle.hash` only reaches slot status in
# 1.9. See src/update_tool/rauc.rs.
FROM debian:trixie-slim AS device

# Which rig this image is. `fwup` keeps slot selection in the boot script and
# `fw_active`; `rauc` hands it to RAUC's BOOT_ORDER/BOOT_x_LEFT convention.
# They are different systems, not two settings of one, so they differ in their
# boot script, their fw_env.config, and which u-boot they run.
ARG BOOT_SCHEME=fwup

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        systemd systemd-sysv \
        linux-image-arm64 \
        ca-certificates sudo \
        libarchive13 libconfuse2 libsodium23 \
        iproute2 iputils-ping \
        openssh-server \
        libubootenv-tool \
        u-boot-tools \
        rauc rauc-service \
    && rm -rf /var/lib/apt/lists/*

# Both, and they are not alternatives. Debian 12's `u-boot-tools` has `mkimage`
# but no `fw_printenv` — that moved to `libubootenv-tool` — and the failure is a
# missing command rather than anything that says why.
#
# Where fw_printenv looks. The offsets match the uboot-environment block in
# test/device/fwup.conf — 1024 blocks in, 256 blocks long — and if the two ever
# disagree the device reads garbage and reports no firmware at all.
# Where fw_printenv/fw_setenv look, which differs by rig.
#
#   fwup  a raw block on the disk, at the offset fwup's uboot-environment writes
#   rauc  a raw offset on the SD card, which is the one store u-boot and Linux
#         can both reach here — see test/device/uboot-rauc.config
RUN if [ "${BOOT_SCHEME}" = "rauc" ]; then \
        printf '/dev/mmcblk0 0x80000 0x20000\n' > /etc/fw_env.config; \
    else \
        printf '/dev/vda 0x80000 0x20000\n' > /etc/fw_env.config; \
    fi

COPY --from=fwup /usr/local/bin/fwup /usr/local/bin/fwup
COPY --from=build /src/target/release/nerves-hub-link-agent /usr/local/bin/
# The only thing on the image that can talk to the agent's socket, and therefore
# the only way a support script can confirm the firmware.
COPY --from=build /src/target/release/agent-ctl /usr/local/bin/
# Carried in the image so the bootloader and the rootfs are built together and
# cannot drift; run-qemu.sh copies it out to seed the board's flash.
COPY --from=uboot /out/u-boot.bin /usr/lib/nerves-hub/u-boot.bin
COPY --from=uboot-rauc /out/u-boot-rauc.bin /usr/lib/nerves-hub/u-boot-rauc.bin

# The agent needs the disk to write the other slot, and to read the u-boot
# environment. `disk` group rather than root: writing one block device is a
# narrower privilege than being root, and it is the same arrangement a real
# device would use.
RUN useradd --system --create-home --uid 10001 agent \
    && usermod -aG disk agent \
    && mkdir -p /var/lib/nerves-hub-link-agent /data \
    && chown -R agent:agent /var/lib/nerves-hub-link-agent \
    && echo 'agent ALL=(root) NOPASSWD: /sbin/reboot' > /etc/sudoers.d/agent-reboot \
    && chmod 0440 /etc/sudoers.d/agent-reboot

# No password on the console. This is a test image on a virtual machine that
# talks to a NervesHub on someone's laptop; a password would be theatre, and
# forgetting it would cost an afternoon.
RUN passwd -d root

# Bring the network up. Debian enables no network manager by default, so
# without this the interface exists, has no address, and everything that needs
# the network fails in a way that looks like the network is broken rather than
# absent — sshd listening on an unreachable host, an agent that cannot resolve
# its own failure to connect.
#
# resolv.conf is written at boot by tmpfiles rather than here: Docker
# bind-mounts /etc/resolv.conf during a build, so writing it in a RUN fails.
#
# The type is `f+`, not `f`. `f` writes its argument only when it *creates* the
# file, and Docker leaves an empty /etc/resolv.conf behind in the image -- so
# tmpfiles found it already there and left it empty, and the guest had no
# resolver at all. Nothing noticed until the agent was pointed at a hostname
# instead of a LAN address. `f+` truncates and writes every boot.
RUN mkdir -p /etc/systemd/network \
    && printf '[Match]\nName=en*\n\n[Network]\nDHCP=yes\n' \
        > /etc/systemd/network/10-ethernet.network \
    && systemctl enable systemd-networkd \
    && mkdir -p /etc/tmpfiles.d \
    # One file, and the name ends in `.conf` because systemd-tmpfiles reads
    # nothing else. This was two files, and the second was called `hosts` --
    # silently ignored, so every sudo warned it could not resolve localhost.
    # The resolv.conf rule worked only because a file named after its target
    # happens to end in `.conf` already.
    #
    # /etc/hosts is written here rather than in a RUN for the same reason as
    # resolv.conf: Docker bind-mounts both during a build.
    && printf 'f+ /etc/resolv.conf 0644 root root - nameserver\\s10.0.2.3\nf+ /etc/hosts 0644 root root - 127.0.0.1\\slocalhost\n' \
        > /etc/tmpfiles.d/rig.conf

# Key-only ssh for the test rig, so the VM can be driven from a script instead
# of typed at. A password would have to be either weak or remembered.
COPY tmp/keys/qemu_ed25519.pub /root/.ssh/authorized_keys
RUN chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys \
    && sed -i "s/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/" /etc/ssh/sshd_config

# The data partition, which an upgrade never touches. Everything that has to
# outlive an update lives here — see the README.
RUN printf "/dev/vda3 /data ext4 defaults,nofail 0 2\n" >> /etc/fstab

# RAUC's view of the same A/B layout fwup writes. The slots are the partitions
# the bootloader already chooses between, so the two tools describe one device
# rather than each bringing their own.
#
# `bootloader=uboot`: RAUC owns slot selection, through the BOOT_ORDER and
# BOOT_x_LEFT variables that test/device/rauc-boot.cmd reads. Build this image
# with `--build-arg BOOT_SCHEME=rauc` or the boot script will ignore them.
# dm-verity, which RAUC needs to mount a verity bundle. Debian builds it as a
# module and nothing loads it on demand, so an install gets as far as verifying
# the signature and then fails with "Failed to load dm table" — which reads as a
# corrupt bundle rather than a missing module.
RUN mkdir -p /etc/modules-load.d \
    && printf 'dm-mod\ndm-verity\nsdhci-pci\n' > /etc/modules-load.d/rauc.conf

COPY test/device/rauc-system.conf /etc/rauc/system.conf
# The trust anchor RAUC verifies bundles against. Provisioned by the image
# build, not by NervesHub — RAUC checks the signature itself and refuses an
# unsigned bundle, so the server never needs to hold this key.
COPY tmp/keys/rauc-cert.pem /etc/rauc/keyring.pem

# The agent, as a service, so the device comes up talking to NervesHub the way a
# real one would rather than because someone ran it.
#
# Restart=always and a delay: the agent exits on a fatal config error, and a
# unit that gives up after five tries leaves a device that looks dead when the
# real problem was a network that had not come up yet.
# The boot script, compiled where mkimage is. It ships inside the rootfs so
# that both slots carry it and u-boot's distro boot finds it by scanning.
COPY test/device/boot.cmd /boot/boot-fwup.cmd
COPY test/device/rauc-boot.cmd /boot/boot-rauc.cmd
RUN cp "/boot/boot-${BOOT_SCHEME}.cmd" /boot/boot.cmd \
    && mkimage -A arm64 -T script -C none -d /boot/boot.cmd /boot/boot.scr \
    && ln -sf "$(cd /boot && ls vmlinuz-* | head -1)" /boot/vmlinuz \
    && ln -sf "$(cd /boot && ls initrd.img-* | head -1)" /boot/initrd.img

COPY test/device/agent.service /etc/systemd/system/nerves-hub-link-agent.service
# Per-scheme, like the boot script above: the two rigs run different update
# tools, so one file cannot serve both. Copying a single agent.toml here is how
# the fwup rig ended up shipping the RAUC configuration.
#
# The glob picks up `agent-<scheme>.local.toml` when there is one, and that is
# preferred. The tracked configs hold placeholders for the shared secret and
# the host, so a live product secret and the IP of whoever built the image stay
# out of the repository -- see .gitignore.
COPY test/device/agent-*.toml /etc/agent-config/
RUN if [ -f "/etc/agent-config/agent-${BOOT_SCHEME}.local.toml" ]; then \
        echo "using agent-${BOOT_SCHEME}.local.toml"; \
        cp "/etc/agent-config/agent-${BOOT_SCHEME}.local.toml" /etc/nerves-hub-link-agent.toml; \
    else \
        echo "using agent-${BOOT_SCHEME}.toml -- the shared secret is a placeholder"; \
        cp "/etc/agent-config/agent-${BOOT_SCHEME}.toml" /etc/nerves-hub-link-agent.toml; \
    fi \
    && rm -rf /etc/agent-config
RUN systemctl enable nerves-hub-link-agent.service \
    && mkdir -p /etc/nerves-hub /data
