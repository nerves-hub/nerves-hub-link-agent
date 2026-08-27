# Extensions

Extensions are everything NervesHub can ask a device for that is not firmware:
health metrics, a location, logs, a shell, network identities. They ride a
separate Phoenix channel from the device channel, because the platform's rule is
that extension traffic must never get in the way of an update.

All five are off by default. An extension sends data — or opens a way in — that
an operator may not expect a device to have, so each is asked for rather than
assumed.

## Negotiation

Both halves have to agree, and either can decline. The device does not start it:

```text
<- extensions:get         {"extensions": {"health": ["0.0.1"], "logging": ["0.1.0", "0.0.1"]}}
-> phx_join "extensions"  {"health": "0.0.1", "logging": "0.1.0"}
<- phx_reply              ["health"]
-> health:attached        {}
```

1. **The platform advertises** which extensions it has, and at which versions.
2. **The device offers back** the intersection: extensions its config enables,
   that the platform named, at a version both sides implement.
3. **The platform replies** with the subset it actually wants attached, which
   can be narrower again — an extension can be switched off per product and per
   device.
4. **The device confirms** each with `<key>:attached`. Only then does the server
   start asking it for anything.

The version is a capability check as much as a version. `logging` at 0.1.0 sends
a second's worth of lines per message; 0.0.1 sends one line per message. They are
different conversations, so a platform that has only the older one is not offered
logging at all, rather than being sent batches it cannot read.

A NervesHub old enough not to advertise never sends the first frame. Waiting
forever would cost the device every extension it has, so the agent joins anyway
a few seconds after the device topic, offering everything it implements — which
is what such a platform has always served. An advertisement the agent cannot
parse is treated as though none arrived.

Negotiation happens per session. A reconnect starts from nothing, because what
the platform has may have changed and it is a new socket either way.

## Events

Everything is scoped `<key>:<event>` in both directions.

```text
<- health:check                {}
-> health:report               {"value": {"metrics": {..}, "metadata": {..}}}

<- geo:location:request        {}
-> geo:location:update         {"latitude": .., "longitude": .., "source": ".."}

-> logging:send                {"lines": [{"level": .., "message": ..}, ..]}

<- local_shell:request_shell   {}
<- local_shell:shell_input     {"data": ".."}
<- local_shell:window_size     {"rows": .., "cols": ..}
-> local_shell:shell_output    {"data": ".."}
```

Nothing is reported on a schedule the device chose. health and geo answer when
asked; logging is the only extension that sends unprompted.

## health

Memory, CPU, load average and temperature, read from `/proc` and `/sys` and
answered when NervesHub asks — hourly, by default.

```toml
[extensions.health]
enabled = false
```

CPU is a delta between reports, so the first report after a restart omits it
rather than sending the since-boot average as though it were current.

**There is no `/proc` on macOS**, so a native macOS run reports an empty metric
set. Inventing numbers would be worse, but it does mean health is one of the
things you need a Linux box or a container to see working.

## geo

A position, sent when asked. Nothing is sent when a lookup fails: a location the
agent could not establish is not a location at the origin.

```toml
[extensions.geo]
enabled = false
source = { whenwhere = {} }
# source = { fixed = { latitude = -41.28, longitude = 174.77, accuracy = 10.0 } }
# source = { command = "/usr/bin/read-gps" }
```

| | |
| --- | --- |
| `whenwhere` | The Nerves project's GeoIP service, which reads the address the request came from. The same service and the same `source: "geoip"` that `nerves_hub_link` uses, so a mixed fleet lands on one map with one set of caveats. Nothing polls it. |
| `fixed` | A position someone measured. Wrong the moment the device moves, which for most installed hardware is never — and far more accurate than GeoIP. |
| `command` | Anything that prints `{"latitude": .., "longitude": ..}`. For devices with a GPS. |

## logging

```toml
[extensions.logging]
enabled = false
source = { journald = {} }
# source = { journald = { unit = "my-app.service" } }
# source = { command = "tail -F /var/log/messages" }
max_lines_per_batch = 100
```

`journalctl --follow` by default, narrowed to one service with `unit`. For a
system without systemd, any command that writes lines to stdout.

NervesHub limits how *often* a device may send rather than how much it may say,
so the agent collects lines for a second and sends them as one message.
`max_lines_per_batch` caps how many one message carries. The platform drops
whatever a message carries past its own cap, so raising this past 100 gains
nothing. Lines that do not fit wait for the next second, and lines the buffer
cannot hold are reported as dropped — a gap you can see beats one you cannot.

Log lines are held rather than discarded while the extension is negotiating, so
whatever the device said during startup still arrives once logging attaches.

Under systemd, run the agent so it logs with systemd's `<N>` priority prefix and
no timestamp of its own. A line then reaches NervesHub with one timestamp and
its real level, rather than two timestamps and `info`.

## local_shell

**Think before turning this on.** Every other extension lets the platform read
something. This one lets a NervesHub user run commands as whatever the agent
runs as, and the device does not get to ask who is on the other end — the
authorization happened entirely in NervesHub.

```toml
[extensions.local_shell]
enabled = false
command = "/bin/sh"
chunk_bytes = 4096
```

A real pty, resizable, streamed to the browser terminal.

Two things have to agree before a shell exists: this config, and NervesHub
attaching the extension. Both are runtime decisions on purpose. A device worth
getting a shell on is usually one that is already misbehaving, and requiring a
firmware update to enable one would put the tool behind the problem it exists
for.

Both QEMU rigs turn it on, because a throwaway VM on a loopback port is the only
place it can be exercised — it needs a real pty, a real terminal attached from
NervesHub, and a session to run under. That is a rig decision, not a template;
[`test/device/agent-fwup.toml`](../test/device/agent-fwup.toml) says so where the
block is.

## network_identity

Identities the device holds on networks NervesHub does not run. Asked for once
on attach and never polled, since an identity is long-lived by construction.

```toml
[extensions.network_identity]
enabled = false

[[extensions.network_identity.identities]]
service = "tailscale"
command = "tailscale status --json"
json_pointer = "/Self/PublicKey"

[[extensions.network_identity.identities]]
service = "wireguard"
command = "wg show wg0 public-key"
instance = "wg0"
```

Iroh, Tailscale, NetBird and WireGuard. Each entry is either a literal value or
a command; a command that emits JSON can be pointed at a field with
`json_pointer`, which saves needing `jq` on the device.

A source that fails is logged and skipped, so a device running one of these and
not the others still reports what it has.

The agent is told where to look rather than detecting anything. Four services,
four CLIs, four output formats — an agent that tried to track all of them would
be confidently wrong about one within a release.

## Build-time

Extensions are runtime configuration only; there are no cargo features for them.
The update tools are the build-time choice — see the
[README](../README.md#building-from-source).
