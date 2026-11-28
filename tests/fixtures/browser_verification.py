#!/usr/bin/env python3
"""Opt-in end-to-end check of the OX browser with real Chromium and a local issue tracker."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
from statistics import median
import subprocess
import tempfile
from threading import Thread
from time import perf_counter, sleep
import urllib.request
from urllib.parse import urlsplit


PAGE = b"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Issue tracker</title>
  <style>
    * { box-sizing: border-box; }
    body { margin: 0; font: 17px system-ui; background: #f4f6fa; color: #18213b; }
    header { background: #162842; color: white; padding: 20px max(28px, 11vw); }
    header strong { letter-spacing: .03em; }
    main { max-width: 760px; margin: 46px auto; padding: 0 26px; }
    h1 { margin: 0 0 18px; }
    form, section { background: white; padding: 24px; border: 1px solid #d8dee9;
      border-radius: 12px; margin: 14px 0; }
    label { display: block; font-weight: 650; margin-bottom: 8px; }
    input { padding: 11px 12px; width: 100%; font: inherit; border: 1px solid #8899b4;
      border-radius: 6px; }
    button { background: #1b4cb1; color: white; font: inherit; padding: 10px 16px;
      border: 0; border-radius: 6px; margin-top: 12px; }
    [role=alert] { color: #9c1720; }
    [role=status] { color: #1e6a44; }
  </style>
</head>
<body>
  <header><strong>Trailhead</strong> / Issue tracker</header>
  <main>
    <h1>Project issues</h1>
    <p>Track defects discovered during a local build.</p>
    <form>
      <label for="title">Issue title</label>
      <input id="title" placeholder="Describe the defect" autocomplete="off">
      <button>Create issue</button>
      <p role="alert" id="error"></p>
      <p role="status" id="status"></p>
    </form>
    <section>
      <h2>Open issues</h2>
      <ul id="issues"></ul>
    </section>
  </main>
  <script>
    const title = document.querySelector('#title');
    const status = document.querySelector('#status');
    const error = document.querySelector('#error');
    const issues = document.querySelector('#issues');
    title.value = localStorage.getItem('draft') || '';
    title.oninput = () => localStorage.setItem('draft', title.value);
    async function loadIssues() {
      const records = await fetch('/issues').then(res => res.json());
      issues.replaceChildren(...records.map(record => {
        const issue = document.createElement('li');
        issue.textContent = record;
        return issue;
      }));
    }
    document.querySelector('form').onsubmit = async event => {
      event.preventDefault();
      error.textContent = '';
      status.textContent = '';
      const value = title.value.trim();
      if (!value) { error.textContent = 'Enter an issue title.'; return; }
      const response = await fetch('/issues', {method: 'POST', body: value});
      if (!response.ok) { error.textContent = 'The issue could not be saved.'; return; }
      title.value = '';
      localStorage.removeItem('draft');
      await loadIssues();
      history.pushState({}, '', '/issues/open?view=detail#latest');
      status.textContent = 'Issue created.';
    };
    loadIssues();
  </script>
</body>
</html>
"""


class IssueTracker(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, status, mime, body):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = urlsplit(self.path).path
        if path == "/issues":
            self.reply(200, "application/json", json.dumps(self.server.issues).encode())
        elif path in ("/external", "/dev-redirect"):
            location = ("http://example.invalid/" if path == "/external" else
                        f"http://ox-dev.localhost:{self.server.server_port}/")
            self.send_response(302)
            self.send_header("Location", location)
            self.send_header("Content-Length", "0")
            self.end_headers()
        elif path in ("/", "/issues/open"):
            self.reply(200, "text/html; charset=utf-8", PAGE)
        else:
            self.reply(404, "text/plain", b"Not found")

    def do_POST(self):
        if self.path != "/issues":
            self.reply(404, "text/plain", b"Not found")
            return
        self.server.issues.append(self.rfile.read(int(self.headers["Content-Length"])).decode())
        self.reply(201, "text/plain", b"Created")


def process_tree_memory(root):
    output = subprocess.check_output(["ps", "-axo", "pid=,ppid=,rss=,command="], text=True)
    processes = []
    for line in output.splitlines():
        fields = line.split(maxsplit=3)
        if len(fields) == 4:
            pid, parent, kib = map(int, fields[:3])
            processes.append((pid, parent, kib, fields[3]))
    owned = {
        pid for pid, _, _, command in processes
        if command.startswith(str(root / "agent-browser-"))
    }
    while True:
        current = owned | {pid for pid, parent, _, _ in processes if parent in owned}
        if current == owned:
            break
        owned = current
    rss = sum(kib for pid, _, kib, _ in processes if pid in owned)
    memory = {"browser_process_tree_rss_mib": round(rss / 1024, 2)}
    pss = []
    for pid in owned:
        try:
            status = Path(f"/proc/{pid}/smaps_rollup").read_text()
        except OSError:
            continue
        pss.extend(int(value) for value in re.findall(r"^Pss:\s*(\d+) kB$", status, re.M))
    if pss:
        memory["browser_process_tree_pss_mib"] = round(sum(pss) / 1024, 2)
    return memory


