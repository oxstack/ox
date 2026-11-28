"""Verify the stock OpenCode server contract used by a future OX hosted adapter.

Run with a stock binary and a built OX gateway.
python3 desktop/tests/opencode_host_contract.py /path/to/opencode /path/to/ox
"""

import argparse
import base64
import json
import os
from pathlib import Path
import secrets
import select
import socket
import subprocess
import tempfile
import threading
import time
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode, urlsplit
from urllib.request import Request, urlopen


MODEL = "fixture/local"
SHELL_CASES = {"deny": "deny.txt", "allow": "allow.txt", "cancel approval": "cancel.txt"}


def report_version(binary):
    with tempfile.TemporaryDirectory(prefix="ox-opencode-version-") as temp:
        state = Path(temp)
        env = {
            "PATH": os.environ["PATH"], "HOME": temp,
            **{f"XDG_{name}_HOME": str(state / name.lower())
               for name in ("CONFIG", "DATA", "STATE", "CACHE")},
        }
        shown = subprocess.check_output([binary, "--version"], text=True, cwd=state, env=env)
        print("Testing stock OpenCode", shown.strip(), flush=True)


def wait(check, description):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if result := check():
            return result
        time.sleep(0.05)
    raise AssertionError(f"Timed out waiting for {description}")


def event(kind, **fields):
    return f"event: {kind}\ndata: {json.dumps(dict(type=kind, **fields))}\n\n".encode()


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        if self.path != "/v1/messages" or self.headers.get("x-api-key") != self.server.key:
            self.send_error(401)
            return
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if payload["model"] != "local":
            self.send_error(400, "OX did not map the model alias to its upstream ID")
            return
        last_user = next(row["content"] for row in reversed(payload["messages"]) if row["role"] == "user")
        case = " ".join(part.get("text", "") for part in last_user if part["type"] == "text")
        self.respond(case)

    def respond(self, case):
        filename = SHELL_CASES.get(case)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        try:
            self.stream(case, filename)
        except (BrokenPipeError, ConnectionResetError):
            # Cancellation closes the upstream stream.
            pass

    def write_event(self, kind, **fields):
        self.wfile.write(event(kind, **fields))
        self.wfile.flush()

    def stream(self, case, filename):
        response_id = secrets.token_hex(8)
        self.write_event("message_start", message={
            "id": f"msg_fixture_{response_id}", "type": "message", "role": "assistant", "model": "local",
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 12, "output_tokens": 0},
        })
        if filename:
            self.write_event("content_block_start", index=0, content_block={
                "type": "tool_use", "id": f"toolu_fixture_{response_id}", "name": "bash", "input": {},
            })
            self.write_event("content_block_delta", index=0, delta={
                "type": "input_json_delta", "partial_json": json.dumps({
                    "command": f"printf OK >> {filename}", "description": "Record the test approval",
                    "timeout": 2000,
                }),
            })
        else:
            self.write_event("content_block_start", index=0, content_block={"type": "text", "text": ""})
            text = {"reconnect": "first", "cancel stream": "partial", "next": "fresh"}.get(case, "done")
            self.write_event("content_block_delta", index=0, delta={"type": "text_delta", "text": text})
            if case in ("reconnect", "cancel stream"):
                if not self.server.gates[case].wait(15):
                    return
                self.write_event("content_block_delta", index=0, delta={
                    "type": "text_delta", "text": " restored" if case == "reconnect" else " unwanted tail",
                })
        self.write_event("content_block_stop", index=0)
        self.write_event("message_delta", delta={
            "stop_reason": "tool_use" if filename else "end_turn", "stop_sequence": None,
        }, usage={"output_tokens": 5})
        self.write_event("message_stop")


