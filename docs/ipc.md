# Talking to the agent

Newline-delimited JSON over a Unix domain socket, one object per line. Default
path `/run/nerves-hub-link-agent.sock`.

Any language with a JSON library and a socket can speak it in a few lines,
which matters more than compactness — the traffic is a handful of messages an
hour. Rust callers can use the types in `nerves_hub_link_agent::ipc::protocol`
instead of building the JSON by hand.

## It is a peer protocol, not a client/server one

The interesting question is not what an application can ask the agent. It is how
the agent asks the application *whether to install*. So requests travel in both
directions and both sides answer them.

Request ids are per-direction and independent. The agent's `"1"` and the
application's `"1"` are different requests.

## Connecting

The first line an application sends is `hello`; nothing else is accepted before
it.

```json
{"type":"hello","name":"line-controller","role":"controller","api":1,
 "subscribe":["connection","update_progress"]}
```

```json
{"type":"welcome","agent_version":"0.1.0","api":1,"role":"controller","update_tool":"fwup"}
```

`role` is `observer` (default) or `controller`.

- **observer** — receives subscribed events, may call methods, is never asked to
  decide anything. Any number may connect.
- **controller** — additionally answers `update_available` and `reboot_request`.
  **At most one.** A second is refused with `controller_taken` rather than
  replacing the first: two processes each believing they decide whether the
  device updates is a bug, and it should surface on a bench at connect time
  rather than as a fleet that updates when it was told not to.

`update_tool` in the welcome lets an application refuse to run on a device whose
format it does not understand.

## Application → agent

| Method | Result |
| --- | --- |
| `status` | connection state, identifier, running firmware, update in flight, whether validation is still owed |
| `mark_valid` | `{}` |
| `reboot` | `{}` — does not return; the agent tells NervesHub, then reboots |
| `subscribe` | `{}` |
| `metrics` | `{}` |

```json
{"type":"request","id":"1","method":"status"}
{"type":"response","id":"1","result":{
  "connection":"connected",
  "identifier":"1000000012345678",
  "update_tool":"fwup",
  "firmware":{"uuid":"7f3c...","version":"1.4.2","product":"gateway","platform":"rpi4","architecture":"arm"},
  "update":null,
  "pending_validation":true}}
```

`mark_valid` is the application's call and not the agent's. The agent knows the
download succeeded and the system booted, which is not the same as knowing the
device works. Until someone calls it, the bootloader is still holding a
rollback — `status.pending_validation` says whether that is outstanding.

`metrics` merges application-supplied readings into the health report, so an
application can publish queue depth or sensor state without opening its own
connection to NervesHub.

Errors carry a stable code:

```json
{"type":"response","id":"1","error":{"code":"not_connected","message":"no session with the server"}}
```

`not_connected`, `unknown_method`, `controller_taken`, `unsupported`.

## Agent → application

Only the controller receives these.

### `update_available`

```json
{"type":"request","id":"a1","method":"update_available","params":{
  "firmware":{"uuid":"91be...","version":"1.5.0","product":"gateway","platform":"rpi4","architecture":"arm"},
  "size":41127936,
  "deployment_id":42}}
```

```json
{"type":"response","id":"a1","result":{"action":"reschedule","delay_ms":600000,"reason":"cycle in progress"}}
```

`action` is `apply`, `ignore` (with `reason`), or `reschedule` (with `delay_ms`
and `reason`). The three map onto statuses NervesHub already understands, which
is the difference between a deliberate deferral and a device that looks broken:
`ignore` puts the device in its deployment's penalty box, and `reschedule`
blocks updates for exactly the delay asked for. `delay_ms` is not decoration —
the server computes `updates_blocked_until` from it and refuses the message
without it.

### `reboot_request`

Sent once the update is installed and staged, before rebooting into it.

```json
{"type":"request","id":"a2","method":"reboot_request","params":{
  "firmware":{"uuid":"91be...","version":"1.5.0"}}}
```

```json
{"type":"response","id":"a2","result":{"action":"defer","delay_ms":1800000,"reason":"operator present"}}
```

`action` is `reboot` or `defer`. This is a separate question from
`update_available` on purpose: an application happy to download at any time may
still be in the middle of something it cannot be interrupted during, and
conflating the two forces it to refuse the download in order to protect the
reboot. Deferral is bounded by `reboot.max_defer_secs`.

### `identify`

An operator pressed Identify in the web UI. Blink something. Answer `{}`.

## Events

Fire-and-forget, sent to subscribers, never answered.

| Event | Payload |
| --- | --- |
| `connection` | `{"state":"connected"}` |
| `update_progress` | `{"stage":"downloading","percent":42}` |
| `update_installed` | `{"firmware":{...}}` |
| `update_failed` | `{"reason":"..."}` |
| `reboot_pending` | `{"deferred_until_ms":1800000}` |

```json
{"type":"event","event":"update_progress","payload":{"stage":"downloading","percent":42}}
```

`stage` is `downloading` or `updating`. With fwup they are genuinely different
phases and the second is the slow one on eMMC; with RAUC streaming they overlap.

## When nobody is listening

Every question the agent asks has a deadline and a configured answer for each
way it can go unanswered — see `[updates]` and `[reboot]` in
`examples/agent.toml`. "No controller connected" and "controller did not
answer" are configured separately, because a device between application restarts
is not the same as a device whose application is wedged.

An application disconnecting mid-request resolves that request as a timeout
immediately rather than leaving the agent parked on an id.

## Access control

There is none beyond the filesystem. Anyone who can open the socket can approve
an update, defer a reboot, and read the device's identity — set `ipc.group` and
`ipc.mode` accordingly.
