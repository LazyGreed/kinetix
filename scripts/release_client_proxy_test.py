#!/usr/bin/env python3
"""Regression test for incremental SSE forwarding and streaming evidence capture."""

import importlib.util
import json
import pathlib
import queue
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

ROOT = pathlib.Path(__file__).resolve().parents[1]
PROXY_PATH = ROOT / "scripts/release-client-proxy.py"

spec = importlib.util.spec_from_file_location("release_client_proxy", PROXY_PATH)
proxy_module = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(proxy_module)

FIRST_SENT = threading.Event()
RELEASE_SECOND = threading.Event()


class DelayedSseHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def _chunk(self, payload):
        self.wfile.write(f"{len(payload):X}\r\n".encode())
        self.wfile.write(payload)
        self.wfile.write(b"\r\n")
        self.wfile.flush()

    def do_POST(self):
        length = int(self.headers.get("content-length", "0") or 0)
        if length:
            self.rfile.read(length)

        self.send_response(200)
        self.send_header("content-type", "text/event-stream; charset=utf-8")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()

        self._chunk(b"data: first\n\n")
        FIRST_SENT.set()

        if not RELEASE_SECOND.wait(timeout=3):
            return

        self._chunk(b"data: second\n\n")
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


def main():
    FIRST_SENT.clear()
    RELEASE_SECOND.clear()

    upstream = ThreadingHTTPServer(("127.0.0.1", 0), DelayedSseHandler)
    upstream.daemon_threads = True
    upstream_thread = threading.Thread(target=upstream.serve_forever, daemon=True)

    with tempfile.TemporaryDirectory() as tmp:
        log_path = pathlib.Path(tmp) / "proxy.jsonl"
        proxy_module.ProxyHandler.upstream = urlsplit(
            f"http://127.0.0.1:{upstream.server_port}"
        )
        proxy_module.ProxyHandler.log_path = log_path

        proxy = ThreadingHTTPServer(("127.0.0.1", 0), proxy_module.ProxyHandler)
        proxy.daemon_threads = True
        proxy_thread = threading.Thread(target=proxy.serve_forever, daemon=True)

        result = queue.Queue()

        def read_stream():
            try:
                request = urllib.request.Request(
                    f"http://127.0.0.1:{proxy.server_port}/v1/chat/completions",
                    data=json.dumps({"model": "test-model", "stream": True}).encode(),
                    headers={"content-type": "application/json"},
                    method="POST",
                )
                with urllib.request.urlopen(request, timeout=3) as response:
                    result.put(response.readline())
                    response.read()
            except Exception as error:
                result.put(error)

        upstream_thread.start()
        proxy_thread.start()
        client_thread = threading.Thread(target=read_stream, daemon=True)
        client_thread.start()

        try:
            if not FIRST_SENT.wait(timeout=1):
                raise SystemExit("upstream never sent first SSE event")

            try:
                first = result.get(timeout=0.5)
            except queue.Empty:
                raise SystemExit(
                    "proxy buffered first SSE event until upstream completion"
                )

            if isinstance(first, Exception):
                raise first
            if first != b"data: first\n":
                raise SystemExit(f"unexpected first SSE line: {first!r}")

            RELEASE_SECOND.set()
            client_thread.join(timeout=2)
            if client_thread.is_alive():
                raise SystemExit("client did not finish after upstream completion")

            deadline = time.monotonic() + 1
            while time.monotonic() < deadline and not log_path.read_text().strip():
                time.sleep(0.01)

            records = [
                json.loads(line)
                for line in log_path.read_text().splitlines()
                if line.strip()
            ]
            if len(records) != 1:
                raise SystemExit(f"expected one proxy evidence row, got {len(records)}")
            record = records[0]
            if record.get("request_stream") is not True:
                raise SystemExit(
                    f"proxy did not record stream:true: {record.get('request_stream')!r}"
                )
            content_type = record.get("response_content_type", "")
            if content_type.split(";", 1)[0].strip().lower() != "text/event-stream":
                raise SystemExit(
                    f"proxy did not record SSE content type: {content_type!r}"
                )
        finally:
            RELEASE_SECOND.set()
            proxy.shutdown()
            upstream.shutdown()
            proxy.server_close()
            upstream.server_close()

    print("release-client-proxy incremental streaming evidence: PASS")


if __name__ == "__main__":
    main()
