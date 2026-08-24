# Using the agent with RAUC

RAUC is the updater to reach for on Yocto, and the one that fits NervesHub best.
Signing is mandatory in the bundle format, and a device installs by streaming
the bundle over HTTP range requests, fetching only the blocks its target slot
does not already hold. A small change costs a small download without NervesHub
generating a patch for it.

That last part is the whole reason to choose RAUC, and it puts two requirements
on the server that are easy to miss until a device is in front of you: the
firmware URL must honour `Range`, and it must outlive the install, which on a
slow link is considerably longer than a download of the same bundle.

- [What the image must provide](#what-the-image-must-provide)
- [Configuring the agent](#configuring-the-agent)
- [Try it: the QEMU rig](#try-it-the-qemu-rig)
- [How slot selection works](#how-slot-selection-works)
- [Gotchas](#gotchas)

## What the image must provide

**RAUC 1.9 or newer**, at both ends. The agent refuses to start below it, for
two unrelated reasons that happen to share a version: `[meta.<label>]` sections
arrived in 1.9, and `bundle.hash` was added to slot status in 1.9. A 1.8 device
installs and reboots perfectly well, and then cannot say what it is running.

**The `rauc` service, not just the binary.** `rauc install` is a D-Bus client;
the work happens in `rauc service`. A device without it gets `Error creating
proxy: Could not connect`, which reads like a broken bundle rather than a
missing daemon. The agent probes the service at startup so that failure lands
while someone is looking at it.

**A `system.conf` describing the slots and naming a bootloader.** The complete
rig version is [`test/device/rauc-system.conf`](../test/device/rauc-system.conf):

```ini
[system]
compatible=nerves-hub-agent-qemu
bootloader=uboot
# Outside the rootfs: an update overwrites the whole slot, and RAUC's record of
# what it installed has to outlive that.
statusfile=/data/rauc.status

[keyring]
path=/etc/rauc/keyring.pem

[slot.rootfs.0]
device=/dev/vda1
type=ext4
bootname=a
```

`bootloader=noop` is the trap here. RAUC will write the free slot and mark
nothing, so the device reboots onto the firmware it was already running and the
update looks like it did nothing.

**A keyring.** RAUC verifies bundles against its own keyring and refuses an
unsigned one, so there is no signature code in the agent and NervesHub never
holds the signing key. The trust anchor is provisioned by the image build.

**Verity bundles.** Streaming requires them, and NervesHub refuses a plain-format
bundle at upload: its signature is detached and its manifest lives inside the
SquashFS, so reading four lines of INI would mean unpacking a filesystem.

## Configuring the agent

```toml
[update_tool]
name = "rauc"
binary = "/usr/bin/rauc"

# The point of RAUC. Handing it a downloaded file gives up the streaming
# install entirely.
stream_from_url = true

# RAUC does its own transfer, so the agent's `danger_accept_invalid_certs` does
# not reach it. A NervesHub with a self-signed certificate needs both, and it
# should take two deliberate acts to give up both.
tls_no_verify = false
```

There is no `public_key`, no `device` and no `confirm_command`: RAUC knows its
own slots from `system.conf`, verifies against its own keyring, and marks a boot
good through `rauc status mark-good booted`, which the agent runs on
`mark_valid`.

### Identity

RAUC has no notion of a UUID and NervesHub requires one. It is derived from a
SHA-256 over the bundle's manifest, which is the digest RAUC itself computes:
`rauc info` reports it as `hash`, and RAUC records it against the slot it
installed into as `bundle.hash`.

That is what makes it recoverable rather than merely unique. The device reads
back the same value NervesHub derived at upload, without ever having been told
it.

## Try it: the QEMU rig

The RAUC rig is a *different system* from the fwup one, not the same system with
a flag: different boot script, different `fw_env.config`, different u-boot
build, different agent config. Pick it at build time.

```bash
docker build --build-arg BOOT_SCHEME=rauc -t nerves-hub-link-agent:device-rauc .
```

```bash
./test/device/build.sh 12.0.0
```

```bash
./test/device/build-rauc-bundle.sh 12.0.1
```

`build.sh` produces the rootfs the bundle is built from, so it has to run first.
Write the disk and boot:

```bash
docker run --rm -v "$PWD/tmp/device:/out" --entrypoint bash nerves-hub-link-agent:device-rauc -c 'fwup -a -d /out/disk.img -i /out/SmartKiosk-12.0.0.fw -t complete -U --quiet'
```

```bash
BOOT_SCHEME=rauc DEVICE_IMAGE=nerves-hub-link-agent:device-rauc ./test/device/run-qemu.sh
```

Upload the `.raucb` to NervesHub and push it, or install it by hand from a
range-capable server:

```bash
./test/device/range-server.py 8055 tmp/device
```

```bash
rauc install http://10.0.2.2:8055/SmartKiosk-12.0.1.raucb
```

### Watching the lifecycle

RAUC writes the free slot and records its choice in the u-boot environment:

```
before: BOOT_ORDER=a b BOOT_a_LEFT=2 BOOT_b_LEFT=3
after:  BOOT_ORDER=b a BOOT_a_LEFT=2 BOOT_b_LEFT=3
```

The boot script reads that, spends an attempt, and boots slot b:

```
rauc: booting slot b from /dev/vda2 (order b a, a=2 b=2)
```

Confirm the boot and the attempt count is restored, so a validated slot stops
counting down:

```bash
rauc status mark-good booted     # or: agent-ctl mark-valid
```

```
BOOT_b_LEFT: 2 -> 3
```

Leave it unconfirmed and each boot spends another attempt. At zero the boot
script falls through to the other entry in `BOOT_ORDER`, which is RAUC's
rollback.

## How slot selection works

**RAUC owns the decision.** The boot script only reads `BOOT_ORDER` and
`BOOT_x_LEFT`; it makes no choices of its own. That is the real difference from
the fwup rig, where the script picks the slot from `fw_active`.

**The environment lives on an SD card**, and getting there meant ruling out
everything else. RAUC records its choice by calling `fw_setenv` from Linux, so
Linux and u-boot have to share one store. On QEMU `virt` with virtio:

- **pflash**, u-boot's default, is invisible to Linux. Debian's arm64 kernel has
  no CFI or physmap driver, so `/proc/mtd` stays empty.
- **A raw offset on the disk** has no u-boot environment driver for virtio.
- **A file on ext4** fails twice over: u-boot loads its environment before
  virtio is enumerated, and its ext4 writer refuses a filesystem made by a
  current `mke2fs`.

An SD card works, and not by luck. Raw-block environments on MMC are what real
devices use, which is why u-boot has that driver and none for virtio.

**The boot script loads the environment itself.** Even on MMC, u-boot's built-in
load runs before PCI is enumerated and fails with `MMC Device 0 not found`. Left
alone, every boot starts from defaults and the attempt counter never advances —
the entire rollback mechanism, silently dead. `mmc read` plus `env import` after
board init reads the same bytes. Writing still goes through `saveenv`, which
works, unlike `env export`.

## Gotchas

**`rauc status --output-format=json` omits `slot_status` entirely.** Bundle
info, including the hash, needs `--detailed`. The plain form looks complete,
which is what makes this expensive.

**RAUC refuses to stream from a server without `Range` support.** This is not
optional and not a fallback. Python's `http.server` has none, which is why
[`range-server.py`](../test/device/range-server.py) exists; NervesHub's
`Plug.Static` does.

**`dm-verity` is a module Debian does not autoload.** Without it an install
verifies the signature and then fails with `Failed to load dm table`, which
reads like a corrupt bundle.

**A device's first firmware cannot come from fwup.** RAUC records identity when
*it* installs, so a slot written any other way leaves the device unable to say
what it is running until the first RAUC update lands.

**Lowercase bootnames.** They have to match what the boot script passes through.
A case mismatch does not fail loudly: RAUC finds no booted slot and the service
exits.

**`RAUC_TEST_CMDLINE` only exists on master**, not in any release, so it cannot
be used to fake a booted slot in a test image.
