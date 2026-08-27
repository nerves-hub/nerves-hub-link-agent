# NervesHub Link Agent

A NervesHub device agent for Linux systems that are not running Nerves.

The agent is a single Rust binary that links nothing but libc. Your application
stays its own process in whatever language you already use, and firmware is
written by a tool the image already has — [`fwup`](docs/fwup.md) on Buildroot
and Nerves-adjacent images, [`rauc`](docs/rauc.md) on Yocto.

It connects to NervesHub over Phoenix Channels and:

- reports which firmware the device is running
- receives update assignments, downloads them, and runs the updater
- asks your application whether an update may be installed, and whether the
  device may reboot
- confirms a good boot so the bootloader releases its rollback
- runs support scripts, and answers health, geo, logging, remote shell and
  network identity requests

## Status

Exercised end to end against a real NervesHub. Both update tools install, roll
back and validate on a QEMU rig with a real bootloader. It has not yet run on
production hardware.

Two things are not implemented. Client-certificate identity — the agent
*refuses* a config containing it rather than starting and failing later — and
resumable downloads.

## Requirements

| | |
| --- | --- |
| **A Linux device** | The agent reads `/proc` and `/sys` and shells out to your updater. It builds and runs on macOS for development, but health metrics come back empty there. |
| **An update tool** | `fwup` or `rauc`, already in the image and already configured for A/B updates. The agent runs it; it does not replace it. |
| **A NervesHub** | [NervesCloud](https://nervescloud.com), or your own deployment. |
| **Rust 1.85+** | To build. See [docs/deploying.md](docs/deploying.md) for cross-compiling and for Buildroot and Yocto packaging. |

## Getting started

The goal of this section is a device that shows up in NervesHub. It uses the
**sandbox** update tool, which downloads firmware, verifies its SHA-256, writes
it to a file and stops — no block devices, no bootloader, and "reboot" is a log
line. That exercises authentication, identity, deployment targeting, progress
reporting and reconnects, which is everything except the part that writes to a
disk. Swap in a real update tool once it connects.

### 1. Build

```bash
cargo build --release
```

This produces `nerves-hub-link-agent` and `agent-ctl` in `target/release/`. The
default feature set is `sandbox` and `fwup`; see
[Building](#building-from-source) for the others.

### 2. Create a shared secret

In the NervesHub web UI: create an organization and a product, then go to
**Product → Settings → Shared Secrets** and create one. You get a product key
(`nhp_...`) and a product secret.

A shared secret authenticates the *product*, not the device, so the agent sends
a device identifier alongside it. NervesHub registers an identifier it has not
seen before — which is what makes one factory image work for a whole fleet, and
also means a wrong identifier quietly creates a second device instead of
failing.

### 3. Write a config

The agent reads one TOML file: `/etc/nerves-hub-link-agent.toml`, unless
`--config` or `$NERVES_HUB_AGENT_CONFIG` points elsewhere. Nothing is read from
the environment field by field, so a device's configuration is one file you can
read and diff.

For **NervesCloud**:

```toml
[server]
host = "devices.nervescloud.com"
# port = 443, tls = true and path = "/device-socket" are the defaults

[identity]
product_key = "nhp_..."
product_secret = "..."
identifier = { literal = "bench-01" }

[update_tool]
name = "sandbox"
work_dir = "/tmp/nerves-hub-agent"
initial_firmware = { uuid = "00000000-0000-0000-0000-000000000000", version = "0.1.0", product = "my-product", platform = "sandbox", architecture = "x86_64" }

[ipc]
socket = "/tmp/nerves-hub-agent.sock"
```

For **your own NervesHub**, change `host` to your deployment's device endpoint.
Which port and path depends on how it is deployed and which endpoint accepts
shared secrets — [docs/connecting.md](docs/connecting.md) covers both endpoints,
TLS, and what each authentication failure actually means.

### 4. Run it

```bash
./target/release/nerves-hub-link-agent --config agent.toml
```

The device appears in NervesHub under the identifier you configured. Add
`RUST_LOG=nerves_hub_link_agent=debug` to log the URL it dials and every frame
in both directions.

The startup line reads `update tool fwup (sandboxed)`. That is not a mistake —
the sandbox reports itself to NervesHub as `fwup`, because it stands in for the
fwup path rather than being a firmware format of its own. `(sandboxed)` is the
part that tells you nothing on this device can be written to.

If it does not connect, the failure is almost always the credential, the clock
or the path. [docs/connecting.md](docs/connecting.md) has the table.

### 5. Send it an update

Upload a firmware to the product and target the device from a deployment group.
The sandbox tool downloads it, verifies it and writes it into `work_dir`, and
you can watch progress in the UI.

### 6. Switch to a real update tool

Sandbox exists to prove the connection. Replace the `[update_tool]` block with
[fwup](docs/fwup.md) or [rauc](docs/rauc.md) — each guide covers what the image
must provide, how to configure the agent, and a QEMU rig that boots, rolls back
and validates for real.

Then read [docs/deploying.md](docs/deploying.md) for the parts that only matter
on hardware: cross-compiling, the service user, the systemd unit, and where the
device identifier has to live so it survives a rootfs update.

## Update tools

One per device, chosen in the config and compiled in as a feature.

| | |
| --- | --- |
| **[fwup](docs/fwup.md)** | The archive streams into `fwup`'s stdin as it downloads, so the device needs no free space beyond the slot it writes into. Deltas are NervesHub's job; the agent reports its fwup version and lets the server decide. |
| **[rauc](docs/rauc.md)** | `rauc install <url>`. The bundle is never downloaded first — RAUC streams it and fetches only the blocks the target slot lacks, so a small change costs a small download without anyone generating a patch. |
| **sandbox** | Downloads, verifies, writes to a file, stops. For bring-up and for CI, and it reports itself to NervesHub as `fwup` since it stands in for that path. In the default feature set deliberately: a build that has not been told which real updater to use should not be able to write to a disk. |

The agent does not write firmware itself. It hands the update to a tool that
does, and those tools disagree about where bytes come from — `fwup` reads an
archive from stdin, `rauc` wants a URL so it can stream. So an update tool owns
the transfer rather than being handed a sink to fill.

## Configuration

[`examples/agent.toml`](examples/agent.toml) is every option, annotated, with
defaults marked. The decisions worth making before you ship anything:

**Identity.** Where the device identifier comes from: a literal, a file, or a
command. On real hardware prefer the hardware's own serial
(`{ file = "/sys/firmware/devicetree/base/serial-number" }`) or a value on a
data partition — a literal in a shipped image makes every device the same
device, and an identifier baked into the rootfs is gone after the first update.

**Update policy.** `apply` installs whatever arrives, matching `nerves_hub_link`
out of the box. `ask` puts your application in the path.

**Reboot policy.** Separate from update policy on purpose. An application happy
to download at any time may still be unable to reboot right now, and conflating
the two forces it to refuse the download in order to protect the reboot.

**How it reboots.** `sudo reboot` by default. The agent downloads from the
network and runs support scripts, so it should not run as root — which means one
sudoers rule: `agent ALL=(root) NOPASSWD: /sbin/reboot`. Use `reboot` where it
already runs as root, or `systemctl reboot` under an init system that wants to
sequence its own shutdown.

**Timeouts.** Every question the agent asks your application has a deadline and
a configured answer for each way it can go unanswered. An agent that blocks
forever on an application that died is a device that has quietly left the fleet
while still looking healthy from the server.

## Talking to the agent

Newline-delimited JSON over a Unix socket, and it is a peer protocol rather than
a client/server one: your application asks the agent for status, and the agent
asks your application whether to install. Full protocol and worked exchanges in
[docs/ipc.md](docs/ipc.md).

```
application -> agent          agent -> application
  hello                         update_available -> apply | ignore | reschedule
  status                        reboot_request   -> reboot | defer
  mark_valid                    identify
  reboot

events: connection, update_progress, update_installed, update_failed,
        reboot_pending
```

Exactly one connection may be the **controller**, the one asked to decide;
everything else is an observer. A second controller is refused rather than
replacing the first, so the mistake surfaces at connect time on a bench instead
of as a fleet that updates when it was told not to.

[`examples/controller.py`](examples/controller.py) is a working controller in
about a hundred lines, if you want to see the shape of one.

### agent-ctl

A minimal image has no python, no socat and no nc, which otherwise leaves
`mark_valid` unreachable from a support script — the one place it most needs to
be reachable from.

```bash
agent-ctl status        # connection, identity, firmware, whether validation is owed
agent-ctl mark-valid    # confirm the running firmware, releasing the rollback
agent-ctl reboot
agent-ctl watch         # stream events
```

It connects as an observer, asks one question and exits, so running it never
takes the controller slot from the application that owns it.

## Extensions

Everything NervesHub can ask a device for that is not firmware. All five are off
by default, and both halves have to agree before one attaches — the platform
advertises what it has, the agent offers back what it also implements, and the
platform replies with the subset it wants.

| | |
| --- | --- |
| **health** | Memory, CPU, load and temperature from `/proc` and `/sys`, answered when asked. |
| **geo** | A position from GeoIP, a fixed configured location, or a command for devices with a GPS. |
| **logging** | `journalctl --follow`, or any command that writes lines, batched a second at a time. |
| **local_shell** | A real pty running a shell, streamed to the browser terminal. |
| **network_identity** | Iroh, Tailscale, NetBird or WireGuard keys, from configured commands. |

`local_shell` hands out a shell to whoever can open the tab in NervesHub. Read
[docs/extensions.md](docs/extensions.md) before enabling it — that guide also
covers the negotiation, what each extension collects, and how to configure them.

## Support scripts

A support script arrives from NervesHub as text and runs as a shell script,
because the things people reach for when a device misbehaves — `journalctl`,
`systemctl status`, `ip addr`, `df` — are commands rather than expressions.

Enabled by default, unlike the extensions. Scripts are per-product, and a
product can hold both Nerves devices and agent devices, which is a trap worth
understanding: see [docs/support-scripts.md](docs/support-scripts.md).

## Building from source

```bash
cargo build --release                                  # sandbox + fwup
cargo build --release --no-default-features --features rauc
cargo test --all-features
```

Minimum supported Rust is **1.85**, which is what decides the Buildroot and
Yocto releases that can build the agent. It is tested in CI rather than guessed.

Update tools are cargo features, so a build that was not told about a real
updater cannot be talked into using one by a config file. Extensions are runtime
configuration only.

The tests cover the parts with no I/O in them: the policy table, the wire
frames, the auth token, identifier resolution, and the payload shapes the server
matches on. That is deliberate — those are where a mistake is silent. Progress
reports going to NervesHub under a key the server does not read would fail
nowhere.

## Documentation

| | |
| --- | --- |
| [connecting.md](docs/connecting.md) | Endpoints, shared secrets, TLS, and what each connection failure means |
| [fwup.md](docs/fwup.md) | The fwup update tool, what the image must provide, and a QEMU rig |
| [rauc.md](docs/rauc.md) | The RAUC update tool, what the image must provide, and a QEMU rig |
| [deploying.md](docs/deploying.md) | Onto real hardware: cross-compiling, systemd, Buildroot, Yocto |
| [ipc.md](docs/ipc.md) | The protocol your application speaks to the agent |
| [extensions.md](docs/extensions.md) | health, geo, logging, local_shell, network_identity |
| [support-scripts.md](docs/support-scripts.md) | How scripts run, and the mixed-fleet trap |

A Buildroot br2-external tree is in [`support/buildroot/`](support/buildroot/)
and the Yocto layer in [meta-nerveshub][meta-nerveshub], each with a script that
builds it in a container. Buildroot is tested against 2025.08, Yocto against
scarthgap. The Yocto layer needs
[meta-rust-bin](https://github.com/rust-embedded/meta-rust-bin), because no
released Yocto ships a Rust new enough.

## Project layout

```
src/
  main.rs              the daemon: read config, build pieces, run
  lib.rs               module map, and the payload types the server sends
  agent.rs             the run loop, and the update tool it dispatches to
  config.rs            the TOML in examples/agent.toml, as types
  identity.rs          resolving the device identifier
  transport.rs         the websocket, and where authentication happens
  link.rs              the channel conversation, as pure functions
  message.rs           Phoenix Channels v2 wire format
  shared_secret.rs     the Plug.Crypto token NervesHub expects
  scripts.rs           support scripts
  bin/agent-ctl.rs     the on-device client
  ipc/
    protocol.rs        the wire format: frames, methods, events
    mod.rs             listener, peers, the one-controller rule
    policy.rs          answer + config -> action, including "nobody answered"
  update_tool/
    mod.rs             the UpdateTool trait
    sandbox.rs         downloads, verifies, writes to a file, stops
    fwup.rs
    rauc.rs
  extensions/          health, geo, logging, local_shell, network_identity
test/
  device/              bootable A/B QEMU rigs, both boot schemes
  image/               a container rig with a real fwup and no bootloader
  ab/                  the fwup A/B mechanics, without an agent
```

## Not implemented yet

**Resumable downloads.** A download interrupted at 90% over a metered link
starts again. `fwup` cannot resume, so it would have to be the agent's HTTP
client; `rauc` streaming can, and would do it itself.

**Client certificates.** NervesHub supports them, and they are the better answer
for a device that can hold a per-device key, since the identifier comes from the
certificate's CN and cannot drift from what the server sees. The config shape is
written down in `examples/agent.toml`; nothing behind it is implemented, and the
agent refuses such a config rather than starting and failing later.

**A D-Bus interface.** The idiomatic answer on Yocto, and RAUC is D-Bus-native.
Not the primary interface, because it needs a bus daemon that a minimal
single-purpose image often does not run.

## License

Apache-2.0. See [LICENSE](LICENSE).

[meta-nerveshub]: https://github.com/nerves-hub/meta-nerveshub
