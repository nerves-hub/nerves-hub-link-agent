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
- [Rust version](#rust-version)
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

## Rust version

The agent needs **Rust 1.85**, and that constrains which Buildroot and Yocto
releases can build it more than anything else on this page.

1.85 is where edition 2024 was stabilised, and that is the whole of the
constraint. Crates the agent genuinely compiles have moved to it -- `zeroize`
through rustls, `hashbrown` through toml -- and an older cargo cannot parse
their manifests to vendor them. Nothing here can lower it further without
pinning a widening set of transitive crates below their current releases,
which trades a build-system version for missed security fixes.

| | Rust | vs the 1.85 floor |
| --- | --- | --- |
| Buildroot 2025.02 | 1.82 | no |
| Buildroot 2025.05 | 1.86 | yes, untested here |
| **Buildroot 2025.08** | **1.88** | **yes, and built** |
| Yocto, any release | from meta-rust-bin | yes, and built on scarthgap |

No released Yocto ships 1.85, which is why the layer requires meta-rust-bin —
see [Yocto](#yocto). It was 1.88 until reqwest was replaced. reqwest carried an optional QUIC stack
whose `chacha20` is edition 2024, plus `url` reaching idna and icu, which want
1.86 and 1.88 -- none of it compiled, all of it in the lockfile, because a
lockfile records the maximal resolution rather than the enabled one. Removing it
took the tree from 221 crates to 158 and the release binary from 9.6 MB to
7.6 MB.

CI builds with exactly the declared version, read out of `Cargo.toml`, so the
two cannot disagree. They did once: `rust-version` said 1.77 for months and the
first thing to notice was a Buildroot image failing to vendor the lock.

## Buildroot

A complete br2-external tree is in
[`support/buildroot/`](../support/buildroot/): the package, its `Config.in`,
the systemd unit and a starting `agent.toml`.

```bash
./support/buildroot/build-test.sh          # build the package
./support/buildroot/build-test.sh image    # and the whole rootfs
```

That script builds it in a container against Buildroot 2025.08.1 with
`qemu_aarch64_virt_defconfig` -- the same board the QEMU rigs emulate -- and it
is how the tree is verified rather than assumed. The resulting image contains:

```
/usr/bin/nerves-hub-link-agent          aarch64, 6.8M stripped
/etc/nerves-hub-link-agent.toml
/usr/lib/systemd/system/nerves-hub-link-agent.service
/etc/systemd/system/multi-user.target.wants/nerves-hub-link-agent.service
agent:x:104:110:NervesHub agent:/:/bin/false
```

and the binary needs `libgcc_s`, `libm` and `libc`, nothing else -- no OpenSSL
to install or keep in step, which is the whole reason TLS is rustls.

To use it in your own tree, point `BR2_EXTERNAL` at `support/buildroot` and
enable `BR2_PACKAGE_NERVES_HUB_LINK_AGENT`. The package pins a commit; replace
it with a tag when there is one.

Two things the package does not do, because they belong to a product rather
than to a package: the identifier on a partition an update does not overwrite,
and the fwup.conf that defines the A/B layout. See [fwup.md](fwup.md).

## Yocto

The layer is [meta-nerveshub](https://github.com/nerves-hub/meta-nerveshub), a
repository of its own. It pins the agent with `SRCREV`, so bumping it is a
separate step from releasing the agent.

```bash
./build-test.sh parse    # layers and recipe parse
./build-test.sh fetch    # also every crate, checksums included
./build-test.sh build    # also compile
```

Verified on **scarthgap**, the current LTS. The package it produces:

```
/usr/bin/nerves-hub-link-agent                            aarch64
/usr/bin/agent-ctl
/etc/nerves-hub-link-agent.toml
/usr/lib/systemd/system/nerves-hub-link-agent.service
/usr/lib/systemd/system-preset/98-nerves-hub-link-agent.preset
```

with `agent` added as a system user and the binary needing `libgcc_s`, `libm`
and `libc` and nothing else.

### It requires meta-rust-bin

The agent needs Rust 1.85 and scarthgap ships cargo 1.75, so the toolchain has
to come from somewhere other than the release.
[meta-rust-bin](https://github.com/rust-embedded/meta-rust-bin) supplies
prebuilt upstream toolchains — it currently packages up to 1.98 and its
`LAYERSERIES_COMPAT` spans kirkstone through wrynose, so one layer covers every
release worth targeting.

The recipe therefore inherits `cargo_bin` rather than poky's `cargo`, and
`LAYERDEPENDS_nerveshub` names `rust-bin-layer` alongside `rauc` so a missing
layer fails at layer-add time rather than as a cargo that cannot parse edition
2024.

The tradeoff is theirs to state: these are upstream binaries rather than a
toolchain built from source in your build system. For a product with
reproducibility or supply-chain audit requirements that is a real
consideration, and it is why `meta-rust` still exists alongside it.

### Two things that fail quietly

**`systemd` has to be in `DISTRO_FEATURES`.** Poky defaults to sysvinit, where
`systemd_system_unitdir` expands to nothing and the unit installs *nowhere* —
the package builds, ships a binary with nothing to start it, and says nothing.
The recipe guards the install on the feature, and the test build sets
`INIT_MANAGER = "systemd"` so the path is actually exercised.

**`UNPACKDIR` is undefined before styhead.** Newer releases unpack `file://`
entries there; on scarthgap the reference resolves to nothing and you get
`install: cannot stat '/agent.toml'`. The recipe sets a weak default so one
path works on both.

`rauc` also has to be in `DISTRO_FEATURES`, which is meta-rauc's requirement
rather than this layer's, and meta-rauc supplies the `rauc` the agent shells
out to.

`bitbake -c update_crates nerves-hub-link-agent` regenerates the crate list
after a `Cargo.lock` change.

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
