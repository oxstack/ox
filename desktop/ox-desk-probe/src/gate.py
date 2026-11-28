"""Drive a private native window and measure the GUI, session host and runtime processes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import shlex
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import tomllib

if "session_host" not in globals():
    sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tests"))
    from host_fixture import session_host

parser = argparse.ArgumentParser()
parser.add_argument("--out", required=True, type=Path)
parser.add_argument("--app", type=Path)
parser.add_argument("--ox", type=Path, help="matching real ox for the secure settings scenario")
parser.add_argument("--probe-commit", default="", help=argparse.SUPPRESS)
parser.add_argument("--limits-toml", help=argparse.SUPPRESS)
parser.add_argument("--wayland", action="store_true", help="use a private Sway compositor")
parser.add_argument("--scenarios", default="G1,G2,G3,G4,G5,G6,G7,G8,G9,G10,G11,G12")
parser.add_argument("--restore-samples", type=int, default=10, help="launches per restored-thread case")
parser.add_argument("--probe", type=Path, required=True)
parser.add_argument("--mac-key-driver", type=Path, help="native macOS shortcut driver for G14")
args = parser.parse_args()
if args.restore_samples < 1:
    parser.error("--restore-samples must be at least 1")
limits = (tomllib.loads(args.limits_toml) if args.limits_toml else
          tomllib.loads((Path(__file__).resolve().parents[2] / "gate.toml").read_text()))["linux"]
probe = args.probe.resolve()
gui = (args.app or probe.with_name("ox-desktop")).resolve()
fake = probe.with_name("fake-app-server")
selected = set(args.scenarios.split(","))
report = {"platform": os.uname().machine, "renderer": os.environ.get("VK_DRIVER_FILES", "system"),
          "display": "wayland" if args.wayland else ("macos" if sys.platform == "darwin" else "x11"), "scenarios": {}}


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for data in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(data)
    return digest.hexdigest()


report["probe_commit"] = args.probe_commit or None
report["application_sha256"] = sha256(gui)
report["host_sha256"] = sha256(gui.with_name("ox-host"))
report["probe_sha256"] = sha256(probe)
if args.ox:
    report["companion_sha256"] = sha256(args.ox)


# Wall-clock thresholds (frame times, latencies, CPU) measure the machine as
# much as the app. On shared CI runners they are recorded and reported, and
# only counts and structure decide a scenario. Set OX_GATE_TIMING=block on
# dedicated hardware to make them decide it too.
TIMING_BLOCKS = os.environ.get("OX_GATE_TIMING") == "block"


def timed(structure_ok, timing_ok, name):
    if not timing_ok:
        print(f"::warning title=Gate timing::{name} is over a timing threshold "
              f"({'blocking' if TIMING_BLOCKS else 'reported, not blocking'})", flush=True)
    return structure_ok and (timing_ok or not TIMING_BLOCKS)


def record(key, passed, **values):
    report["scenarios"][key] = {"passed": passed, **values}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, indent=2) + "\n")
    print(key, report["scenarios"][key], flush=True)


def wait(fn, timeout=12):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = fn()
        if value:
            return value
        time.sleep(.04)
    raise TimeoutError(f"Timed out waiting for {fn}")


class Fixture:
    def __init__(self, root):
        self.root = root
        self.app = root / "ox-desktop"
        self.host_binary = root / "ox-host"
        self.host = None
        self.host_process = None
        for source, dest in [(gui, self.app), (gui.with_name("ox-host"), self.host_binary)]:
            try:
                os.link(source, dest)
            except OSError:
                shutil.copy2(source, dest)
        helper = root / "ox"
        helper.write_text(f'#!/bin/sh\nexec {shlex.quote(str(fake))} --synthetic "$@"\n')
        helper.chmod(0o755)
        self.project = root / "project"
        self.project.mkdir()
        self.log = (root / "app.log").open("w+")
        self.client = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.client.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1 << 20)
        self.client.bind(str(root / "probe.sock"))
        self.client.settimeout(3)
        self.proc = None
        self.sequence = 0
        self.pointer = None
        if args.wayland:
            self.pointer = subprocess.Popen([probe, "wayland-pointer"], stdin=subprocess.PIPE,
                                            stdout=subprocess.PIPE, text=True, bufsize=1)
            output = next(o for o in json.loads(self.sway("-t", "get_outputs")) if o["active"])
            self.output = output["rect"]

    def start(self, threads, items, smoke=False, real_settings=False, routes=False, restore=False, claude=False, remote=False):
        self.stop()
        previous = self.env if restore else {}
        if real_settings:
            ox = (args.ox or gui.with_name("ox")).resolve(strict=True)
            (self.root / "ox").write_text("#!/bin/sh\n" +
                'if [ "$1" = "--ox-desktop-settings-v1" ]; then exec ' + shlex.quote(str(ox)) + ' "$@"; fi\n' +
                "exec " + shlex.quote(str(fake)) + ' --synthetic "$@"\n')
        self.sequence += 1
        stage = self.root / str(self.sequence)
        stage.mkdir(mode=0o700)
        self.sock = str(stage / "stats")
        self.control_socket = str(stage / "control")
        self.trace = stage / "trace"
        self.env = {key: os.environ[key] for key in
                    ["PATH", "LD_LIBRARY_PATH", "DISPLAY", "XAUTHORITY", "VK_DRIVER_FILES"]
                    if key in os.environ}
        self.env.update(HOME=str(stage), CODEX_HOME=str(stage / ".codex"),
                        XDG_RUNTIME_DIR=str(stage), XDG_STATE_HOME=str(stage / "state"),
                        XDG_CONFIG_HOME=str(stage / "config"), XDG_SESSION_TYPE="x11",
                        OX_DESK_STATS_SOCKET=self.sock, OX_DESK_SYNTH_CONTROL=self.control_socket,
                        OX_DESK_SYNTH_TRACE=str(self.trace), OX_DESK_SYNTH_THREADS=str(threads),
                        OX_DESK_SYNTH_ITEMS=str(items))
        if sys.platform == "darwin":
            # Keep the real WindowServer login session while isolating all desktop and agent state.
            self.env.update(HOME=os.environ["HOME"], XDG_DATA_HOME=str(stage / "data"),
                            CLAUDE_CONFIG_DIR=str(stage / "claude"))
            self.env.pop("XDG_SESSION_TYPE", None)
        if restore:
            for key in ["HOME", "CODEX_HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME"]:
                self.env[key] = previous[key]
        if routes:
            self.env["OX_DESK_SYNTH_ROUTES"] = "1"
            # The synthetic settings report a saved subscription. Give the
            # desktop's first-launch probe the matching local sign-in state.
            profile = Path(self.env["CODEX_HOME"])
            profile.mkdir(parents=True, exist_ok=True)
            (profile / "auth.json").write_text('{"auth_mode":"chatgpt"}')
        if args.wayland:
            self.env.pop("DISPLAY", None)
            self.env.update(WAYLAND_DISPLAY=os.path.join(os.environ["XDG_RUNTIME_DIR"], os.environ["WAYLAND_DISPLAY"]),
                            XDG_SESSION_TYPE="wayland")
        if real_settings:
            config = stage / ".codex"
            config.mkdir(mode=0o700)
            (config / "codeox.toml").write_text('[providers.probe]\nbase_url = "https://example.invalid/v1"\n'
                    'env_key = "OX_FIXTURE_KEY"\ndiscovery = false\n[[providers.probe.models]]\nid = "demo"\n')
        if remote:
            self.remote_setup(stage)
        if claude:
            self.claude_setup(stage)
        cmd = [self.app, "--desk-stats"]
        if smoke:
            cmd.append("--desk-exit-after-first-frame")
        start = time.monotonic_ns()
        self.host = session_host(self.host_binary, stage, self.env, self.log)
        self.host_process = self.host.__enter__()
        self.proc = subprocess.Popen([*cmd, self.project], env=self.env, stdout=subprocess.PIPE,
                                     stderr=self.log, text=True)
        if smoke:
            import select
            if not select.select([self.proc.stdout], [], [], 10)[0]:
                raise TimeoutError("No first frame")
            line = self.proc.stdout.readline()
            elapsed = (time.monotonic_ns() - start) / 1e6
            if not line.startswith("desk-first-frame ms=") or self.proc.wait(timeout=5):
                raise RuntimeError(f"Invalid first-frame observation {line!r}")
            return elapsed
        if args.wayland:
            def find(node):
                if node.get("pid") == self.proc.pid and node.get("visible"):
                    return node
                return next((match for child in node["nodes"] + node["floating_nodes"]
                             if (match := find(child))), None)
            node = wait(lambda: find(json.loads(self.sway("-t", "get_tree"))))
            self.window = node["id"]
            self.rect = node["rect"]
        elif sys.platform != "darwin":
            self.window = self.x("search", "--sync", "--onlyvisible", "--pid", self.proc.pid,
                             "--name", "OX Desktop").splitlines()[-1]
            self.x("windowfocus", self.window)
        try:
            wait(lambda: Path(self.sock).is_socket() and self.stats()["catalog_len"] > 0)
        except Exception as error:
            # Say why the app never listed its catalog: log, last stats, helper trace, host log.
            self.log.flush()
            print(f"::group::Startup failed: {type(error).__name__}", file=sys.stderr)
            try:
                print("stats:", json.dumps(self.stats())[:2000], file=sys.stderr)
            except Exception as stats_error:
                print(f"stats unavailable: {stats_error!r}", file=sys.stderr)
            print("socket exists:", Path(self.sock).is_socket(), "app exited:", self.proc.poll(), file=sys.stderr)
            for name, path in (("app.log", self.root / "app.log"), ("trace", self.trace)):
                try:
                    print(f"--- {name}\n{path.read_text()[-3000:]}", file=sys.stderr)
                except OSError as read_error:
                    print(f"--- {name}: {read_error}", file=sys.stderr)
            print("::endgroup::", file=sys.stderr)
            raise
        return None

    def wait_for(self, what, fn, timeout=12):
        """wait(), but a timeout first says what was awaited and what the app showed."""
        try:
            return wait(fn, timeout)
        except TimeoutError:
            self.log.flush()
            print(f"::group::Timed out waiting for {what}", file=sys.stderr)
            try:
                view = self.stats()
                layout = [f"{c['id']}={c['label']!r}{'' if c['enabled'] else ' (disabled)'}"
                          for c in view.pop("layout", [])]
                print("stats:", json.dumps(view)[:2000], file=sys.stderr)
                print("controls:", "; ".join(layout)[:3000], file=sys.stderr)
            except Exception as stats_error:
                print(f"stats unavailable: {stats_error!r}", file=sys.stderr)
            try:
                print("--- app.log\n" + (self.root / "app.log").read_text()[-3000:], file=sys.stderr)
            except OSError as read_error:
                print(f"--- app.log: {read_error}", file=sys.stderr)
            print("::endgroup::", file=sys.stderr)
            raise

    def remote_setup(self, stage):
        fixture = stage / "fixture-bin"
        fixture.mkdir()
        cli = fixture / "tailscale"
        cli.write_text("""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
