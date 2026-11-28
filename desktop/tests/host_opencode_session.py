"""Exercise a host-owned OpenCode session through the authenticated OX socket.

Run with stock OpenCode, a built OX gateway, and a built OX session host.
python3 desktop/tests/host_opencode_session.py STOCK OX OX_HOST
"""

import argparse
from contextlib import closing
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import tempfile
import threading
import time
import uuid

from host_antigravity_session import Client, ready, response, setup, snapshot
from host_fixture import session_host
from opencode_host_contract import MODEL, report_version
from opencode_host_runtime import RuntimeProvider


class Provider(RuntimeProvider):
    def respond(self, case):
        with self.server.requests_lock:
            self.server.requests.append(case)
        super().respond(case)


def await_result(client, key):
    reply = response(client, key, {"ack", "error"})
    assert reply["type"] == "ack", reply


def request_failure(client, scope, key, action, text, token=None):
    client.act(scope, key, action, token)
    reply = response(client, key, {"error"})
    assert text in reply["message"], reply


def state_with(client, scope, thread, predicate):
    # The server publishes deltas for live changes. Read a current baseline at
    # each checkpoint so this fixture also exercises resubscription.
    deadline = time.monotonic() + 16
    while time.monotonic() < deadline:
        state = snapshot(client, scope)
        if predicate(state["transcripts"].get(thread, {}), state["sessions"]):
            return state
        time.sleep(.08)
    raise AssertionError(state)


def route_worker_switches(pid):
    tasks = Path(f"/proc/{pid}/task")
    if not tasks.is_dir():
        return None
    for task in tasks.iterdir():
        try:
            if (task / "comm").read_text().strip() != "ox-opencode-rou":
                continue
            return int(next(line.split()[-1] for line in (task / "status").read_text().splitlines()
                            if line.startswith("voluntary_ctxt_switches:")))
        except FileNotFoundError:
            continue
    return None


def assistant(timeline, text):
    return any(item["role"] == "assistant" and item["text"] == text
               for item in timeline.get("items", []))


def receipt(rows, thread):
    return next((row["detail"]["last_result"] for row in rows
                 if row["id"] == thread and row["detail"].get("last_result")), {})


def outcome(rows, thread):
    return receipt(rows, thread).get("outcome")


def completed(timeline, rows, thread, prompt):
    result = receipt(rows, thread)
    return result.get("outcome") == "completed" and any(
        item["role"] == "user" and item["text"] == prompt and item["turn"] == result["turn"]
        for item in timeline.get("items", []))


