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

## The Rust version

**The agent needs Rust 1.85, and no released Yocto ships one that new.**
scarthgap has cargo 1.75 and walnascar 1.84 -- one version short. The recipe is otherwise correct on
scarthgap -- it parses, resolves layers, fetches the source and all 220 crates,
and reaches `do_compile`, where cargo stops with:

```
feature `edition2024` is required
… not stabilized in this version of Cargo (1.75.0)
```

1.85 is where edition 2024 was stabilised, and crates the agent compiles have
moved to it. Dropping reqwest already took the floor from 1.88 to 1.85; going
lower would mean pinning a widening set of transitive crates below their
releases.

Two ways out:

1. **A poky release with Rust >= 1.85**, once there is one.
2. **[meta-rust-bin](https://github.com/rust-embedded/meta-rust-bin)**, which
   supplies a prebuilt toolchain independent of the release. This is the usual
   answer for a recipe needing a newer Rust than its release ships.

## Updating the crate list

Yocto fetches offline, so every crate is declared as a source rather than
resolved by cargo during `do_compile`. After a `Cargo.lock` change:

```
bitbake -c update_crates nerves-hub-link-agent
```

That rewrites `nerves-hub-link-agent-crates.inc`.
