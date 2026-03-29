import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

import requests


HOST = os.environ.get("CARGO_PROXY_HOST", "127.0.0.1")
PORT = int(os.environ.get("CARGO_PROXY_PORT", "38473"))
INDEX_BASE = "https://index.crates.io"
CRATE_BASE = "https://static.crates.io/crates"
SESSION = requests.Session()


def build_upstream_url(path: str) -> str:
    if path == "/config.json":
        return ""
    if path.startswith("/crates/"):
        suffix = path[len("/crates/") :]
        return f"{CRATE_BASE}/{suffix}"
    return f"{INDEX_BASE}{path}"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        self.handle_proxy(head_only=False)

    def do_HEAD(self):
        self.handle_proxy(head_only=True)

    def log_message(self, format, *args):
        return

    def handle_proxy(self, head_only: bool):
        path = urlsplit(self.path).path
        if path == "/config.json":
            payload = json.dumps(
                {
                    "dl": f"http://{HOST}:{PORT}/crates",
                    "api": "https://crates.io",
                }
            ).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            if not head_only:
                self.wfile.write(payload)
            return

        upstream = build_upstream_url(path)
        if not upstream:
            self.send_error(404)
            return

        try:
            response = SESSION.get(
                upstream,
                headers={"User-Agent": "codex-openai-wrapper-cargo-proxy"},
                timeout=60,
                stream=True,
            )
        except Exception as exc:
            payload = f"proxy fetch failed: {exc}\n".encode("utf-8")
            self.send_response(502)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            if not head_only:
                self.wfile.write(payload)
            return

        self.send_response(response.status_code)
        for header in ("Content-Type", "Content-Length", "ETag", "Cache-Control", "Last-Modified"):
            value = response.headers.get(header)
            if value:
                self.send_header(header, value)
        self.end_headers()

        if head_only:
            response.close()
            return

        for chunk in response.iter_content(chunk_size=1024 * 128):
            if chunk:
                self.wfile.write(chunk)
        response.close()


if __name__ == "__main__":
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    print(f"cargo registry proxy listening on http://{HOST}:{PORT}", flush=True)
    server.serve_forever()
