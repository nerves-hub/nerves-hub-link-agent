# meta-nerveshub

A Yocto layer for the NervesHub device agent.

```
bitbake-layers add-layer /path/to/support/yocto/meta-nerveshub
```

```
IMAGE_INSTALL:append = " nerves-hub-link-agent"
DISTRO_FEATURES:append = " rauc"
```

`rauc` in `DISTRO_FEATURES` is meta-rauc's requirement, not this layer's; without
it meta-rauc warns and the image has no RAUC support to speak of.

The agent talks to `rauc install` over D-Bus, so the image needs `rauc` and its
service running. [meta-rauc](https://github.com/rauc/meta-rauc) does the bundle
and slot work; this layer is only the agent. See `docs/rauc.md` in the source
tree for what `system.conf` has to say, and `docs/deploying.md` for the service
user, the identifier and the rest of the deployment contract.

## Layers this one needs

```
bitbake-layers add-layer /path/to/meta-rauc
bitbake-layers add-layer /path/to/meta-rust-bin
bitbake-layers add-layer /path/to/support/yocto/meta-nerveshub
```

Both are declared in `LAYERDEPENDS`, so a missing one fails when the layer is
added rather than deep into a build.

**meta-rauc** supplies the `rauc` the agent shells out to.

**[meta-rust-bin](https://github.com/rust-embedded/meta-rust-bin)** supplies the
toolchain. The agent needs Rust 1.85 -- that is where edition 2024 was
stabilised, and crates it compiles have moved to it -- and no released Yocto
ships one that new: scarthgap has cargo 1.75, walnascar 1.84. meta-rust-bin
packages prebuilt upstream toolchains up to 1.98 and spans kirkstone through
wrynose, so the Rust version stops being a function of the Yocto release.

The recipe inherits `cargo_bin` from that layer rather than poky's `cargo`.

Their tradeoff, in their words: prebuilt toolchains "will never be able to
support architectures or options not supported by the Rust team itself", and
the prebuilt standard library may be less efficient than a custom-compiled one.
They are also upstream binaries rather than something your build system
compiled, which matters if you have reproducibility or audit requirements.

## Also required

`rauc` and `systemd` both have to be in `DISTRO_FEATURES`:

```
DISTRO_FEATURES:append = " rauc"
INIT_MANAGER = "systemd"
```

Without systemd, `systemd_system_unitdir` expands to nothing and the unit
installs nowhere -- the package builds, ships a binary with nothing to start
it, and says nothing. The recipe guards the install so that is a missing unit
rather than a broken one, but the image still needs systemd for the agent to
run as intended.

## Updating the crate list

Yocto fetches offline, so every crate is declared as a source rather than
resolved by cargo during `do_compile`. After a `Cargo.lock` change:

```
bitbake -c update_crates nerves-hub-link-agent
```

That rewrites `nerves-hub-link-agent-crates.inc`.
