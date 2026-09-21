#!/usr/bin/env python3
"""Boundary fixture: fake Codex children, real stdio + Unix WebSocket transport."""
import base64
import hashlib
import json
import os
from pathlib import Path
import select
import queue
import threading
import signal
import socket
import struct
import subprocess
import sys
import time

root = Path(os.environ["CX_TEST_DIR"])
scenario = os.environ.get("CX_TEST_SCENARIO", "idle")
relay_thread = "11111111-1111-4111-8111-111111111111"
relay_attempt_receipt = "33333333-3333-4333-8333-333333333333"
relay_binding = "a" * 64

def log(event):
    with (root / "events").open("a") as f:
        f.write(json.dumps(event) + "\n")

def relay(request):
    relay_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    relay_socket.connect(os.environ["CX_LAM_RELAY"])
    raw = json.dumps(request).encode()
    relay_socket.sendall(struct.pack("!I", len(raw)) + raw)
    length_bytes = b""
    while len(length_bytes) < 4:
        length_bytes += relay_socket.recv(4 - len(length_bytes))
    length = struct.unpack("!I", length_bytes)[0]
    response = b""
    while len(response) < length:
        response += relay_socket.recv(length - len(response))
    relay_socket.close()
    return json.loads(response)

if "relay-helper" in sys.argv:
    (root / "relay_path").write_text(os.environ["CX_LAM_RELAY"])
    log({"relay_bind": relay({
        "version": 1,
        "operation": "bind",
        "thread_id": relay_thread,
        "binding": relay_binding,
    })})
    if scenario == "lam-conflict":
        log({"relay_conflict": relay({
            "version": 1,
            "operation": "bind",
            "thread_id": relay_thread,
            "binding": "b" * 64,
        })})
    sys.exit(0)

