#!/usr/bin/env python3
"""The build container's only way out: an HTTPS CONNECT proxy that lets
through port 443 of the hosts in ALLOW_HOSTS (comma separated) and nothing
else. Plain-HTTP proxying is refused. One line per decision on stdout:

  ALLOW host | DENY host:port | UPSTREAM_FAIL host (error class)

score.sh reads these lines: UPSTREAM_FAIL during a failed build means the
registry was unreachable (system_error, retryable), DENY is reported in the
logs so a build that needed another host is visible.
"""

import os
import select
import socket
import socketserver
import sys

ALLOW = {h.strip().lower() for h in os.environ.get("ALLOW_HOSTS", "").split(",") if h.strip()}
PORT = int(os.environ.get("PROXY_PORT", "3128"))


def log(msg):
    sys.stdout.write(msg + "\n")
    sys.stdout.flush()


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        c = self.request
        c.settimeout(30)
        head = b""
        while b"\r\n\r\n" not in head and len(head) < 16384:
            chunk = c.recv(4096)
            if not chunk:
                return
            head += chunk
        line = head.split(b"\r\n", 1)[0].decode("latin-1", "replace").split()
        if len(line) < 2 or line[0].upper() != "CONNECT":
            target = line[1] if len(line) > 1 else "?"
            log(f"DENY {target[:200]} (not CONNECT)")
            c.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            return
        host, _, port = line[1].rpartition(":")
        host = host.strip("[]").lower()
        if host not in ALLOW or port != "443":
            log(f"DENY {host[:200]}:{port[:10]}")
            c.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            return
        try:
            u = socket.create_connection((host, 443), timeout=30)
        except OSError as e:
            log(f"UPSTREAM_FAIL {host} ({type(e).__name__})")
            c.sendall(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            return
        log(f"ALLOW {host}")
        c.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        c.settimeout(None)
        u.settimeout(None)
        socks = [c, u]
        try:
            while True:
                r, _, x = select.select(socks, [], socks, 300)
                if x or not r:
                    break
                for s in r:
                    data = s.recv(65536)
                    if not data:
                        return
                    (u if s is c else c).sendall(data)
        except OSError:
            pass
        finally:
            u.close()


class Server(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    log(f"egress proxy on :{PORT}, allow {sorted(ALLOW)}")
    Server(("0.0.0.0", PORT), Handler).serve_forever()
