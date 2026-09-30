#!/usr/bin/env python3
"""Dev server for dist/web (cwd): python's http.server, plus `X.gz` sent as
`X` with `Content-Encoding: gzip` (and `X-Uncompressed-Length`) when the client accepts it. Only files that
`cargo xtask web` pre-compressed (the wasm) are affected; everything else, symlinked
game.pack included, is served raw.

usage: web_serve.py [PORT]   (binds 127.0.0.1)
"""
import os
import struct
import sys
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer


class Handler(SimpleHTTPRequestHandler):
    def send_head(self):
        path = self.translate_path(self.path)
        gz = path + ".gz"
        accepts = "gzip" in self.headers.get("Accept-Encoding", "")
        if accepts and not os.path.isdir(path) and os.path.isfile(gz):
            try:
                f = open(gz, "rb")
            except OSError:
                return super().send_head()
            size = os.fstat(f.fileno()).st_size
            # gzip ISIZE (last 4 bytes, mod 2^32): the page's progress bar counts decoded bytes.
            f.seek(-4, os.SEEK_END)
            (raw_size,) = struct.unpack("<I", f.read(4))
            f.seek(0)
            self.send_response(200)
            self.send_header("Content-Type", self.guess_type(path))
            self.send_header("Content-Encoding", "gzip")
            self.send_header("Content-Length", str(size))
            self.send_header("X-Uncompressed-Length", str(raw_size))
            self.send_header("Vary", "Accept-Encoding")
            self.end_headers()
            return f
        return super().send_head()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    with ThreadingHTTPServer(("127.0.0.1", port), Handler) as server:
        server.serve_forever()