state = Path(os.environ["HOME"]) / "fixture-serve.json"
args = sys.argv[1:]
node = "workstation.fixture.ts.net"
authority = node + ":8443"
if args == ["status","--json","--peers=false"]:
    logged_in = not state.with_name("fixture-logged-out").exists()
    print(json.dumps({"BackendState":"Running" if logged_in else "NeedsLogin", "Self":{"DNSName":node+".", "UserID":44},
                      "User":{"44":{"LoginName":"fixture@example.test"}}}))
elif args == ["serve","status","--json"]:
    print(state.read_text() if state.exists() else "{}")
elif args == ["serve","--bg","--https=8443","http://127.0.0.1:48162"]:
    state.write_text(json.dumps({"TCP":{"8443":{"HTTPS":True}},
        "Web":{authority:{"Handlers":{"/":{"Proxy":args[-1]}}}}}))
elif args == ["serve","--https=8443","off"]:
    state.unlink()
else:
    raise SystemExit("Unexpected Tailscale CLI operation in private fixture")
""")
        cli.chmod(0o700)
        terminal = fixture / "xdg-terminal-exec"
        terminal.write_text('#!/bin/sh\nexec "$@"\n')
        terminal.chmod(0o700)
        # Only a synthetic interactive CLI receives the setup prompt.
        for name in ("codex", "claude"):
            guide = fixture / name
            guide.write_text("""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
if len(sys.argv) == 2 and "Tailscale" in sys.argv[1]:
    (Path(os.environ["HOME"])/"fixture-guidance.json").write_text(json.dumps({
        "program":Path(sys.argv[0]).name, "project":os.getcwd(), "message":sys.argv[1]}))
