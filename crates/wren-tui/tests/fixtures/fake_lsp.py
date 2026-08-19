import json
import sys
import time

log_path = sys.argv[1]
while True:
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1].strip())
    message = json.loads(sys.stdin.buffer.read(length))
    method = message.get("method", "")
    with open(log_path, "a", encoding="utf-8") as output:
        detail = " " + message.get("params", {}).get("rootUri", "") if method == "initialize" else ""
        output.write(method + detail + "\n")
    if "id" not in message:
        continue
    if method == "initialize":
        result = {"capabilities": {
            "semanticTokensProvider": {"legend": {
                "tokenTypes": ["variable"], "tokenModifiers": []
            }, "full": True},
            "declarationProvider": True,
            "definitionProvider": True,
            "implementationProvider": True,
            "referencesProvider": True
        }}
    elif method == "textDocument/definition":
        time.sleep(0.2)
        result = None
    elif method == "textDocument/hover":
        time.sleep(0.2)
        result = {"contents": "delayed hover details"}
    elif method == "textDocument/semanticTokens/full":
        result = {"data": []}
    else:
        result = None
    response = json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(response)}\r\n\r\n".encode() + response)
    sys.stdout.buffer.flush()
