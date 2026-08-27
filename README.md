# NervesHubLink Agent

A NervesHub device agent for Linux that is not running Nerves. There is no BEAM
on the device, the application is its own process in its own language, and
firmware is written by a program that already exists: `fwup` on a Buildroot or
Nerves-adjacent image, `rauc` on Yocto.

The agent connects over Phoenix Channels, reports what firmware the device is
running, asks the application whether an update may be installed, and runs the
updater.

## Status

Working, and exercised end to end against a real NervesHub. Both update tools
install, roll back and validate on a QEMU rig with a real bootloader. It has not
yet run on production hardware.

Not implemented: client-certificate identity (the agent refuses that config
rather than starting and failing later) and resumable downloads.

## Try it

The fastest loop runs the agent natively against a NervesHub on your own
machine. It cannot touch anything outside `./tmp`.

Start NervesHub with `WEB_HOST` set to your machine's LAN IP, so the firmware
URLs it generates are reachable from wherever the agent runs:

```bash
WEB_HOST=192.168.1.10 mix phx.server
```

In the UI, create an org and a product, then Product → Settings → Shared
Secrets → create one. Put the key, the secret and that IP into
[`examples/local.toml`](examples/local.toml), then:

```bash
cargo run -- --config examples/local.toml
```

In another terminal, the stand-in for the application on the device:

```bash
python3 examples/controller.py tmp/agent.sock
```

Upload a firmware and send it to the device from a deployment group. The agent
asks the controller, the controller answers, and the whole conversation is
visible in both terminals.

### Why that is safe to run natively

`examples/local.toml` uses the **sandbox** update tool. It downloads firmware,
checks its SHA-256, writes it into `tmp/sandbox/` and stops. It never invokes an
updater, never opens a block device, and "reboot" is a log line.

That covers most of the agent: authentication, the join payload, deployment
targeting, progress, reconnects and the entire IPC decision path. It tells you
nothing about signature verification, slot switching or rollback, which is what
the real tools decide.

The moment the update tool is not the sandbox, that guarantee is gone — `fwup`
writes to whatever `-d` names, immediately and with no undo. Two things stand in
the way: the agent refuses to start if fwup's target is not a regular file
unless `allow_block_device` says otherwise, and `docker compose up --build` runs
it with no devices mapped in and no host paths mounted but the config.

### If it will not connect

The agent talks to the **web** endpoint on port 4000 at `/device-socket` with an
HMAC shared secret — one address for both the socket and the firmware download,
and no self-signed certificate to work around. The separate device endpoint on
4001 is the mutual-TLS one.

| | |
| --- | --- |
| `401 Unauthorized` | right path, secret rejected. Check the key and secret, and **check the clock**: the signature carries a timestamp the server refuses when it is more than 90 seconds stale, so a device whose clock has drifted fails in a way that reads exactly like a bad secret |
| `403 Forbidden` | you are on `/socket`, which on port 4000 is the *user* socket |
| connection refused | wrong port, or the server is not up |

`RUST_LOG=nerves_hub_link_agent=debug` logs the URL it dials and every frame in
both directions.

## Update tools

One per device, chosen in the config. Each has a guide covering what the image
must provide, how to configure the agent, and a QEMU rig that boots, rolls back
and validates for real.

| | |
| --- | --- |
| **[fwup](docs/fwup.md)** | The archive streams into `fwup`'s stdin as it downloads, so the device needs no free space beyond the slot it writes into. Deltas are NervesHub's job; the agent only has to report its fwup version honestly. |
| **[rauc](docs/rauc.md)** | `rauc install <url>`. The bundle is never downloaded first: RAUC streams it and fetches only the blocks the target slot lacks, so a small change costs a small download without anyone generating a patch. |
| **sandbox** | Downloads, verifies, writes to a file, stops. In the default feature set deliberately: a build that has not been told which real updater to use should not be able to write to a disk. |

Both rigs live in [`test/device/`](test/device/) and are selected at build time
with `--build-arg BOOT_SCHEME=fwup|rauc`. They are different systems, not one
system with a flag: different boot scripts, different bootloader environments,
different agent configs.

For a real device rather than a rig — cross-compiling, the service user, the
systemd unit, and Buildroot and Yocto packaging — see
**[docs/deploying.md](docs/deploying.md)**. The binary links nothing but libc,
which is the main reason TLS is rustls rather than the system OpenSSL.

