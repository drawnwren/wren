#!/usr/bin/env python3
import json
import os
import sys

log_path = os.environ["WREN_TEST_LSP_LOG"]
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise SystemExit(0)
        if line == b"\r\n":
            break
        name, value = line.decode().split(":", 1)
        headers[name.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
    method = message.get("method", "")
    with open(log_path, "a", encoding="utf-8") as output:
        root = " " + message.get("params", {}).get("rootUri", "") if method == "initialize" else ""
        output.write(method + root + "\n")
        output.flush()
    if "id" not in message:
        continue
    result = {"capabilities": {"hoverProvider": True}} if method == "initialize" else None
    response = json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(response)}\r\n\r\n".encode() + response)
    sys.stdout.buffer.flush()
