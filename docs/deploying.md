# Putting the agent on a device

The agent is one binary, one TOML file and a service unit. What takes thought is
everything around it: which user it runs as, which paths survive an update, and
where the device identifier comes from.

This page is the part that is the same everywhere. The bootloader contract — A/B
slots, validation, rollback — is per update tool and lives in
[fwup.md](fwup.md) and [rauc.md](rauc.md).

- [What the device needs](#what-the-device-needs)
- [Cross-compiling](#cross-compiling)
- [The systemd unit](#the-systemd-unit)
- [Buildroot](#buildroot)
- [Yocto](#yocto)
- [TLS](#tls)

## What the device needs

**The binary**, at `/usr/bin/nerves-hub-link-agent`. It links nothing but libc:

```
$ ldd nerves-hub-link-agent
    libgcc_s.so.1 => /lib/aarch64-linux-gnu/libgcc_s.so.1
    libm.so.6 => /lib/aarch64-linux-gnu/libm.so.6
    libc.so.6 => /lib/aarch64-linux-gnu/libc.so.6
```

TLS is rustls, so there is no OpenSSL to install, no version to keep in step
with the image, and nothing to find when cross-compiling. That is deliberate and
it is the main reason the dependency is not native-tls. See [TLS](#tls).

**`agent-ctl`**, at `/usr/bin/agent-ctl`. A minimal image has no python, no
socat and no nc, which otherwise leaves `mark_valid` unreachable from a support
script — the one place it most needs to be reachable from.

**A configuration file** at `/etc/nerves-hub-link-agent.toml`, or wherever
`--config` points. Config is firmware: it should version with the image and be
replaced by an update. See [`examples/agent.toml`](../examples/agent.toml).

**A service user.** The agent downloads from the network and runs support
scripts, so it has no business running as root. It needs:

| | |
| --- | --- |
| the socket directory | `RuntimeDirectory=` gives it `/run/nerves-hub-link-agent`, owned by the service user and recreated each boot |
| write access to the update target | For fwup, group access to the block device. For RAUC, membership of whatever group owns the slots, or let `rauc service` (which does run as root) own the write |
| the bootloader environment | `fw_setenv` writes it, so the same group access again |
| `sudo reboot` | one sudoers rule, below |

**Somewhere the identifier survives an update.** An update overwrites the whole
rootfs slot, so a serial number baked into the rootfs is gone after the first
one — and if it were baked in at build time, every device from that image would
be the same device. Put it on a data partition:

```toml
identifier = { file = "/data/nerves-hub/identifier" }
```

Or read it from the hardware, which is better when the hardware has one:

```toml
identifier = { file = "/sys/firmware/devicetree/base/serial-number" }
identifier = { command = "/usr/bin/read-serial" }
```

NervesHub registers an identifier it has not seen before, so a wrong one
silently creates a second device rather than failing. Resolution happens once at
startup and a failure is fatal for that reason.

**A sudoers rule**, if the agent reboots the device itself:

```
agent ALL=(root) NOPASSWD: /sbin/reboot
```

Set `reboot.command` to `reboot` where the agent already runs as root, or to
`systemctl reboot` for an init system that wants to sequence its own shutdown.

## Cross-compiling

With no OpenSSL in the dependency tree this is ordinary Rust cross-compilation.
For a glibc arm64 target:

```bash
rustup target add aarch64-unknown-linux-gnu
```

```bash
cargo build --release --target aarch64-unknown-linux-gnu --no-default-features --features fwup
```

That needs a linker for the target. [`cross`](https://github.com/cross-rs/cross)
supplies one in a container and is the shortest path:

```bash
cross build --release --target aarch64-unknown-linux-gnu --no-default-features --features fwup
```

For musl, `aarch64-unknown-linux-musl` produces a static binary that needs
nothing on the target at all.

**Build only the tool the device has.** The features exist so an image that will
never see a RAUC bundle does not carry the code to install one, and so a build
without `local-shell` cannot be talked into serving a shell:

```
--no-default-features --features fwup
--no-default-features --features rauc
--no-default-features --features rauc,local-shell
```

`sandbox` is in the default set on purpose — a build that has not been told
which real updater to use should not be able to write to a disk — so
`--no-default-features` is how you get a device build.

## The systemd unit

This is the rig's unit, which is a working reference rather than an
illustration. [`test/device/agent.service`](../test/device/agent.service):

```ini
[Unit]
Description=NervesHub device agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/nerves-hub-link-agent --config /etc/nerves-hub-link-agent.toml
Restart=always
RestartSec=5
Environment=RUST_LOG=nerves_hub_link_agent=info

User=agent
Group=agent
RuntimeDirectory=nerves-hub-link-agent
StateDirectory=nerves-hub-link-agent

[Install]
WantedBy=multi-user.target
```

`Restart=always` matters more than it looks. The agent exits on a configuration
it cannot serve — a missing fwup, a target that is not what it should be, an
identity it cannot resolve — and those are startup failures a person has to fix.
Everything transient is handled internally by the reconnect backoff, so a
restart loop here means a real problem rather than a flaky network.

The agent detects systemd through `JOURNAL_STREAM` and logs accordingly: no
timestamp of its own, and systemd's `<N>` priority prefix so the journal records
the real level. Nothing to configure.

## Buildroot

Buildroot's `cargo-package` infrastructure handles the build. In
`package/nerves-hub-link-agent/`:

`Config.in`:

```
config BR2_PACKAGE_NERVES_HUB_LINK_AGENT
	bool "nerves-hub-link-agent"
	depends on BR2_PACKAGE_HOST_RUSTC_TARGET_ARCH_SUPPORTS
	depends on BR2_USE_MMU
	select BR2_PACKAGE_HOST_RUSTC
	help
	  NervesHub device agent: connects to NervesHub, reports the running
	  firmware, and applies updates through fwup.

	  https://github.com/nerves-hub/nerves-hub-link-agent
```

`nerves-hub-link-agent.mk`:

```make
NERVES_HUB_LINK_AGENT_VERSION = 0.1.0
NERVES_HUB_LINK_AGENT_SITE = \
	$(call github,nerves-hub,nerves-hub-link-agent,v$(NERVES_HUB_LINK_AGENT_VERSION))
NERVES_HUB_LINK_AGENT_LICENSE = Apache-2.0
NERVES_HUB_LINK_AGENT_LICENSE_FILES = LICENSE

# fwup only: a Buildroot image is the fwup case, and building the RAUC
# installer into it would ship code the device can never reach.
NERVES_HUB_LINK_AGENT_CARGO_BUILD_OPTS = --no-default-features --features fwup

# The agent does not run as root. See the sudoers rule in docs/deploying.md.
NERVES_HUB_LINK_AGENT_USERS = agent -1 agent -1 * - - - NervesHub agent

define NERVES_HUB_LINK_AGENT_INSTALL_CONFIG
	$(INSTALL) -D -m 0644 $(NERVES_HUB_LINK_AGENT_PKGDIR)/agent.toml \
		$(TARGET_DIR)/etc/nerves-hub-link-agent.toml
endef
NERVES_HUB_LINK_AGENT_POST_INSTALL_TARGET_HOOKS += NERVES_HUB_LINK_AGENT_INSTALL_CONFIG

define NERVES_HUB_LINK_AGENT_INSTALL_INIT_SYSTEMD
	$(INSTALL) -D -m 0644 $(NERVES_HUB_LINK_AGENT_PKGDIR)/nerves-hub-link-agent.service \
		$(TARGET_DIR)/usr/lib/systemd/system/nerves-hub-link-agent.service
endef

$(eval $(cargo-package))
```

Add `source "package/nerves-hub-link-agent/Config.in"` to
`package/Config.in`, and drop `agent.toml` and the unit file beside the `.mk`.

The identifier wants a data partition that `genimage.cfg` keeps outside both
rootfs slots, and the fwup.conf that writes those slots is
[the one in this repo](../test/device/fwup.conf).

## Yocto

RAUC's own layer, [meta-rauc](https://github.com/rauc/meta-rauc), does the
bundle and slot work. This recipe is only the agent.

`recipes-support/nerves-hub-link-agent/nerves-hub-link-agent_0.1.0.bb`:

```bitbake
SUMMARY = "NervesHub device agent"
DESCRIPTION = "Connects a Linux device to NervesHub and applies updates through RAUC."
HOMEPAGE = "https://github.com/nerves-hub/nerves-hub-link-agent"
LICENSE = "Apache-2.0"
LIC_FILES_CHKSUM = "file://LICENSE;md5=<fill this in>"

SRC_URI = "git://github.com/nerves-hub/nerves-hub-link-agent.git;protocol=https;branch=main \
           file://nerves-hub-link-agent.service \
           file://agent.toml"
SRCREV = "<pin a commit>"

S = "${WORKDIR}/git"

inherit cargo cargo-update-recipe-crates systemd useradd

# Generated by `bitbake -c update_crates nerves-hub-link-agent`. Yocto builds
# offline, so every crate has to be declared as a source rather than fetched
# by cargo during do_compile.
require ${BPN}-crates.inc

# RAUC only, on a Yocto image. The agent talks to `rauc install` over D-Bus, so
# rauc has to be in the image and its service running.
CARGO_BUILD_FLAGS += "--no-default-features --features rauc"
RDEPENDS:${PN} += "rauc"

SYSTEMD_SERVICE:${PN} = "nerves-hub-link-agent.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

USERADD_PACKAGES = "${PN}"
GROUPADD_PARAM:${PN} = "--system agent"
USERADD_PARAM:${PN} = "--system --no-create-home --shell /sbin/nologin --gid agent agent"

do_install:append() {
    install -d ${D}${sysconfdir}
    install -m 0644 ${WORKDIR}/agent.toml ${D}${sysconfdir}/nerves-hub-link-agent.toml

    install -d ${D}${systemd_unitdir}/system
    install -m 0644 ${WORKDIR}/nerves-hub-link-agent.service \
        ${D}${systemd_unitdir}/system/nerves-hub-link-agent.service
}

FILES:${PN} += "${systemd_unitdir}/system/nerves-hub-link-agent.service"
```

Three things that catch people out:

**`cargo-update-recipe-crates` is not optional.** Yocto fetches offline, so
cargo cannot resolve dependencies during `do_compile`. Run
`bitbake -c update_crates nerves-hub-link-agent` to generate `-crates.inc`, and
regenerate it whenever `Cargo.lock` changes.

**The agent needs `rauc` at runtime, not just at build time.** `rauc install` is
a D-Bus client and the work happens in `rauc service`; without it the agent gets
`Error creating proxy: Could not connect`, which reads like a broken bundle. The
agent probes for the service at startup so that lands early.

**`statusfile` has to be outside the rootfs.** RAUC records what it installed,
an update overwrites the whole slot, and that record has to outlive it. See
[rauc.md](rauc.md).

**These two are written from the conventions, not from a build.** Neither has
been run here — the rigs in this repo are Debian under QEMU, which is what made
the bootloader work verifiable. Treat the recipes as a starting point and expect
to fix the checksums and the SRCREV at minimum.

## TLS

The agent uses rustls with the platform trust store, so a device that already
trusts a CA needs nothing configured. For a private CA that is not in the store:

```toml
ca_certificate = "/etc/ssl/certs/your-ca.pem"
```

That certificate is used for both the websocket and the firmware download. They
are two connections to the same deployment, and trusting one without the other
is not a safer device, only one that fails later and less obviously.

**rustls is stricter than OpenSSL about self-signed server certificates.** A
certificate generated with `openssl req -x509` carries `basicConstraints=CA:TRUE`,
and rustls refuses to accept a CA certificate as the end-entity certificate:

```
invalid peer certificate: Other(OtherError(CaUsedAsEndEntity))
```

OpenSSL accepts this, so a setup that worked against a native-tls build can fail
here. Issue a leaf certificate from your CA rather than serving the CA itself:

```bash
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca-key.pem -out leaf.pem \
    -extfile <(printf "subjectAltName=DNS:device.example\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth")
```

`danger_accept_invalid_certs` turns verification off entirely. It is for a
NervesHub on a laptop with a self-signed certificate and nothing else: anything
on the path can present its own certificate and read the shared secret out of
the handshake headers. It is logged loudly at startup for that reason.