if "app-server" in sys.argv:
    log({"server_pid": os.getpid(), "server_has_relay": "CX_LAM_RELAY" in os.environ})
    if scenario == "actual":
        os.execvp("codex", ["codex"] + sys.argv[1:])
    pending = None
    began = False
    completed = False
    held_login = None
    def send(value):
        print(json.dumps(value), flush=True)
    incoming = queue.Queue()
    def reader():
        for line in sys.stdin:
            incoming.put(line)
        incoming.put("")
    threading.Thread(target=reader, daemon=True).start()
    while True:
        try:
            line = incoming.get(timeout=0.02)
        except queue.Empty:
            line = None
        if line is not None:
            if not line:
                break
            msg = json.loads(line)
            method = msg.get("method")
            if method == "initialize":
                assert msg["params"]["capabilities"]["experimentalApi"] is True
                send({"id": msg["id"], "result": {"userAgent": "codex/0.154.0", "codexHome": os.environ["CODEX_HOME"], "platformFamily": "unix", "platformOs": "linux"}})
            elif method == "account/login/start":
                account = msg["params"]["chatgptAccountId"]
                log({"login": account, "server_pid": os.getpid()})
                if scenario == "hold-login" and account == "personal":
                    held_login = msg
                    (root / "auth_pending").touch()
                elif scenario == "reject" and account == "work":
                    send({"id": msg["id"], "error": {"code": -32000, "message": "secret-do-not-print"}})
                else:
                    send({"id": msg["id"], "result": {"type": "chatgptAuthTokens"}})
                    send({"method": "account/updated", "params": {"authMode": "chatgpt", "planType": "plus"}})
            elif method == "thread/list":
                send({"id": msg["id"], "result": {"data": [], "nextCursor": None}})
            elif method == "config/read":
                send({"id": msg["id"], "result": {"config": {"model": "gpt-5.6-sol", "model_reasoning_effort": "low"}, "origins": {}}})
            elif method == "thread/start":
                model = msg["params"].get("model", "gpt-5.6-sol")
                effort = msg["params"].get("config", {}).get("model_reasoning_effort", "low")
                send({"id": msg["id"], "result": {"thread": {"id": "thread-a", "preview": "", "ephemeral": False, "modelProvider": "openai", "createdAt": 0, "updatedAt": 0, "status": {"type": "idle"}, "path": "/tmp/thread.jsonl", "cwd": "/tmp", "cliVersion": "0.155.1", "source": "cli", "agentPath": "root", "name": None, "turns": []}, "model": model, "modelProvider": "openai", "serviceTier": None, "cwd": "/tmp", "approvalPolicy": "never", "approvalsReviewer": "user", "sandbox": {"type": "dangerFullAccess"}, "reasoningEffort": effort}})
            elif method == "thread/read":
                thread_id = msg["params"]["threadId"]
                if scenario == "lam-wrong-thread" and str(msg["id"]).startswith("cx.lam."):
                    thread_id = "44444444-4444-4444-8444-444444444444"
                send({"id": msg["id"], "result": {"thread": {"id": thread_id, "status": {"type": "idle"}, "canAcceptDirectInput": True}}})
            elif method == "thread/queue/add":
                params = msg["params"]
                log({"relay_queue": params})
                if scenario == "lam-queue-drop":
                    pass
                elif scenario == "lam-queue-error":
                    send({"id": msg["id"], "error": {"code": -32000, "message": "provider-private-text"}})
                else:
                    queued = {"id": relay_attempt_receipt, "clientUserMessageId": params["clientUserMessageId"], "input": params["input"]}
                    if scenario == "lam-missing-receipt":
                        queued.pop("id")
                    if scenario == "lam-wrong-attempt":
                        queued["clientUserMessageId"] = "55555555-5555-4555-8555-555555555555"
                    send({"id": msg["id"], "result": {"queuedSubmission": queued}})
                    send({"method": "turn/started", "params": {"threadId": params["threadId"], "turn": {"id": "relay-turn", "items": [], "status": "inProgress", "error": None}}})
                    send({"method": "turn/completed", "params": {"threadId": params["threadId"], "turn": {"id": "relay-turn", "items": [], "status": "completed", "error": None}}})
            elif method == "config/batchWrite":
                keys = [edit["keyPath"] for edit in msg["params"]["edits"]]
                log({"config_write": keys})
                (root / "home/config.toml").write_text("\n".join(keys) + "\n")
                send({"id": msg["id"], "result": {"status": "ok", "version": "saved", "filePath": str(root / "home/config.toml"), "overriddenMetadata": None}})
            elif method == "thread/settings/update":
                log({"thread_model": msg["params"].get("model")})
                send({"id": msg["id"], "result": {}})
            elif method == "config/value/write":
                log({"config_write": [msg["params"]["keyPath"]]})
                (root / "home/config.toml").write_text(msg["params"]["keyPath"] + "\n")
                send({"id": msg["id"], "result": {"status": "ok", "version": "saved", "filePath": str(root / "home/config.toml"), "overriddenMetadata": None}})
            elif method in ["turn/start", "review/start", "thread/compact/start", "thread/queue/start"]:
                pending = msg
                (root / "busy").touch()
            elif method == "initialized":
                if scenario.startswith("lam-"):
                    subprocess.Popen([sys.executable, __file__, "relay-helper"])
        if held_login and (root / "auth_release").exists():
            send({"id": held_login["id"], "result": {"type": "chatgptAuthTokens"}})
            held_login = None
        if pending and (root / "release").exists() and not began:
            began = True
            send({"method": "turn/started", "params": {"threadId": "thread-a", "turn": {"id": "turn-1", "items": [], "status": "inProgress", "error": None}}})
            send({"id": pending["id"], "result": {"turn": {"id": "turn-1", "status": "inProgress"}, "reviewThreadId": "thread-a"}})
        if began and (root / "complete").exists() and not completed:
            completed = True
            send({"method": "turn/completed", "params": {"threadId": "thread-a", "turn": {"id": "turn-1", "items": [], "status": "completed", "error": None}}})
    sys.exit(0)

log({"tui_pid": os.getpid(), "args": sys.argv[1:], "tui_has_relay": "CX_LAM_RELAY" in os.environ})
signal.signal(signal.SIGINT, lambda *_: (root / "interrupted").touch())
if scenario == "startup":
    time.sleep(60)
    sys.exit(0)

