# Choose a slot, arm the boot counter, and let u-boot handle the rest.
#
# Run by u-boot's distro boot, which scans partitions for /boot/boot.scr. It
# lives in the rootfs, so both slots carry an identical copy.
#
# # Two environments, and why
#
# The *disk* environment holds the firmware's identity and whether it has been
# validated. fwup writes it when it applies an update, Linux reads and writes it
# with fw_printenv/fw_setenv, and this script imports it read-only. Nothing here
# writes it: u-boot's `env export -c` produces a blob its own `env import -c`
# rejects, so that path is not usable.
#
# The *flash* environment is u-boot's own, and holds the boot counter. u-boot
# maintains it natively through `saveenv`, which does work.
#
# The counter is only armed while an update is unvalidated. That is not an
# optimisation — u-boot's bootcount_env backend refuses to store a count unless
# `upgrade_available` is set, so leaving it clear is how a validated system
# stops counting.

# Variables are `fw_*`: this is not a Nerves system. u-boot's own names —
# bootcount, bootlimit, altbootcmd, upgrade_available — are its and stay as
# they are.

setenv env_lba 0x400
setenv env_size 0x20000

virtio scan

if virtio read ${loadaddr} ${env_lba} 0x100; then
    if env import -c ${loadaddr} ${env_size}; then
        echo "fw: env loaded, active=${fw_active} validated=${fw_validated}"
    else
        echo "fw: environment failed its CRC, booting slot a unvalidated-safe"
        setenv fw_active a
        setenv fw_validated 1
    fi
else
    echo "fw: no environment, booting slot a"
    setenv fw_active a
    setenv fw_validated 1
fi

# A rollback has to outlive the boot that decided it.
#
# The disk environment still says the new slot is active — u-boot cannot write
# it, and fwup will not until the next update. So the decision is recorded in
# u-boot's own flash environment as an override, tagged with the UUID it was
# made against. Without the tag the override would be permanent; without the
# override the device would retry the broken firmware on every power cycle,
# roll back, and retry again forever.
if test "${fw_rollback}" = "1"; then
    echo "fw: ROLLING BACK from ${fw_active} to ${fw_previous}"

    setenv fw_override ${fw_previous}
    setenv fw_override_for ${fw_uuid}

    # Stop counting: the slot being booted now is the one that was working, and
    # counting it would eventually roll back to the firmware just abandoned.
    setenv upgrade_available 0
    setenv bootcount 0
    setenv fw_rollback 0
    saveenv

    setenv fw_active ${fw_previous}
else
    # A different firmware has been installed since the override was recorded,
    # so the override is about something that is no longer on the device.
    if test "${fw_override_for}" != "${fw_uuid}"; then
        setenv fw_override
        setenv fw_override_for
    fi

    if test -n "${fw_override}"; then
        echo "fw: ${fw_active} was rolled back, staying on ${fw_override}"
        setenv fw_active ${fw_override}
        # The slot being booted is the one that was working before the failed
        # update, so it is validated by construction. Leaving this at the disk
        # value would have Linux believe it owes a validation for firmware it
        # is not running.
        setenv fw_validated 1
        setenv upgrade_available 0
        setenv bootcount 0
    else
        if test "${fw_validated}" = "1"; then
            # Validated: disarm, so a later reboot is not counted against
            # anything.
            setenv upgrade_available 0
            setenv bootcount 0
        else
            # On probation. Arming the counter is what makes u-boot persist it
            # and run altbootcmd once bootlimit is passed.
            echo "fw: slot ${fw_active} is unvalidated, arming the boot counter"
            setenv upgrade_available 1
        fi
    fi

    setenv bootlimit 3
    # `run bootcmd`, not `run distro_bootcmd`. u-boot 2025 replaced the distro
    # boot scripts with bootstd, so `bootcmd` is `bootflow scan -lb` and
    # `distro_bootcmd` no longer exists — a rollback referencing it triggers
    # correctly and then drops to a prompt, which reads as the counter failing
    # rather than the command being wrong.
    setenv altbootcmd "setenv fw_rollback 1; run bootcmd"
    saveenv
fi

if test "${fw_active}" = "b"; then
    setenv slot_part 2
else
    setenv slot_part 1
fi

echo "fw: booting slot ${fw_active} from /dev/vda${slot_part}"

# The kernel comes out of the slot being booted. A/B is only honest if the
# kernel is part of what gets replaced.
ext4load virtio 0:${slot_part} ${kernel_addr_r} /boot/vmlinuz
ext4load virtio 0:${slot_part} ${ramdisk_addr_r} /boot/initrd.img

# `fw_slot` is the truth about what is being booted, including after a
# rollback, when the environment on disk still names the slot that failed.
# Linux has no other way to know: it can see which partition it is rooted on,
# but not which slot that is meant to be.
# `rauc.slot` is how RAUC works out which slot is booted. It is the same answer
# as `fw_slot`, under the name RAUC looks for, so the two tools cannot disagree
# about which half of the device is running.
setenv bootargs "root=/dev/vda${slot_part} rw rootwait console=ttyAMA0 systemd.show_status=false fw_slot=${fw_active} fw_validated=${fw_validated} rauc.slot=${fw_active}"

booti ${kernel_addr_r} ${ramdisk_addr_r}:${filesize} ${fdt_addr}