def ref(snapshot, role, label):
    return "@" + re.search(
        rf'{role} "{re.escape(label)}".*?\[ref=(e\d+)\]', snapshot
    ).group(1)


def verify(args, server, home):
    env = {
        "PATH": os.environ["PATH"],
        "HOME": str(home),
        "XDG_CACHE_HOME": str(home / "cache"),
        "LANG": os.environ.get("LANG", "C.UTF-8"),
    }
    for key in ("SSL_CERT_FILE", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
                "http_proxy", "https_proxy", "all_proxy", "no_proxy"):
        if key in os.environ:
            env[key] = os.environ[key]
    if args.chromium:
        env["OX_BROWSER_EXECUTABLE"] = str(args.chromium)
    worktree = home / "project"
    subdir = worktree / "src"
    other = home / "other-project"
    subprocess.run(["git", "init", "-q", worktree], env=env, check=True)
    git = ["git", "-C", worktree, "-c", "user.name=OX Fixture", "-c", "user.email=ox@example.invalid"]
    subprocess.run([*git, "commit", "-qm", "Fixture", "--allow-empty"], env=env, check=True)
    subprocess.run([*git, "worktree", "add", "-q", "--detach", other], env=env, check=True)
    subdir.mkdir()
    url = f"http://127.0.0.1:{server.server_port}"
    cache = home / "cache" / "ox" / "browser"

    def run(*command, session=None, allow_host=None, cwd=worktree, check=True):
        scope = [] if session is None else ["--session", session]
        if allow_host is not None:
            scope.extend(["--allow-host", allow_host])
        start = perf_counter()
        completed = subprocess.run(
            [args.ox, "browser", *scope, *command], cwd=cwd, env=env,
            capture_output=True, text=True, timeout=120, check=False,
        )
        if check and completed.returncode:
            raise RuntimeError(f"{command}: {completed.stderr}\n{completed.stdout}")
        return (perf_counter() - start) * 1000, completed

    assert run("preview", check=False)[1].returncode == 2
    missing_url = run("url", check=False)[1]
    assert missing_url.returncode == 2 and not missing_url.stdout
    setup_ms, _ = run("install")
    assert run("preview", check=False)[1].returncode == 3
    assert run("snapshot", check=False)[1].returncode == 1
    closed_url = run("url", check=False)[1]
    assert closed_url.returncode == 3 and not closed_url.stdout
    assert run("close")[1].returncode == 0
    assert process_tree_memory(cache)["browser_process_tree_rss_mib"] == 0
    try:
        open_ms, _ = run("open", url)
        first_snapshot_ms, snapshot = run("snapshot", cwd=subdir)
        assert run("url", cwd=subdir)[1].stdout == f"{url}/\n"
        title_ref = ref(snapshot.stdout, "textbox", "Issue title")
        create_ref = ref(snapshot.stdout, "button", "Create issue")
        _, output = run("preview")
        preview = output.stdout.strip()
        assert re.fullmatch(r"http://127\.0\.0\.1:[0-9]+", preview), preview
        with urllib.request.urlopen(preview, timeout=5) as response:
            assert response.status == 200
        memory = process_tree_memory(cache)
        args.artifacts.mkdir(parents=True, exist_ok=True)
        run("screenshot", str(args.artifacts / "initial-issue-list.png"), cwd=subdir)

        # The first click exposes a realistic validation failure. Repair it
        # from a nested directory using the same browser, then check persistence.
        click_times, snapshots = [], []
        for _ in range(8):
            click_ms, _ = run("click", create_ref, cwd=subdir)
            warm_ms, failed = run("snapshot")
            assert "Enter an issue title." in failed.stdout
            click_times.append(click_ms)
            snapshots.append(warm_ms)
        title = "--headed is ordinary issue text"
        run("fill", title_ref, title, cwd=subdir)
        assert title in run("snapshot")[1].stdout

        # Same worktree, a different conversation. Then same label, different worktree.
        run("open", url, session="second")
        assert title not in run("snapshot", session="second")[1].stdout
        run("open", f"{url}/?worktree=other", cwd=other)
        assert title not in run("snapshot", cwd=other)[1].stdout
        assert title in run("snapshot", cwd=subdir)[1].stdout
        assert run("url", cwd=other)[1].stdout == f"{url}/?worktree=other\n"

        run("click", create_ref)
        # A click can complete before the app's fetch and resulting DOM update.
        deadline = perf_counter() + 5
        while perf_counter() < deadline:
            _, saved = run("snapshot")
            if "Issue created." in saved.stdout:
                break
            sleep(.02)
        assert title in server.issues, saved.stdout
        assert "Issue created." in saved.stdout, saved.stdout
        assert title in saved.stdout
        assert run("url", cwd=subdir)[1].stdout == f"{url}/issues/open?view=detail#latest\n"
        assert run("url", cwd=other)[1].stdout == f"{url}/?worktree=other\n"
        assert run("url", session="second")[1].stdout == f"{url}/\n"
        run("screenshot", str(args.artifacts / "issue-created.png"), cwd=subdir)
        assert (args.artifacts / "issue-created.png").read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
        assert "Project issues" in json.dumps(json.loads(run("snapshot", "--json")[1].stdout))
        assert run("open", "https://example.invalid", check=False)[1].returncode
        assert "Project issues" in run("snapshot")[1].stdout
        _, redirected = run("open", f"{url}/external", check=False)
        assert redirected.returncode and "blocked by the domain filter" in redirected.stderr, redirected
        assert "example.invalid" not in run("url", check=False)[1].stdout

        run("close")
        assert run("preview", check=False)[1].returncode == 3
        closed_url = run("url", check=False)[1]
        assert closed_url.returncode == 3 and not closed_url.stdout
        assert run("snapshot", check=False)[1].returncode == 1
        assert "Project issues" in run("snapshot", session="second")[1].stdout
        run("open", url)
        assert "Issue created." not in run("snapshot")[1].stdout

        # Chromium resolves *.localhost to loopback without editing system DNS.
        # The exact host still needs explicit opt-in in both OX and the daemon.
        dev_host = "ox-dev.localhost"
        dev_url = f"http://{dev_host}:{server.server_port}"
        assert run("open", dev_url, check=False)[1].returncode
        _, blocked = run("open", f"{url}/dev-redirect", check=False)
        assert blocked.returncode and "blocked by the domain filter" in blocked.stderr, blocked
        assert run("preview", allow_host=dev_host, check=False)[1].returncode == 3
        run("open", f"{url}/dev-redirect", allow_host=dev_host)
        _, trusted = run("snapshot", allow_host=dev_host)
        assert "Project issues" in trusted.stdout
        assert run("url", allow_host=dev_host)[1].stdout == f"{dev_url}/\n"
        wrong_host = run("url", allow_host="other.localhost", check=False)[1]
        assert wrong_host.returncode == 3 and not wrong_host.stdout
        assert dev_host not in run("url", check=False)[1].stdout
        assert run("preview", allow_host=dev_host)[1].stdout != run("preview")[1].stdout
        assert run("open", dev_url, allow_host="other.localhost", check=False)[1].returncode
        _, redirected = run("open", f"{url}/external", allow_host=dev_host, check=False)
        assert redirected.returncode and "blocked by the domain filter" in redirected.stderr, redirected
        assert "example.invalid" not in run("url", allow_host=dev_host, check=False)[1].stdout
        run("close", allow_host=dev_host)
        assert run("preview", allow_host=dev_host, check=False)[1].returncode == 3
        closed_url = run("url", allow_host=dev_host, check=False)[1]
        assert closed_url.returncode == 3 and not closed_url.stdout
        # The original scope is still running after the explicitly opted-in one closes.
        run("open", url)
        assert "Project issues" in run("snapshot")[1].stdout
        assert run("url")[1].stdout == f"{url}/\n"
    finally:
        run("close", check=False)
        run("close", allow_host="ox-dev.localhost", check=False)
        run("close", session="second", check=False)
        run("close", cwd=other, check=False)

    print(json.dumps({
        "component_setup_ms": round(setup_ms, 2),
        "cold_browser_and_first_snapshot_ms": round(open_ms + first_snapshot_ms, 2),
        "warm_snapshot_median_ms": round(median(snapshots), 2),
        "warm_click_median_ms": round(median(click_times), 2),
        **memory,
        "checked": ["validation and repair", "server save", "worktree and session isolation",
                    "live preview", "screenshot", "current URL after SPA navigation",
                    "URL scope", "trusted host override", "close and reopen"],
    }, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ox", type=lambda path: Path(path).resolve())
    parser.add_argument("--chromium", type=Path)
    parser.add_argument("--artifacts", required=True, type=lambda path: Path(path).resolve())
    args = parser.parse_args()
    with ThreadingHTTPServer(("127.0.0.1", 0), IssueTracker) as server:
        server.issues = []
        Thread(target=server.serve_forever, daemon=True).start()
        with tempfile.TemporaryDirectory(prefix="ox-browser-verify-") as home:
            verify(args, server, Path(home))


if __name__ == "__main__":
    main()
