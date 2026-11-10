#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
exec python3 - "$repo" "$@" <<'PY'
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import threading
import time

parser = argparse.ArgumentParser(description="Stress t3's pid-only Codex teardown without model calls")
parser.add_argument("--binary", type=Path, default=Path(sys.argv[1]) / "target/debug/cx")
parser.add_argument("--cycles", type=int, default=200)
parser.add_argument("--concurrency", type=int, default=8)
args = parser.parse_args(sys.argv[2:])
binary = args.binary.resolve()
if not binary.is_file():
    parser.error("build cx first with cargo build, or pass --binary")
if args.cycles < 6 or not 4 <= args.concurrency <= 8:
    parser.error("use at least 6 cycles and concurrency 4..8")

with tempfile.TemporaryDirectory(prefix="cx-t3-stress-") as temporary:
    root = Path(temporary)
    bin_dir = root / "bin"
    bin_dir.mkdir()
    alias = bin_dir / "cx-t3-codex"
    alias.symlink_to(binary)
    fake = bin_dir / "codex"
    fake.write_text("#!" + sys.executable + "\n" + r'''
import json, os, pathlib, signal, sys
home = os.environ["CODEX_HOME"]
pathlib.Path(os.environ["STRESS_RECORDS"], str(os.getpid()) + ".json").write_text(
    json.dumps({"pid": os.getpid(), "home": home, "args": sys.argv[1:]}))
if sys.argv[1:] == ["--version"]:
    print("codex-cli stress-fake")
    sys.exit(0)
if sys.argv[1:] != ["app-server"]:
    sys.exit(2)
if os.environ.get("FAKE_IGNORE_TERM") == "1":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({"id": request["id"], "result": {"userAgent": "stress-fake"}}), flush=True)
''')
    fake.chmod(0o700)
    records = root / "records"
    records.mkdir()
    codex_home = root / "codex-home"
    codex_home.mkdir()
    (codex_home / "config.toml").write_text('model = "stress-fake"\n')
    (codex_home / "sessions").mkdir()
    shared_auth = codex_home / "auth.json"
    shared_auth.write_text('{"sentinel":"shared-login-must-stay-untouched"}\n')
    shared_before = shared_auth.read_bytes()
    shared_mtime = shared_auth.stat().st_mtime_ns
    data = root / "data/cx"
    data.mkdir(parents=True, mode=0o700)
    # Synthetic access-only credentials keep the harness independent of real logins.
    (data / "state.json").write_text(json.dumps({"accounts": {"stress": {
        "email": "stress@example.test", "account_id": "stress", "usage": None,
        "data": {"auth_mode": "chatgpt", "tokens": {
            "access_token": "synthetic-access", "account_id": "stress"}}
    }}}))
    base_env = dict(os.environ, HOME=str(root), XDG_CONFIG_HOME=str(root / "config"),
                    CODEX_HOME=str(codex_home), PATH=str(bin_dir) + os.pathsep + os.environ["PATH"],
                    CX_USAGE_URL="http://127.0.0.1:9", CODEX_REFRESH_TOKEN_URL_OVERRIDE="http://127.0.0.1:9")
    for name in ["CX_BORROW", "CX_BORROW_JUMP", "CX_SSH", "CX_CODEX_BIN", "OPENAI_API_KEY", "CODEX_API_KEY"]:
        base_env.pop(name, None)

    # Fake lend: answers `cx lend --inventory` and `cx lend --alias X` with synthetic
    # well-formed JWTs, and logs each call so the harness can count SSH use.
    fake_ssh = bin_dir / "fake-ssh"
    fake_ssh.write_text("#!" + sys.executable + "\n" + r'''
import base64, json, os, sys
command = sys.argv[-1]
with open(os.environ["STRESS_SSH_LOG"], "a") as log:
    log.write(command.split("exec cx ", 1)[-1] + "\n")
def jwt(alias):
    claims = {"email": alias + "@example.test", "exp": 4102444800,
              "https://api.openai.com/auth": {"chatgpt_account_id": alias}}
    body = base64.urlsafe_b64encode(json.dumps(claims).encode()).rstrip(b"=").decode()
    return "e30." + body + ".sig"
aliases = ["owner-a", "owner-b"]
if "--inventory" in command:
    print(json.dumps({"accounts": [{"alias": a, "email": a + "@example.test", "account_id": a,
                                    "expires_at": 4102444800, "usage": None} for a in aliases]}))
else:
    alias = command.split("--alias ", 1)[1].strip("' ")
    print(json.dumps({"alias": alias, "email": alias + "@example.test", "account_id": alias,
                      "lease_id": "", "expires_at": 4102444800,
                      "access_token": "synthetic-access-" + alias, "id_token": jwt(alias)}))
''')
    fake_ssh.chmod(0o700)

    def run_phase(name, data, env, records, borrow, pin=None):
        started = set()
        lock = threading.Lock()

        def owned_processes():
            recorded = {int(p.stem) for p in records.glob("*.json")}
            with lock:
                owned = started | recorded
            rows = subprocess.check_output(["ps", "-axo", "pid=,ppid=,stat=,args="], text=True)
            found = []
            for row in rows.splitlines():
                fields = row.strip().split(None, 3)
                if len(fields) != 4:
                    continue
                pid, parent, status, command = fields
                if int(pid) in owned or str(bin_dir) in command:
                    found.append((int(pid), status, command))
            return found

        def counts(rows):
            return {
                "cx-t3-codex": sum(str(alias) in command for _, _, command in rows),
                "cx t3-codex": sum(str(binary) + " t3-codex" in command for _, _, command in rows),
                "fake-codex": sum(str(fake) in command for _, _, command in rows),
                "fake-ssh": sum(str(fake_ssh) in command for _, _, command in rows),
                "zombies": sum("Z" in status for _, status, _ in rows),
                "total": len(rows),
            }

        def initialize(child):
            child.stdin.write(b'{"id":1,"method":"initialize","params":{}}\n')
            child.stdin.flush()
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                if not selector.select(timeout=5 if not borrow else 20):
                    raise AssertionError("fake Codex did not initialize within 5s")
            reply = json.loads(child.stdout.readline())
            if reply.get("id") != 1 or "result" not in reply:
                raise AssertionError("invalid initialize reply")
            record = json.loads((records / f"{child.pid}.json").read_text())
            if record["pid"] != child.pid:
                raise AssertionError("tracked pid is not fake Codex")

        def cycle(index):
            mode = index % 6
            command = [str(alias)] if index % 2 else [str(binary), "t3-codex"]
            command += ["--version"] if mode == 0 else ["app-server"]
            child_env = dict(env, FAKE_IGNORE_TERM="1" if mode == 2 and index % 12 == 2 else "0")
            child = subprocess.Popen(command, env=child_env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            with lock:
                started.add(child.pid)
            try:
                if mode == 0:
                    output, error = child.communicate(timeout=5)
                    if child.returncode != 0 or output != b"codex-cli stress-fake\n":
                        raise AssertionError("--version failed")
                elif mode == 3:
                    child.kill()  # Kill at any point, including overlay preparation.
                    child.communicate(timeout=5)
                    if child.returncode != -signal.SIGKILL:
                        raise AssertionError("immediate SIGKILL did not kill the tracked pid")
                elif mode == 4:
                    child.communicate(timeout=5)  # stdin EOF with no signal or initialize.
                    if child.returncode != 0:
                        raise AssertionError("EOF exit failed")
                else:
                    initialize(child)
                    if mode == 1:
                        child.stdin.close()
                        child.stdin = None
                        child.communicate(timeout=5)
                        if child.returncode != 0:
                            raise AssertionError("normal initialized EOF exit failed")
                    elif mode == 2:
                        child.terminate()
                        # t3 waits two seconds before force-killing only the tracked pid.
                        try:
                            child.wait(timeout=2)
                        except subprocess.TimeoutExpired:
                            child.kill()
                        child.communicate(timeout=5)
                        if child.returncode not in (-signal.SIGTERM, -signal.SIGKILL):
                            raise AssertionError("pid-only probe abort failed")
                    else:
                        child.kill()  # Explicitly kill after exec and initialization.
                        child.communicate(timeout=5)
                        if child.returncode != -signal.SIGKILL:
                            raise AssertionError("SIGKILL left the fake Codex pid alive")
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait(timeout=5)
                for stream in [child.stdin, child.stdout, child.stderr]:
                    if stream is not None:
                        stream.close()

        def cold_wave():
            # Eight probes against an empty borrow cache must trigger one refresh in total.
            children = [subprocess.Popen([str(alias), "app-server"], env=env, stdin=subprocess.PIPE,
                                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                        for _ in range(8)]
            with lock:
                started.update(c.pid for c in children)
            for child in children:
                try:
                    initialize(child)
                    child.stdin.close()
                    child.stdin = None
                    child.communicate(timeout=10)
                    if child.returncode != 0:
                        raise AssertionError("cold-cache probe failed")
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.wait(timeout=5)

        before = counts(owned_processes())
        print(f"[{name}] before:", json.dumps(before, sort_keys=True), flush=True)
        ssh_log = Path(env["STRESS_SSH_LOG"]) if borrow else None
        try:
            if borrow and pin:
                warm_calls = len(ssh_log.read_text().splitlines()) if ssh_log.exists() else 0
            elif borrow:
                cold_wave()
                calls = ssh_log.read_text().splitlines()
                if sum("--inventory" in c for c in calls) != 1 or len(calls) != 3:
                    raise AssertionError(f"cold-cache refresh was not single-flight: {len(calls)} calls")
                warm_calls = len(calls)
                for record in records.glob("*.json"):
                    record.unlink()
            with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
                list(pool.map(cycle, range(args.cycles)))
            time.sleep(1.5)
            rows = owned_processes()
            after = counts(rows)
            print(f"[{name}] after: ", json.dumps(after, sort_keys=True), flush=True)
            if after != before or after["total"]:
                raise AssertionError("leftover shim/child/zombie processes vs baseline")
            homes = {json.loads(p.read_text())["home"] for p in records.glob("*.json")}
            if shared_auth.read_bytes() != shared_before or shared_auth.stat().st_mtime_ns != shared_mtime:
                raise AssertionError("shared login changed")
            if not borrow:
                overlays = list((data / "t3-overlays").iterdir())
                if homes != {str(data / "t3-overlays/stress")} or len(overlays) != 1:
                    raise AssertionError("overlay directory was not stable across spawns")
                overlay = overlays[0]
                if overlay.stat().st_mode & 0o777 != 0o700 or (overlay / "auth.json").stat().st_mode & 0o777 != 0o600:
                    raise AssertionError("overlay credentials are not private")
                print(f"PASS [{name}]: {args.cycles} cycles, concurrency {args.concurrency}, one reused overlay, pid-only teardown left 0 processes")
            else:
                if len(ssh_log.read_text().splitlines()) != warm_calls:
                    raise AssertionError("SSH was used on the warm spawn path")
                expected = {str(data / f"borrow-overlays/owner/{pin}")} if pin else {str(data / f"borrow-overlays/owner/owner-{x}") for x in "ab"}
                if homes != expected:
                    raise AssertionError(
                        f"{'pinned' if pin else 'borrowed'} spawns used {homes}, expected {expected}"
                    )
                if (data / "t3-overlays").exists():
                    raise AssertionError("borrow phase fell back to the local route")
                for home in homes:
                    home = Path(home)
                    auth_file = home / "auth.json"
                    if home.stat().st_mode & 0o777 != 0o700 or auth_file.stat().st_mode & 0o777 != 0o600:
                        raise AssertionError("borrowed overlay credentials are not private")
                    tokens = json.loads(auth_file.read_text())
                    if (tokens["tokens"]["refresh_token"] != "" or not tokens["last_refresh"]
                            or tokens["tokens"]["id_token"].count(".") != 2
                            or not tokens["tokens"]["access_token"] or not tokens["tokens"]["account_id"]):
                        raise AssertionError("borrowed auth.json has the wrong shape")
                cache = data / "borrow/owner.json"
                if cache.stat().st_mode & 0o777 != 0o600 or "refresh_token" in cache.read_text():
                    raise AssertionError("borrow cache is not private or holds a refresh token")
                detail = f"pinned to {pin}" if pin else "rotated across 2 overlays"
                prefix = f"{args.cycles} cycles with 0 ssh calls" if pin else f"cold single-flight refresh ({warm_calls} ssh calls), then {args.cycles} cycles with 0 ssh calls"
                print(f"PASS [{name}]: {prefix}, {detail}, pid-only teardown left 0 processes")
        finally:
            # Only processes created by this harness are eligible for cleanup.
            for pid, status, command in owned_processes():
                if "Z" not in status and str(bin_dir) in command:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    local_records = root / "records"
    local_env = dict(base_env, XDG_DATA_HOME=str(root / "data"), STRESS_RECORDS=str(local_records))
    run_phase("local", data, local_env, local_records, False)

    borrow_records = root / "records-borrow"
    borrow_records.mkdir()
    borrow_data = root / "data-borrow/cx"
    borrow_data.mkdir(parents=True, mode=0o700)
    (borrow_data / "state.json").write_text((data / "state.json").read_text())
    borrow_env = dict(base_env, XDG_DATA_HOME=str(root / "data-borrow"), STRESS_RECORDS=str(borrow_records),
                      CX_BORROW="owner", CX_SSH=str(fake_ssh), STRESS_SSH_LOG=str(root / "ssh.log"))
    run_phase("borrow", borrow_data, borrow_env, borrow_records, True)
    pin_records = root / "records-pin"
    pin_records.mkdir()
    pin_env = dict(borrow_env, CX_PIN="owner-a", STRESS_RECORDS=str(pin_records))
    run_phase("pin", borrow_data, pin_env, pin_records, True, pin="owner-a")
PY