endpoint = sys.argv[sys.argv.index("--remote") + 1]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(endpoint.removeprefix("unix://"))
key = base64.b64encode(os.urandom(16)).decode()
s.sendall((f"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
header = b""
while not header.endswith(b"\r\n\r\n"):
    header += s.recv(1)
assert b"101 Switching Protocols" in header
expected = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
assert expected.lower() in header.lower()

def send(value):
    raw = json.dumps(value).encode()
    mask = os.urandom(4)
    length = len(raw)
    head = bytes([0x81, 0x80 | length]) if length < 126 else bytes([0x81, 0x80 | 126]) + struct.pack("!H", length)
    s.sendall(head + mask + bytes(c ^ mask[i % 4] for i, c in enumerate(raw)))

def exact(n):
    data = b""
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            sys.exit(0)
        data += chunk
    return data

def receive():
    opcode, length = exact(2)
    if opcode & 15 == 8:
        sys.exit(0)
    length &= 127
    if length == 126:
        length = struct.unpack("!H", exact(2))[0]
    elif length == 127:
        length = struct.unpack("!Q", exact(8))[0]
    return json.loads(exact(length))

send({"id": "cx.auth.1", "method": "initialize", "params": {"clientInfo": {"name": "codex_cli_rs", "version": "0.154.0"}, "capabilities": {"experimentalApi": False}}})
assert receive()["id"] == "cx.auth.1"
send({"method": "initialized"})
send({"id": 7, "method": "thread/list", "params": {}})
while True:
    msg = receive()
    if msg.get("id") == 7:
        assert msg["result"]["data"] == []
        break
(root / "ready").touch()
if scenario == "model-isolation":
    send({"id": 29, "method": "thread/settings/update", "params": {"threadId": "thread-a", "model": "gpt-5.6-luna", "effort": "high"}})
    while receive().get("id") != 29:
        pass
    send({"id": 30, "method": "config/batchWrite", "params": {"edits": [
        {"keyPath": "model", "value": "gpt-5.6-luna", "mergeStrategy": "replace"},
        {"keyPath": "model_reasoning_effort", "value": "high", "mergeStrategy": "replace"}
    ], "filePath": None, "expectedVersion": None, "reloadUserConfig": True}})
    while True:
        msg = receive()
        if msg.get("id") == 30:
            log({"model_write_response": msg.get("result")})
            break
if scenario == "model-single-write":
    send({"id": 31, "method": "config/value/write", "params": {"keyPath": "model_reasoning_effort", "value": "high"}})
    while True:
        msg = receive()
        if msg.get("id") == 31:
            log({"model_write_response": msg.get("result")})
            break
if scenario == "mixed-config-write":
    send({"id": 32, "method": "config/batchWrite", "params": {"edits": [
        {"keyPath": "model", "value": "gpt-5.6-luna", "mergeStrategy": "replace"},
        {"keyPath": "tui.notifications", "value": True, "mergeStrategy": "replace"}
    ], "filePath": None, "expectedVersion": None, "reloadUserConfig": True}})
    while True:
        msg = receive()
        if msg.get("id") == 32:
            log({"mixed_write_response": msg.get("result")})
            break
if scenario == "clear-model-isolation":
    send({"id": 40, "method": "thread/start", "params": {"model": "gpt-5.6-sol", "config": {"model_reasoning_effort": "low"}}})
    while receive().get("id") != 40:
        pass
    send({"id": 41, "method": "thread/settings/update", "params": {"threadId": "thread-a", "model": "gpt-5.6-luna", "effort": "high"}})
    while receive().get("id") != 41:
        pass
    send({"id": 42, "method": "config/read", "params": {"includeLayers": False, "cwd": "/tmp"}})
    while True:
        msg = receive()
        if msg.get("id") == 42:
            config = msg["result"]["config"]
            log({"clear_defaults": {"model": config.get("model"), "effort": config.get("model_reasoning_effort")}})
            break
if scenario in ["turn/start", "review/start", "thread/compact/start", "thread/queue/start"]:
    send({"id": 8, "method": scenario, "params": {"threadId": "thread-a"}})
while not (root / "quit").exists():
    if scenario == "actual":
        send({"id": "read-account", "method": "account/read", "params": {"refreshToken": False}})
    ready, _, _ = select.select([s], [], [], 0.02)
    if ready:
        msg = receive()
        if msg.get("id") == "read-account":
            log({"actual_email": msg["result"]["account"]["email"]})
            time.sleep(0.05)
        elif str(msg.get("id", "")).startswith("cx.lam."):
            log({"relay_response_leaked_to_tui": msg["id"]})
s.sendall(bytes([0x88, 0x80]) + os.urandom(4))
s.close()
