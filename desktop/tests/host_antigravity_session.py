"""Drive an interactive Antigravity handoff through the real OX session host.

Usage: python3 desktop/tests/host_antigravity_session.py OX_HOST OX [LINUX_AGY]
The optional official binary exercises the signed-out service in an isolated home.
"""
from contextlib import closing
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile

from host_fixture import session_host
from host_stock_session import Client as SessionClient, ask, change


FIXTURES = Path(__file__).resolve().parents[1] / "ox-host/tests/fixtures"
SOURCE_PLAN = "Inspect the workspace before changing anything."
REQUEST = "Continue this plan in Antigravity."
FIRST_REPLY = "The plan is ready for review."
REOPEN_REPLY = "Continuing the same Antigravity session."
PINNED_MEMORY = "The first Antigravity turn should receive this pinned fact."
LOCAL_MEMORY = "This pinned local-only fact must stay in OX."


class Client(SessionClient):
    def __init__(self, path, version=2):
        self.conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.conn.settimeout(25)
        self.conn.connect(path)
        self.input = self.conn.makefile("rb")
        self.send(type="hello", version=version)
        self.hello = self.until(lambda f: f["type"] == "hello")
        assert self.hello["version"] == version, self.hello


def setup(root, host, helper):
    for name in ("bin", "home", "project", "codex"):
        (root / name).mkdir()
    for name, source in (("ox-host", host), ("ox", helper)):
        target = root / "bin" / name
        try:
            os.link(source, target)
        except OSError:
            shutil.copy2(source, target)
    return {"PATH": os.pathsep.join((str(root / "bin"), str(Path(sys.executable).parent), os.defpath)),
            "HOME": str(root / "home"), "CODEX_HOME": str(root / "codex"),
            "XDG_STATE_HOME": str(root / "state"), "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_CACHE_HOME": str(root / "cache"), "SHELL": "/bin/sh", "LANG": "C"}


def install_fixture(root, name, fixture):
    program = root / "bin" / name
    shutil.copyfile(FIXTURES / fixture, program)
    program.chmod(0o700)


def install_claude(root):
    install_fixture(root, "fixture-claude", "worker-claude.py")
    launcher = root / "bin/claude"
    launcher.write_text('''#!/bin/sh
for arg do
    if [ "$arg" = auth ]; then
        printf '%s\\n' '{"loggedIn":true,"apiProvider":"firstParty","authMethod":"claude.ai"}'
        exit 0
    fi
done
exec "$(dirname "$0")/fixture-claude" "$@"
''')
    launcher.chmod(0o700)


def scope(root, runtime):
    return {"project": str(root / "project"), "route": {"runtime": runtime, "billing": "subscription"}}


def snapshot(client, route):
    client.open(route)
    return client.until(lambda frame: frame["type"] == "snapshot")["scopes"][0]


def ready(client, route):
    state = snapshot(client, route)
    if "start" not in state["capabilities"]:
        client.until(lambda f: (s := change(f, "state")) and "start" in s["capabilities"])


def response(client, key, kinds):
    return client.until(lambda f: f.get("id") == key and f["type"] in kinds)


def finished(client, thread):
    client.until(lambda f: (n := change(f, "notification")) and
                 n["thread"] == thread and n["event"] == "finished")


def texts(timeline, role):
    return [item["text"] for item in timeline["items"] if item["role"] == role]


def timeline_detail(frame, thread):
    for update in frame.get("changes", []):
        if update.get("thread") != thread:
            continue
        if update["kind"] == "timeline_state":
            return update["detail"]
        if update["kind"] == "transcript":
            return update["timeline"]["detail"]