class Api:
    def __init__(self, base, password, project):
        self.base = base
        self.headers = {"Authorization": "Basic " + base64.b64encode(f"opencode:{password}".encode()).decode()}
        self.project = project

    def open(self, path, body=None, *, project=None, authenticate=True, headers=None):
        directory = self.project if project is None else project
        separator = "&" if "?" in path else "?"
        url = self.base + path + separator + urlencode({"directory": str(directory)})
        req = Request(url, data=None if body is None else json.dumps(body).encode(),
                      headers={"Content-Type": "application/json",
                               **(self.headers if authenticate else {}), **(headers or {})})
        try:
            return urlopen(req, timeout=15)
        except HTTPError as error:
            return error

    def read(self, path, body=None, *, status=200, **options):
        with self.open(path, body, **options) as response:
            raw = response.read()
            if response.status != status:
                raise AssertionError((path, status, response.status, raw[:1200]))
            return json.loads(raw) if raw and status == 200 else None

    def prompt(self, session, text):
        self.read(f"/session/{session}/prompt_async", {
            "model": {"providerID": "ox", "modelID": MODEL}, "parts": [{"type": "text", "text": text}],
        }, status=204)

    def history(self, session, limit=20):
        return self.read(f"/session/{session}/message?limit={limit}")

    def idle(self, session):
        return session not in self.read("/session/status")


class Events:
    def __init__(self, stream):
        self.stream = stream
        self.records = []

    def until(self, name, condition=lambda _: True):
        deadline = time.monotonic() + 15
        for line in self.stream:
            if line.startswith(b"data:"):
                record = json.loads(line[5:])
                self.records.append(record)
                if record["type"] == name and condition(record["properties"]):
                    return record
            if time.monotonic() >= deadline:
                break
        raise AssertionError(f"Stream ended before {name}; last events: {[r['type'] for r in self.records[-10:]]}")


@contextmanager
def process(command, **settings):
    with subprocess.Popen(command, **settings) as child:
        try:
            yield child
        finally:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextmanager
def start_server(opencode, root, env, project):
    with tempfile.TemporaryFile(mode="w+", dir=root) as errors, process(
        [opencode, "serve", "--hostname", "127.0.0.1", "--port", "0", "--pure"],
        env=env, cwd=project, stdout=subprocess.PIPE, stderr=errors, text=True,
    ) as server:
        try:
            if not select.select([server.stdout], [], [], 15)[0]:
                raise AssertionError("OpenCode server did not report its listening address")
            prefix = "opencode server listening on "
            line = server.stdout.readline().strip()
            assert line.startswith(prefix), line
            address = line.removeprefix(prefix)
            assert urlsplit(address).hostname == "127.0.0.1", address
            yield Api(address, env["OPENCODE_SERVER_PASSWORD"], project)
        except Exception:
            errors.seek(0)
            print(errors.read()[-2000:])
            raise


def assert_reconnect(api, session, provider):
    started = time.monotonic()
    with api.open("/event") as stream:
        events = Events(stream)
        events.until("server.connected")
        api.prompt(session, "reconnect")
        initial = events.until("message.part.delta", lambda p: p.get("sessionID") == session and p["delta"] == "first")
    provider.gates["reconnect"].set()
    wait(lambda: api.idle(session), "disconnected turn completion")
    part_id = initial["properties"]["partID"]
    history = api.history(session)
    assert [part["text"] for item in history for part in item["parts"] if part["id"] == part_id] == ["first restored"]
    with api.open(f"/session/{session}/message?limit=1") as response:
        assert response.status == 200
        assert json.load(response) == history[-1:]
        cursor = response.headers["X-Next-Cursor"]
    assert cursor
    older = api.read(f"/session/{session}/message?{urlencode({'limit': 1, 'before': cursor})}")
    assert older == history[-2:-1]

    with api.open("/event", headers={"Last-Event-ID": initial["id"]}) as stream:
        events = Events(stream)
        events.until("server.connected")
        api.prompt(session, "next")
        events.until("session.idle", lambda p: p["sessionID"] == session)
        deltas = [r["properties"] for r in events.records if r["type"] == "message.part.delta"]
        assert any(d["delta"] == "fresh" for d in deltas), deltas
        assert all(d["partID"] != part_id for d in deltas), deltas
    return round((time.monotonic() - started) * 1000)