def exercise(root, host, ox, stock, provider):
    env = setup(root, host, ox)
    (root / "bin/opencode").symlink_to(stock)
    project = root / "project"
    config = root / "codex/codeox.toml"
    provider.key = secrets.token_urlsafe(24)
    provider.requests, provider.requests_lock = [], threading.Lock()
    provider.gates = {name: threading.Event() for name in ("reconnect", "cancel stream")}
    config.write_text(
        '[providers.fixture]\n'
        f'base_url = "http://127.0.0.1:{provider.server_port}/v1"\n'
        'wire_api = "anthropic_messages"\nenv_key = "FIXTURE_PROVIDER_KEY"\ndiscovery = false\n'
        f'[[providers.fixture.models]]\nid = "{MODEL.split("/", 1)[1]}"\n'
    )
    env.update(FIXTURE_PROVIDER_KEY=provider.key, LANG="C.UTF-8")
    route = {"project": str(project), "route": {"runtime": "opencode", "billing": {"api": MODEL}}}
    binary = root / "bin/ox-host"
    with (root / "host.log").open("w") as log:
        with session_host(binary, root, env, log) as local, closing(Client(env["OX_HOST_SOCKET"], 3)) as client:
            client.open(route)
            error = client.until(lambda frame: frame["type"] == "error")
            assert "not enrolled" in error["message"], error
            assert route_worker_switches(local.pid) is None
            assert not (Path(env["OX_HOST_STATE_DIR"]) / "opencode").exists()

        env.update(OX_EXPERIMENTAL_HOSTED_OPENCODE_PROJECT=str(project),
                   OX_EXPERIMENTAL_HOSTED_OPENCODE_ALIAS=MODEL)
        with session_host(binary, root, env, log) as local, closing(Client(env["OX_HOST_SOCKET"], 3)) as client:
            assert all(r["runtime"] != "opencode" for r in client.hello["routes"]), client.hello
            client.open({"project": str(project), "route": dict(route["route"], billing="subscription")})
            error = client.until(lambda frame: frame["type"] == "error")
            assert "exact OX API alias" in error["message"], error
            client.open({"project": str(project), "route": dict(route["route"], billing={"api": "other/model"})})
            error = client.until(lambda frame: frame["type"] == "error")
            assert "not enrolled" in error["message"], error
            client.open(dict(route, project=str(root / "home")))
            error = client.until(lambda frame: frame["type"] == "error")
            assert "not enrolled" in error["message"], error
            ready(client, route)
            # The worker settles after signaling Ready. It must remain blocked
            # during an interval long enough for multiple scheduled refreshes.
            time.sleep(.1)
            idle_switches = route_worker_switches(local.pid)
            if Path("/proc").is_dir():
                assert idle_switches is not None
                time.sleep(.85)
                assert idle_switches == route_worker_switches(local.pid)
            with closing(Client(env["OX_HOST_SOCKET"], 3)) as unscoped:
                unscoped.open(None)
                assert not any(s["scope"] == route for s in
                               unscoped.until(lambda f: f["type"] == "snapshot")["scopes"])
            with closing(Client(env["OX_HOST_SOCKET"], 2)) as legacy:
                legacy.open(None)
                old = legacy.until(lambda frame: frame["type"] == "snapshot")
                assert all(s["scope"]["route"]["runtime"] != "opencode" for s in old["scopes"]), old
                legacy.open(route)
                assert "protocol version" in legacy.until(lambda frame: frame["type"] == "error")["message"]

            client.act(route, "new", {"send_text": [None, "reconnect"]})
            opened = response(client, "new", {"opened"})
            assert opened["scope"] == route, opened
            thread = opened["thread"]
            assert str(uuid.UUID(thread)) == thread, opened
            await_result(client, "new")
            state = state_with(client, route, thread, lambda timeline, _: assistant(timeline, "first"))
            assert not provider.gates["reconnect"].is_set()
            assert state["transcripts"][thread]["detail"]["active"] is not None
            assert state["capabilities"] == ["start"], state["capabilities"]
            row = next(row for row in state["sessions"] if row["id"] == thread)
            assert set(row["capabilities"]) == {"open", "stop"}, row
            if idle_switches is not None:
                active_switches = route_worker_switches(local.pid)
                time.sleep(.35)
                assert route_worker_switches(local.pid) > active_switches

            with closing(Client(env["OX_HOST_SOCKET"], 3)) as second:
                state = snapshot(second, route)
                assert assistant(state["transcripts"][thread], "first"), state
                second.act(route, "new", {"send_text": [None, "reconnect"]})
                assert response(second, "new", {"opened"})["thread"] == thread
                await_result(second, "new")
                request_failure(second, route, "new", {"send_text": [None, "wrong"]}, "different")
                provider.gates["reconnect"].set()
                second.until(lambda frame: any(
                    change["kind"] == "append" and change["thread"] == thread and change["text"] == " restored"
                    for change in frame.get("changes", [])))
                state_with(second, route, thread, lambda tl, rows:
                           assistant(tl, "first restored") and outcome(rows, thread) == "completed")
            assert provider.requests.count("reconnect") == 1, provider.requests
            request_failure(client, route, "move", {"terminal": thread}, "unavailable")

            client.act(route, "approval-turn", {"send_text": [thread, "allow"]})
            await_result(client, "approval-turn")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                state = snapshot(client, route)
                if state["asks"]:
                    break
                time.sleep(.08)
            pending = state["asks"][0]
            assert pending["thread"] == thread and pending["choices"] == ["Allow once", "Reject"], pending
            request_failure(client, route, "wrong-token", {"answer_ask": [pending["id"], 0]}, "already resolved", "wrong")
            assert not (project / "allow.txt").exists()
            client.act(route, "allow-once", {"answer_ask": [pending["id"], 0]}, pending["token"])
            await_result(client, "allow-once")
            state_with(client, route, thread, lambda tl, rows:
                       assistant(tl, "done") and completed(tl, rows, thread, "allow"))
            assert (project / "allow.txt").read_text() == "OK"
            request_failure(client, route, "stale-token", {"answer_ask": [pending["id"], 0]},
                            "already resolved", pending["token"])

            client.act(route, "cancel-turn", {"send_text": [thread, "cancel stream"]})
            await_result(client, "cancel-turn")
            state_with(client, route, thread, lambda tl, _: assistant(tl, "partial"))
            client.act(route, "stop", {"stop": thread})
            await_result(client, "stop")
            provider.gates["cancel stream"].set()
            state_with(client, route, thread, lambda tl, rows:
                       outcome(rows, thread) == "stopped" and not assistant(tl, "partial unwanted tail"))

        with session_host(binary, root, env, log), closing(Client(env["OX_HOST_SOCKET"], 3)) as client:
            ready(client, route)
            sessions = snapshot(client, route)["sessions"]
            assert [row["id"] for row in sessions] == [thread], sessions
            client.act(route, "reopen", {"select_thread": thread})
            await_result(client, "reopen")
            state_with(client, route, thread, lambda tl, _: assistant(tl, "first restored"))
            client.act(route, "continue", {"send_text": [thread, "next"]})
            await_result(client, "continue")
            state_with(client, route, thread, lambda tl, rows:
                       assistant(tl, "fresh") and outcome(rows, thread) == "completed")
            for turn in range(27):
                case = f"history {turn}"
                client.act(route, case, {"send_text": [thread, case]})
                await_result(client, case)
                state_with(client, route, thread, lambda tl, rows:
                           completed(tl, rows, thread, case))
        with session_host(binary, root, env, log), closing(Client(env["OX_HOST_SOCKET"], 3)) as client:
            ready(client, route)
            client.act(route, "known-recent", {"select_thread": thread})
            await_result(client, "known-recent")
            recent = snapshot(client, route)["transcripts"][thread]
            assert recent["detail"]["older"] and not assistant(recent, "first restored"), recent
            client.act(route, "older", {"load_older": thread})
            await_result(client, "older")
            archive = snapshot(client, route)["transcripts"][thread]
            assert not archive["detail"]["older"] and assistant(archive, "first restored"), archive
    print("OpenCode socket enrollment, exact API billing, text before native text-end, transcript deltas, receipts, approval, Stop, restart and history paging passed")
    if Path("/proc").is_dir():
        print("Verified off route starts no worker and idle worker blocks without scheduled refreshes")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("stock", "ox", "host"):
        parser.add_argument(arg, type=Path)
    args = parser.parse_args()
    stock, ox, host = (getattr(args, k).resolve(strict=True) for k in ("stock", "ox", "host"))
    report_version(stock)
    with tempfile.TemporaryDirectory(prefix="ox-host-opencode-") as temp, \
            ThreadingHTTPServer(("127.0.0.1", 0), Provider) as provider:
        service = threading.Thread(target=provider.serve_forever, daemon=True)
        service.start()
        try:
            exercise(Path(temp).resolve(), host, ox, stock, provider)
        except BaseException:
            log = Path(temp) / "host.log"
            if log.exists():
                print(log.read_text())
            raise
        finally:
            for gate in getattr(provider, "gates", {}).values():
                gate.set()
            provider.shutdown()
            service.join()


if __name__ == "__main__":
    main()
