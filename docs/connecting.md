# Connecting the agent to NervesHub

The agent opens one WebSocket to NervesHub and keeps it open. Everything else —
update assignments, progress, support scripts, extensions — runs over that
connection. This guide covers getting it established and diagnosing it when it
does not.

## The `[server]` block

```toml
[server]
host = "devices.nervescloud.com"   # host only: no scheme, no path
port = 443
path = "/device-socket"
tls = true
# ca_certificate = "/etc/ssl/certs/internal-ca.pem"   # default: system trust store
heartbeat_interval_secs = 30
reconnect_backoff_secs = [1, 2, 5, 10, 30, 60]
```

`port`, `path` and `tls` shown above are the defaults, so a NervesCloud config
only needs `host`.

`reconnect_backoff_secs` is walked in order and then repeats the last value, so
the list above retries after 1, 2, 5, 10, 30, 60, 60, 60… seconds. The agent
reconnects forever; there is no give-up.

`heartbeat_interval_secs` must stay below the server's socket timeout, or the
server closes a connection the device believes is fine.

## NervesCloud

```toml
[server]
host = "devices.nervescloud.com"
```

That is the whole of it. Port 443, TLS against the system trust store, and
`/device-socket`.

## Your own NervesHub

A NervesHub deployment exposes two endpoints, and which one you point at
decides how the device authenticates.

| | |
| --- | --- |
| **the web endpoint** | Serves the UI, accepts shared secrets, and generates the firmware download URLs. One address for both the socket and the download. |
| **the device endpoint** | The mutual-TLS one, on `web_port + 1`. It also accepts shared secrets, but in a default deployment it serves a certificate your device has no reason to trust. |

While the credential is an HMAC shared secret, the web endpoint is the simpler
target: one host and port to get right, and no certificate to work around.
Client certificates are the reason the device endpoint exists, and the agent
does not implement them yet.

`DeviceSocket` is mounted at `/device-socket` on both. On the web endpoint,
`/socket` is the **user** socket — pointing a device there gets a WebSocket that
authenticates nothing and speaks a different protocol, rather than an error
saying so.

Behind a reverse proxy, the proxy has to forward the `Upgrade` and `Connection`
headers, and it must not strip the `x-nh-*` request headers the agent sends on
the handshake.

## Identity

A shared secret authenticates the product. The device identifier says which
device is presenting it, and is configured alongside:

```toml
[identity]
product_key = "nhp_..."
product_secret = "..."
identifier = { file = "/sys/firmware/devicetree/base/serial-number" }
# identifier = { command = "/usr/bin/read-serial" }
# identifier = { literal = "bench-01" }        # never in a shipped image
```

Create the key and secret under **Product → Settings → Shared Secrets**.

Resolution happens once at startup, and a failure is fatal — a device that
connected under a fallback identifier would be a second device in the fleet that
nobody meant to create. NervesHub registers an identifier it has not seen
before, which is what makes one image work for a whole fleet, and is also why a
wrong identifier fails silently rather than loudly.

On real hardware the identifier has to survive an update, which overwrites the
whole rootfs slot. Read it from the hardware, or keep it on a data partition.
[deploying.md](deploying.md) covers this.

### Client certificates

Not implemented. The config shape is written down in
[`examples/agent.toml`](../examples/agent.toml), and the agent refuses a config
containing it rather than starting and failing at the first connection.

## TLS

TLS is [rustls](https://github.com/rustls/rustls), not the system OpenSSL. There
is nothing to install on the device, no version to keep in step with the image,
and nothing to find when cross-compiling. It is the main reason the binary links
nothing but libc.

Certificates are verified against the system trust store. For an internal CA,
point `ca_certificate` at the PEM root:

```toml
ca_certificate = "/etc/ssl/certs/internal-ca.pem"
```

There is also `danger_accept_invalid_certs`, which accepts any certificate at
all. It is named to be uncomfortable to type and is logged loudly at startup,
because it turns TLS into obfuscation: anything on the path can present its own
certificate and read the shared secret straight out of the handshake headers.
For a NervesHub running on a laptop, and for nothing else.

RAUC does its own transfer, so `danger_accept_invalid_certs` does not reach it.
A self-signed NervesHub needs the equivalent told to RAUC separately, which is
deliberate — giving up both should take two deliberate acts.

## How the handshake authenticates

The agent sends four headers on the WebSocket upgrade:

```
x-nh-alg         NH1-HMAC-SHA256-<iterations>-<key length>
x-nh-key         the product key
x-nh-time        seconds since the epoch
x-nh-signature   a Plug.Crypto token over {identifier, signed_at_ms, max_age}
```

The signing key is derived from the product secret with PBKDF2, salted with the
other three headers — so changing `x-nh-time` in flight changes the salt, and
the signature stops deriving.

**The signature carries a timestamp, and the server rejects one more than 90
seconds old.** A device whose clock has drifted fails in a way that looks
exactly like a bad secret. Run SNTP before the agent, or expect to lose an
afternoon to it.

## When it will not connect

Run with frame-level logging:

```bash
RUST_LOG=nerves_hub_link_agent=debug ./nerves-hub-link-agent --config agent.toml
```

That logs the URL it dials and every frame in both directions.

| | |
| --- | --- |
| `401 Unauthorized` | Right path, credential rejected. Check the key and secret — then **check the clock**, which is the same failure and much more common. |
| `403 Forbidden` | You are on `/socket`. On the web endpoint that is the *user* socket; the device socket is `/device-socket`. |
| connection refused | Wrong port, or the server is not running. |
| TLS handshake failure | A certificate the system trust store does not verify. Use `ca_certificate` for an internal CA. |
| connects, then closes immediately | Usually a proxy that dropped the `Upgrade` header, or a `heartbeat_interval_secs` above the server's socket timeout. |
| connects, but no device in the UI | It connected under an identifier you did not expect. Check what `identity.identifier` resolved to — the agent logs it at startup. |

## Firmware downloads

The download URL comes from NervesHub, not from the agent's config, which means
it has to be reachable from wherever the device is. A self-hosted deployment
that generates URLs pointing at `localhost` hands the device an address that is
correct for the server and useless for the device.

The agent verifies the SHA-256 of what it downloaded before handing it to the
update tool. For fwup, also set `public_key` — NervesHub checked the archive's
signature at upload, which says nothing about the bytes that arrived here.
