#!/usr/bin/env python3
"""A smart-HTTP server for the credential scenarios: `git http-backend` behind HTTP Basic auth.

usage: authserver.py <port> <project-root> <user> <password> [<request-log>]

With AUTH_ONLY_POST=1 in the environment only POST requests are challenged; GET is public.
Every request without the right `Authorization: Basic` header is answered 401 with a
`WWW-Authenticate: Basic realm="rivet-test"` challenge. Each request is appended to the request log
as `<method> <path> <auth|noauth|badauth>`, so a scenario can check how many round trips each
client needed.
"""
import base64
import http.server
import os
import subprocess
import sys

port, root, user, password = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
log = sys.argv[5] if len(sys.argv) > 5 else None
only_post = os.environ.get("AUTH_ONLY_POST") == "1"
expected = "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, *args):
        pass

    def handle_any(self):
        header = self.headers.get("Authorization")
        state = "noauth" if header is None else ("auth" if header == expected else "badauth")
        if log:
            with open(log, "a") as out:
                out.write(f"{self.command} {self.path.split('?')[0]} {state}\n")
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        if state != "auth" and not (only_post and self.command == "GET"):
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="rivet-test"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        path, _, query = self.path.partition("?")
        env = {
            "PATH": os.environ["PATH"],
            "GIT_PROJECT_ROOT": root,
            "GIT_HTTP_EXPORT_ALL": "1",
            "REQUEST_METHOD": self.command,
            "PATH_INFO": path,
            "QUERY_STRING": query,
            "CONTENT_TYPE": self.headers.get("Content-Type", ""),
            "CONTENT_LENGTH": str(len(body)),
            "REMOTE_USER": user,
            "REMOTE_ADDR": "127.0.0.1",
        }
        if self.headers.get("Git-Protocol"):
            env["GIT_PROTOCOL"] = self.headers["Git-Protocol"]
        if self.headers.get("Content-Encoding"):
            env["HTTP_CONTENT_ENCODING"] = self.headers["Content-Encoding"]
        result = subprocess.run(["git", "http-backend"], input=body, env=env, capture_output=True)
        head, _, payload = result.stdout.partition(b"\r\n\r\n")
        status = "200 OK"
        headers = []
        for line in head.decode("latin-1").split("\r\n"):
            if line.lower().startswith("status:"):
                status = line.split(":", 1)[1].strip()
            elif line:
                headers.append(line)
        self.send_response(int(status.split()[0]))
        for line in headers:
            name, _, value = line.partition(":")
            self.send_header(name, value.strip())
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    do_GET = handle_any
    do_POST = handle_any


http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
