#!/bin/bash
# Parse, fetch and build the nerves-hub-link-agent recipe against poky.
#
#   ./support/yocto/build-test.sh parse    # layer + recipe parse only
#   ./support/yocto/build-test.sh fetch    # also fetch, validating every checksum
#   ./support/yocto/build-test.sh build    # also compile (needs a cross toolchain: hours)
#
# Yocto needs Linux and refuses to run as root, so this runs as an ordinary user
# inside a container.
#
# The build lives in a named Docker volume rather than a bind mount from the
# host. Yocto's sanity checker refuses a case-insensitive TMPDIR, and a macOS
# bind mount is case-insensitive -- APFS is by default. The volume is inside the
# Linux VM, so it is ext4 and case-sensitive. `docker volume rm nhla-yocto`
# reclaims the space.
set -euo pipefail

stage="${1:-parse}"
branch="${POKY_BRANCH:-scarthgap}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
volume="${YOCTO_VOLUME:-nhla-yocto}"

docker volume create "$volume" >/dev/null

docker run --rm \
    -v "$volume:/out" \
    -v "$root/support/yocto:/layers:ro" \
    -e "STAGE=$stage" \
    -e "POKY_BRANCH=$branch" \
    debian:bookworm bash -euxc '
        apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
            gawk wget git diffstat unzip texinfo gcc build-essential chrpath \
            socat cpio python3 python3-pip python3-pexpect xz-utils \
            debianutils iputils-ping python3-git python3-jinja2 python3-subunit \
            zstd liblz4-tool file locales libacl1 ca-certificates \
            python3-setuptools >/dev/null
        # bitbake insists on a UTF-8 locale.
        sed -i "s/^# en_US.UTF-8/en_US.UTF-8/" /etc/locale.gen && locale-gen

        useradd -m -u 1000 yb
        chown -R yb /out

        # -s /bin/bash because oe-init-build-env is a bash script, while the
        # default shell for this account is dash, which silently ignored the
        # build directory argument and put TMPDIR somewhere else entirely.
        #
        # No apostrophes in this block: it is inside a single-quoted bash -c.
        su yb -s /bin/bash -c "
            set -eux
            export LANG=en_US.UTF-8
            cd /out
            # https, not git://: port 9418 is blocked on a lot of networks and
            # fails as a connection timeout rather than as anything explanatory.
            [ -d poky ] || git clone -b \${POKY_BRANCH} --depth 1 \
                https://git.yoctoproject.org/git/poky /out/poky

            # The build config is recreated every run so layer ordering is
            # deterministic. A previous run left meta-nerveshub in
            # bblayers.conf, and once that layer declared a dependency on
            # meta-rauc, every bitbake-layers command failed before it could
            # add the layer that would have satisfied it.
            #
            # The caches that cost real time -- downloads and sstate -- live
            # outside the build directory and survive.
            rm -rf /out/build/conf

            cd /out/poky
            set +u; . ./oe-init-build-env /out/build; set -u
            cd /out/build

            cat >> conf/local.conf <<CONF
DL_DIR = \"/out/downloads\"
SSTATE_DIR = \"/out/sstate\"
# meta-rauc warns without this, and an image built without it has no RAUC
# support to speak of. See meta-rauc README.rst.
DISTRO_FEATURES:append = \" rauc\"
CONF

            # meta-rauc provides the rauc recipe the agent depends on. It is
            # cloned rather than vendored: it is upstream and does the bundle
            # and slot work, and this layer is only the agent.
            [ -d /out/meta-rauc ] || git clone -b \${POKY_BRANCH} --depth 1 \
                https://github.com/rauc/meta-rauc /out/meta-rauc

            # meta-rauc first: meta-nerveshub declares a dependency on it.
            bitbake-layers add-layer /out/meta-rauc
            bitbake-layers add-layer /layers/meta-nerveshub

            # qemuarm64 matches the architecture the rigs run.
            sed -i \"s/^MACHINE ??=.*/MACHINE ??= \\\"qemuarm64\\\"/\" conf/local.conf
            grep -q BB_NUMBER_THREADS conf/local.conf || {
                echo \"BB_NUMBER_THREADS = \\\"\$(nproc)\\\"\" >> conf/local.conf
                echo \"PARALLEL_MAKE = \\\"-j \$(nproc)\\\"\" >> conf/local.conf
            }

            bitbake-layers show-layers

            case \"\${STAGE}\" in
                parse) bitbake -p ;;
                fetch) bitbake -p && bitbake -c fetch nerves-hub-link-agent ;;
                build) bitbake nerves-hub-link-agent ;;
            esac
        "
    '