A Buildroot br2-external tree is in [`support/buildroot/`](support/buildroot/)
and the Yocto layer in [meta-nerveshub][meta-nerveshub], each with a script that
builds it in a container. Both produce a working package: Buildroot against
2025.08, Yocto against scarthgap. The Yocto layer requires
[meta-rust-bin](https://github.com/rust-embedded/meta-rust-bin) for the
toolchain, because no released Yocto ships a Rust new enough.

## Why it looks like this

Two constraints account for most of the design.

**The agent cannot decide when to update.** The application knows whether the
machine is mid-cycle, whether an operator is standing at it, whether the queue
is drained. So "should I install this?" and "may I reboot now?" have to cross a
process boundary and come back with an answer — including when the application
has crashed, has not started yet, or never answers.

**The agent does not write firmware.** It runs something that does, and those
programs disagree about where bytes come from. `fwup` reads an archive from
stdin; `rauc` wants a URL so it can stream. So an update tool is handed the
update and owns the transfer, rather than being handed a sink to fill.

## Talking to the agent

Newline-delimited JSON over a Unix socket. Requests go both ways: the
application asks for status, the agent asks whether to install. Full protocol
and worked exchanges in [`docs/ipc.md`](docs/ipc.md).

```
hello        -> welcome
status       -> connection, identity, firmware, pending validation
mark_valid   -> confirm this boot, releasing the bootloader's rollback
reboot       -> reboot through the agent

update_available -> apply | ignore | reschedule
reboot_request   -> reboot | defer
identify         -> blink something

events: connection, update_progress, update_installed, update_failed, reboot_pending
```

Exactly one connection may be the `controller`, the one asked to decide. Others
are observers. A second controller is refused rather than replacing the first,
so the mistake surfaces at connect time on a bench instead of as a fleet that
updates when it was told not to.

Every question has a deadline and a configured answer for each way it can go
unanswered. An agent that blocks forever on an application that died is a device
that has quietly left the fleet while still looking healthy from the server.

### agent-ctl

A minimal image has no python, no socat and no nc, which leaves `mark_valid`
unreachable from a support script — the one place it most needs to be reachable
from.

```bash
agent-ctl status        # connection, identity, firmware, whether validation is owed
agent-ctl mark-valid    # confirm the running firmware, releasing the rollback
agent-ctl reboot
agent-ctl watch
```

It connects as an observer, asks one question and exits, so running it never
takes the controller slot from the application that owns it.

## Extensions

Everything NervesHub can ask a device for that is not firmware. All five are off
by default: an extension sends data, or opens a way in, that an operator may not
expect a device to have.

Both halves have to agree, and the platform asks first. It says which
extensions it has and at which versions; the agent offers back the ones its
config enables and it also implements; the platform replies with the subset it
wants attached, which narrows again per product and per device. A platform too
old to ask is offered everything, a few seconds after connecting.

Nothing is reported on a schedule the device chose — health and geo answer when
asked, and logging is the only one that sends unprompted.

| | |
| --- | --- |
| **health** | Memory, CPU, load and temperature from `/proc` and `/sys`. CPU is a delta between reports, so the first after a restart omits it rather than sending the since-boot average as though it were current. |
| **geo** | A position, from `whenwhere` GeoIP, a fixed configured location, or a command for devices with a GPS. Nothing is sent when a lookup fails: a location the agent could not establish is not a location at the origin. |
| **logging** | `journalctl --follow`, or any command that writes lines. Collected for a second and sent as one message, because NervesHub limits how often a device may send rather than how much it may say. Under systemd the agent logs with systemd's `<N>` priority prefix and no timestamp of its own, so a line reaches NervesHub with one timestamp and its real level rather than two timestamps and `info`. |
| **local_shell** | A real pty running a shell, resizable, streamed to the browser terminal. |
| **network_identity** | Iroh, Tailscale, NetBird or WireGuard keys, from configured commands. Asked for once on attach, never polled. |

**health reports nothing useful on macOS.** It reads `/proc`, which is not
there, so a native run sends an empty metric set. Inventing numbers would be
worse, but it does mean health is one of the things you need a container for.

**local_shell hands out a shell.** Whoever can open the tab in NervesHub runs
commands as whatever the agent runs as, and the device does not get to ask who
they are. It is off unless the config turns it on *and* NervesHub attaches it,
and both are runtime decisions: a device that needs looking at is one you can
already no longer reach, and having to ship it a firmware update first to get a
shell would put the tool behind the problem it exists for.

Both QEMU rigs turn it on, because a throwaway VM on a loopback port is the only
place it can be exercised: it needs a real pty, a real terminal attached from
NervesHub, and a session to run under. That is a rig decision, not a template —
`test/device/agent-fwup.toml` says so where the block is.

## Support scripts

A support script arrives as text and runs as a shell script. On Nerves it would
be Elixir evaluated in the running VM; there is no VM here, and the things
someone reaches for when a device misbehaves — `journalctl`, `systemctl status`,
`ip addr`, `df` — are commands rather than expressions.

The text is written to a file and run with `bash`. A script starting with `#!`
is made executable and run directly, so Python or `sh` works without configuring
anything. Running from a file rather than `-c` also means an error carries a
line number. `stdout` and `stderr` are merged in the order they happened, and
scripts get `NERVES_HUB_DEVICE_IDENTIFIER`, `NERVES_HUB_FIRMWARE_UUID` and
`NERVES_HUB_FIRMWARE_VERSION` in their environment.

**Scripts are per-product, and a product can hold both kinds of device.** The
same text goes to a Nerves device and to one running this agent, so
`VintageNet.info()` reaches a bash shell and comes back as a syntax error.
Scripts carry tags and NervesHub filters on them, which is the seam to use when
a product ends up mixed.

**Timeouts matter more than they look.** NervesHub drops a script's reference
after 15 seconds and stops listening, so the agent's deadline is 10 by default
and it kills the whole process group — not just the shell, which would leave
anything the script backgrounded still running with nobody watching.

Enabled by default, unlike the extensions. Turned off, the agent still *answers*
and says so: a device that silently ignored scripts would be indistinguishable
from one that had gone offline.

## Configuration

One TOML file, `/etc/nerves-hub-link-agent.toml` unless `--config` or
`$NERVES_HUB_AGENT_CONFIG` says otherwise. Nothing is read from the environment
field by field: a device's configuration should be one file an operator can look
at and diff, not a file plus a unit file plus whatever the init system exported.

[`examples/agent.toml`](examples/agent.toml) is every option, annotated. The
parts worth deciding before shipping anything:

- **Identity.** A shared secret says nothing about which device presents it, so
  the identifier is configured alongside it — a literal, a file to read, or a
  command to run. Nerves devices get this from `nerves_runtime`; a Yocto image
  has no such convention, which is why all three exist.
- **Update policy.** Defaults to applying without asking, matching
  `nerves_hub_link` out of the box. `ask` puts the controller in the path.
- **Reboot policy.** Separate from update policy on purpose. An application
  happy to download at any time may still be unable to reboot right now, and
  conflating the two forces it to refuse the download to protect the reboot.
- **How it reboots.** `sudo reboot` by default. The agent downloads from the
  network and runs support scripts, so it has no business running as root, which
  means a sudoers rule for exactly this one command:
  `agent ALL=(root) NOPASSWD: /sbin/reboot`. Set `reboot.command` to `reboot`
  where it already runs as root, or `systemctl reboot` for an init system that
  wants to sequence its own shutdown.

## Layout

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
docs/
  fwup.md              the fwup guide, including the QEMU rig
  rauc.md              the RAUC guide, including the QEMU rig
  deploying.md         onto a real device: cross-compiling, systemd, Buildroot, Yocto
  ipc.md               the protocol applications speak
test/
  device/              the bootable A/B rigs, both boot schemes
  image/               a container rig with a real fwup and no bootloader
  ab/                  the fwup A/B mechanics, without an agent
```

```bash
cargo test --all-features
```

The tests are on the parts with no I/O in them: the policy table, the frames,
the token, identifier resolution, and the payload shapes the server matches on.
That is deliberate. Those are where a mistake is silent — progress reports went
to NervesHub under a key the server does not read, and nothing anywhere failed.

## Still open

**Resumption.** A download interrupted at 90% over a metered link should not
start again. `fwup` cannot resume, so it would have to be the agent's HTTP
client; `rauc` streaming can, and would do it itself.

**A D-Bus interface.** The idiomatic answer on Yocto, and RAUC is D-Bus-native,
so an adapter is worth having. Not the primary interface, because it needs a bus
daemon that a minimal single-purpose image often does not run.

**Client certificates.** NervesHub supports them and they are the better answer
for a device that can hold a per-device key. The config shape is written down in
`examples/agent.toml`; nothing behind it is implemented.

[meta-nerveshub]: https://github.com/nerves-hub/meta-nerveshub