def run(root, host, helper, log):
    env = setup(root, host, helper)
    install_claude(root)
    install_fixture(root, "agy", "antigravity-acp.py")
    frames_path = root / "agy.jsonl"
    prompt_path = root / "agy-prompt"
    env.update(OX_TEST_AGY_LOG=str(frames_path), OX_TEST_AGY_PROMPT=str(prompt_path))
    env["CLAUDE_CONFIG_DIR"] = str(root / "project/native")
    source_scope = scope(root, "claude")
    target_scope = scope(root, "antigravity")
    binary = root / "bin/ox-host"

    with session_host(binary, root, env, log), \
            closing(Client(env["OX_HOST_SOCKET"])) as source, \
            closing(Client(env["OX_HOST_SOCKET"])) as target:
        routes = source.hello["routes"]
        assert [r for r in routes if r["runtime"] == "antigravity"] == [target_scope["route"]], routes
        with closing(Client(env["OX_HOST_SOCKET"], version=1)) as legacy:
            assert all(r["runtime"] != "antigravity" for r in legacy.hello["routes"]), legacy.hello
        ready(source, source_scope)
        ready(target, target_scope)
        source.act(source_scope, "source", {"send_text": [None, SOURCE_PLAN]})
        source_id = response(source, "source", {"opened"})["thread"]
        finished(source, source_id)

        handoff = {"source": source_id, "runtime": "antigravity", "mode": "plan", "request": REQUEST}
        source.act(source_scope, "api", {"start_handoff": dict(handoff, model="fixture/api")})
        api = response(source, "api", {"error"})
        assert "Google account" in api["message"], api
        source.act(source_scope, "worker", {"start_worker": {"parent": source_id,
                   "runtime": "antigravity", "mode": "session", "task": "Run unattended"}})
        worker = response(source, "worker", {"error", "worker_reply"})
        assert "operator handoff" in (worker.get("message") or worker.get("error") or ""), worker
        assert not frames_path.exists(), "A rejected API or worker request spawned agy"

        source.act(source_scope, "handoff", {"start_handoff": handoff})
        result = response(source, "handoff", {"worker_reply"})
        assert not result.get("error") and result["result"]["scope"] == target_scope, result
        session = result["result"]["session"]
        first_ask = target.until(ask)
        assert first_ask["thread"] == session and first_ask["choices"] == ["Allow once", "Reject once"], first_ask
        active = snapshot(target, target_scope)
        timeline = active["transcripts"][session]
        assert FIRST_REPLY in texts(timeline, "assistant"), timeline
        assert any("Review the workspace before editing" in plan for plan in texts(timeline, "plan")), timeline
        row = next(row for row in active["sessions"] if row["id"] == session)
        origin = row["detail"]["origin"]
        assert origin["relation"] == "handoff" and origin["parent"]["session"] == source_id, origin
        assert origin["started"] == "plan" and row["detail"]["access"] == "plan", row
        assert set(row["detail"]["command_caps"]["models"]) == {"fixture-gemini-a", "fixture-gemini-b"}, row
        target.act(target_scope, "reject", {"answer_ask": [first_ask["id"], 1]}, first_ask["token"])
        finished(target, session)

        target.act(target_scope, "select-model", {"model_options": [session, {"model": "fixture-gemini-b"}]})
        target.until(lambda f: (t := timeline_detail(f, session)) and t["model"] == "fixture-gemini-b")
        target.act(target_scope, "second-prompt", {"send_text": [session, "Cancel this turn"]})
        second_ask = target.until(ask)
        assert second_ask["id"] != first_ask["id"] and second_ask["thread"] == session, second_ask
        target.act(target_scope, "cancel", {"stop": session})
        target.until(lambda f: (t := timeline_detail(f, session)) and t["interrupted"] and t["active"] is None)
        stopped = snapshot(target, target_scope)
        assert stopped["asks"] == [], "The cancelled turn left a permission request open"
        stopped_result = next(row for row in stopped["sessions"] if row["id"] == session)["detail"]["last_result"]
        assert stopped_result["outcome"] == "stopped", stopped_result

    (Path(env["OX_HOST_STATE_DIR"]) / "host-mode-policy.json").write_text(json.dumps("always_read"))
    with session_host(binary, root, env, log), closing(Client(env["OX_HOST_SOCKET"])) as target:
        state = snapshot(target, target_scope)
        assert any(row["id"] == session for row in state["sessions"]), state
        target.act(target_scope, "continue", {"send_text": [session, "Continue after reopening"]})
        finished(target, session)
        resumed = snapshot(target, target_scope)
        timeline = resumed["transcripts"][session]
        assert texts(timeline, "assistant") == [FIRST_REPLY, REOPEN_REPLY], timeline
        assert texts(timeline, "user") == [prompt_path.read_text(), "Continue after reopening"], timeline
        detail = next(row for row in resumed["sessions"] if row["id"] == session)["detail"]
        assert detail["access"] == "plan", detail
        assert detail["last_result"]["outcome"] == "completed", detail
        assert detail["last_result"]["turn"] != stopped_result["turn"], detail

    first = prompt_path.read_text()
    assert first.startswith("<ox-handoff>\n") and first.endswith("</ox-handoff>\n\n" + REQUEST), first
    assert SOURCE_PLAN in first and "read_ox_thread" not in first, first
    frames = [json.loads(line) for line in frames_path.read_text().splitlines()]
    methods = [frame["method"] for frame in frames if "method" in frame]
    assert methods == ["initialize", "session/new", "session/set_config_option", "session/prompt",
                       "session/set_config_option", "session/prompt", "session/cancel", "initialize",
                       "session/load", "session/set_config_option", "session/prompt"], methods
    print("PASS: Google account handoff carries the plan and ACP streams, changes model, answers, cancels and reopens")