def approval(api, session, project, case):
    with api.open("/event") as stream:
        events = Events(stream)
        events.until("server.connected")
        api.prompt(session, case)
        ask = events.until("permission.asked", lambda p: p["sessionID"] == session)["properties"]
    # Reconcile a detached client against the pending asks, rather than
    # treating the lifetime of the event stream as the lifetime of an ask.
    asks = api.read("/permission")
    requests = [ask for ask in asks if ask["sessionID"] == session and ask["permission"] == "bash"]
    assert requests == [ask], asks
    target = project / SHELL_CASES[case]
    assert not target.exists(), target
    request = f"/permission/{requests[0]['id']}/reply"
    if case == "cancel approval":
        assert api.read(f"/session/{session}/abort", {}) is True
    else:
        assert api.read(request, {"reply": "reject" if case == "deny" else "once"}) is True
    wait(lambda: api.idle(session), f"{case} terminal state")
    pending = api.read("/permission")
    cleanup = case == "cancel approval" and ask in pending
    if cleanup:
        # Stable v1 can become idle before discarding the approval. Once the
        # host has fenced that turn, it can reject the exact stale native ask.
        with api.open(request, {"reply": "reject"}) as response:
            assert response.status == 404 or (response.status == 200 and json.load(response) is True)
        pending = api.read("/permission")
    assert not pending, (case, pending)
    with api.open(request, {"reply": "once"}) as response:
        # The stable server may acknowledge a stale reply as a no-op. The host
        # must still enforce single consumption under its own session authority.
        assert response.status in (200, 404), response.status
        late_reply = response.status
    if case == "allow":
        assert target.read_text() == "OK"
    else:
        assert not target.exists(), target
    return {"late_reply_status": late_reply, "explicit_reject_after_abort": cleanup}


def assert_cancel_stream(api, session, provider):
    with api.open("/event") as stream:
        events = Events(stream)
        events.until("server.connected")
        api.prompt(session, "cancel stream")
        first = events.until("message.part.delta", lambda p: p.get("sessionID") == session and p["delta"] == "partial")
        assert first["properties"]["field"] == "text"
        assert api.read(f"/session/{session}/abort", {}) is True
        provider.gates["cancel stream"].set()
        events.until("session.idle", lambda p: p["sessionID"] == session)
    assert api.idle(session)
    assert "unwanted tail" not in json.dumps(api.history(session))