""")
            guide.chmod(0o700)
        self.env["PATH"] = str(fixture) + os.pathsep + self.env["PATH"]

    def claude_setup(self, stage):
        import secrets

        real_ox = (args.ox or gui.with_name("ox")).resolve(strict=True)
        # Run the packaged OX helper and its real relay. Only the stock process
        # is substituted with the stream-json peer for repeatable timing.
        (self.root / "ox").write_text(
            "#!/bin/sh\nexec " + shlex.quote(str(real_ox)) + ' "$@"\n'
        )
        (self.root / "ox").chmod(0o755)
        binary = stage / "bin"
        binary.mkdir()
        # Count every invocation, including short-lived auth/version checks that
        # would be invisible to process sampling and the conversation trace.
        self.claude_launches = stage / "claude-launches"
        self.claude_launches.touch()
        launcher = binary / "claude"
        peer = probe.with_name("fake-claude").resolve(strict=True)
        launcher.write_text(
            "#!/bin/sh\n"
            + "echo launch >> " + shlex.quote(str(self.claude_launches)) + "\n"
            + "exec " + shlex.quote(str(peer)) + ' "$@"\n'
        )
        launcher.chmod(0o755)
        self.canary = "ox_gate_secret_" + secrets.token_hex(16)
        self.env.update(
            PATH=str(binary) + ":/usr/bin:/bin",
            CLAUDE_CONFIG_DIR=str(stage / "native"),
            OX_CLAUDE_GATE_KEY=self.canary,
        )
        home = stage / ".codex"
        home.mkdir()
        (home / "codeox.toml").write_text(
            '[providers.probe]\nname="Probe"\nbase_url="http://127.0.0.1:1/v1"\n'
            'env_key="OX_CLAUDE_GATE_KEY"\nwire_api="anthropic_messages"\ndiscovery=false\n'
            '[[providers.probe.models]]\nid="demo"\ncontext_window=131072\n'
        )
        state = stage / "state/ox/desktop"
        state.mkdir(parents=True)
        pane = {
            "project": str(self.project),
            "runtime": "claude",
            "api_model": "probe/demo",
        }
        (state / "layout.json").write_text(
            json.dumps({"panes": [pane], "width": 1130, "height": 760})
        )
        key = "".join(
            c if c.isascii() and c.isalnum() else "-" for c in str(self.project)
        )
        project = home / "ox-clients/claude/projects" / key
        project.mkdir(parents=True)
        self.claude_ids = []
        for i, name in enumerate(("Mercury", "Venus", "Saturn", "Neptune"), 1):
            ident = f"00000000-0000-4000-8000-{i:012d}"
            self.claude_ids.append(ident)
            rows = [
                {
                    "type": "user",
                    "uuid": f"11111111-0000-4000-8000-{i:012d}",
                    "sessionId": ident,
                    "cwd": str(self.project),
                    "message": {"role": "user", "content": name},
                },
                {
                    "type": "assistant",
                    "uuid": f"22222222-0000-4000-8000-{i:012d}",
                    "sessionId": ident,
                    "cwd": str(self.project),
                    "requestedModel": "probe/demo",
                    "message": {
                        "id": f"msg_{i}",
                        "role": "assistant",
                        "model": "demo",
                        "content": [{"type": "text", "text": "Fixture ready."}],
                    },
                },
            ]
            journal = project / (ident + ".jsonl")
            journal.write_text("".join(json.dumps(r) + "\n" for r in rows))
            # Saved sessions open read/write. Start them in an earlier date group so
            # the sidebar exercises the same reorder regardless of the runner's clock.
            old = time.time() - 24 * 3600
            os.utime(journal, (old, old))

    def stats(self, reset=False):
        self.client.sendto(b"reset" if reset else b"snapshot", self.sock)
        return json.loads(self.client.recv(1 << 20))

    def control(self, label):
        """A laid-out, enabled control by id or label, or None. Icon buttons carry no word to OCR."""
        return next((c for c in self.stats().get("layout", [])
                     if label in (c["id"], c["label"]) and c["enabled"]), None)

    def search_pos(self):
        """Centre of the sidebar's thread search field in window pixels, from the layout probe."""
        c = self.control("thread-search")
        return (c["x"] + c["w"] // 2, c["y"] + c["h"] // 2) if c else None

    def press(self, label):
        """Click a marked control inside the app (no OS pointer), so it works on X11 and Wayland."""
        self.client.sendto(b"press " + label.encode(), self.sock)
        reply = json.loads(self.client.recv(1 << 20))
        if "error" in reply:
            raise RuntimeError(f"press {label!r}: {reply['error']}")

    def agent(self, query, control="billing-route"):
        """Switch the active window's route with the requested model picker: open it and press the
        model row whose probe label (display name · id) names `query`. Picker ids are namespaced:
        an API alias is "@api:<alias>" and a subscription plan "@plan:<name>", and a runtime's own
        model can share a bare id with an API alias, so an API query prefers the "@api:" row and
        a plan query the "@plan:" row; a runtime-model row matches only when neither exists."""
        self.press(control)
        needle = query.casefold()
        preferred = (f"@api:{needle}", f"@plan:{needle}")

        def row():
            rows = [
                (c["id"], c["label"].casefold())
                for c in self.stats().get("layout", [])
                if c["id"].startswith("model-row") and c["enabled"]
            ]
            for want in preferred:
                for ident, label in rows:
                    if want in label:
                        return ident
            return None

        self.press(self.wait_for(f"a model picker row matching {query!r}", row))

    def sample(self):
        desktop = json.loads(subprocess.check_output([probe, str(self.proc.pid)]))
        daemon = json.loads(subprocess.check_output([probe, str(self.host_process.pid)]))
        desktop["host"] = daemon["app"]
        desktop["children"] += daemon["children"]
        return desktop

    def stop_app(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            self.proc.wait(timeout=8)
        if self.proc and self.proc.stdout:
            self.proc.stdout.close()
        self.proc = None

    def stop(self):
        self.stop_app()
        if self.host is not None:
            self.host.__exit__(None, None, None)
            self.host = self.host_process = None

    def geometry(self):
        def find(node):
            if node["id"] == self.window:
                return node["rect"]
            return next((rect for child in node["nodes"] + node["floating_nodes"] if (rect := find(child))), None)
        return find(json.loads(self.sway("-t", "get_tree")))

    def point(self, text):
        self.pointer.stdin.write(text + "\n")
        self.pointer.stdin.flush()
        if self.pointer.stdout.readline().strip() != "ok":
            raise RuntimeError("Wayland virtual pointer disconnected")

    def sway(self, *argv):
        return subprocess.check_output(["swaymsg", "-r", *argv], timeout=6, text=True)

    def x(self, *argv):
        if args.wayland:
            rect = self.rect
            if argv[0] == "mousemove":
                self.point(f"move {rect['x'] + argv[3]} {rect['y'] + argv[4]} {self.output['width']} {self.output['height']}")
                time.sleep(.035)
                if len(argv) > 5:
                    self.x(*argv[5:])
            elif argv[0] == "click":
                button = int(argv[-1])
                repeat = int(argv[2]) if argv[1] == "--repeat" else 1
                for _ in range(repeat):
                    if button == 1:
                        self.point("press")
                        time.sleep(.05)
                        self.point("release")
                    else:
                        self.point({5: "down"}[button])
                    if repeat > 1:
                        time.sleep(.005)
            elif argv[0] == "key":
                if argv[1] in ("ctrl+a", "ctrl+l", "ctrl+Return"):
                    subprocess.run(["wtype", "-s", "120", "-M", "ctrl", "-k", argv[1].split("+")[1], "-m", "ctrl"], check=True)
                elif argv[1] in ("Up", "Down", "Home", "End", "Return"):
                    subprocess.run(["wtype", "-s", "120", "-k", argv[1]], check=True)
                else:
                    raise ValueError(argv)
            elif argv[0] == "type":
                subprocess.run(["wtype", "-s", "120", "-d", "10", "--", argv[1]], check=True)
            elif argv[0] in ("windowunmap", "windowmap"):
                self.sway("workspace " + ("2" if argv[0] == "windowunmap" else "1"))
            elif argv[0] == "getwindowgeometry":
                return f"HEIGHT={rect['height']}\nWIDTH={rect['width']}"
            else:
                raise ValueError(argv)
            return ""
        return subprocess.check_output(["xdotool", *map(str, argv)], text=True, timeout=12)

    def click(self, pos):
        self.x("mousemove", "--window", self.window, *pos, "click", 1)

    def words(self, scale=1):
        shot = str(self.root / "view.png")
        if args.wayland:
            self.rect = self.geometry()
            r = self.rect
            subprocess.run(["grim", "-g", f"{r['x']},{r['y']} {r['width']}x{r['height']}", shot],
                           check=True, timeout=5)
        else:
            subprocess.run(["import", "-window", self.window, shot], check=True, timeout=5)
        if scale != 1:
            scaled = str(self.root / "ocr.png")
            resize = shutil.which("magick") or "convert"
            subprocess.run([resize, shot, "-resize", f"{100*scale}%", scaled], check=True, timeout=5)
            shot = scaled
        data = subprocess.check_output(["tesseract", shot, "stdout", "--psm", "11", "tsv"],
                                       stderr=subprocess.DEVNULL, text=True, timeout=5)
        words = {}
        for line in data.splitlines()[1:]:
            p = line.split("\t")
            if len(p) == 12 and p[-1].strip():
                words[p[-1].lower().strip(" .…")] = ((int(p[6]) + int(p[8])//2)//scale, (int(p[7])+int(p[9])//2)//scale)
        return words

    def events(self, name):
        if not self.trace.exists():
            return []
        return [d for line in self.trace.read_text().splitlines()
                if (d := json.loads(line))["event"] == name]

    def admin(self, instruction):
        self.client.sendto(instruction.encode(), self.control_socket)


def idle_footprint(before, after, seconds):
    previous = {(r["pid"], r["start_ticks"]): r["cpu_ticks"] for r in before["children"]}
    child_ticks = sum(r["cpu_ticks"] - previous.get((r["pid"], r["start_ticks"]), 0)
                      for r in after["children"])
    percent_per_tick = 100 / after["clock_ticks_per_second"] / seconds
    return {
        "gui_cpu_percent": (after["app"]["cpu_ticks"] - before["app"]["cpu_ticks"]) * percent_per_tick,
        "host_cpu_percent": (after["host"]["cpu_ticks"] - before["host"]["cpu_ticks"]) * percent_per_tick,
        "child_cpu_percent": child_ticks * percent_per_tick,
        "child_rss_kib": sum(r["rss_kib"] for r in after["children"]),
        "gui_and_host_rss_kib": after["app"]["rss_kib"] + after["host"]["rss_kib"],
        "gui_and_host_pss_kib": after["app"]["pss_kib"] + after["host"]["pss_kib"],
    }


def idle(f):
    # Allow the startup thread pool to park after the first visible catalog.
    time.sleep(1)
    start_switches = f.stats()["voluntary_switches"]
    before = f.sample()
    begin = time.monotonic()
    time.sleep(60)
    after = f.sample()
    seconds = time.monotonic() - begin
    a, b = before["app"], after["app"]
    gui_ticks = b["cpu_ticks"] - a["cpu_ticks"]
    host_ticks = after["host"]["cpu_ticks"] - before["host"]["cpu_ticks"]
    footprint = idle_footprint(before, after, seconds)
    cpu = footprint["gui_cpu_percent"] + footprint["host_cpu_percent"]
    wakes = (f.stats()["voluntary_switches"] - start_switches) / seconds
    if "G2" in selected:
        record("G2", cpu <= limits["idle_cpu_percent"], cpu_percent=cpu, seconds=seconds, app=b, host=after["host"], gui_cpu_ticks=gui_ticks, host_cpu_ticks=host_ticks, children=after["children"], **footprint)
    if "G3" in selected:
        record("G3", None, voluntary_switches_per_second=wakes, target=limits["visible_context_switches_per_s_target"])


def memory(f):
    positions = wait(lambda: words if "0000" in (words := f.words()) else None)
    first = positions["0000"]
    search = f.wait_for("the thread search field", f.search_pos)
    for i in range(30):
        f.click(search)
        f.x("key", "ctrl+a")
        f.x("type", f"{i:04}")
        wait(lambda: any(e["id"] == f"{i:04}" for e in f.events("query")))
        wait(lambda: f.stats()["catalog_len"] == 1)
        # Search replaces the sole result in place as each response arrives.
        def opened():
            if any(e["id"] == f"desk-{i:04}" for e in f.events("history-reply")):
                return True
            f.click(first)
            return False
        wait(opened)
    usage = f.sample()
    rss = usage["app"]["rss_kib"] + usage["host"]["rss_kib"]
    record("G5", rss <= limits["gui_rss_mib"] * 1024, opened_threads=30, rss_bytes=rss * 1024,
           pss_bytes=(usage["app"]["pss_kib"] + usage["host"]["pss_kib"]) * 1024, app=usage["app"], host=usage["host"], children=usage["children"])


def scroll_catalog(f):
    positions = wait(lambda: words if "0000" in (words := f.words()) else None)
    f.x("mousemove", "--window", f.window, *positions["0000"])
    f.stats(reset=True)
    # Scroll to the end within the original 150-second budget, and record how
    # long it took. It takes under 30 seconds since the host sends a new page
    # as its rows rather than the whole list (store/projection.rs
    # session_changes); before that the desktop fell behind on a slow runner,
    # reloaded the catalog from its first page and paged all 5,000 threads again.
    started = time.monotonic()
    stats = f.stats()
    trace, traced = [], started
    while stats["furthest_row"] < 5000 and time.monotonic() < started + 150:
        f.x("click", "--repeat", "100", "--delay", "5", "5")
        stats = f.stats()
        # A catalog_len that falls back (issue 9) is a reset to the first page.
        if time.monotonic() - traced >= 0.5:
            traced = time.monotonic()
            trace.append((round(traced - started, 1), stats["furthest_row"], stats["catalog_len"]))
    resets = sum(1 for a, b in zip(trace, trace[1:]) if b[2] < a[2])
    if resets:
        print(f"G6 trace (s, furthest_row, catalog_len), {resets} reset(s): {trace}", file=sys.stderr)
    seconds = round(time.monotonic() - started, 1)
    # Count only the GPUI visible range and its overscan. Window height gives an
    # independent upper bound since every button alone consumes 32 logical pixels.
    size = f.x("getwindowgeometry", "--shell", f.window)
    height = int(next(s[7:] for s in size.splitlines() if s.startswith("HEIGHT=")))
    budget = (height + 31) // 32 + 2
    record("G6", stats["furthest_row"] == 5000 and stats["max_list_range"] <= budget,
           reached=stats["furthest_row"], catalog_len=stats["catalog_len"],
           maximum_rows_in_layout=stats["max_list_range"], row_budget=budget,
           draw_p95_ms=stats["draw_p95_ns"]/1e6, scroll_seconds=seconds)


def hidden(f):
    f.x("windowunmap", f.window)
    if args.wayland:
        wait(lambda: any(w["name"] == "2" and w["focused"] for w in json.loads(f.sway("-t", "get_workspaces"))))
    else:
        wait(lambda: not f.stats()["visible"])
    before = f.stats()
    host_before = f.sample()["host"]
    time.sleep(60)
    host_after = f.sample()["host"]
    after = f.stats()
    wakes = after["voluntary_switches"] - before["voluntary_switches"]
    draws = after["frames_drawn"] - before["frames_drawn"]
    record("G4", draws == limits["hidden_frames"], hidden_frames=draws, voluntary_switches=wakes, seconds=60,
           gpui_reported_visible=after["visible"], host_cpu_ticks=host_after["cpu_ticks"] - host_before["cpu_ticks"],
           host_voluntary_switches=host_after["voluntary_context_switches"] - host_before["voluntary_context_switches"],
           host_rss_kib=host_after["rss_kib"])
    f.x("windowmap", f.window)
    wait(lambda: f.stats()["visible"])


def streaming(f):
    positions = wait(lambda: words if "0000" in (words := f.words()) else None)
    def thread_pos(i):
        # Each uniform thread row has the same height. OCR can read a run of zeroes as letters.
        if f"{i:04}" in positions:
            return positions[f"{i:04}"]
        # Fall back to the list's observed row centers, ordered vertically.
        raise ValueError(f"Thread {i:04} missing from window: {list(positions)}")

    before = f.stats()
    def select_first():
        if f.events("history-reply"):
            return True
        f.click(thread_pos(0))
        return False
    wait(select_first)
    wait(lambda: f.stats()["tail_ns"] > 0)
    after = f.stats()
    if "G8" in selected:
        ms = (after["tail_ns"] - f.events("history-reply")[-1]["ns"]) / 1e6
        budget = after["uniform_row_budget"] or 0
        record("G8", timed(0 <= ms and 0 < after["measured_items"] <= budget, ms <= limits["history_tail_ms"], "G8"),
               timing_ok=ms <= limits["history_tail_ms"], reply_to_tail_paint_ms=ms,
               distinct_history_rows=after["measured_items"], viewport_with_overscan=budget,
               rows_built=after["rows"]-before["rows"], history_items=2000)
    if "G7" in selected:
        for i in range(1, 4):
            f.click(thread_pos(i))
            wait(lambda: len(f.events("history-reply")) == i+1)
        start = f.stats(reset=True)
        f.admin("stream")
        wait(lambda: f.events("stream-start"))
        f.press("composer")
        f.x("type", "draft during streaming")
        wait(lambda: f.events("stream-end"), 18)
        # The final stream event has crossed the UI pump when the turn controls return to idle.
        wait(lambda: not f.stats()["visible_turn"])
        end = f.stats()
        # Typing reached the composer while the streams rendered. Read from the
        # app's state: OCR glued the text cursor to the last word ("streaming|").
        typed = end.get("draft") == "draft during streaming"
        cohort = int(f.events("stream-start")[-1]["id"])
        record("G7", timed(cohort == 4 and typed and end["frames_drawn"] > 0
                           and end["drains"] - start["drains"] <= end["frames_drawn"],
                           end["draw_p95_ns"] <= limits["stream_frame_p95_ms"]*1e6, "G7"),
               timing_ok=end["draw_p95_ns"] <= limits["stream_frame_p95_ms"]*1e6,
               simultaneous_threads=cohort, message_bytes=200000, deltas_per_second_per_thread=200,
               draw_p95_ms=end["draw_p95_ns"]/1e6, dirty_to_present_p95_ms=end["dirty_to_present_p95_ns"]/1e6,
               present_interval_p95_ms=end["present_interval_p95_ns"]/1e6,
               drains=end["drains"]-start["drains"], frames=end["frames_drawn"], typed_draft_visible=typed)
    if "G10" in selected:
        # With text in the composer the submit slot is Send; it turns into Stop only for an
        # empty draft. G7 leaves its typed draft behind, so clear it first.
        if f.stats().get("draft"):
            f.press("composer")
            f.x("key", "ctrl+a")
            f.x("key", "BackSpace")
            f.wait_for("an empty composer draft", lambda: not f.stats().get("draft"))
        f.admin("busy")
        f.wait_for("the busy turn to show", lambda: f.stats()["visible_turn"])
        f.wait_for("the Stop control (shown only while a turn runs and the draft is empty)",
                   lambda: f.control("stop-turn"))
        search = f.wait_for("the thread search field", f.search_pos)
        f.click(search)
        f.x("type", "hold")
        wait(lambda: f.events("list-held"))
        f.press("stop-turn")
        end = wait(lambda: f.events("stop-read"))
        elapsed = (end[-1]["ns"] - f.stats()["stop_ns"])/1e6
        # Stop must reach the runtime promptly; 250 ms catches a hang, the frame
        # budget is the timing target.
        on_time = elapsed <= limits["stop_latency_frames"]*1000/60
        record("G10", timed(0 <= elapsed <= 250, on_time, "G10"), timing_ok=on_time,
               control_to_pipe_ms=elapsed, frame_budget_ms=1000/60)


def navigate(f):
    first = wait(lambda: f.words().get("0000"))
    f.click(first)
    wait(lambda: f.stats()["selected_thread"] == "desk-0000")
    for key, target in [("Down", 1), ("End", 29), ("Up", 28), ("Home", 0)]:
        f.x("key", key)
        wait(lambda: f.stats()["selected_thread"] == f"desk-{target:04}")
    record("navigation", True, threads=30, keys=["Down", "End", "Up", "Home"])


def recovery(f):
    pos = wait(lambda: f.words().get("0000"))
    f.click(pos)
    wait(lambda: f.events("history-reply") and f.stats()["tail_ns"] > 0)
    f.press("composer")
    f.x("type", "preserve this draft")
    wait(lambda: "preserve" in f.words())
    def disconnect():
        pid = f.events("spawn")[-1]["pid"]
        Path(f.control_socket).unlink()
        os.kill(pid, signal.SIGKILL)
    disconnect()
    wait(lambda: len(f.events("spawn")) == 2 and len(f.events("history-reply")) == 2)
    wait(lambda: "preserve" in f.words())
    disconnect()
    wait(lambda: not f.stats()["connected"])
    # The recovery screen words its message ("The session host stopped"), so wait for the
    # Reconnect button itself rather than any one word.
    wait(lambda: f.control("retry-connection"))
    time.sleep(.5)
    automatic = len(f.events("spawn")) - 1
    f.press("retry-connection")
    wait(lambda: len(f.events("spawn")) == 3 and len(f.events("history-reply")) == 3)
    survived = "preserve" in f.words()
    record("reconnect", automatic == 1 and survived and not f.events("send"), automatic_restarts=automatic,
           manual_restarts=1, draft_restored=survived, replayed_sends=len(f.events("send")))


def reuse(f):
    words = wait(lambda: w if "0000" in (w := f.words()) else None)
    first = words["0000"]
    wait(lambda: f.stats()["discovery_pending"] == 0 and len(f.events("spawn")) == 1)
    before_open = len(f.events("spawn")) + len(f.events("helper"))
    f.agent("fixture-model")
    f.wait_for(
        "the fixture-model route pane to open and connect",
        lambda: f.stats()["pane"] == 1 and f.stats()["connected"] and len(f.events("spawn")) == 2,
    )
    cold_route_spawns = len(f.events("spawn")) + len(f.events("helper")) - before_open
    f.agent("codex")
    f.wait_for("the codex pane to be active again", lambda: f.stats()["pane"] == 0)
    base = len(f.events("spawn")) + len(f.events("helper"))
    for i in range(20):
        f.click(first)
        f.press("new-session")
        f.wait_for("composer", lambda: f.control("composer"))
        f.press("composer")
        f.x("type", f"Create thread {i}")
        f.x("key", "ctrl+Return")
        wait(lambda: len(f.events("send")) == i + 1)
    created = len(f.events("new-thread"))
    before_route = len(f.events("spawn")) + len(f.events("helper"))
    first_route_spawns = 0
    for i in range(20):
        target = 1 - i % 2
        f.agent("fixture-model" if target else "codex")
        f.wait_for(f"pane {target} to be active after route change {i} of 20",
                   lambda: f.stats()["pane"] == target)
        if i == 0:
            f.wait_for("the first route change to reuse both servers", lambda: len(f.events("spawn")) == 2)
            first_route_spawns = len(f.events("spawn")) + len(f.events("helper")) - before_route
    servers = f.events("spawn")
    total = len(servers) + len(f.events("helper"))
    # Both pickers must also offer a model reported only by provider discovery.
    # This third route opens after the process-reuse measurement.
    f.agent("fixture-discovered")
    f.wait_for("the discovered-model pane to open and connect",
               lambda: f.stats()["pane"] == 2 and f.stats()["connected"])
    f.agent("codex")
    f.wait_for("the codex pane to be active after the discovered model", lambda: f.stats()["pane"] == 0)
    f.agent("fixture-discovered", "model-setting-0")
    f.wait_for("the discovered-model pane to be active from the settings picker", lambda: f.stats()["pane"] == 2)
    record("G9", created == 20 and base == before_route and cold_route_spawns == 2 and first_route_spawns == 0 and
           total == before_route + first_route_spawns and len(servers) == 2,
           new_threads=created, thread_selections=20, route_changes=20, cold_route_spawns=cold_route_spawns,
           new_and_switch_spawns=before_route-base, first_api_route_spawns=first_route_spawns,
           subsequent_route_spawns=total-before_route-first_route_spawns,
           app_servers=[e["id"] for e in servers], discovery_only_route="fixture-discovered")


def saved_layout(f):
    f.start(30, 1, routes=True)
    # A new window creates only its selected route. Use the same billing picker
    # as the route-reuse gate to create both panes before saving their layout.
    f.wait_for("the thread search field", f.search_pos)
    for target in (1, 0):
        f.agent("fixture-model" if target else "codex")
        wait(lambda: f.stats()["pane"] == target and f.stats()["connected"])
    f.click(wait(lambda: f.words().get("0000")))
    selected = wait(lambda: f.stats()["selected_thread"])
    file = Path(f.env["XDG_STATE_HOME"]) / "ox/desktop/layout.json"

    def snapshot():
        return json.loads(file.read_text()) if file.exists() else {}

    wait(lambda: selected in file.read_text() if file.exists() else False)
    previous = snapshot()
    width = previous["sidebar_width"]
    if args.wayland:
        f.x("mousemove", "--window", f.window, width + 2, 375)
        f.point("press")
        f.x("mousemove", "--window", f.window, width + 48, 375)
        f.point("release")
    else:
        f.x("mousemove", "--window", f.window, width + 2, 375, "mousedown", "1")
        f.x("mousemove", "--window", f.window, width + 48, 375, "mouseup", "1")
    updated = wait(lambda: saved if (saved := snapshot())["sidebar_width"] > width + 15 else None)
    panes = len(updated["panes"])
    assert panes >= 2, "Both billing profiles should be open"
    f.stop()
    f.start(30, 1, routes=True, restore=True)
    wait(lambda: f.stats()["selected_thread"] == selected)
    restored = snapshot()
    assert restored["sidebar_width"] == updated["sidebar_width"]
    assert len(restored["panes"]) == panes
    first_project = f.project
    f.stop()
    f.project = f.root / "other-project"
    f.project.mkdir()
    f.start(30, 1, routes=True, restore=True)
    assert f.stats()["selected_thread"] is None
    f.click(wait(lambda: f.words().get("0001")))
    other_selected = wait(lambda: f.stats()["selected_thread"])
    wait(lambda: any(p["project"] == str(f.project) and p.get("selection") and
                     p["selection"]["id"] == other_selected for p in snapshot()["panes"]))
    other_project = f.project
    for project, thread in [(first_project, selected), (other_project, other_selected)]:
        f.stop()
        f.project = project
        f.start(30, 1, routes=True, restore=True)
        wait(lambda: f.stats()["selected_thread"] == thread)
        if project == first_project:
            wait(lambda: snapshot()["sidebar_width"] == updated["sidebar_width"])
    record("layout", True, sidebar_width=restored["sidebar_width"], open_panes=panes,
           selected_thread_restored=True, independent_projects_restored=True)


RESTORE_MARKS = ("first_frame_callback", "composer_ready", "transcript_visible", "thread_live")


def restored_launch(f, env, logfile):
    started = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
    with logfile.open("w") as output:
        # No project argument and no stats flag. Exercise the normal startup
        # path from saved state. The child timestamps the milestones itself.
        f.proc = subprocess.Popen([f.app], cwd=f.project, env=env, stdout=output, stderr=f.log)

        def milestones():
            marks = {}
            for line in logfile.read_text().splitlines(keepends=True):
                if line.startswith("desk-restore ") and line.endswith("\n"):
                    _, key, ns = line.split()
                    marks[key] = int(ns)
            if all(key in marks for key in RESTORE_MARKS):
                assert all(marks[key] >= started for key in RESTORE_MARKS), marks
                return {key: (marks[key] - started) / 1e6 for key in RESTORE_MARKS}
            if f.proc.poll() is not None:
                raise RuntimeError(f"Desktop exited before restored-thread milestones: {marks}")
            return None

        try:
            return wait(milestones, timeout=20)
        except TimeoutError as error:
            raise RuntimeError(f"Missing restored-thread milestones:\n{logfile.read_text()}") from error
        finally:
            f.stop_app()


def restored_thread(f):
    target = "desk-0000"
    cases = {}
    for size, threads, items in (("small", 30, 20), ("large", 5000, 2000)):
        f.start(threads, items, routes=True)
        f.wait_for("the seed thread to appear in the sidebar", lambda: f.control("Thread 0000"))
        f.press("Thread 0000")
        f.wait_for("the seeded history", lambda: f.stats()["selected_thread"] == target
                   and f.stats()["tail_ns"] > 0)
        layout = Path(f.env["XDG_STATE_HOME"]) / "ox/desktop/layout.json"
        f.wait_for("the selected thread to be saved", lambda: layout.exists() and target in layout.read_text())

        f.stop_app()
        env = {**f.env, "OX_DESK_RESTORE_PROBE_THREAD": target}
        env.pop("OX_DESK_STATS_SOCKET")
        if sys.platform == "darwin":
            # Keep the stopped-host launch independent of the logged-in user's
            # LaunchAgent while retaining the fixture's saved workspace.
            env["HOME"] = env["XDG_DATA_HOME"]
            Path(env["HOME"]).mkdir(mode=0o700, exist_ok=True)

        for mode in ("warm", "stopped"):
            if mode == "stopped":
                f.stop()
                env["OX_HOST_SOCKET"] = str(Path(f.env["OX_DESK_SYNTH_TRACE"]).with_name("restore-host.sock"))
            samples = []
            logfile = f.root / f"restore-{size}-{mode}.log"
            for sample in range(args.restore_samples):
                if mode == "warm" and f.host_process.poll() is not None:
                    raise RuntimeError("The warm fixture host exited before a measured launch")
                if mode == "stopped":
                    # The synthetic runtime's control socket is a fixture-only
                    # path. Its previous process may leave the pathname on exit.
                    env["OX_DESK_SYNTH_CONTROL"] = str(f.root / f"restore-{size}-{sample}.sock")
                try:
                    samples.append(restored_launch(f, env, logfile))
                finally:
                    if mode == "stopped":
                        subprocess.run([f.host_binary, "stop"], env=env, check=True,
                                       stdout=f.log, stderr=f.log, timeout=18)
            if mode == "warm" and f.host_process.poll() is not None:
                raise RuntimeError("The warm fixture host exited during the measured launches")
            def percentile90(values):
                return sorted(values)[(9 * len(values) + 9) // 10 - 1]
            cases[f"{size}_{mode}"] = {
                "catalog_threads": threads,
                "history_items": items,
                "samples_ms": samples,
                "median_ms": {key: statistics.median(s[key] for s in samples) for key in RESTORE_MARKS},
                "p90_ms": {key: percentile90([s[key] for s in samples]) for key in RESTORE_MARKS},
            }
    record("restore-thread", True, filesystem_cache="seeded profile, unmanaged page cache",
           machine=f"{os.uname().machine} {os.uname().sysname} {os.uname().release}",
           cpu_count=os.cpu_count(), fixture_sha256=sha256(fake), cases=cases)


def remote_access(f):
    # The GUI talks to the real packaged host and a private synthetic Serve CLI.
    # Every domain and identity here is local test data.
    f.press("settings")
    f.click(wait(lambda: f.words().get("remote")))
    f.click(wait(lambda: f.words().get("enable")))
    mapping = Path(f.env["HOME"]) / "fixture-serve.json"
    wait(mapping.exists)
    f.click(wait(lambda: f.words().get("pair")))
    wait(lambda: "scan" in f.words())
    f.click(wait(lambda: f.words().get("disable")))
    wait(lambda: not mapping.exists())
    wait(lambda: "enable" in f.words())
    (mapping.parent / "fixture-logged-out").touch()
    f.click(wait(lambda: f.words().get("refresh")))
    f.click(wait(lambda: f.words().get("ask")))
    capture = mapping.parent / "fixture-guidance.json"
    wait(capture.exists)
    guide = json.loads(capture.read_text())
    assert guide["program"] in ("codex", "claude") and Path(guide["project"]) == f.project
    assert "Tailscale Serve" in guide["message"] and "before installing" in guide["message"]
    record("remote-access", True, enabled_private_https=True, paired_qr_visible=True,
           removed_fixture_mapping=True, guided_stock_cli=guide["program"])


def secure_key(f):
    import secrets
    since = int(time.time())
    f.press("settings")
    f.press("API keys")
    words = wait(lambda: w if "enter" in (w := f.words()) else None)
    f.click(words["enter"])
    canary = ("ox-probe-" + secrets.token_hex(16)).encode()
    command = ["wtype", "-s", "120", "-"] if args.wayland else ["xdotool", "type", "--file", "-"]
    subprocess.run(command, input=canary, check=True)
    f.press("key-save-0")
    keyfile = Path(f.env["CODEX_HOME"]) / "ox-api-keys.json"
    wait(lambda: keyfile.exists() and canary in keyfile.read_bytes())
    f.stop()  # Snapshot files after the desktop stops replacing its state atomically.
    hits = []
    for p in f.root.rglob("*"):
        if p.is_file() and p not in (keyfile, f.app) and canary in p.read_bytes():
            hits.append(str(p.relative_to(f.root)))
    journal = subprocess.run(["journalctl", "--user", "--since", f"@{since}", "--no-pager", "-o", "cat"],
                             capture_output=True, check=True)
    if canary in journal.stdout or canary in journal.stderr:
        hits.append("user journal")
    record("G12", len(hits) == limits["credential_leaks"], outside_store_hits=hits,
           keyfile_mode=oct(keyfile.stat().st_mode & 0o777), saved=True,
           journal_output_bytes=len(journal.stdout))


def mac_processes(pid):
    # The macOS ps resident set is KiB. TIME is accumulated user and system CPU time.
    text = subprocess.check_output(["ps", "-ax", "-o", "pid=,ppid=,rss=,time="], text=True)
    rows = {}
    for line in text.splitlines():
        fields = line.split()
        if len(fields) != 4:
            continue
        process, parent, rss = map(int, fields[:3])
        days, _, clock = fields[3].rpartition("-")
        seconds = 0.
        for component in clock.split(":"):
            seconds = 60 * seconds + float(component)
        rows[process] = (parent, {"pid": process, "rss_kib": rss,
                                  "cpu_seconds": seconds + 86400 * int(days or 0)})
    children = []
    parents = {pid}
    while parents:
        next_pids = {p for p, (parent, _) in rows.items() if parent in parents}
        children.extend(rows[p][1] for p in sorted(next_pids))
        parents = next_pids
    return rows[pid][1], children


def mac_gate(f):
    if sys.platform != "darwin" or not args.mac_key_driver:
        raise ValueError("G14 needs a macOS host and --mac-key-driver")
    times = [f.start(30, 1, smoke=True) for _ in range(10)]
    f.start(30, 1)
    before, _ = mac_processes(f.proc.pid)
    before_host, _ = mac_processes(f.host_process.pid)
    start = time.monotonic()
    time.sleep(60)
    seconds = time.monotonic() - start
    after, idle_children = mac_processes(f.proc.pid)
    after_host, host_children = mac_processes(f.host_process.pid)
    idle_cpu = (after["cpu_seconds"] - before["cpu_seconds"] + after_host["cpu_seconds"] - before_host["cpu_seconds"]) * 100 / seconds
    measurements = dict(cold_start_ms=times, ninth_ms=sorted(times)[8], idle_seconds=seconds,
                        idle_cpu_percent=idle_cpu, idle_host=after_host, idle_children=idle_children + host_children)
    record("G14", None, **measurements, memory={"pending": True})
    def opened():
        return {event["id"] for event in f.events("history-reply")}
    ids = [f"desk-{i:04}" for i in range(30)]
    for i, thread in enumerate(ids):
        if thread in opened():
            continue
        step = subprocess.run([args.mac_key_driver, str(f.proc.pid)], timeout=5)
        if step.returncode == 77:
            record("G14", None, **measurements, memory={"unverified": "Runner lacks permission to post native keyboard events."})
            return
        step.check_returncode()
        try:
            wait(lambda: thread in opened(), timeout=2)
        except TimeoutError:
            subprocess.run([args.mac_key_driver, str(f.proc.pid), f"Thread {i:04}"], timeout=6, check=True)
            try:
                wait(lambda: thread in opened())
            except TimeoutError as error:
                raise RuntimeError(f"Could not open {thread}; selected={f.stats().get('selected_thread')}, "
                                   f"visited={sorted(opened())}, trace={f.trace.read_text()[-1400:]}") from error
    assert set(ids) <= opened()
    app, children = mac_processes(f.proc.pid)
    daemon, runtime_children = mac_processes(f.host_process.pid)
    record("G14", None, **measurements, memory={"opened_threads": len(ids), "app": app, "host": daemon, "children": children + runtime_children})


def claude_gate(f):
    def launches():
        return len(f.claude_launches.read_text().splitlines())

    times = []
    for _ in range(10):
        times.append(f.start(4, 2, smoke=True, claude=True))
        if launches():
            raise RuntimeError("Claude executed during desktop first-frame startup")
    p90 = sorted(times)[8]
    f.start(4, 2, claude=True)
    try:
        wait(lambda: f.stats()["connected"])
    except TimeoutError as error:
        s = f.stats()
        raise RuntimeError(
            f"Claude startup: ready={s['connected']}, catalog={s['catalog_len']}, native launches={launches()}"
        ) from error
    # Match the existing G2 interval after startup workers park.
    time.sleep(1)
    before_idle_frame = f.stats()["frames_drawn"]
    idle_start = f.sample()
    start_time = time.monotonic()
    time.sleep(60)
    idle_end = f.sample()
    idle_frames = f.stats()["frames_drawn"] - before_idle_frame
    seconds = time.monotonic() - start_time
    footprint = idle_footprint(idle_start, idle_end, seconds)
    cpu = footprint["gui_cpu_percent"] + footprint["host_cpu_percent"]
    idle_launches = launches()
    if idle_launches:
        raise RuntimeError(f"Claude executed {idle_launches} time(s) before the first user action")
    for sent, ident in enumerate(f.claude_ids):
        thread = "claude:api:" + ident
        # Sending moves the previous row into today's group. Resolve the intended
        # sidebar control at click time instead of clicking old screenshot coordinates.
        row = f"thread-{f.project}-true-{thread}"
        f.wait_for(f"Claude sidebar row {thread}", lambda: f.control(row))
        f.press(row)
        f.wait_for(f"Claude session {thread} to be selected", lambda: f.stats()["selected_thread"] == thread)
        f.x("key", "ctrl+l")
        if launches() != sent:
            raise RuntimeError(f"Opening Claude history started a process: {launches()} launches after {sent} sends")
        f.x("type", "Generate a measured Claude stream")
        f.x("key", "ctrl+Return")
        f.wait_for(
            f"the Claude fixture to receive the user turn for {ident}",
            lambda: any(e["id"] == ident for e in f.events("claude-user")),
        )
    first = f.stats(reset=True)
    for ident in f.claude_ids:
        f.client.sendto(b"stream", str(Path(f.control_socket).with_name("c-" + ident)))
    wait(lambda: len(f.events("claude-stream-start")) == 4)
    f.x("key", "ctrl+l")
    f.x("type", "draft during streaming")
    wait(lambda: len(f.events("claude-stream-end")) == 4, 18)
    wait(lambda: not f.stats()["visible_turn"])
    end = f.stats()
    # Typing reached the composer while the streams rendered. Read from the
    # app's state: OCR glued the text cursor to the last word ("streaming|").
    typed = bool(wait(lambda: f.stats().get("draft") == "draft during streaming"))
    f.x("key", "ctrl+Return")
    wait(lambda: len(f.events("claude-user")) == 5)
    wait(lambda: f.stats()["visible_turn"])
    wait(lambda: f.control("stop-turn"))
    f.press("stop-turn")
    stop = wait(lambda: f.events("claude-stop-read"))[-1]
    elapsed = (stop["ns"] - f.stats()["stop_ns"]) / 1e6
    started = f.events("claude-stream-start")
    ended = f.events("claude-stream-end")
    overlap = len({e["id"] for e in started}) == 4 and max(
        e["ns"] for e in started
    ) < min(e["ns"] for e in ended)
    # All live measurements are complete. Stop the writer before walking the persisted
    # state so a temporary file cannot be renamed between directory enumeration and open.
    f.stop()
    f.log.flush()
    leaks = len(f.events("claude-credential-leak"))
    surface = [f.root / "app.log", f.trace]
    for directory in [
        Path(f.env["XDG_STATE_HOME"]),
        Path(f.env["CODEX_HOME"]) / "ox-clients" / "claude",
    ]:
        surface.extend(p for p in directory.rglob("*") if p.is_file())
    leaks += sum(f.canary.encode() in path.read_bytes() for path in surface)
    budget = limits["stop_latency_frames"] * 1000 / 60
    timing_ok = (
        p90 <= limits["cold_start_p90_ms"]
        and cpu <= limits["idle_cpu_percent"]
        and end["draw_p95_ns"] <= limits["stream_frame_p95_ms"] * 1e6
        and elapsed <= budget
    )
    passed = timed(
        typed
        and overlap
        and 0 < end["drains"] - first["drains"] <= end["frames_drawn"]
        and 0 <= elapsed <= 250
        and leaks == 0
        and launches() == len(f.claude_ids)
        and len({e["id"] for e in f.events("claude-clean-env")}) == len(f.claude_ids),
        timing_ok,
        "claude",
    )
    record(
        "claude",
        passed,
        timing_ok=timing_ok,
        launch_ms=times,
        cold_start_p90_ms=p90,
        idle_cpu_percent=cpu,
        idle_seconds=seconds,
        idle_frames=idle_frames,
        idle_claude_launches=idle_launches,
        total_claude_launches=launches(),
        idle_host=idle_end["host"],
        idle_children=idle_end["children"],
        **footprint,
        simultaneous_threads=4,
        streams_overlap=overlap,
        message_bytes=200000,
        deltas_per_second_per_thread=200,
        draw_p95_ms=end["draw_p95_ns"] / 1e6,
        dirty_to_present_p95_ms=end["dirty_to_present_p95_ns"] / 1e6,
        present_interval_p95_ms=end["present_interval_p95_ns"] / 1e6,
        frames=end["frames_drawn"],
        drains=end["drains"] - first["drains"],
        typed_draft_visible=typed,
        control_to_pipe_ms=elapsed,
        frame_budget_ms=budget,
        credential_leaks=leaks,
    )


def run():
    with tempfile.TemporaryDirectory(prefix="ox-gate-", dir="/tmp" if sys.platform == "darwin" else None) as path:
        f = Fixture(Path(path))
        try:
            if "G14" in selected:
                mac_gate(f)
            if "G1" in selected:
                times = [f.start(30, 1, smoke=True) for _ in range(10)]
                ninth = sorted(times)[8]
                record("G1", ninth <= limits["cold_start_p90_ms"], launch_ms=times, ninth_ms=ninth)
            if selected & {"G2", "G3", "G4"}:
                f.start(30, 1)
                if selected & {"G2", "G3"}:
                    idle(f)
                if "G4" in selected:
                    hidden(f)
                f.stop()
            if "G5" in selected:
                f.start(30, 1)
                memory(f)
                f.stop()
            if "G6" in selected:
                f.start(5000, 1)
                scroll_catalog(f)
                f.stop()
            if selected & {"G7", "G8", "G10"}:
                f.start(5000, 2000)
                streaming(f)
            if "navigation" in selected:
                f.stop()
                f.start(30, 1)
                navigate(f)
            if "reconnect" in selected:
                f.stop()
                f.start(30, 1)
                recovery(f)
            if "G9" in selected:
                f.stop()
                f.start(30, 1, routes=True)
                reuse(f)
            if "layout" in selected:
                f.stop()
                saved_layout(f)
            if "restore-thread" in selected:
                restored_thread(f)
            if "G12" in selected:
                f.stop()
                f.start(30, 1, real_settings=True)
                secure_key(f)
            if "remote-access" in selected:
                f.stop()
                f.start(30, 1, real_settings=True, remote=True)
                remote_access(f)
            if "G11" in selected:
                stripped = f.root / "stripped"
                shutil.copy2(gui, stripped)
                subprocess.run(["strip", str(stripped)], check=True)
                record("G11", None, stripped_bytes=stripped.stat().st_size)
            if "claude" in selected:
                f.stop()
                claude_gate(f)
        except Exception:
            if (f.root / "view.png").exists():
                shutil.copy2(f.root / "view.png", args.out.with_suffix(".png"))
            f.log.flush()
            print((f.root / "app.log").read_text()[-1500:], file=sys.stderr)
            raise
        finally:
            f.stop()
            f.log.close()
            f.client.close()
            if f.pointer:
                f.pointer.stdin.close()
                f.pointer.wait(timeout=3)


run()
missing = selected - set(report["scenarios"])
if missing or any(v["passed"] is False for v in report["scenarios"].values()):
    sys.exit(f"Performance gate incomplete or failed. Missing scenarios: {sorted(missing)}")
