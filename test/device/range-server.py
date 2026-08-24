#!/usr/bin/env python3
"""A static file server that honours Range requests.

RAUC refuses to stream from a server without them — "range requests not
supported by server" — because fetching only the blocks a slot lacks is the
whole point. Python's `http.server` does not implement Range, so this adds it.

NervesHub itself serves firmware through Plug.Static, which does support Range,
so this stands in only for a bootstrap install where NervesHub is not in play.

    ./test/device/range-server.py [port] [directory]
"""

import os
import re
import sys
from http.server import HTTPServer, SimpleHTTPRequestHandler


class RangeHandler(SimpleHTTPRequestHandler):
    def send_head(self):
        header = self.headers.get("Range")

        if not header:
            return super().send_head()

        path = self.translate_path(self.path)

        if not os.path.isfile(path):
            return super().send_head()

        size = os.path.getsize(path)
        match = re.fullmatch(r"bytes=(\d*)-(\d*)", header.strip())

        if not match:
            self.send_error(400, "malformed Range")
            return None

        start, end = match.group(1), match.group(2)

        if start:
            first = int(start)
            last = int(end) if end else size - 1
        else:
            # A suffix range: the last N bytes. RAUC uses this to read the
            # footer before it knows anything else about the bundle.
            first = max(0, size - int(end))
            last = size - 1

        if first >= size or first > last:
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{size}")
            self.end_headers()
            return None

        last = min(last, size - 1)

        handle = open(path, "rb")
        handle.seek(first)

        self.send_response(206)
        self.send_header("Content-Type", self.guess_type(path))
        self.send_header("Content-Range", f"bytes {first}-{last}/{size}")
        self.send_header("Content-Length", str(last - first + 1))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

        # SimpleHTTPRequestHandler copies to EOF, so hand it only the slice.
        return _Slice(handle, last - first + 1)

    def end_headers(self):
        if "Accept-Ranges" not in self._headers_buffer_names():
            self.send_header("Accept-Ranges", "bytes")
        super().end_headers()

    def _headers_buffer_names(self):
        return b"".join(getattr(self, "_headers_buffer", [])).decode("latin-1")

    def log_message(self, *args):
        pass


class _Slice:
    """Reads at most `remaining` bytes, so a ranged response stops on time."""

    def __init__(self, handle, remaining):
        self.handle = handle
        self.remaining = remaining

    def read(self, amount=-1):
        if self.remaining <= 0:
            return b""

        if amount < 0 or amount > self.remaining:
            amount = self.remaining

        chunk = self.handle.read(amount)
        self.remaining -= len(chunk)

        return chunk

    def close(self):
        self.handle.close()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8055
    directory = sys.argv[2] if len(sys.argv) > 2 else "."

    os.chdir(directory)
    HTTPServer(("0.0.0.0", port), RangeHandler).serve_forever()