def run(opencode, ox, root, provider):
    paths = {name: root / name for name in ("config", "data", "state", "cache", "project", "other", "relay")}
    for path in paths.values():
        path.mkdir(mode=0o700)
    for name in ("project", "other"):
        subprocess.run(["git", "init", "-q", paths[name]], check=True)
        subprocess.run(["git", "-C", paths[name], "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                        "commit", "--allow-empty", "-qm", name], check=True)
    (paths["relay"] / "codeox.toml").write_text(
        '[providers.fixture]\n'
        f'base_url = "http://127.0.0.1:{provider.server_port}/v1"\n'
        'wire_api = "anthropic_messages"\nenv_key = "FIXTURE_PROVIDER_KEY"\ndiscovery = false\n'
        '[[providers.fixture.models]]\nid = "local"\n'
    )
    gateway_port = unused_port()
    gateway_key = secrets.token_urlsafe(24)
    gateway = f"http://127.0.0.1:{gateway_port}/v1"
    env = {
        "PATH": os.environ["PATH"], "HOME": str(root), "LANG": "C",
        **{f"XDG_{name.upper()}_HOME": str(paths[name]) for name in ("config", "data", "state", "cache")},
        "OPENCODE_DISABLE_MODELS_FETCH": "1", "OPENCODE_DISABLE_PROJECT_CONFIG": "1",
        "OPENCODE_SERVER_PASSWORD": secrets.token_urlsafe(24), "FIXTURE_GATEWAY_KEY": gateway_key,
        "OPENCODE_CONFIG_CONTENT": json.dumps({
            "autoupdate": False, "share": "disabled", "enabled_providers": ["ox"],
            "model": f"ox/{MODEL}", "small_model": f"ox/{MODEL}", "permission": {"*": "deny", "bash": "ask"},
            "provider": {"ox": {
                "npm": "@ai-sdk/anthropic", "name": "Hosted OX fixture", "env": ["FIXTURE_GATEWAY_KEY"],
                "options": {"baseURL": gateway}, "models": {MODEL: {
                    "name": "Fixture", "tool_call": True, "limit": {"context": 64000, "output": 4096},
                }},
            }},
        }),
    }
    relay_env = {**env, "CODEX_HOME": str(paths["relay"]), "OX_GATEWAY_KEY": gateway_key,
                 "FIXTURE_PROVIDER_KEY": provider.key}
    catalog = Request(gateway + "/models", headers={"Authorization": f"Bearer {gateway_key}"})

    def ready():
        try:
            with urlopen(catalog, timeout=1) as response:
                return MODEL in {model["id"] for model in json.load(response)["data"]}
        except URLError:
            return False

    with process([ox, "serve", "--port", str(gateway_port)], cwd=paths["project"], env=relay_env,
                 stdout=subprocess.DEVNULL), start_server(opencode, root, env, paths["project"]) as api:
        wait(ready, "OX gateway model catalog")
        assert api.read("/global/health", authenticate=False, status=401) is None
        assert api.read("/global/health")["healthy"] is True
        native = api.read("/path")
        assert native["directory"] == str(paths["project"])
        assert native["state"] == str(paths["state"] / "opencode")
        assert native["config"] == str(paths["config"] / "opencode")
        session = api.read("/session", {"title": "OX host contract"})
        other = api.read("/session", {"title": "Another project"}, project=paths["other"])
        assert session["projectID"] != other["projectID"]
        session_id = session["id"]
        assert api.read(f"/session/{session_id}", project=paths["other"])["directory"] == str(paths["project"])
        print("Confirmed directory routing does not authorize a session ID across repositories.")
        millis = assert_reconnect(api, session_id, provider)
        print(f"Completed and recovered a missed SSE delta through authoritative history in {millis} ms.")
        reply_codes = {case: approval(api, session_id, paths["project"], case) for case in SHELL_CASES}
        print(f"Reconciled pending asks after disconnect. Verified allow once, reject and abort. {reply_codes}")
        assert_cancel_stream(api, session_id, provider)
        print("Aborted an open stream. The terminal history did not acquire its tail.")

    with start_server(opencode, root, env, paths["project"]) as reopened:
        assert reopened.read(f"/session/{session_id}")["directory"] == str(paths["project"])
        assert "first restored" in json.dumps(reopened.history(session_id, limit=100))
        assert reopened.idle(session_id)
        print("Recovered the same session and history after restarting OpenCode.")

    other_data = root / "other-route-data"
    other_data.mkdir(mode=0o700)
    with start_server(opencode, root, {**env, "XDG_DATA_HOME": str(other_data)}, paths["project"]) as isolated:
        assert isolated.read(f"/session/{session_id}", status=404) is None
        print("A server with an independent data store could not read the same session ID.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("opencode", type=Path, help="path to a stock OpenCode binary")
    parser.add_argument("ox", type=Path, help="path to the OX gateway executable")
    args = parser.parse_args()
    opencode, ox = (path.resolve(strict=True) for path in (args.opencode, args.ox))
    report_version(opencode)
    with tempfile.TemporaryDirectory(prefix="ox-opencode-contract-") as temp, \
            ThreadingHTTPServer(("127.0.0.1", 0), Provider) as provider:
        provider.key = secrets.token_urlsafe(24)
        provider.gates = {case: threading.Event() for case in ("reconnect", "cancel stream")}
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        try:
            run(opencode, ox, Path(temp).resolve(), provider)
        finally:
            for gate in provider.gates.values():
                gate.set()
            provider.shutdown()
            thread.join()


if __name__ == "__main__":
    main()
