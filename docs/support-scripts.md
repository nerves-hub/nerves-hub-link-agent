# Support scripts

A support script is text you write in NervesHub and run on a device. On a Nerves
device it is Elixir, evaluated in the running VM. There is no VM here, so it is a
shell script — which is what a support script on a Linux device wants to be
anyway: `journalctl`, `systemctl status`, `ip addr`, `df`.

Unlike the extensions, support scripts are **enabled by default**. They are how
anyone diagnoses a device they cannot reach, and NervesHub already decides who
may run one.

## Configuration

```toml
[scripts]
enabled = true
work_dir = "/var/lib/nerves-hub-link-agent/scripts"
interpreter = "bash"
timeout_secs = 10
max_output_bytes = 65536
```

## How a script runs

The text arrives over the device channel, is written to a file in `work_dir`,
and is run with `interpreter`.

Running from a file rather than `bash -c` means an error carries a line number.

A script starting with `#!` is made executable and run directly, so Python or
`sh` works without changing `interpreter`:

```bash
#!/usr/bin/env python3
import subprocess
print(subprocess.run(["ip", "addr"], capture_output=True, text=True).stdout)
```

`stdout` and `stderr` are merged in the order they actually happened, so
interleaved output reads the way it would on a terminal.

Scripts get three environment variables:

```
NERVES_HUB_DEVICE_IDENTIFIER
NERVES_HUB_FIRMWARE_UUID
NERVES_HUB_FIRMWARE_VERSION
```

Output past `max_output_bytes` is truncated from the **end**, keeping the
beginning and appending a marker. A script's first lines say what it was doing;
a truncated tail is usually the part you can live without.

## Timeouts

**NervesHub drops a script's reference after 15 seconds and stops listening.**
The agent's `timeout_secs` defaults to 10 to stay under that. A script that
overruns produces output nobody receives.

On timeout the agent kills the whole **process group**, not just the shell.
Killing only the shell would leave anything the script backgrounded still
running with nobody watching it.

If you need something long-running, have the script start it and return
immediately, then collect the result on a later run.

## Scripts are per-product, and a product can hold both kinds of device

This is the trap worth knowing about. The same script text goes to a Nerves
device and to one running this agent, so `VintageNet.info()` reaches a bash
shell and comes back as a syntax error.

Scripts carry tags and NervesHub filters on them. That is the seam to use when a
product ends up mixed — tag the shell scripts for the agent devices and the
Elixir ones for the Nerves devices.

## Turning them off

```toml
[scripts]
enabled = false
```

The agent still *answers*, saying scripts are disabled. A device that silently
ignored a script would be indistinguishable from one that had gone offline, and
would leave an operator watching a spinner.

## Security

A support script is arbitrary shell, running as whatever the agent runs as,
chosen by whoever NervesHub decided may run scripts on this device. That is the
same power as [`local_shell`](extensions.md#local_shell), differing only in that
someone wrote the commands down first.

This is a reason to run the agent as a service user rather than as root — see
[deploying.md](deploying.md).