def signed_out(root, host, helper, agy, log):
    env = setup(root, host, helper)
    if agy is None:
        install_fixture(root, "agy", "antigravity-acp.py")
        env.update(OX_TEST_AGY_LOG=str(root / "agy.jsonl"), OX_TEST_AGY_PROMPT=str(root / "agy-prompt"),
                   OX_TEST_AGY_SIGNED_OUT="1")
    else:
        assert sys.platform.startswith("linux"), "The isolated real-CLI check uses Linux"
        env["AGY_CLI_DISABLE_AUTO_UPDATE"] = "true"
        (root / "bin/agy").symlink_to(agy)
    target_scope = scope(root, "antigravity")
    with session_host(root / "bin/ox-host", root, env, log), closing(Client(env["OX_HOST_SOCKET"])) as target:
        ready(target, target_scope)
        target.act(target_scope, "signed-out", {"send_text": [None, "Unsigned smoke prompt"]})
        error = state = None
        while error is None or state is None:
            line = target.input.readline()
            assert line, "Host closed before it reported sign-in failure"
            frame = json.loads(line)
            if frame.get("id") == "signed-out":
                assert frame["type"] == "error", frame
                error = frame
            if (update := change(frame, "state")) and update["connection"] == "sign_in_required":
                state = update
        assert "agy acp --login" in state["detail"]["notice"], state
        assert snapshot(target, target_scope)["sessions"] == [], "Signed-out launch created a session"
        assert "agy acp --login" in error["message"], error
    print(f"PASS: OX correlates the signed-out {'official CLI' if agy else 'ACP fixture'} launch failure")


def yolo_with_memory(root, host, helper, log):
    env = setup(root, host, helper)
    install_fixture(root, "agy", "antigravity-acp.py")
    frames_path = root / "agy.jsonl"
    env.update(OX_TEST_AGY_LOG=str(frames_path), OX_TEST_AGY_PROMPT=str(root / "agy-prompt"),
               OX_TEST_AGY_YOLO="1", OX_MEMORY_PATH=str(root / "memory.json"))
    for text, sensitivity in ((PINNED_MEMORY, "private"), (LOCAL_MEMORY, "local_only")):
        subprocess.run([helper, "memory", "add", text, "--scope", "global", "--pin",
                        "--sensitivity", sensitivity], env=env, cwd=root / "project",
                       stdout=subprocess.DEVNULL, check=True, timeout=8)

    target_scope = scope(root, "antigravity")
    first, follow_up = "Start in explicit YOLO", "Follow up in explicit YOLO"
    with session_host(root / "bin/ox-host", root, env, log), closing(Client(env["OX_HOST_SOCKET"])) as target:
        target.send(type="surface", surface="desktop")
        ready(target, target_scope)
        target.act(target_scope, "choose-yolo", {"set_next_access": "yolo"})
        response(target, "choose-yolo", {"ack"})
        assert not frames_path.exists(), "Changing a draft mode started an agent"
        target.act(target_scope, "new-thread", {"send_text": [None, first]})
        session = response(target, "new-thread", {"opened"})["thread"]
        state = snapshot(target, target_scope)
        if state["transcripts"][session]["detail"]["active"] is not None:
            finished(target, session)
            state = snapshot(target, target_scope)
        row = next(row for row in state["sessions"] if row["id"] == session)
        assert row["detail"]["access"] == "yolo", row
        assert row["detail"]["memory_attachment"] == {
            "entries": 1,
            "from_parent_host": False,
        }, row
        assert texts(state["transcripts"][session], "user") == [first], state["transcripts"][session]
        target.act(target_scope, "follow-up", {"send_text": [session, follow_up]})
        finished(target, session)

    frames = [json.loads(line) for line in frames_path.read_text().splitlines()]
    methods = [frame["method"] for frame in frames if "method" in frame]
    assert methods == ["initialize", "session/new", "session/prompt", "session/prompt"], methods
    before, after = [frame["params"]["prompt"] for frame in frames if frame.get("method") == "session/prompt"]
    assert [part["type"] for part in before] == ["resource", "text"], before
    assert before[0]["annotations"]["audience"] == ["assistant"], before
    resource = before[0]["resource"]["text"]
    assert "OX Desktop" in resource and PINNED_MEMORY in resource and LOCAL_MEMORY not in resource, resource
    assert before[-1] == {"type": "text", "text": first}, before
    assert after[-1] == {"type": "text", "text": follow_up}, after
    assert PINNED_MEMORY not in json.dumps(after) and LOCAL_MEMORY not in json.dumps(after), after
    print("PASS: an explicit YOLO draft uses Google's launch flag and sends eligible memory in the first turn only")


def check(test, *paths):
    with tempfile.TemporaryDirectory(prefix="ox-host-agy-", dir="/tmp") as tmp:
        root = Path(tmp).resolve()
        with (root / "host.log").open("w+") as log:
            try:
                test(root, *paths, log)
            except Exception:
                log.seek(0)
                print(log.read(), file=sys.stderr, end="")
                frames = root / "agy.jsonl"
                if frames.is_file():
                    print(frames.read_text(), file=sys.stderr, end="")
                raise


if __name__ == "__main__":
    if len(sys.argv) not in (3, 4):
        raise SystemExit("Usage: host_antigravity_session.py OX_HOST OX [LINUX_AGY]")
    paths = [Path(value).resolve(strict=True) for value in sys.argv[1:]]
    check(run, *paths[:2])
    check(yolo_with_memory, *paths[:2])
    check(signed_out, *paths[:2], None)
    if len(paths) == 3:
        check(signed_out, *paths)
