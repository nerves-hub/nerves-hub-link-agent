################################################################################
#
# nerves-hub-link-agent
#
################################################################################

# Pinned to a commit rather than a tag because there is no release yet. Replace
# both lines with a tag when there is one:
#
#   NERVES_HUB_LINK_AGENT_VERSION = v0.1.0
#   NERVES_HUB_LINK_AGENT_SITE = $(call github,nerves-hub,nerves-hub-link-agent,$(NERVES_HUB_LINK_AGENT_VERSION))
#
NERVES_HUB_LINK_AGENT_VERSION = 2e8c6e21e7f3416430bac3591fba8e85422d07cb
NERVES_HUB_LINK_AGENT_SITE = https://github.com/nerves-hub/nerves-hub-link-agent.git
NERVES_HUB_LINK_AGENT_SITE_METHOD = git

NERVES_HUB_LINK_AGENT_LICENSE = Apache-2.0
NERVES_HUB_LINK_AGENT_LICENSE_FILES = LICENSE

# fwup only. A Buildroot image is the fwup case, and building the RAUC
# installer into it would ship code the device can never reach. `sandbox` is in
# the crate's default feature set on purpose -- a build that has not been told
# which real updater to use should not be able to write to a disk -- so a device
# build has to opt out of the defaults.
NERVES_HUB_LINK_AGENT_CARGO_BUILD_OPTS = --no-default-features --features fwup

# The agent downloads from the network and runs support scripts, so it does not
# run as root. It still needs to write the update slot and the bootloader
# environment, which on a real device means group access to the block device.
#
# Fields: username uid group gid password home shell groups comment
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
