SUMMARY = "NervesHub device agent"
DESCRIPTION = "Connects a Linux device to NervesHub over Phoenix Channels, reports \
the running firmware, asks the application on the device whether an update may be \
installed, and applies it through RAUC."
HOMEPAGE = "https://github.com/nerves-hub/nerves-hub-link-agent"
BUGTRACKER = "https://github.com/nerves-hub/nerves-hub-link-agent/issues"

LICENSE = "Apache-2.0"
LIC_FILES_CHKSUM = "file://LICENSE;md5=86d3f3a95c324c9479bd8986968f4327"

SRC_URI = "git://github.com/nerves-hub/nerves-hub-link-agent.git;protocol=https;branch=main \
           file://nerves-hub-link-agent.service \
           file://agent.toml \
           "

# Pinned to a commit because there is no release tag yet. Replace with the tag
# and set PV to match when there is one.
SRCREV = "2e8c6e21e7f3416430bac3591fba8e85422d07cb"
PV = "0.1.0+git"

S = "${WORKDIR}/git"

# Where `file://` entries in SRC_URI land. Newer releases unpack them into
# UNPACKDIR; on scarthgap it is undefined, and referring to it there silently
# resolves to nothing -- `install: cannot stat '/agent.toml'`. A weak default
# keeps one path working on both.
UNPACKDIR ??= "${WORKDIR}"

# `cargo_bin`, from meta-rust-bin, rather than poky's `cargo`. The toolchain
# comes from that layer because no released Yocto ships a Rust new enough --
# see the layer README. The class was called `cargo` in older meta-rust-bin and
# collided with poky's; `cargo_bin` is the current name.
inherit cargo_bin cargo-update-recipe-crates systemd useradd

# Yocto fetches offline, so cargo cannot resolve dependencies during
# do_compile. Regenerate with `bitbake -c update_crates nerves-hub-link-agent`
# whenever Cargo.lock changes.
require ${BPN}-crates.inc

# RAUC on a Yocto image. The crate's default feature set includes fwup and the
# sandbox, and a device should carry only the tool it has -- the features exist
# so an image that will never see a fwup archive does not contain the code to
# apply one.
CARGO_BUILD_FLAGS += "--no-default-features --features rauc"

# `rauc install` is a D-Bus client; the work happens in `rauc service`. Without
# it the agent gets "Error creating proxy: Could not connect", which reads like
# a broken bundle rather than a missing daemon. The agent probes for the
# service at startup so that lands while someone is looking at it.
RDEPENDS:${PN} += "rauc"

SYSTEMD_SERVICE:${PN} = "nerves-hub-link-agent.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

# The agent downloads from the network and runs support scripts, so it does not
# run as root. It still needs group access to whatever it writes -- see
# docs/deploying.md in the source tree.
USERADD_PACKAGES = "${PN}"
GROUPADD_PARAM:${PN} = "--system agent"
USERADD_PARAM:${PN} = "--system --no-create-home --shell /sbin/nologin --gid agent agent"

do_install:append() {
    install -d ${D}${sysconfdir}
    install -m 0644 ${UNPACKDIR}/agent.toml ${D}${sysconfdir}/nerves-hub-link-agent.toml

    # Guarded, because `systemd_system_unitdir` is empty on a distro without
    # systemd and the install then quietly puts the unit nowhere: the package
    # builds, ships a binary with nothing to start it, and says nothing.
    if ${@bb.utils.contains('DISTRO_FEATURES', 'systemd', 'true', 'false', d)}; then
        install -d ${D}${systemd_system_unitdir}
        install -m 0644 ${UNPACKDIR}/nerves-hub-link-agent.service \
            ${D}${systemd_system_unitdir}/nerves-hub-link-agent.service
    fi
}

FILES:${PN} += "${systemd_system_unitdir}/nerves-hub-link-agent.service"

CONFFILES:${PN} = "${sysconfdir}/nerves-hub-link-agent.toml"
