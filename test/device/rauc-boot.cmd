# Boot script for the RAUC rig.
#
# Separate from boot.cmd, which serves the fwup rig, because the two are
# different systems rather than two configurations of one. There, the boot
# script owns slot selection and fwup writes what it is told. Here RAUC owns it,
# and the script's job is to carry out a decision already recorded.
#
# # RAUC's u-boot convention
#
#   BOOT_ORDER    bootnames, most preferred first: "b a"
#   BOOT_a_LEFT   attempts remaining for slot a
#   BOOT_b_LEFT   attempts remaining for slot b
#
# `rauc install` puts the newly written slot first in BOOT_ORDER and gives it a
# fresh attempt count. `rauc status mark-good` restores the count for the booted
# slot; `mark-bad` zeroes it. This script picks the first slot in BOOT_ORDER
# with attempts left, spends one, and boots it.
#
# Spending the attempt *before* booting is deliberate and is the whole mechanism:
# a slot that hangs or panics never runs code again, so anything that waited
# until after a failure would never record one.
#
# # Why the slots are written out longhand
#
# u-boot has no nested expansion — `${BOOT_${slot}_LEFT}` is not a thing — so a
# loop cannot reach a per-slot variable by name. Two slots written out is
# duller than a general solution and it is the one that works.

# The environment offset on the SD card, in 512-byte blocks: 0x80000 bytes in,
# 0x20000 long. The same three numbers appear in uboot-rauc.config's
# CONFIG_ENV_OFFSET/SIZE and in /etc/fw_env.config, and nothing checks that they
# agree — a mismatch reads as a bad CRC rather than as a mistake.
setenv env_lba 0x400
setenv env_size 0x20000

virtio scan
mmc dev 0

# Load the environment here rather than relying on u-boot to have done it.
#
# u-boot loads its environment before PCI is enumerated, so the MMC device does
# not exist yet and the load fails with "MMC Device 0 not found" — every boot
# would start from the defaults below and the attempt counter would never
# advance, which is the whole mechanism. By this point the device is up, so the
# same bytes read fine.
#
# Writing them back is `saveenv`, which works: it runs after board init and goes
# through u-boot's own MMC environment driver rather than `env export`, whose
# output its own `env import` rejects.
if mmc read ${loadaddr} ${env_lba} 0x100; then
    if env import -c ${loadaddr} ${env_size}; then
        echo "rauc: environment loaded from mmc"
    else
        echo "rauc: environment failed its CRC, using defaults"
    fi
else
    echo "rauc: no environment on mmc, using defaults"
fi

# Defaults for a device that has never been updated: RAUC writes these on its
# first install, and before that the environment has nothing to say.
if test -z "${BOOT_ORDER}"; then
    setenv BOOT_ORDER "a b"
fi
if test -z "${BOOT_a_LEFT}"; then
    setenv BOOT_a_LEFT 3
fi
if test -z "${BOOT_b_LEFT}"; then
    setenv BOOT_b_LEFT 3
fi

setenv rauc_slot ""

for slot in ${BOOT_ORDER}; do
    if test -z "${rauc_slot}"; then
        if test "${slot}" = "a"; then
            if test ${BOOT_a_LEFT} -gt 0; then
                setexpr BOOT_a_LEFT ${BOOT_a_LEFT} - 1
                setenv rauc_slot a
                setenv slot_part 1
            fi
        fi
        if test "${slot}" = "b"; then
            if test ${BOOT_b_LEFT} -gt 0; then
                setexpr BOOT_b_LEFT ${BOOT_b_LEFT} - 1
                setenv rauc_slot b
                setenv slot_part 2
            fi
        fi
    fi
done

if test -z "${rauc_slot}"; then
    # Every slot has spent its attempts. Booting the first one anyway beats
    # stopping at a prompt: a device in a cupboard cannot be rescued from a
    # u-boot console, and the slot may well work now for reasons that had
    # nothing to do with the firmware.
    echo "rauc: no slot has attempts left, falling back to a"
    setenv rauc_slot a
    setenv slot_part 1
    setenv BOOT_a_LEFT 1
fi

saveenv

echo "rauc: booting slot ${rauc_slot} from /dev/vda${slot_part} (order ${BOOT_ORDER}, a=${BOOT_a_LEFT} b=${BOOT_b_LEFT})"

ext4load virtio 0:${slot_part} ${kernel_addr_r} /boot/vmlinuz
ext4load virtio 0:${slot_part} ${ramdisk_addr_r} /boot/initrd.img

# `rauc.slot` is how RAUC works out which slot is running. It is the only
# thing here that has to agree with system.conf's bootnames.
setenv bootargs "root=/dev/vda${slot_part} rw rootwait console=ttyAMA0 systemd.show_status=false rauc.slot=${rauc_slot}"

booti ${kernel_addr_r} ${ramdisk_addr_r}:${filesize} ${fdt_addr}
