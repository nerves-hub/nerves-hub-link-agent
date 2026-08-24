#!/usr/bin/env python3
"""A stand-in for the application on the device.

    python3 examples/controller.py tmp/agent.sock

Connects as the controller, so the agent asks it whether to install an update
and whether it may reboot. Answers are hardcoded below — edit them and
reconnect, no rebuild needed. That is the point of this script: the agent is
the thing under test, and the decisions should be trivial to change.

Stdlib only.
"""

import json
import socket
import sys

SOCKET = sys.argv[1] if len(sys.argv) > 1 else "tmp/agent.sock"

# Edit these.
#
#   {"action": "apply"}
#   {"action": "ignore", "reason": "not now"}
#   {"action": "reschedule", "delay_ms": 600000, "reason": "cycle in progress"}
UPDATE_ANSWER = {"action": "apply"}

#   {"action": "reboot"}
#   {"action": "defer", "delay_ms": 60000, "reason": "operator present"}
REBOOT_ANSWER = {"action": "defer", "delay_ms": 60000, "reason": "operator present"}


def main():
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(SOCKET)
    stream = sock.makefile("rw", encoding="utf-8", newline="\n")

    send(stream, {
        "type": "hello",
        "name": "controller.py",
        "role": "controller",
        "api": 1,
        "subscribe": ["connection", "update_progress", "update_installed",
                      "update_failed", "reboot_pending"],
    })

    welcome = recv(stream)
    if welcome.get("type") != "welcome":
        print(f"refused: {welcome}")
        return
    print(f"connected to agent {welcome['agent_version']}, tool {welcome['update_tool']}")

    send(stream, {"type": "request", "id": "1", "method": "status"})

    while True:
        frame = recv(stream)
        if frame is None:
            print("agent went away")
            return

        kind = frame.get("type")

        if kind == "event":
            print(f"  event {frame['event']}: {frame.get('payload')}")

        elif kind == "response":
            print(f"  reply {frame['id']}: {frame.get('result') or frame.get('error')}")

        elif kind == "request":
            method = frame["method"]
            print(f"? {method} {frame.get('params')}")

            if method == "update_available":
                answer = UPDATE_ANSWER
            elif method == "reboot_request":
                answer = REBOOT_ANSWER
            elif method == "identify":
                print("  *** blink ***")
                answer = {}
            else:
                send(stream, {"type": "response", "id": frame["id"],
                              "error": {"code": "unknown_method", "message": method}})
                continue

            print(f"! answering {answer}")
            send(stream, {"type": "response", "id": frame["id"], "result": answer})


def send(stream, frame):
    stream.write(json.dumps(frame) + "\n")
    stream.flush()


def recv(stream):
    line = stream.readline()
    return json.loads(line) if line else None


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
