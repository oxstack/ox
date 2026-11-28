#!/usr/bin/env python3
"""Exercise Open, Close, Snapshot, and Preview ordering during a real component refresh."""

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from threading import Thread
from time import monotonic, sleep

from browser_verification import IssueTracker


def session_name(worktree, label):
    key = os.fsencode(worktree) + b"\0" + label.encode()
    return "ox-" + hashlib.sha256(key).hexdigest()[:24]


def installed(cache, old):
    version = subprocess.check_output([old, "--version"], text=True).split()[-1]
    digest = hashlib.sha256(old.read_bytes()).hexdigest()
    identity = f"{version}-{digest}"
    binary = cache / f"agent-browser-{identity}"
    shutil.copyfile(old, binary)
    binary.chmod(0o700)
    marker = cache / "current"
    marker.write_text(identity + "\n")
    marker.chmod(0o600)
    return identity, binary


def gate_browser(binary):
    stock = binary.with_suffix(".stock")
    binary.rename(stock)
    gate = binary.parent / "command-gate"
    binary.write_text(f"""#!{sys.executable}
import os
from pathlib import Path
import sys
import time

gate = Path({str(gate)!r})
action = gate / "action"
if action.exists() and action.read_text() in sys.argv[1:]:
    (gate / "entered").touch()
    deadline = time.monotonic() + 20
    while not (gate / "resume").exists():
        if time.monotonic() >= deadline:
            sys.exit("Timed out waiting to resume the browser fixture")
        time.sleep(.02)
os.execv({str(stock)!r}, [{str(stock)!r}, *sys.argv[1:]])
""")
    binary.chmod(0o700)
    return gate


@contextmanager
def pause(gate, action):
    gate.mkdir()
    (gate / "action").write_text(action)
    try:
        yield
    finally:
        (gate / "resume").touch()


def await_gate(gate, child):
    deadline = monotonic() + 10
    while not (gate / "entered").exists():
        if child.poll() is not None or monotonic() >= deadline:
            raise RuntimeError("The agent-browser subprocess did not reach the fixture gate.")
        sleep(.02)


def finish(child):
    stdout, stderr = child.communicate(timeout=30)
    if child.returncode:
        raise RuntimeError(f"{child.args} returned {child.returncode}\n{stderr}\n{stdout}")
    return stdout


def blocks_lock(path, mode):
    with path.open("a") as handle:
        try:
            fcntl.flock(handle, mode | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
    return False


def completes_while_paused(child):
    try:
        child.wait(timeout=1)
    except subprocess.TimeoutExpired:
        return False
    return True


def verify(ox, old, home, url):
    cache = home / "cache" / "ox" / "browser"
    cache.mkdir(mode=0o700, parents=True)
    previous, binary = installed(cache, old)
    env = {**os.environ, "HOME": str(home), "XDG_CACHE_HOME": str(home / "cache")}
    worktree = home / "worktree"
    worktree.mkdir()
    children = []
    labels = ("reopen", "snapshot", "preview")

    def start(label, *args):
        child = subprocess.Popen(
            [ox, "browser", "--session", label, *args],
            cwd=worktree, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        children.append(child)
        return child

    def scope(label):
        return cache / "scopes" / session_name(worktree, label)

    try:
        for label in labels:
            finish(start(label, "open", url))
        finish(start("reopen", "install"))
        latest = (cache / "current").read_text().splitlines()[0]
        assert latest != previous, "Use an older stock release as the previous component."
        assert all(scope(label).read_text() == previous for label in labels)
        gate = gate_browser(binary)

        with pause(gate, "close"):
            closing = start("reopen", "close")
            await_gate(gate, closing)
            close_exclusive = blocks_lock(scope("reopen").with_suffix(".lock"), fcntl.LOCK_SH)
            reopening = start("reopen", "open", url)
            reopen_raced = completes_while_paused(reopening)
        finish(closing)
        finish(reopening)
        shutil.rmtree(gate)
        new_component = scope("reopen").read_text()

        with pause(gate, "snapshot"):
            snapshot = start("snapshot", "snapshot")
            await_gate(gate, snapshot)
            lock = scope("snapshot").with_suffix(".lock")
            snapshot_shared = blocks_lock(lock, fcntl.LOCK_EX) and not blocks_lock(lock, fcntl.LOCK_SH)
            closing = start("snapshot", "close")
            close_raced = completes_while_paused(closing)
        finish(snapshot)
        finish(closing)
        preview = start("snapshot", "preview")
        stdout, _ = preview.communicate(timeout=10)
        shutil.rmtree(gate)

        with pause(gate, "status"):
            inspecting = start("preview", "preview")
            await_gate(gate, inspecting)
            lock = scope("preview").with_suffix(".lock")
            preview_shared = blocks_lock(lock, fcntl.LOCK_EX) and not blocks_lock(lock, fcntl.LOCK_SH)
            closing = start("preview", "close")
            preview_raced = completes_while_paused(closing)
        inspecting.communicate(timeout=10)
        finish(closing)
        finish(start("preview", "open", url))
        preview_reopened_component = scope("preview").read_text()
        result = {
            "previous_component": previous,
            "installed_component": latest,
            "reopened_component": new_component,
            "close_held_exclusive_lock": close_exclusive,
            "open_completed_before_close": reopen_raced,
            "snapshot_held_shared_lock": snapshot_shared,
            "close_completed_before_snapshot": close_raced,
            "preview_status_after_close": preview.returncode,
            "preview_output_after_close": stdout,
            "preview_held_shared_lock": preview_shared,
            "close_completed_before_preview": preview_raced,
            "preview_reopened_component": preview_reopened_component,
        }
        print(json.dumps(result, indent=2), flush=True)
        assert close_exclusive and not reopen_raced and new_component == latest, result
        assert snapshot_shared and not close_raced and preview.returncode == 3 and not stdout, result
        assert preview_shared and not preview_raced and preview_reopened_component == latest, result
    finally:
        for child in children:
            if child.poll() is None:
                child.kill()
                child.communicate()
        for label in labels:
            start(label, "close").communicate(timeout=15)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ox", type=lambda path: Path(path).resolve())
    parser.add_argument("previous_agent_browser", type=lambda path: Path(path).resolve())
    args = parser.parse_args()
    with ThreadingHTTPServer(("127.0.0.1", 0), IssueTracker) as server:
        server.issues = []
        Thread(target=server.serve_forever, daemon=True).start()
        try:
            with tempfile.TemporaryDirectory(prefix="ox-browser-refresh-") as directory:
                verify(args.ox, args.previous_agent_browser, Path(directory),
                       f"http://127.0.0.1:{server.server_port}")
        finally:
            server.shutdown()


if __name__ == "__main__":
    main()
