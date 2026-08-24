# Using the agent with fwup

`fwup` is the updater Nerves uses, and the one to reach for on a Buildroot or
Debian image where you control the partition layout. The archive streams into
`fwup`'s stdin as it downloads, so the device needs no free space beyond the
slot it writes into, and download and write are one phase rather than two.

This guide covers what the image has to provide, how to configure the agent,
and how to run the whole lifecycle on a QEMU rig that boots, rolls back and
validates for real.

- [What the image must provide](#what-the-image-must-provide)
- [Configuring the agent](#configuring-the-agent)
- [Try it: the QEMU rig](#try-it-the-qemu-rig)
- [How rollback actually works](#how-rollback-actually-works)
- [Gotchas](#gotchas)

## What the image must provide

Four things, and the agent checks the ones it can at startup rather than
discovering them when a deployment arrives.

**An A/B fwup.conf with an `upgrade` task.** The agent runs
`fwup --apply --task upgrade -d <device> -i -`, so the config needs a task of
that name that writes the slot the device is *not* running from.
[`test/device/fwup.conf`](../test/device/fwup.conf) is a complete worked
example: a fixed partition table, two rootfs slots, a data partition, and
`fw_active` in the u-boot environment naming which slot to boot.

**Somewhere to read the running firmware's metadata.** Off Nerves there is no
convention, so this is configuration. Either a file the build wrote into the
rootfs, or `fw_printenv` on a device with a u-boot environment. Both parse as
`key=value` lines, so one parser covers both:

```toml
metadata = { command = "fw_printenv" }          # a u-boot device
metadata = { file = "/etc/nerves-hub/firmware.env" }   # anything else
```

The file is true by construction, since the rootfs *is* the firmware. What it
cannot tell you is anything about the other slot or whether this boot is still
on probation, so a device with a bootloader should read the environment.

**Per-slot metadata, if you want rollback to report honestly.** Write each
slot's identity under its own prefix — `a.fw_uuid`, `b.fw_version` — with
`fw_active` naming which is live. With a single unprefixed set, a rollback
leaves the environment describing firmware that is no longer running, and the
device reports the version that just failed as though it had succeeded.

**A way to mark a boot good.** See [validation](#validation-runs-a-fwup-task).

### Variable naming

The agent reads `fw_validated`, `fw_active`, `fw_uuid` and friends, and falls
back to `nerves_fw_*` for each. Both work. Use the bare names on a system that
is not Nerves, since the prefix implies a dependency that does not exist; keep
the prefixed ones if you adapted a Nerves system's fwup.conf, which is the most
likely way anyone gets a real one.

NervesHub's *wire* protocol is a separate matter and does use `nerves_fw_*`.
That is what the server parses from a join, it is per-update-tool, and the agent
maps between the two. The server never sees an environment variable name.

## Configuring the agent

```toml
[update_tool]
name = "fwup"
device = "/dev/mmcblk0"
binary = "/usr/bin/fwup"
task = "upgrade"

# Verify the archive on the device. NervesHub checked the signature at upload,
# which says nothing about the bytes that arrived here.
public_key = "..."

# Required. Without it the agent refuses to start -- see below.
confirm_command = "/usr/bin/fwup --apply --task validate --no-unmount -d /dev/mmcblk0 -i /usr/share/fwup/ops.fw --quiet"

metadata = { command = "fw_printenv" }

# `device` is a block device, which the agent refuses unless told in as many
# words. fwup writes to whatever `-d` names, immediately and with no undo, and
# the difference between a test rig and a workstation is one typo.
allow_block_device = true
```

### Validation runs a fwup task

A device that boots new firmware and never confirms it gets rolled back. The
agent never confirms on its own — only the application on the device knows
whether it actually works — so confirmation arrives over IPC as `mark_valid`,
and `confirm_command` is what the agent runs when it does.

Point it at a **fwup task**, not at `fw_setenv`. The config that wrote
`fw_validated=0` when it applied the update should be the one that clears it.
An `fw_setenv fw_validated 1` in the agent's configuration names the variable a
second time, in a second place, and that is exactly how this project's own two
spellings drifted apart.

The task needs an archive to run from, and a device holds no copy of its own
firmware, so ship a small resource-less one. This is the same job Nerves gives
`revert.fw`. The whole of it is
[`test/device/ops.conf`](../test/device/ops.conf):

```
uboot-environment uboot-env {
    block-offset = ${UBOOT_ENV_OFFSET}
    block-count = ${UBOOT_ENV_COUNT}
}

task validate {
    on-init {
        uboot_setenv(uboot-env, "fw_validated", "1")
    }
}
```

Build it into the rootfs at image build time and the command above finds it. The
task is indifferent to which slot is running — `fw_active` already names that —
so it is safe to run twice.

`confirm_command` is **required** for fwup, and the agent refuses to start
without one. An earlier version started fine, then answered `mark_valid` with
success, reported `firmware_validated` to NervesHub, and left `fw_validated` at
`0` on the disk. The server is told the update is good while the bootloader
counts down to reverting it, and the two only disagree out loud after the
reboot.

## Try it: the QEMU rig

[`test/device/`](../test/device/) builds a bootable A/B image — Debian, stock
kernel, systemd — and runs it under QEMU with `hvf`, so an arm64 guest on an
arm64 host is not emulated. It boots, reboots, rolls back and validates, which
is everything a container cannot do.

```bash
docker build --build-arg BOOT_SCHEME=fwup -t nerves-hub-link-agent:device .
```

```bash
./test/device/build.sh 3.0.0
```

Write it to a disk image the way a factory would, then boot it:

```bash
docker run --rm -v "$PWD/tmp/device:/out" --entrypoint bash nerves-hub-link-agent:device -c 'fwup -a -d /out/disk.img -i /out/SmartKiosk-3.0.0.fw -t complete -U --quiet'
```

```bash
BOOT_SCHEME=fwup ./test/device/run-qemu.sh
```

`ctrl-a x` quits; `--headless` logs to `tmp/device/console.log` instead. The VM
forwards ssh on 2222:

```bash
ssh -i tmp/keys/qemu_ed25519 -p 2222 root@localhost
```

Point the agent at your NervesHub by editing
[`test/device/agent-fwup.toml`](../test/device/agent-fwup.toml) before the
docker build. See [Running against a local NervesHub](../README.md#try-it) for
the shared-secret setup.

### Watching the lifecycle

Build a second version and push it from a deployment group. On the device:

```bash
agent-ctl status
```

```
"version": "3.0.1"
"pending_validation": true
```

The boot script says the same thing on the console:

```
fw: env loaded, active=b validated=0
fw: slot b is unvalidated, arming the boot counter
fw: booting slot b from /dev/vda2
```

Confirm it, and the countdown stops:

```bash
agent-ctl mark-valid
```

Or do nothing, reboot three times, and watch it revert:

```
cycle 1-3   booting slot b   (unvalidated, counting)
cycle 4     booting slot a   <- Bootlimit (3) exceeded, altbootcmd
cycle 5-6   booting slot a   <- and it stays there
```

### The container rig

[`test/image/`](../test/image/) is the same idea without a bootloader: a real
fwup, a real signed archive, a real A/B disk image, and an update that verifies
its signature, picks the free slot and leaves the running one untouched. It is
faster than the VM and it cannot boot either slot, so `boot_state` stays
`Unknown` and rollback goes unexercised. Use it for the install path, the VM for
everything after it.

The integration tests run there too:

```bash
docker build --target test -t nerves-hub-link-agent:ci .
```

```bash
docker run --rm -v "$PWD:/work" -w /work nerves-hub-link-agent:ci cargo test --test fwup_install -- --ignored
```

## How rollback actually works

Worth reading before adapting the rig to real hardware, because most of it is
forced by constraints that are not obvious.

**The bootloader chooses the slot.** An earlier layout swapped the MBR so that
partition entry 0 was always live, which needs no bootloader at all — and cannot
roll back, because reverting means deciding at boot that the slot you were told
to use has failed, and nothing in Linux runs early enough to decide it. So
`fw_active` names the slot, u-boot reads it, and rollback becomes a variable
u-boot can change.

**Two environments, with different owners.** The *disk* block holds the
firmware's identity and `fw_validated`: fwup writes it on apply, Linux reads and
writes it through `fw_printenv`/`fw_setenv`, u-boot imports it read-only. The
*flash* environment is u-boot's own and holds the boot counter, maintained
through `saveenv`.

They are split because u-boot cannot write the disk block — its `env export -c`
produces a blob its own `env import -c` rejects, verified at the same address
and size with no disk involved. The split turned out to be the better design
anyway: the counter is u-boot's business and nothing else needs to see it.

**The bootloader passes the truth on the kernel command line.** After a rollback
the disk still names the slot that failed, and u-boot cannot rewrite it. So
`fw_slot` and `fw_validated` go in `bootargs` and the agent trusts those over
the environment. Linux can see which *partition* it is rooted on, but not which
*slot* that partition is meant to be.

**The agent corrects the environment at startup.** That disagreement is not
cosmetic: fwup picks the slot to write by reading `fw_active`, so left stale,
the next update would target the slot currently running and overwrite the
working system from under itself.

**A rollback has to outlive the boot that decided it.** The disk still says the
new slot is active, and nothing will change that until the next update, so the
decision is recorded in flash as an override tagged with the UUID it was made
against. Without the tag the override is permanent; without the override the
device retries the broken firmware on every power cycle.

## Gotchas

**`CONFIG_BOOTCOUNT_LIMIT` is off in Debian's u-boot.** The rig builds u-boot
from source ([`test/device/uboot.config`](../test/device/uboot.config)) for this
one option. Without it nothing counts and nothing reverts.

**The boot counter only counts while an update is unvalidated.** `bootcount_env`
increments only when `upgrade_available` is set, so leaving it clear is how a
validated system stops counting down.

**`distro_bootcmd` does not exist in u-boot 2025.** It moved to bootstd
(`bootflow scan -lb`). A boot script copied from an older system triggers a
rollback and then drops to a prompt.

**fwup writes errors to stdout, not stderr.** With `-n` its progress goes to
stdout and so does the reason it gave up, so a parser that keeps only the
integers throws away the one sentence explaining a failed update. The agent
keeps both.

**`u-boot-tools` has no `fw_printenv` on Debian 12.** It is in
`libubootenv-tool`.

**fwup refuses to write a mounted device** unless passed `-U`/`--no-unmount`.
The agent always passes it; a command you run by hand needs it too.
