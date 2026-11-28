"""Exercise pairing, actions, reconnect and revocation in an isolated Chromium phone viewport.

Pass the Rust unit test executable produced by cargo test -p ox-host-web --features server --no-run.
The TLS forwarder mimics Serve's identity injection. Uses no real Tailscale account or profile.
"""

import argparse
from contextlib import ExitStack
import http.client
import json
import os
from pathlib import Path
import select
import shutil
import socket
import socketserver
import ssl
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


def wait(fn, seconds=12):
    until = time.monotonic() + seconds
    error = None
    while time.monotonic() < until:
        try:
            result = fn()
            if result:
                return result
        except (OSError, ValueError, KeyError) as exc:
            error = exc
        time.sleep(0.06)
    raise AssertionError(f"Timed out ({type(error).__name__})")


def admin(directory, action):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(4)
        client.connect(str(directory / "remote-admin.sock"))
        client.sendall(json.dumps(action).encode() + b"\n")
        reply = json.loads(client.makefile("rb").readline())
        assert "Ok" in reply, reply.get("Err")
        return reply["Ok"]


class ThreadedTCP(socketserver.ThreadingTCPServer):
    daemon_threads = True


def forward_upgrade(client, upstream):
    """Drive both directions of one TLS socket on the same thread."""
    client.setblocking(False)
    upstream.setblocking(False)
    to_client = b""
    to_upstream = b""
    upstream_open = True
    client_open = True
    read_needs_write = False
    write_needs_read = False
    while upstream_open or to_client:
        read_client = client_open and not to_upstream
        receive_buffered = (read_client and isinstance(client, ssl.SSLSocket)
                            and client.pending() > 0 and not read_needs_write)
        read = []
        write = []
        if (read_client and not read_needs_write) or (to_client and write_needs_read):
            read.append(client)
        if (read_client and read_needs_write) or (to_client and not write_needs_read):
            write.append(client)
        if upstream_open and not to_client:
            read.append(upstream)
        if to_upstream:
            write.append(upstream)
        readable, writable, _ = select.select(read, write, [], 0 if receive_buffered else None)

        if to_client and client in (readable if write_needs_read else writable):
            try:
                sent = client.send(to_client)
                to_client = to_client[sent:]
                write_needs_read = False
            except ssl.SSLWantReadError:
                write_needs_read = True
            except (ssl.SSLWantWriteError, BlockingIOError):
                write_needs_read = False
        if read_client and (receive_buffered or client in (writable if read_needs_write else readable)):
            try:
                to_upstream = client.recv(65536)
                client_open = bool(to_upstream)
                read_needs_write = False
                if not client_open:
                    upstream.shutdown(socket.SHUT_WR)
            except ssl.SSLWantWriteError:
                read_needs_write = True
            except (ssl.SSLWantReadError, BlockingIOError):
                read_needs_write = False
        if upstream in readable:
            try:
                to_client = upstream.recv(65536)
                upstream_open = bool(to_client)
            except BlockingIOError:
                pass
        if upstream in writable:
            try:
                to_upstream = to_upstream[upstream.send(to_upstream):]
            except BlockingIOError:
                pass


class Forward(socketserver.StreamRequestHandler):
    """TLS-to-loopback proxy, accepting no client-supplied identity."""

    def handle(self):
        headers = []
        first = self.rfile.readline(8192)
        if not first:
            return
        length = 0
        upgrade = False
        while line := self.rfile.readline(8192):
            if line == b"\r\n":
                break
            name, value = line.decode("latin1").split(":", 1)
            if name.lower() == "content-length":
                length = int(value)
            if name.lower() == "upgrade":
                upgrade = True
            if name.lower() in ("connection", "tailscale-user-login"):
                continue
            headers.append(line)
        body = self.rfile.read(length)
        if fault := getattr(self.server, "job_fault", None):
            if reply := fault(first, body):
                self.wfile.write(reply)
                return
        with socket.create_connection(
            ("127.0.0.1", self.server.backend), timeout=10
        ) as upstream:
            connection = "upgrade" if upgrade else "close"
            upstream.sendall(
                first
                + b"".join(headers)
                + f"Connection: {connection}\r\nTailscale-User-Login: phone@example.test\r\n\r\n".encode()
                + body
            )
            try:
                if upgrade:
                    forward_upgrade(self.request, upstream)
                else:
                    upstream.settimeout(None)
                    while data := upstream.recv(65536):
                        self.request.sendall(data)
            except OSError:
                pass


def build_report_items():
    def message(turn, key, role, text, detail=None):
        return {"turn": turn, "id": key, "role": role, "text": text, "revision": 1, "detail": detail or {}}

    def command(turn, key, name):
        directory = f"/workspace/sample/{name}/target"
        argv = f"/usr/bin/bash -c 'du -sh {directory}/debug/deps {directory}/release/deps {directory}/debug/incremental'"
        output = f"{name} build artifacts\n" + "\n".join(f"{i} MiB  {directory}/debug/build/package-{i}" for i in range(80))
        item = {"command": argv, "status": "completed", "aggregated_output": output, "exit_code": 0, "cwd": "/workspace/sample"}
        return message(turn, key, "tool", argv + "\n" + output,
                       {"item": {"CommandExecution": item}, "output": output, "status": "completed", "exit": 0})

    return [
        message("disk", "prompt", "user", "Which Rust build folders use the most disk space?"),
        command("disk", "a", "orchard"), command("disk", "b", "garden"), command("disk", "c", "meadow"),
        message("disk", "answer", "assistant",
                "## Disk usage\n\n**Build artifacts** in `target/debug` account for most of the used space.\n\n"
                "| Folder | Size |\n| --- | ---: |\n| Dependencies | 12 GiB |\n| Incremental | 4 GiB |"),
        message("follow", "prompt", "user", "What about the other workspaces?"),
        command("follow", "a", "coast"), command("follow", "b", "river"),
        message("follow", "answer", "assistant", "Both workspaces fit on the current volume."),
    ]


WRITE = threading.Lock()


class Host(socketserver.StreamRequestHandler):
    def handle(self):
        def send(value):
            with WRITE:
                self.wfile.write(json.dumps(value).encode() + b"\n")
                self.wfile.flush()

        f = self.server
        sequence = 20
        try:
            for data in self.rfile:
                frame = json.loads(data)
                if frame["type"] == "hello":
                    send(
                        {
                            "type": "hello",
                            "version": 1,
                            "routes": [f.scope["route"], f.antigravity_scope["route"]],
                            "projects": [f.scope["project"]],
                            "capabilities": ["snapshot", "resume", "actions", "worker_placement"],
                            "host": {"id":f.host_id, "name":"Laptop", "version":"fixture", "epoch":"fixture"},
                        }
                    )
                elif frame["type"] == "worker_hosts":
                    assert frame["project"] == f.scope["project"]
                    send({"type":"worker_reply","id":frame["id"],"result":{"hosts":[
                        {"host":{"id":f.host_id,"name":"Laptop"},"status":"online","parent_host":True,"routes":[f.scope["route"]]},
                        {"host":{"id":"studio","name":"Studio"},"status":"unavailable","parent_host":False},
                        {"host":{"id":"build-id","name":"Build","subscription_limits":{"claude":[
                            {"label":"five hour","used_percent":72,"status":"allowed","observed_at":1700000000,"resets_at":None},
                            {"label":"old week","used_percent":99,"status":"allowed warning","observed_at":1,"resets_at":2}]},
                            "resources":{"load_one_minute":0.4,"available_memory":64_000_000_000}},
                         "status":"online","parent_host":False,"routes":[{"runtime":"claude","billing":"subscription"},{"runtime":"claude","billing":{"api":"build/only"}}]}]}})
                elif frame["type"] == "discover":
                    send(
                        {
                            "type": "discovered",
                            "scope": f.scope,
                            "rows": f.sessions,
                            "id": frame["id"],
                        }
                    )
                    send(
                        {
                            "type": "discovered",
                            "scope": f.other_scope,
                            "rows": f.other_sessions,
                            "id": frame["id"],
                        }
                    )
                    send({"type": "discovered_end", "id": frame["id"], "errors": []})
                elif frame["type"] == "subscribe" and frame.get("scope") == f.antigravity_scope:
                    send({"type": "snapshot", "cursor": {"epoch": "fixture", "seq": 10}, "scopes": [{
                        "scope": f.antigravity_scope, "connection": "ready", "capabilities": ["start", "access"],
                        "sessions": [f.antigravity_session] if f.antigravity_session else [], "asks": [], "transcripts": {},
                        "detail": {"access_choices": ["default", "plan", "accept_edits", "yolo"]},
                    }]})
                elif frame["type"] == "subscribe":
                    f.connections.append(self.request)
                    f.resumes.append(frame.get("after"))
                    item = {
                        "role": "assistant",
                        "text": "Connected phone",
                        "turn": "t",
                        "id": "m",
                        "revision": 1,
                        "detail": {},
                    }
                    ask = {
                        "id": "a",
                        "thread": "parent",
                        "title": "Approve test tool",
                        "detail": "Fixture action",
                        "choices": ["Allow", "Deny"],
                        "questions": [],
                        "token": "fixture-ask-token",
                    }
                    state = {
                        "scope": f.scope,
                        "connection": "ready",
                        "capabilities": f.caps,
                        "sessions": f.sessions,
                        "asks": [] if f.answered else [ask],
                        "transcripts": {"parent": {"items": [item], "detail": {}},
                                        "disk-report": {"items": f.report_items, "detail": {"ready": True}},
                                        "handoff-target": {"items": f.handoff_items, "detail": {}},
                                        "typed-block": {"items": f.handoff_items[2:], "detail": {}}},
                        "detail": f.detail,
                    }
                    scopes=[state]
                    if frame.get("scope") is None:
                        for ident,runtime,billing in [
                            ("codex-source","codex","subscription"),
                            ("api-source","claude",{"api":"fixture/demo"}),
                            ("codex-api-source","codex",{"api":"fixture/demo"})]:
                            scope=dict(f.scope,route={"runtime":runtime,"billing":billing})
                            idle=dict(f.sessions[0],id=ident,title=ident,status="idle",capabilities=["open"],updated_at=0)
                            active=dict(idle,id=ident+"-running",status="running",capabilities=["open","stop"])
                            scopes.append(dict(state,scope=scope,sessions=[idle,active],asks=[],transcripts={}))
                    send(
                        {
                            "type": "snapshot",
                            "cursor": {"epoch": "fixture", "seq": 10},
                            "scopes": scopes,
                        }
                    )
                    change = {
                        "kind": "append",
                        "thread": "parent",
                        "turn": "t",
                        "id": "m",
                        "text": " · streamed",
                        "revision": 2,
                    }
                    for _ in range(2):
                        send(
                            {
                                "type": "delta",
                                "scope": f.scope,
                                "cursor": {"epoch": "fixture", "seq": 11},
                                "changes": [change],
                            }
                        )
                elif frame["type"] == "act" and frame["scope"] == f.antigravity_scope:
                    action = frame["action"]
                    if "send_text" in action:
                        assert action["send_text"] == [None, "/inspect this plan"]
                        if not f.antigravity_signed_in:
                            send({"type": "delta", "scope": f.antigravity_scope, "cursor": {"epoch": "fixture", "seq": sequence},
                                  "changes": [{"kind": "state", "connection": "sign_in_required", "capabilities": [], "detail": {}}]})
                            send({"type": "error", "id": frame["id"], "message": "Sign in to Antigravity on this host."})
                            continue
                        f.antigravity_session = {"id": "antigravity:resume", "title": "Check the plan", "parent": None,
                            "status": "idle", "updated_at": 0, "capabilities": ["open", "send", "access", "model_options"],
                            "detail": {"access": "plan", "access_choices": ["plan"], "options": {"model": "gemini-fixture"},
                                "command_caps": {"models": {"gemini-fixture": {"label": "Gemini Fixture"}, "gemini-alt": {"label": "Gemini Alt"}}}}}
                        send({"type": "opened", "id": frame["id"], "scope": f.antigravity_scope, "thread": f.antigravity_session["id"]})
                    else:
                        assert action == {"select_thread": f.antigravity_session["id"]}, action
                    send({"type": "ack", "id": frame["id"]})
                elif frame["type"] == "act":
                    assert frame["scope"] == f.scope
                    if "answer_ask" in frame["action"]:
                        assert (
                            frame["approval"] == "fixture-ask-token" and not f.answered
                        )
                        f.answered = True
                        send(
                            {
                                "type": "delta",
                                "scope": f.scope,
                                "cursor": {"epoch": "fixture", "seq": 12},
                                "changes": [{"kind": "asks", "asks": []}],
                            }
                        )
                    action = frame["action"]
                    changes = []
                    if "set_access" in action or "set_next_access" in action:
                        owner, mode = action.get("set_access", [None, action.get("set_next_access")])
                        if mode == "plan":
                            send({"type": "error", "id": frame["id"], "message": "Mode disabled by native policy."})
                            continue
                        f.mode_requested.set()
                        assert f.confirm_mode.wait(10)
                        if owner:
                            target = next(r for r in f.sessions if r["id"] == owner)
                            target["detail"]["access"] = mode
                            changes.append({"kind":"session", "session":target})
                        else:
                            f.detail["applied_draft_access"] = mode
                            changes.append({"kind":"state", "capabilities":f.caps, "detail":f.detail, "connection":"ready"})
                    elif "mention" in action:
                        owner, query = action["mention"]
                        f.detail["mention"] = {"owner":owner, "query":query, "version":1,"paths":["src/mobile.rs"]}
                        changes.append({"kind":"state","connection":"ready","capabilities":f.caps,"detail":f.detail})
                    elif "model_options" in action:
                        owner, choice = action["model_options"]
                        target = next(r for r in f.sessions if r["id"] == owner)
                        target["detail"]["options"].update(choice)
                        changes.append({"kind":"session", "session":target})
                    elif "native_command" in action:
                        owner, command = action["native_command"]
                        assert owner in ("parent", None)
                        if command == "/only-in-terminal":
                            send({"type":"error", "id":frame["id"], "message":"Command requires the terminal."})
                            continue
                        assert command in ("/status", "/project:lint src/mobile.rs", "/goal Check mobile")
                        f.detail["notice"] = "Native status received" if command == "/status" else "Native lint received"
                        if command == "/goal Check mobile":
                            f.detail["notice"] = "Goal (active): Check mobile"
                        changes.append({"kind":"state","connection":"ready","capabilities":f.caps,"detail":f.detail})
                    if "start_worker" in action:
                        refused = action["start_worker"]["task"] == "Unmet hardware"
                        send({"type":"worker_reply","id":frame["id"],"error":"No host has 128 CPUs" if refused else None,
                              "result":None if refused else {"job_id":52,"status":"running","model":"fixture/demo"}})
                    if changes:
                        sequence += 1
                        send({"type":"delta", "scope":f.scope, "cursor":{"epoch":"fixture","seq":sequence}, "changes":changes})
                    late = None
                    goal_start = action.get("native_command") == [None, "/goal Check mobile"]
                    if goal_start or ("send_text" in action and action["send_text"][0] is None):
                        said = "/goal" if goal_start else action["send_text"][1]
                        # "Ack first" and "Opened first" hold the other reply until the
                        # test releases it, so the phone can be edited in between.
                        late = {"Ack first": "opened", "Opened first": "ack"}.get(said)
                        ident = {"Ack first": "new-ack", "Opened first": "new-opened", "/goal": "new-goal"}.get(said, "new")
                        f.sessions.append(
                            dict(
                                f.sessions[0],
                                id=ident,
                                title=ident if late or goal_start else "Newly created phone session",
                                parent=None,
                            )
                        )
                        opened = {
                            "type": "opened",
                            "id": frame["id"],
                            "scope": f.scope,
                            "thread": ident,
                        }
                        if late == "opened":
                            def later():
                                f.release.wait(20)
                                send(opened)
                            threading.Thread(target=later, daemon=True).start()
                        elif late == "ack":
                            def later():
                                f.release.wait(20)
                                send(opened)
                                send({"type": "ack", "id": frame["id"]})
                            threading.Thread(target=later, daemon=True).start()
                        else:
                            send(opened)
                    if "answer_ask" in action:
                        q = {
                            "id": "q",
                            "thread": "parent",
                            "token": "question-token",
                            "title": "Choose a database",
                            "detail": "",
                            "choices": [],
                            "questions": [
                                {
                                    "id": "db",
                                    "header": "Backend",
                                    "question": "Which database?",
                                    "isSecret": False,
                                    "isOther": False,
                                    "multiSelect": False,
                                    "options": [
                                        {
                                            "label": "Postgres",
                                            "description": "Relational",
                                        }
                                    ],
                                }
                            ],
                        }
                        send(
                            {
                                "type": "delta",
                                "scope": f.scope,
                                "cursor": {"epoch": "fixture", "seq": 13},
                                "changes": [{"kind": "asks", "asks": [q]}],
                            }
                        )
                    if "answer_questions" in action:
                        assert frame["approval"] == "question-token" and action[
                            "answer_questions"
                        ] == ["q", ["Postgres"]]
                        send(
                            {
                                "type": "delta",
                                "scope": f.scope,
                                "cursor": {"epoch": "fixture", "seq": 14},
                                "changes": [{"kind": "asks", "asks": []}],
                            }
                        )
                    f.actions.append(frame["action"])
                    if action == {"native_command": ["parent", "/status"]}:
                        # Its visible state delta can arrive before the action acknowledgement.
                        assert f.allow_status_ack.wait(15), "Browser did not release the /status acknowledgement"
                    if late != "ack":
                        send({"type": "ack", "id": frame["id"]})
        except (OSError, ValueError):
            pass


def managed_server(stack, server):
    stack.enter_context(server)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    stack.callback(server.shutdown)
    return server


def browser(port, method, path, data=None):
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}" + path,
        method=method,
        data=None if data is None else json.dumps(data).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=14) as resp:
        return json.load(resp)["value"]


def camera_movie(path, foreign, local):
    # Feed the browser's native fake camera with the actual host-generated QR symbols.
    # A blank period allows cancellation before either QR is accepted.
    width, height = 384, 320
    neutral = bytes([128]) * (width * height // 2)

    def luma(svg):
        image = subprocess.run(
            [shutil.which("magick") or "convert", "svg:-", "-background", "white", "-alpha", "remove",
             "-gravity", "center", "-extent", f"{width}x{height}", "-depth", "8", "gray:-"],
            input=svg.encode(), capture_output=True, check=True).stdout
        assert len(image) == width * height
        return image

    with path.open("wb") as out:
        out.write(f"YUV4MPEG2 W{width} H{height} F5:1 Ip A1:1 C420jpeg\n".encode())
        for frame, count in [(bytes([235]) * (width*height), 6), (luma(foreign), 13), (luma(local), 14)]:
            for _ in range(count):
                out.write(b"FRAME\n" + frame + neutral)


def camera_pair(js, cdp, navigate, origin):
    def perm(name, setting):
        cdp("Browser.setPermission", {"permission": {"name": name}, "setting": setting, "origin": origin})

    installed = cdp("Page.addScriptToEvaluateOnNewDocument", {
        "source": "Object.defineProperty(navigator, 'standalone', {value:true})"
    })["identifier"]
    navigate(origin)
    wait(lambda: js("return document.querySelector('#pair-hint').innerText.includes('Home Screen app has its own pairing')"))
    perm("camera", "denied")
    js("document.querySelector('#scan-pair').click()")
    wait(lambda: js("return document.querySelector('#camera-status').innerText.includes('Camera access is off')"))
    assert js("return !document.querySelector('#paste-pair').hidden && !document.querySelector('#scan-pair').disabled")
    perm("camera", "granted")
    js("document.querySelector('#scan-pair').click()")
    wait(lambda: js("return !!(window.fixtureTrack = document.querySelector('#camera-preview').srcObject?.getVideoTracks()[0])"))
    js("document.querySelector('#cancel-camera').click()")
    assert js("""return fixtureTrack.readyState==='ended' && document.querySelector('#camera-panel').hidden &&
        document.activeElement===document.querySelector('#scan-pair') && !document.querySelector('#paste-pair').hidden""")
    js("document.querySelector('#scan-pair').click()")
    wait(lambda: js("return document.querySelector('#camera-status').innerText.includes('something else')"))
    assert js("return !document.querySelector('#paste-pair').hidden && location.origin===arguments[0]", origin)
    wait(lambda: js("return document.querySelector('#paste-pair').hidden && document.activeElement.id==='device-label'"))
    assert js("return document.querySelector('#camera-preview').srcObject===null && !document.querySelector('#pair-link').value && !location.hash")
    return installed


def push_guidance(js, cdp, navigate, origin, installed):
    assert js("""return document.querySelector('#notification-setup').open &&
        !document.querySelector('#notifications').hidden && document.querySelector('#notification-guide').innerText.includes('Allow')""")
    # The native Chromium permission store supplies the denial. The iOS display context
    # is emulated to exercise Home Screen vs Safari instructions in the shipped app.
    cdp("Browser.setPermission", {"permission": {"name": "notifications"}, "setting": "denied", "origin": origin})
    js("document.querySelector('#notifications').click()")
    wait(lambda: js("return document.querySelector('#notification-guide').textContent.includes('iPhone Settings') && document.querySelector('#notifications').hidden"))
    cdp("Page.removeScriptToEvaluateOnNewDocument", {"identifier": installed})
    navigate(origin)
    wait(lambda: js("return !document.querySelector('#catalog').hidden && !!document.querySelector('.session')"))
    js("document.querySelector('#notification-summary').click()")
    assert js("""const guide=document.querySelector('#notification-guide'); return document.querySelector('#notification-setup').open &&
        guide.innerText.includes('Add to Home Screen') && guide.innerText.includes('Safari') &&
        guide.innerText.includes('fresh pairing QR') && document.querySelector('#notifications').hidden""")
    print("PASS native Chromium camera denial, cancel/track shutdown, foreign QR, real host QR pairing, and iPhone notification guidance in emulated Safari and Home Screen modes", flush=True)


def paired_browser(d, state_dir, stack, camera=False):
    assert not admin(state_dir, {"type": "status"})["enabled"]
    public = ThreadedTCP(("127.0.0.1", 0), Forward)
    origin = f"https://studio.fixture.ts.net:{public.server_address[1]}"
    if camera:
        admin(state_dir, {"type": "enable", "origin": "https://other.fixture.ts.net", "port": 0})
        foreign = admin(state_dir, {"type": "pair", "login": "phone@example.test"})["pairing"]["qr_svg"]
        admin(state_dir, {"type": "disable"})
    backend = admin(state_dir, {"type": "enable", "origin": origin, "port": 0})["port"]
    stack.callback(admin, state_dir, {"type": "disable"})

    def http_request(method, path, headers, body=None):
        c = http.client.HTTPConnection("127.0.0.1", backend, timeout=4)
        try:
            c.request(
                method,
                path,
                body=json.dumps(body).encode() if body else None,
                headers=headers,
            )
            r = c.getresponse()
            r.read()
            return r.status
        finally:
            c.close()

    h = {
        "Host": origin.removeprefix("https://"),
        "Tailscale-User-Login": "phone@example.test",
        "Origin": origin,
        "Content-Type": "application/json",
    }
    assert http_request("POST", "/api/challenge", h, {"purpose": "connect"}) == 403
    assert http_request("GET", "/", {"Host": h["Host"]}) == 403
    assert (
        http_request(
            "POST",
            "/api/pair",
            dict(h, Origin="https://foreign.invalid"),
            {"code": "unused"},
        )
        == 403
    )
    invitation = admin(state_dir, {"type": "pair", "login": "phone@example.test"})[
        "pairing"
    ]
    assert invitation["expires_at"] > time.time() and "<svg" in invitation["qr_svg"]
    subprocess.run(
        [
            "openssl",
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-x509",
            "-days",
            "1",
            "-subj",
            "/CN=studio.fixture.ts.net",
            "-addext",
            "subjectAltName=DNS:studio.fixture.ts.net",
            "-out",
            d / "cert",
            "-keyout",
            d / "key",
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(d / "cert", d / "key")
    public.socket = tls.wrap_socket(public.socket, server_side=True)
    public.backend = backend
    managed_server(stack, public)
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        port = reserve.getsockname()[1]
    driver_env = os.environ.copy()
    movie_args = []
    if camera:
        movie = d / "pairing-camera.y4m"
        camera_movie(movie, foreign, invitation["qr_svg"])
        driver_env["OX_TEST_CAMERA_FILE"] = str(movie)
        movie_args = ["--use-fake-device-for-media-stream", "--use-file-for-fake-video-capture=" + str(movie)]
    driver = subprocess.Popen(
        ["chromedriver", f"--port={port}"], env=driver_env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    stack.callback(driver.wait, 5)
    stack.callback(driver.terminate)
    wait(lambda: browser(port, "GET", "/status"))
    caps = {
        "browserName": "chrome",
        "acceptInsecureCerts": True,
        "goog:chromeOptions": {
            "binary": os.environ.get("OX_TEST_CHROMIUM")
            or shutil.which("chromium")
            or shutil.which("google-chrome"),
            "args": [
                "--headless=new",
                *movie_args,
                "--disable-dev-shm-usage",
                "--no-sandbox",
                "--no-proxy-server",
                "--disable-background-networking",
                "--disable-component-update",
                f"--user-data-dir={os.environ.get('OX_TEST_CHROMIUM_PROFILE', d / 'browser')}",
                "--host-resolver-rules=MAP studio.fixture.ts.net 127.0.0.1, MAP * ~NOTFOUND, EXCLUDE localhost",
            ],
            "mobileEmulation": {
                "deviceMetrics": {
                    "width": 390,
                    "height": 844,
                    "pixelRatio": 1,
                    "touch": True,
                },
                "userAgent": ("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 "
                              "Version/18.0 Mobile/15E148 Safari/604.1" if camera else
                              "Mozilla/5.0 Android Mobile Chrome/132 Safari/537.36"),
            },
        },
    }
    ident = browser(port, "POST", "/session", {"capabilities": {"alwaysMatch": caps}})[
        "sessionId"
    ]
    base = "/session/" + ident

    def end_session():
        # Teardown only: every check has already run. ChromeDriver can drop the
        # connection while Chromium exits (http.client.BadStatusLine on ARM),
        # which says nothing about OX.
        try:
            browser(port, "DELETE", base)
        except (OSError, http.client.HTTPException) as e:
            print(f"ChromeDriver session teardown: {e!r} (ignored)")

    stack.callback(end_session)

    def js(script, *args):
        return browser(port, "POST", base + "/execute/sync", {"script": script, "args": args})

    def cdp(command, params):
        return browser(port, "POST", base + "/goog/cdp/execute", {"cmd": command, "params": params})

    def navigate(url):
        browser(port, "POST", base + "/url", {"url": url})

    if camera:
        installed = camera_pair(js, cdp, navigate, origin)
    else:
        navigate(invitation["url"])
    wait(lambda: js("return !document.getElementById('pair-form').hidden"))
    js(
        "document.getElementById('device-label').value='Fixture phone';document.getElementById('pair-form').requestSubmit();"
    )
    wait(lambda: js("return !!document.querySelector('.session')"))
    device = next(
        r
        for r in admin(state_dir, {"type": "status"})["devices"]
        if r["label"] == "Fixture phone"
    )
    stack.callback(admin, state_dir, {"type": "revoke", "id": device["id"]})
    if camera:
        push_guidance(js, cdp, navigate, origin, installed)
    return js, port, base


def answer_from_phone(host_socket, state_dir, thread_id, env):
    """Pair a real mobile Chromium client to the fixture host and allow this thread's pending tool once."""
    state_dir = Path(state_dir)
    if not Path(host_socket).is_socket() or not state_dir.is_dir() or not thread_id:
        raise AssertionError("Expected a running disposable session host")
    if (
        env.get("OX_HOST_STATE_DIR")
        and Path(env["OX_HOST_STATE_DIR"]).resolve() != state_dir.resolve()
    ):
        raise AssertionError(
            "Disposable host state does not match the supplied fixture"
        )
    wait(lambda: (state_dir / "remote-admin.sock").exists())
    if admin(state_dir, {"type": "status"})["devices"]:
        raise AssertionError("Expected a fresh disposable remote state")
    with tempfile.TemporaryDirectory() as tmp, ExitStack() as stack:
        js, _, _ = paired_browser(Path(tmp), state_dir, stack)
        select = f"const item=[...document.querySelectorAll('.session')].find(b=>b.dataset.threadId==={json.dumps(thread_id)}); if(item){{item.click();return true}} return false;"
        wait(lambda: js(select), 30)
        allow = """const b=[...document.querySelectorAll('#asks .ask button')].find(b=>/^(allow once|allow tool call|allow for this turn|allow|approve)$/i.test(b.textContent.trim()));
          if(b&&!b.disabled){b.click();return true;}return false;"""
        wait(lambda: js(allow), 45)
        wait(lambda: js("return !document.querySelector('#asks .ask')"), 30)
        if not js("return document.getElementById('network').innerText==='Connected'"):
            raise AssertionError("Mobile client disconnected during approval")


def phone_report(js, port, base, keypress, capture, screenshot):
    def viewport(width, height):
        return browser(port, "POST", base + "/goog/cdp/execute", {
            "cmd": "Emulation.setDeviceMetricsOverride",
            "params": {"width": width, "height": height, "deviceScaleFactor": 1, "mobile": True},
        })

    def one_line(selector, width):
        return js("""const e=document.querySelector(arguments[0]), r=new Range();
            r.selectNodeContents(e);
            return e.textContent.includes('Sessions') && r.getClientRects().length===1 &&
                   document.documentElement.clientWidth===arguments[1] &&
                   document.documentElement.scrollWidth<=arguments[1];""", selector, width)

    js("document.querySelector('#home').click()")
    wait(lambda: js("return !!document.querySelector('[data-thread-id=disk-report]')"))
    for width in [390, 360, 320]:
        viewport(width, 740)
        assert one_line("#catalog h1", width), f"Sessions heading wrapped or overflowed at {width}px"
    viewport(360, 740)
    if screenshot:
        capture(Path(screenshot).with_stem(Path(screenshot).stem + "-narrow-sessions"))
    js("document.querySelector('[data-thread-id=disk-report]').click()")
    wait(lambda: js("return document.querySelectorAll('#timeline details.tool-group').length===2"))
    assert one_line("#conversation .back", 360)
    metrics = js("""return [...document.querySelectorAll('#timeline details.tool-group')].map(g=>({
        open:g.open, count:g.querySelectorAll('details.tool').length,
        summary:g.querySelector(':scope > summary').innerText, height:g.getBoundingClientRect().height
    }));""")
    assert all(not g["open"] and g["count"] == 0 and g["height"] <= 76 for g in metrics), metrics
    assert [g["summary"].strip() for g in metrics] == ["3 tool calls", "2 tool calls"], metrics
    assert js("""const a=document.querySelector('#timeline strong')?.closest('article');
        a?.querySelector('table')?.scrollIntoView({block:'center'});
        const cell=a?.querySelector('td'), r=cell?.getBoundingClientRect();
        return a?.querySelector('strong').textContent==='Build artifacts' &&
               a.querySelector('code')?.textContent==='target/debug' &&
               a.querySelector('table th')?.textContent==='Folder' &&
               [...a.querySelectorAll('table td')].some(td=>td.textContent==='12 GiB') &&
               parseFloat(getComputedStyle(a.querySelector('strong')).fontSize)>=15 &&
               cell.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2)) &&
               !a.innerText.includes('**Build artifacts**') && !a.innerText.includes('| ---');""")
    first = "#timeline details.tool-group"
    js("document.querySelector(arguments[0]).focus()", first + " > summary")
    keypress("\ue007")
    wait(lambda: js("return document.querySelectorAll('details.tool-group[open] details.tool').length===3"))
    js("document.querySelector(arguments[0]).querySelectorAll('details.tool > summary')[1].focus()", first)
    keypress("\ue007")
    wait(lambda: js("return document.querySelector('details.tool[open] pre')?.textContent.includes('garden build artifacts')"))
    visible = js("""const groups=[...document.querySelectorAll('details.tool-group')];
        const tools=[...groups[0].querySelectorAll('details.tool')];
        return [groups.map(g=>g.open), tools.map(t=>t.open), tools[1].innerText.includes('garden build artifacts'),
                document.documentElement.scrollWidth<=360];""")
    assert visible == [[True, False], [False, True, False], True, True], visible
    if screenshot:
        js("document.querySelector('details.tool[open] > summary').scrollIntoView({block:'center'})")
        capture(Path(screenshot).with_stem(Path(screenshot).stem + "-tool"))
    js("document.querySelector(arguments[0]).click()", first + " > summary")
    if screenshot:
        js("document.querySelector('#timeline strong').scrollIntoView({block:'center'})")
        capture(Path(screenshot).with_stem(Path(screenshot).stem + "-report"))
    assert js("""const e=document.querySelector('#draft'); e.scrollIntoView({block:'center'}); e.focus();
        const r=e.getBoundingClientRect(), send=document.querySelector('#send').getBoundingClientRect();
        return document.activeElement===e && e.labels.length>0 &&
               document.elementFromPoint(r.x+r.width/2,r.y+r.height/2)===e &&
               send.width>0 && send.right<=360 && document.documentElement.scrollWidth<=360;""")
    viewport(360, 450)
    js("""const draft=document.querySelector('#draft');
        draft.value='Please check the worker result and summarize what changed.';
        draft.dispatchEvent(new Event('input')); scrollTo(0, document.documentElement.scrollHeight);""")
    assert js("""const box=id=>document.getElementById(id).getBoundingClientRect(),
        attach=box('attach-label'), send=box('send'), draft=box('draft');
        return attach.width>=44 && attach.height>=44 && attach.left<draft.left+20 &&
               send.height>=44 && send.bottom<=innerHeight+1 && !document.querySelector('#send').disabled;""")
    if screenshot:
        capture(Path(screenshot).with_stem(Path(screenshot).stem + "-composer"))
    assert js("return scrollY > 200 && document.activeElement.id === 'draft'")
    wait(lambda: js("return document.querySelector('#nav-title').innerText==='Rust build disk usage' && !document.querySelector('#nav-title').hidden"))
    js("document.querySelector('#conversation .back').click()")
    wait(lambda: js("return document.activeElement.id==='sessions-heading' && scrollY===0"))
    assert js("""const row=document.querySelector('[data-thread-id=parent] .worker-outcome');
        return row.innerText.includes('Worker #7') && row.innerText.includes('completed') &&
               document.documentElement.scrollHeight > innerHeight + 500;""")
    js("scrollTo(0, 320); document.querySelector('#find-sessions').dispatchEvent(new Event('input'))")
    assert js("""return new Promise(resolve => requestAnimationFrame(() =>
        requestAnimationFrame(() => resolve(scrollY >= 300))));""")
    viewport(390, 844)
    print("PASS narrow phone catalog, independent disclosures for long commands, semantic Markdown report, and reachable composer", flush=True)


def antigravity_phone(host, js):
    js("document.getElementById('home').click()")
    wait(lambda: js("return !!document.querySelector('[data-thread-id=parent]')"))
    js("document.querySelector('[data-thread-id=parent]').click()")
    wait(lambda: js("return !document.getElementById('continue-in').hidden && !document.getElementById('menu-settings').hidden"))
    js("document.getElementById('continue-in').click(); const r=document.getElementById('handoff-runtime');"
       "r.value='antigravity'; r.dispatchEvent(new Event('change'))")
    assert js("""const route=document.getElementById('handoff-route'), modes=[...document.getElementById('handoff-mode').options];
        return [...route.options].map(o=>o.text).join('')==='Google Antigravity' &&
            modes.filter(o=>!o.disabled).map(o=>o.value).join(',')==='inherit,read,write,yolo' &&
            document.getElementById('handoff-provider').textContent.includes('Google');""")
    js("document.getElementById('handoff-mode').value='read'; document.getElementById('handoff-request').value='Continue in Plan mode';"
       "document.getElementById('handoff-form').requestSubmit()")
    wait(lambda: any(isinstance(a, dict) and a.get("start_handoff", {}).get("runtime") == "antigravity" and
                     a["start_handoff"]["mode"] == "read" and a["start_handoff"]["model"] is None for a in host.actions))
    js("document.getElementById('home').click()")
    # A native Google route has no API alias. Retrying sign-in keeps the typed ACP prompt.
    js("document.getElementById('new').click(); const r=document.getElementById('route');"
       "r.value=[...r.options].find(o=>o.text==='Google Antigravity').value; document.getElementById('new-form').requestSubmit()")
    js("const draft=document.getElementById('draft'); draft.value='/inspect this plan'; draft.dispatchEvent(new Event('input'))")
    wait(lambda: js("return !document.getElementById('send').disabled"))
    js("document.getElementById('composer').requestSubmit()")
    wait(lambda: js("return !document.getElementById('antigravity-sign-in-required').hidden"))
    assert js("return document.getElementById('draft').value==='/inspect this plan' && document.getElementById('send').disabled")
    host.antigravity_signed_in = True
    js("document.getElementById('antigravity-reconnect').click()")
    wait(lambda: js("return !document.getElementById('send').disabled"))
    js("document.getElementById('composer').requestSubmit()")
    wait(lambda: js("return document.getElementById('title').textContent==='Check the plan' && document.getElementById('composer-model')?.value==='gemini-fixture'"))
    assert js("return document.getElementById('start-worker').hidden && document.getElementById('attach-label').hidden && document.getElementById('operator-session').hidden")
    js("document.getElementById('access-mode').click()")
    assert js("return [...document.getElementById('permission').options].map(o=>o.value).join(',')==='plan'")
    print("PASS Chromium Antigravity handoff, sign-in retry, draft retention and model projection", flush=True)


def run(harness, screenshot=None):
    with tempfile.TemporaryDirectory() as tmp, ExitStack() as stack:
        d = Path(tmp)
        host = socketserver.ThreadingUnixStreamServer(str(d / "mock-host.sock"), Host)
        host.daemon_threads = True
        host.host_id = "06f2bbe4-12bb-4243-ab20-657f19c02d36"
        (d / "host-id").write_text(host.host_id + "\n")
        host.scope = {
            "project": str(d / "fixture"),
            "route": {"runtime": "claude", "billing": "subscription"},
        }
        host.antigravity_scope = dict(host.scope, route={"runtime": "antigravity", "billing": "subscription"})
        host.antigravity_signed_in = False
        host.antigravity_session = None
        now = int(time.time())
        Path(host.scope["project"]).mkdir()
        host.caps = ["start", "access", "model_options", "native_commands"]
        host.allow_status_ack = threading.Event()
        host.confirm_mode, host.mode_requested = threading.Event(), threading.Event()
        host.detail = {
            "selected":"parent", "applied_draft_access":"default", "access_choices":["default","plan","bypass_permissions"],
            "commands":{"model":"demo","methods":[],"choices":[["/goal","Session goal"],["/model","Choose model"]],
                        "goal_choices":[["/goal","thread/goal/get"],["/goal pause","thread/goal/set"]],
                        "models":{"other":{"label":"Other","efforts":[]}, "demo":{"label":"Demo","efforts":[{"reasoningEffort":"low"},{"reasoningEffort":"high"}]}}}
        }
        host.sessions = [
            {
                "id": "parent",
                "title": "Database connection timeouts",
                "parent": None,
                "status": "active",
                "updated_at": now - 2 * 86400,
                "capabilities": ["open", "send", "steer", "stop", "access", "model_options", "native_commands", "attach_image", "mention"],
                "detail": {"access":"default", "options":{"model":"demo","effort":"low"}},
            },
            {
                "id": "child",
                "parent": "parent",
                "title": "Investigate connection pool",
                "status": "running",
                "updated_at": now,
                "capabilities": ["open", "steer", "stop"],
                "detail": {"worker": True, "origin": {"relation": "worker", "job_id": 7, "ceiling": "read",
                    "requested": "read", "runtime": "claude", "mode": "session", "by_operator": False,
                    "launch": "helper:fixture", "source": None,
                    "route": {"runtime": "claude", "billing": "subscription"},
                    "parent": {"project": host.scope["project"], "scope": host.scope, "session": "parent", "runtime": "claude"}}},
            },
        ]
        host.sessions.append(dict(host.sessions[0], id="disk-report", title="Rust build disk usage",
                                  status="idle", updated_at=0, capabilities=["open", "send", "attach_image"], detail={"access": "default"}))
        host.other_scope = dict(host.scope, project=str(d / "other-project"))
        host.other_sessions = [dict(host.sessions[-1], id="another-project", title="Another project", updated_at=now - 1)]
        # A catalog longer than the viewport can inherit a conversation's deep scroll.
        saved = host.sessions[-1]
        host.sessions.extend(dict(saved, id=f"saved-{i}", title=f"Earlier session {i}",
                                  status="saved") for i in range(32))
        host.sessions[0]["detail"]["worker_notice"] = {"job_id": 7, "status": "completed"}
        host.report_items = build_report_items()
        # A handoff target's first user message carries the block; the same text typed later
        # in that session, or in an ordinary session, renders as typed.
        block = "<ox-handoff>\nCARRIED earlier context\n</ox-handoff>\n\n"
        host.handoff_items = [
            {"role": "user", "text": block + "Carry on here", "turn": "h1", "id": "hu1", "revision": 1, "detail": {}},
            {"role": "assistant", "text": "On it", "turn": "h1", "id": "ha1", "revision": 1, "detail": {}},
            {"role": "user", "text": block + "Typed later", "turn": "h2", "id": "hu2", "revision": 1, "detail": {}},
        ]
        route = {"runtime": "claude", "billing": "subscription"}
        handoff_origin = {"relation": "handoff", "job_id": 0, "ceiling": "write", "requested": "write",
            "runtime": "claude", "mode": "session", "by_operator": True, "launch": "operator:fixture",
            "source": None, "route": route, "started": "accept_edits", "decided": "inherited",
            "parent": {"project": host.scope["project"], "scope": host.scope, "session": "parent", "runtime": "claude"},
            "handoff": {"scope": host.scope, "session": "parent", "native": None, "title": "Database connection timeouts",
                "runtime": "claude", "route": route, "source_access": "accept_edits", "source_level": "write",
                "decision": "inherited", "selection": {"total": 4, "carried": 3, "omitted": 1, "items": [], "cut": [],
                "ranges": [], "window": 64000, "window_known": False}}}
        host.sessions.append(dict(host.sessions[0], id="handoff-target", title="Continued pool work", status="idle",
                                  updated_at=0, capabilities=["open", "send"], detail={"access": "accept_edits", "origin": handoff_origin}))
        host.sessions.append(dict(host.sessions[0], id="typed-block", title="Typed block", status="idle",
                                  updated_at=0, capabilities=["open", "send"], detail={"access": "default"}))
        host.actions = []
        host.release = threading.Event()
        host.resumes = []
        host.connections = []
        host.answered = False
        managed_server(stack, host)
        isolated=d/"home"
        codex=isolated/".codex"
        codex.mkdir(mode=0o700,parents=True)
        (codex/"codeox.toml").write_text('[providers.garden]\nname="Garden"\nbase_url="https://garden.invalid/v1"\nenv_key="GARDEN_TOKEN"\nwire_api="chat_completions"\ndiscovery=false\n[[providers.garden.models]]\nid="orchard"\n')
        copy_fixture = None
        if os.environ.get("OX_TEST_HOST_BIN"):
            from key_copy import receiver
            copy_fixture = receiver(d, stack)
        source_env = dict(os.environ, **(copy_fixture[0] if copy_fixture else {}), DBUS_SESSION_BUS_ADDRESS="unix:path="+str(d/"no-session-bus"), SHELL="/bin/sh", GARDEN_TOKEN="fixture-env-token", HOME=str(isolated), CODEX_HOME=str(codex), CLAUDE_CONFIG_DIR=str(isolated/".claude"), XDG_STATE_HOME=str(isolated/"state"), XDG_CONFIG_HOME=str(isolated/"config"), OX_TEST_REMOTE_DIR=str(d))
        from operator_browser import check_terminal, install_login_fixture
        source_env["PATH"] = os.pathsep.join((str(install_login_fixture(d)), source_env.get("PATH", os.defpath)))
        # Provider catalogs may be requested after the synthetic cloud key is imported.
        # Only the local fixtures and the paired destination should be reachable.
        for var in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            source_env[var] = source_env[var.lower()] = "http://127.0.0.1:1"
        source_env["NO_PROXY"] = source_env["no_proxy"] = "127.0.0.1,localhost,.fixture.ts.net"
        service = subprocess.Popen(
            [harness, "--exact", "fixture::browser_service", "--ignored"],
            env=source_env,
            stdout=subprocess.DEVNULL,
        )
        stack.callback(service.wait, 5)
        stack.callback(service.terminate)
        wait(lambda: (d / "remote-admin.sock").exists())
        js, port, base = paired_browser(d, d, stack, camera=True)
        wait(lambda: js("""return !location.hash && document.cookie==='' &&
            document.querySelector('[data-view=recent]').ariaPressed==='true' &&
            [...document.querySelectorAll('#list .session')].slice(0,3)
                .map(row=>row.dataset.threadId).join(',')==='child,another-project,parent' &&
            document.querySelector('[data-thread-id=child]').innerText.includes('Worker for Database connection timeouts') &&
            document.querySelector('[data-thread-id=another-project]').innerText.includes('other-project') &&
            !!document.querySelector('#list .catalog-day') &&
            !document.querySelector('.session.nested')"""))
        js("document.querySelector('[data-filter=active]').click();document.querySelector('[data-view=folders]').click()")
        wait(lambda: js("""return document.querySelector('[data-view=folders]').ariaPressed==='true' &&
            document.querySelector('[data-filter=active]').ariaPressed==='true' &&
            !document.querySelector('[data-thread-id=another-project]')"""))
        js("location.reload()")
        wait(lambda: js("""return document.querySelector('[data-view=folders]').ariaPressed==='true' &&
            !!document.querySelector('.session.nested')"""))

        # A phone relaunch keeps only the draft for the exact session, without submitting it.
        js("document.querySelector('[data-thread-id=child]').click()")
        wait(lambda: js("return !document.getElementById('conversation').hidden"))
        js("""const d=document.getElementById('draft');
            d.value='Check the pool limits first'; d.dispatchEvent(new Event('input')); location.reload();""")
        wait(lambda: js("return !!document.querySelector('[data-thread-id=parent]')"))
        js("document.querySelector('[data-thread-id=parent]').click()")
        assert js("return !document.getElementById('draft').value")
        js("document.querySelector('#conversation .back').click(); document.querySelector('[data-thread-id=child]').click()")
        assert js("return document.getElementById('draft').value==='Check the pool limits first'")
        assert {"send_text": ["child", "Check the pool limits first"]} not in host.actions
        js("""const d=document.getElementById('draft');
            d.value=''; d.dispatchEvent(new Event('input'));
            document.querySelector('#conversation .back').click();""")

        report = next(row for row in host.sessions if row["id"] == "disk-report")
        report["detail"]["last_result"] = {"turn": "follow", "completed_at": now, "outcome": "completed"}
        js("location.reload()")
        wait(lambda: js("""return document.querySelector('[data-thread-id=disk-report] .result-badge')
            ?.textContent==='New result'"""))
        js("document.querySelector('[data-filter=active]').click()")
        wait(lambda: js("""return document.querySelector('.just-finished [data-thread-id=disk-report] .result-badge')
            ?.textContent==='Turn done'"""))
        js("document.querySelector('.just-finished [data-thread-id=disk-report]').click()")
        wait(lambda: js("""return !document.querySelector('#conversation').hidden &&
            document.querySelector('#timeline').innerText.includes('Both workspaces fit') &&
            Object.values(JSON.parse(localStorage.getItem('ox-phone-seen-results-v1') || '{}')).includes('follow')"""))
        js("location.reload()")
        wait(lambda: js("""return !!document.querySelector('[data-thread-id=disk-report]') &&
            !document.querySelector('[data-thread-id=disk-report] .result-badge')"""))
        del report["detail"]["last_result"]

        # A short reply can already be in view when the result arrives, with no scroll offset.
        report_items = host.report_items
        host.report_items = [dict(report_items[-1], turn="short-result", text="A short reply")]
        js("document.querySelector('[data-thread-id=disk-report]').click()")
        wait(lambda: js("""return !document.querySelector('#conversation').hidden &&
            document.querySelector('#timeline').innerText.includes('A short reply') &&
            scrollY===0"""))
        report["detail"]["last_result"] = {"turn": "short-result", "completed_at": now + 1, "outcome": "completed"}
        def publish_result(sequence, extra=()):
            delta = {"type": "delta", "scope": host.scope, "cursor": {"epoch": "fixture", "seq": sequence},
                     "changes": [{"kind": "session", "session": report}, *extra]}
            with WRITE:
                for connection in list(host.connections):
                    try:
                        connection.sendall(json.dumps(delta).encode() + b"\n")
                    except OSError:
                        continue

        publish_result(100)
        wait(lambda: js("""return scrollY===0 &&
            Object.values(JSON.parse(localStorage.getItem('ox-phone-seen-results-v1') || '{}')).includes('short-result')"""))
        # A result that arrives below the viewport is unread until the user scrolls to it.
        report["detail"]["last_result"] = {"turn": "later-result", "completed_at": now + 2, "outcome": "completed"}
        messages = [dict(report_items[-1], id=f"later-{i}", turn="later-result", text=f"Later reply {i}")
                    for i in range(25)]
        publish_result(101, [{"kind": "transcript", "thread": "disk-report",
                              "timeline": {"items": messages, "detail": {"ready": True}}}])
        wait(lambda: js("""return document.querySelector('#timeline').innerText.includes('Later reply 24') &&
            scrollY===0 && !Object.values(JSON.parse(localStorage.getItem('ox-phone-seen-results-v1') || '{}')).includes('later-result')"""))
        js("scrollTo(0, document.documentElement.scrollHeight)")
        wait(lambda: js("""return Object.values(JSON.parse(
            localStorage.getItem('ox-phone-seen-results-v1') || '{}')).includes('later-result')"""))
        del report["detail"]["last_result"]
        host.report_items = report_items
        js("location.reload()")
        wait(lambda: js("return !!document.querySelector('[data-thread-id=disk-report]')"))

        from notification_browser import check_notification_links
        check_notification_links(js, wait, host.scope)
        check_terminal(js,admin,wait,d,host)
        # Source silhouettes stay stable across approvals, active work, billing routes, and nesting.
        icons=js("""return Object.fromEntries([...document.querySelectorAll('.session')].map(b=>[
            b.dataset.threadId,[b.dataset.source,b.querySelector('.status').textContent,getComputedStyle(b,'::before').boxShadow.replace(/rgba?[(][^)]*[)]/g,'')]
        ]));""")
        assert icons["parent"][:2]==["claude","Needs you"] and icons["child"][0]=="worker", icons
        for ident,source in [("codex-source","codex"),("api-source","api"),("codex-api-source","api")]:
            assert icons[ident][:2]==[source,""] and icons[ident+"-running"][:2]==[source,"Running"]
            assert icons[ident][2]==icons[ident+"-running"][2] and icons[ident][2]!="none"
        assert len({icons[k][2] for k in ("parent","child","codex-source","api-source")})==4

        def keypress(code):
            browser(
                port,
                "POST",
                base + "/actions",
                {
                    "actions": [
                        {
                            "type": "key",
                            "id": "keyboard",
                            "actions": [
                                {"type": "keyDown", "value": code},
                                {"type": "keyUp", "value": code},
                            ],
                        }
                    ]
                },
            )

        def capture(name):
            import base64

            Path(name).write_bytes(
                base64.b64decode(browser(port, "GET", base + "/screenshot"))
            )

        js("const n=document.querySelector('#notification-setup');if(n.open)n.querySelector('summary').click();document.getElementById('home').focus()")
        keypress("\ue004")
        assert js(
            "return document.activeElement===document.getElementById('new') && document.activeElement.matches(':focus-visible')"
        ), js("return {focused:document.activeElement.id, visible:document.activeElement.matches(':focus-visible'), catalog:!document.querySelector('#catalog').hidden}")
        assert js(
            """return document.getElementById('new').getBoundingClientRect().height>=44 &&
            [...document.querySelectorAll('#catalog-view button')].every(b=>b.getBoundingClientRect().height>=44) &&
            document.documentElement.scrollWidth<=innerWidth"""
        )
        if screenshot:
            s = Path(screenshot)
            js("document.activeElement.blur()")
            capture(s.with_name(s.stem + "-sessions" + s.suffix))
        js("document.querySelector('h2.project .project-star')?.click()")
        wait(lambda: js("return document.querySelector('h2.project')?.dataset.starred === 'true'"))
        js("document.querySelector('h2.project .project-star')?.click()")
        wait(lambda: js("return document.querySelector('h2.project')?.dataset.starred !== 'true'"))
        js("const f=document.getElementById('find-sessions'); f.value='Investigate'; f.dispatchEvent(new Event('input'))")
        wait(lambda: js("return document.querySelectorAll('.session').length===1 && document.querySelector('.session').dataset.threadId==='child'"))
        js("const f=document.getElementById('find-sessions'); f.value=''; f.dispatchEvent(new Event('input'))")
        wait(lambda:js("return !!document.querySelector('[data-thread-id=parent]')"))
        js("document.querySelector('[data-thread-id=parent]').focus()")
        keypress("\ue007")

        wait(
            lambda: js(
                "return document.querySelector('#timeline')?.innerText.includes('Connected phone · streamed')"
            )
        )
        assert js(
            "return !document.querySelector('#timeline').innerText.includes('streamed · streamed')"
        )
        if screenshot:
            capture(screenshot)
        js("document.querySelector('#conversation .back').focus()")
        keypress("\ue004")
        assert js("return document.activeElement.matches('#conversation .action-menu > summary:focus-visible')")
        keypress("\ue007")
        keypress("\ue004")
        # The menu opens on its first visible item (the Session section leads now).
        assert js("""const a=document.activeElement, m=document.querySelector('#conversation .action-menu');
            const first=[...m.querySelectorAll('button')].find(b=>b.offsetParent!==null);
            return m.open && a===first""")
        keypress("\ue00c")
        assert js("return !document.querySelector('#conversation .action-menu').open && document.activeElement.matches('.action-menu > summary')")
        keypress("\ue004")
        assert js("""return document.activeElement.id==='worker-results-title' &&
            document.activeElement.textContent==='Workers (1)'""")
        keypress("\ue007")
        assert js("""return document.getElementById('worker-results').open &&
            document.getElementById('worker-results-list').innerText.includes('Investigate connection pool')""")
        keypress("\ue007")  # Collapse the list before moving to the session controls.
        keypress("\ue004")
        assert js("return document.activeElement.id==='access-mode' && document.activeElement.matches(':focus-visible')")
        keypress("\ue004")
        assert js("return document.activeElement.id==='open-settings'")
        keypress("\ue004")
        assert js("return document.activeElement.id==='start-worker' && document.activeElement.matches(':focus-visible')")
        keypress("\ue007")
        assert js("return document.getElementById('worker-sheet').open && document.activeElement.id==='worker-heading'")
        js("document.getElementById('worker-submit').focus()")
        keypress("\ue004")
        assert js("""const sheet=document.getElementById('worker-sheet');
            return document.activeElement===document.body || sheet.contains(document.activeElement)""")
        keypress("\ue00c")
        wait(lambda: js("return !document.getElementById('worker-sheet').open && document.activeElement.id==='start-worker'"))
        keypress("\ue004")
        for _ in range(8):
            if js("return document.activeElement.textContent==='Allow'"): break
            keypress("\ue004")
        assert js("return document.activeElement.textContent==='Allow' && document.activeElement.matches(':focus-visible')")
        keypress("\ue007")
        wait(lambda: host.answered)
        wait(lambda: js("return !!document.querySelector('#asks input[type=radio]')"))
        js(
            "document.querySelector('#asks input[type=radio]').click();document.querySelector('#asks button').click()"
        )
        wait(lambda: {"answer_questions": ["q", ["Postgres"]]} in host.actions)
        js(
            "const d=document.querySelector('#draft');d.value='Please continue';d.dispatchEvent(new Event('input'));document.querySelector('#composer').requestSubmit();"
        )
        wait(lambda: {"send_text": ["parent", "Please continue"]} in host.actions)
        js("const d=document.querySelector('#draft');d.value='@mob';d.dispatchEvent(new Event('input'))")
        wait(lambda:js("return !!document.querySelector('#mention-choices button')"))
        js("document.querySelector('#mention-choices button').click()")
        assert js("return document.getElementById('draft').value==='@src/mobile.rs ' && !document.querySelector('#mention-choices button')")
        js("""const data=new DataTransfer();
           data.items.add(new File([Uint8Array.from(atob('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg=='),c=>c.charCodeAt(0))], 'pixel.png',{type:'image/png'}));
           const input=document.getElementById('attach-image'); input.files=data.files;
           input.dispatchEvent(new Event('change'));""")
        wait(lambda:js("return !!document.querySelector('#attached-images button')"))
        assert js("return !!document.querySelector('#attached-images button.attached-chip img')")
        js("document.querySelector('#composer').requestSubmit()")
        wait(lambda:any(isinstance(a,dict) and a.get("send_images",[None])[0]=="parent" for a in host.actions))
        wait(lambda:js("return !document.querySelector('#attached-images button')"))
        js("document.querySelector('#stop').click()")
        wait(lambda: {"stop": "parent"} in host.actions)
        for conn in list(host.connections):
            try:
                conn.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        wait(lambda: any(c is not None for c in host.resumes))
        wait(
            lambda: js(
                "return document.getElementById('network').innerText==='Connected'"
            )
        )
        def choose(id, value):
            js(f"const s=document.getElementById({id!r});s.value={value!r};s.dispatchEvent(new Event('change'))")
        def mode(value):
            choose("permission", value)
            js("document.getElementById('apply-permission').click()")
        def type_draft(text):
            js("const d=document.getElementById('draft');d.value=arguments[0];d.dispatchEvent(new Event('input'))", text)
        type_draft("/go")
        wait(lambda: js("return document.querySelector('#slash-choices button')?.textContent==='/goal'"))
        js("document.querySelector('#slash-choices button').click()")
        assert js("return document.activeElement.id==='draft' && document.activeElement.value==='/goal '")
        assert js("return document.getElementById('slash-choices').innerText.includes('/goal pause')")
        type_draft("/goal Check mobile")
        js("document.getElementById('send').click()")
        wait(lambda: {"native_command":["parent", "/goal Check mobile"]} in host.actions)
        wait(lambda: js("return !document.getElementById('composer-status').hidden && document.getElementById('composer-status').textContent==='Goal (active): Check mobile' && !document.getElementById('draft').value"))
        assert {"send_text":["parent", "/goal Check mobile"]} not in host.actions
        type_draft("/only-in-terminal")
        js("document.getElementById('send').click()")
        wait(lambda: js("return document.getElementById('composer-status').innerText.includes('requires the terminal')"))
        assert js("return document.getElementById('draft').value==='/only-in-terminal' && document.getElementById('slash-choices').innerText.includes('Open in native terminal')")
        type_draft("/model")
        js("document.getElementById('send').click()")
        assert js("return document.activeElement.id==='composer-model' && document.getElementById('session-settings').hidden")
        keypress("\ue00c")
        choose("composer-model", "other")
        wait(lambda: {"model_options":["parent", {"model":"other"}]} in host.actions)
        wait(lambda: js("return document.getElementById('composer-model').value==='other' && !document.getElementById('composer-model').disabled"))
        choose("composer-model", "demo")
        wait(lambda: js("return document.getElementById('composer-model').value==='demo'"))
        js("document.getElementById('access-mode').click()")
        mode("plan")
        wait(lambda: js("return document.getElementById('control-status').textContent.includes('Claude subscription · Mode disabled')"))
        mode("bypass_permissions")
        wait(host.mode_requested.is_set)
        assert js("return document.getElementById('access-mode').dataset.bypass==='false' && document.getElementById('access-mode').textContent==='Default'")
        host.confirm_mode.set()
        wait(lambda: js("return document.getElementById('access-mode').dataset.bypass==='true'"))
        assert js("return document.getElementById('message').hidden && getComputedStyle(document.querySelector('#composer')).position!=='sticky'")
        choose("session-effort", "high")
        wait(lambda: {"model_options":["parent", {"effort":"high"}]} in host.actions)
        wait(lambda: js("return document.getElementById('session-effort').value==='high'"))
        def run_native_command():
            # requestSubmit() bypasses the disabled Run button. Click it only when the
            # real control can accept another action, including after a delayed Ack.
            try:
                wait(lambda: js("""
                    const run = document.querySelector('#native-command-form button');
                    if (run.disabled || !run.getClientRects().length) return false;
                    run.click();
                    return true;
                """))
            except AssertionError as error:
                status = js("return document.getElementById('control-status').textContent")
                raise AssertionError(f"Run command never became available; status={status!r}") from error
        run_native_command()
        wait(lambda: js("return document.getElementById('control-status').textContent==='Native status received'"))
        # Inspecting cold Claude history provides no installed command inventory.
        # An explicitly typed command uses the native-only action; after a refusal
        # the exact text stays editable, independent of the ordinary chat draft.
        def enter_custom_command(text):
            choose("native-command", "custom")
            js("const field=document.getElementById('typed-native-command');field.value=arguments[0];field.dispatchEvent(new Event('input'))", text)
        js("const d=document.getElementById('draft');d.value='/home/user/project';d.dispatchEvent(new Event('input'))")
        enter_custom_command("/only-in-terminal")
        assert js("return document.querySelector('#native-command-form button').disabled"), "/status must still await its acknowledgement"
        host.allow_status_ack.set()
        run_native_command()
        wait(lambda: js("return document.getElementById('control-status').textContent.includes('Command requires the terminal')"))
        assert js("return document.getElementById('typed-native-command').value==='/only-in-terminal' && document.getElementById('draft').value==='/home/user/project'")
        enter_custom_command("/project:lint src/mobile.rs")
        run_native_command()
        wait(lambda: {"native_command":["parent", "/project:lint src/mobile.rs"]} in host.actions)
        js("document.getElementById('composer').requestSubmit()")
        wait(lambda: {"send_text":["parent", "/home/user/project"]} in host.actions)
        choose("native-command", "/status")
        if screenshot:
            capture(s.with_name(s.stem+"-settings"+s.suffix))
        js("document.getElementById('home').click()")
        wait(lambda: js("return !!document.querySelector('[data-thread-id=child]')"))
        js("document.querySelector('[data-thread-id=child]').click()")
        wait(
            lambda: js(
                "return document.getElementById('title').textContent==='Investigate connection pool' && !document.getElementById('stop').disabled && document.getElementById('network').innerText==='Connected'"
            )
        )
        assert js("return document.getElementById('context').textContent.includes('worker 7 · read-only ceiling')")
        assert js("return document.getElementById('context').textContent.includes('Laptop')")
        js("document.getElementById('stop').click()")
        wait(lambda: {"stop": "child"} in host.actions)
        # The operator starts a worker attached to the open session.
        def worker_viewport(width, height):
            browser(port, "POST", base + "/goog/cdp/execute", {
                "cmd": "Emulation.setDeviceMetricsOverride",
                "params": {"width": width, "height": height, "deviceScaleFactor": 1, "mobile": True},
            })
        worker_viewport(320, 740)
        assert js("""const start=document.getElementById('start-worker');
            return !start.disabled && start.getBoundingClientRect().height>=44 &&
                document.documentElement.scrollWidth<=320 &&
                document.getElementById('location').textContent==='Laptop';""")
        js("document.getElementById('start-worker').focus()")
        keypress("\ue007")
        assert js("""return document.activeElement.id==='worker-heading' &&
            document.querySelector('label[for=worker-host]').textContent==='Run on'""")
        wait(lambda: js("""const select=document.getElementById('worker-host');
            const offline=[...select.options].find(o=>o.value==='studio');
            return offline?.disabled && offline.text.includes('offline') &&
                select.selectedOptions[0]?.text.startsWith('Laptop ·') &&
                document.documentElement.scrollWidth<=320;"""))
        worker_viewport(320, 420)
        assert js("""const body=document.querySelector('.worker-sheet-body'),
            actions=document.querySelector('.worker-sheet-actions'),
            task=document.getElementById('worker-task'),
            runtime=document.getElementById('worker-runtime');
            const initial=actions.getBoundingClientRect();
            body.scrollTop=body.scrollHeight;
            const scrolled=actions.getBoundingClientRect();
            const scrolls=body.scrollTop>0;
            body.scrollTop=0;
            return scrolls && task.compareDocumentPosition(runtime) & Node.DOCUMENT_POSITION_FOLLOWING &&
                body.getBoundingClientRect().bottom<=scrolled.top &&
                scrolled.top===initial.top && scrolled.bottom<=innerHeight &&
                [...actions.querySelectorAll('button')].every(b=>b.getBoundingClientRect().height>=44);""")
        worker_viewport(320, 740)
        js("document.getElementById('worker-task').value='Local draft'; document.getElementById('worker-open-hosts').click()")
        wait(lambda: js("""return document.querySelector('#settings h2')?.textContent==='Laptop' &&
            document.getElementById('settings').innerText.includes('Current host') &&
            document.getElementById('settings').innerText.includes('No worker destinations are linked.')"""))
        js("document.querySelector('#settings .back').click()")
        assert js("""return !document.getElementById('conversation').hidden &&
            !document.getElementById('worker-form').hidden &&
            document.getElementById('worker-task').value==='Local draft'""")
        worker_viewport(390, 844)
        js("document.getElementById('worker-runtime').value='claude';document.getElementById('worker-task').value='Check the pool limits';document.getElementById('worker-form').requestSubmit()")
        wait(lambda: any(isinstance(a, dict) and a.get("start_worker", {}).get("parent") == "child"
                         and a["start_worker"]["task"] == "Check the pool limits" for a in host.actions))
        wait(lambda: js("return document.getElementById('worker-form').hidden"))
        assert any(a["start_worker"].get("permission") == "inherit" for a in host.actions
                   if isinstance(a, dict) and "start_worker" in a), "workers inherit by default"
        js("document.getElementById('start-worker').click()")
        wait(lambda: js("return [...document.getElementById('worker-host').options].some(o=>o.value==='build-id' && o.text.includes('64.0 GB free'))"))
        choose("worker-runtime","claude")
        choose("worker-host","build-id")
        choose("worker-model","")
        wait(lambda: js("return document.getElementById('worker-subscription-limits').innerText.includes('Build · Claude · five hour · 72% used')"))
        assert not js("return document.getElementById('worker-subscription-limits').innerText.includes('old week')")
        choose("worker-host","parent")
        assert js("return document.getElementById('worker-subscription-limits').hidden")
        choose("worker-host","build-id")
        choose("worker-model","build/only")
        assert js("return document.getElementById('worker-subscription-limits').hidden")
        js("document.getElementById('worker-task').value='Remote-only model'; document.getElementById('worker-form').requestSubmit()")
        wait(lambda: any(a.get("start_worker",{}).get("host")=="build-id" and a["start_worker"]["model"]=="build/only" for a in host.actions if isinstance(a,dict)))
        wait(lambda: js("return document.getElementById('worker-form').hidden"))
        js("document.getElementById('start-worker').click()")
        wait(lambda: js("return [...document.getElementById('worker-host').options].some(o=>o.value==='auto')"))
        choose("worker-runtime","claude")
        choose("worker-host","auto")
        choose("worker-model","build/only")
        js("document.getElementById('worker-memory').value='64'; document.getElementById('worker-cpus').value='8'; document.getElementById('worker-gpu').checked=true; document.getElementById('worker-os').value='linux'; document.getElementById('worker-task').value='Run on the build machine'; document.getElementById('worker-form').requestSubmit()")
        wait(lambda: any(a.get("start_worker",{}).get("needs")=={"memory_gb":64,"cpus":8,"gpu":True,"os":"linux"} and a["start_worker"]["host"]=="auto" for a in host.actions if isinstance(a,dict)))
        wait(lambda: js("return document.getElementById('worker-form').hidden"))
        js("document.getElementById('start-worker').click(); document.getElementById('worker-task').value='Unmet hardware'; document.getElementById('worker-form').requestSubmit()")
        wait(lambda: js("return !document.getElementById('worker-submit').disabled && !document.getElementById('worker-form').hidden && document.getElementById('worker-task').value==='Unmet hardware'") and any(a.get("start_worker", {}).get("task") == "Unmet hardware" for a in host.actions if isinstance(a,dict)))
        js("document.getElementById('worker-cancel').click()")
        print("PASS Chromium worker placement, target-only model and optional Auto requirements",flush=True)
        # The operator continues the open session in another route with the inherited mode.
        js("document.getElementById('continue-in').click()")
        js("const r=document.getElementById('handoff-runtime');for(const v of ['codex','claude']){r.value=v;r.dispatchEvent(new Event('change'));if(document.getElementById('handoff-route').options.length)break}"
           "document.getElementById('handoff-request').value='Carry on elsewhere';document.getElementById('handoff-form').requestSubmit()")
        wait(lambda: any(isinstance(a, dict) and a.get("start_handoff", {}).get("source") == "child"
                         and a["start_handoff"]["mode"] == "inherit" and not a["start_handoff"]["yolo_confirmed"]
                         and a["start_handoff"]["request"] == "Carry on elsewhere" for a in host.actions))
        # A runtime with no configured route never falls back to index 0 of another runtime.
        starts = lambda: sum(1 for a in host.actions if isinstance(a, dict) and "start_handoff" in a)
        before = starts()
        js("document.getElementById('continue-in').click();document.getElementById('handoff-route').replaceChildren();"
           "document.getElementById('handoff-request').value='Nowhere';document.getElementById('handoff-form').requestSubmit()")
        wait(lambda: js("return document.getElementById('message').textContent.includes('is configured on this host')"))
        time.sleep(0.3)
        assert starts() == before, "an empty route selection must not start a handoff"
        js("document.getElementById('handoff-cancel').click()")
        # The handoff marker and the folded block appear only on a handoff target's first user message.
        def bubbles(thread, title):
            js("document.getElementById('home').click()")
            wait(lambda: js(f"return !!document.querySelector('[data-thread-id={thread}]')"))
            js(f"document.querySelector('[data-thread-id={thread}]').click()")
            wait(lambda: js(f"return document.getElementById('title').textContent==={title!r} && document.querySelectorAll('#timeline .bubble.user').length>0"))
            return js("return [...document.querySelectorAll('#timeline .bubble.user')].map(b=>[!!b.querySelector('details.handoff-context'),b.querySelector('.body').textContent])")
        target = bubbles("handoff-target", "Continued pool work")
        assert target[0][0] and target[0][1].endswith("Carry on here"), target
        assert not target[1][0] and target[1][1].startswith("<ox-handoff>"), target
        assert js("return !document.getElementById('handoff-marks').hidden && document.getElementById('handoff-marks').textContent.includes('Continued from')")
        # Typing in the composer must not replace handoff-marks DOM nodes or jump the scroll position.
        assert js("""
            const card = document.querySelector('#handoff-marks .link-card');
            const bubble = document.querySelector('#timeline .bubble');
            const draft = document.getElementById('draft');
            draft.focus({ preventScroll: true });
            window.scrollTo(0, 42);
            const beforeScroll = window.scrollY;
            draft.value = 'Typing in a PWA without screen flash';
            draft.dispatchEvent(new Event('input'));
            return document.querySelector('#handoff-marks .link-card') === card &&
                   document.querySelector('#timeline .bubble') === bubble &&
                   window.scrollY === beforeScroll &&
                   !document.getElementById('send').disabled;
        """)
        typed = bubbles("typed-block", "Typed block")
        assert typed and not typed[0][0] and typed[0][1].startswith("<ox-handoff>"), typed
        assert js("return document.getElementById('handoff-marks').hidden")
        phone_report(js, port, base, keypress, capture, screenshot)
        antigravity_phone(host, js)
        # Cold discovery through the real New action: the Hello listed native subscriptions
        # and no API route, and the connected tab never refreshes. The unstarred provider
        # alias must still be offered for BOTH runtimes once its catalog lands.
        js("document.getElementById('home').click();document.getElementById('new').click()")
        wait(lambda: js(
            "const o=[...document.querySelectorAll('#route option')].map(x=>x.textContent);"
            "const g=[...document.querySelectorAll('#route optgroup')].map(x=>x.label);"
            "return g.includes('Subscriptions') && g.includes('All models') && "
            "['codex','claude'].every(r=>o.some(t=>t.toLowerCase().includes(r)&&t.includes('orchard')))"
        ))
        # The picker remembers Antigravity. The remaining draft and native command checks use Claude.
        def continue_on_claude():
            js("""const route=document.getElementById('route');
                route.value=[...route.options].find(o=>o.text==='Claude subscription').value;
                document.getElementById('new-form').requestSubmit();""")

        continue_on_claude()
        wait(
            lambda: js(
                "return document.getElementById('network').innerText==='Connected' && document.getElementById('title').innerText==='New session'"
            )
        )
        js("document.getElementById('access-mode').click()")
        mode("bypass_permissions")
        wait(lambda: {"set_next_access":"bypass_permissions"} in host.actions)
        wait(lambda: js("return document.getElementById('access-mode').dataset.bypass==='true'"))
        enter_custom_command("/project:lint src/mobile.rs")
        run_native_command()
        wait(lambda: {"native_command":[None, "/project:lint src/mobile.rs"]} in host.actions)
        js("document.getElementById('open-settings').click()")
        js(
            "const d=document.getElementById('draft');d.value='New session';d.dispatchEvent(new Event('input'));document.getElementById('composer').requestSubmit()"
        )
        wait(lambda: {"send_text": [None, "New session"]} in host.actions)
        wait(
            lambda: js(
                "return document.getElementById('title').innerText==='Newly created phone session'"
            )
        )
        # The first message of a new thread is followed at once by more typing; the
        # real thread's Opened must keep it, whichever of Ack and Opened comes first.
        def new_thread(first):
            host.release.clear()
            js("document.getElementById('home').click();document.getElementById('new').click()")
            continue_on_claude()
            wait(lambda: js("return document.getElementById('network').innerText==='Connected' && document.getElementById('title').innerText==='New session'"))
            type_draft(first)
            js("document.getElementById('composer').requestSubmit()")
            wait(lambda: {"send_text": [None, first]} in host.actions)
        type_draft("/new")
        js("document.getElementById('send').click()")
        wait(lambda: js("return document.getElementById('network').innerText==='Connected' && document.getElementById('title').innerText==='New session'"))
        type_draft("/goal Check mobile")
        js("document.getElementById('send').click()")
        wait(lambda: {"native_command":[None, "/goal Check mobile"]} in host.actions)
        wait(lambda: js("return document.getElementById('title').innerText==='new-goal' && !document.getElementById('draft').value"))
        new_thread("Ack first")
        wait(lambda: js("return document.getElementById('draft').value===''"))  # the Ack cleared it
        type_draft("follow-up one")
        host.release.set()  # Opened arrives after the Ack
        wait(lambda: js("return document.getElementById('title').innerText==='new-ack'"))
        assert js("return document.getElementById('draft').value")=="follow-up one"
        type_draft("")
        new_thread("Opened first")
        type_draft("Opened first and the rest")  # before either reply
        host.release.set()  # Opened, then the Ack
        wait(lambda: js("return document.getElementById('title').innerText==='new-opened'"))
        time.sleep(0.5)
        assert js("return document.getElementById('draft').value")=="Opened first and the rest"
        type_draft("")
        from phone_settings import exercise
        exercise(js, wait)

        def settings(op):
            return js("return import('./device.js').then(async m=>m.settingsRequest(await m.deviceKey(),"+json.dumps(op)+"))")
        snapshot=settings({"action":"read_rows"})
        assert any(p["id"]=="garden" for p in snapshot["registry"]["providers"])
        laptop=settings({"action":"read_behavior"})
        changed=dict(laptop["behavior"],reduce_motion=not laptop["behavior"]["reduce_motion"],
                     worker_approvals="operator", claude_start="plan", codex_start="workspace")
        assert settings({"action":"save_behavior","revision":laptop["revision"],"behavior":changed})=={"status":"applied"}
        assert settings({"action":"read_behavior"})["behavior"]==changed
        assert settings({"action":"save_behavior","revision":laptop["revision"],"behavior":laptop["behavior"]})["status"]=="changed_on_disk"
        revision=snapshot["revisions"]["favorites"]
        assert settings({"action":"edit","revision":revision,"edit":{"section":"favorites","models":["garden/orchard"]}})=={"status":"applied"}
        assert settings({"action":"edit","revision":revision,"edit":{"section":"favorites","models":[]}})["status"]=="changed_on_disk"
        assert settings({"action":"read_rows"})["favorites"]==["garden/orchard"]
        credential=next(c for c in snapshot["credentials"] if c["name"]=="GARDEN_TOKEN")
        canary="fixture-mobile-secret-only"
        assert settings({"action":"edit_key","name":credential["name"],"scope":credential["scope"],"value":canary,"revision":snapshot["revisions"]["keys"]})=={"status":"applied"}
        assert canary in (codex/"ox-api-keys.json").read_text()
        remaining=settings({"action":"read_rows"})
        assert canary not in json.dumps(remaining) and any(k["saved"] and k["name"]=="GARDEN_TOKEN" for k in remaining["credentials"])
        brave = {"name": "BRAVE_SEARCH_API_KEY", "scope": "https://api.search.brave.com/"}
        transfer = {"action": "import_keys", "revision": remaining["revisions"]["keys"],
                    "keys": [dict(brave, value="fixture-transfer-brave"),
                             {"name": "ALIBABA_API_KEY", "scope": "https://dashscope.aliyuncs.com/compatible-mode/v1",
                              "value": "fixture-transfer-alibaba"}]}
        assert settings(transfer) == {"status": "applied"}
        assert settings(transfer)["status"] == "changed_on_disk"
        after_transfer = settings({"action": "read_rows"})
        assert "fixture-transfer-brave" not in json.dumps(after_transfer)
        assert "fixture-transfer-alibaba" not in json.dumps(after_transfer)
        assert "alibaba/qwen3.8-flash" in after_transfer["models"]
        assert "fixture-transfer-brave" in (codex / "ox-api-keys.json").read_text()
        if copy_fixture:
            from key_copy import exercise as copy_keys
            copy_keys(js, wait, *copy_fixture[1:5])
            from key_copy import worker_links
            worker_links(js, wait, *copy_fixture[3:], harness, source_env)
        export = {"action": "export_keys", "selected": [brave], "revision": after_transfer["revisions"]["keys"]}
        assert js("return import('./device.js').then(async m=>m.settingsRequest(await m.deviceKey(),"
                  + json.dumps(export) + ").then(()=>false,e=>e.status===403))")
        docs=settings({"action":"read_advanced"})
        assert canary not in json.dumps(docs)
        registry=next(x for x in docs if x["id"]=="codeox.toml")
        assert settings({"action":"save_advanced","id":registry["id"],"revision":registry["revision"],"original":registry["text"],"text":registry["text"].replace('"Garden"','"Garden plots"')})=={"status":"applied"}
        assert "Garden plots" in (codex/"codeox.toml").read_text()
        paired=settings({"action":"devices"})
        assert len(paired["devices"])==1 and paired["self"]==paired["devices"][0]["id"]
        invitation=settings({"action":"pair_device"})
        assert "#pair=" in invitation["url"] and "<svg" in invitation["qr_svg"]
        device = admin(d, {"type": "status"})["devices"][0]
        assert settings({"action":"revoke_device","id":device["id"]})=={"status":"applied"}
        wait(lambda: js("return !document.getElementById('pair').hidden"))
        assert not admin(d, {"type": "status"})["devices"]
        assert js("return import('./device.js').then(async m=>m.settingsRequest(await m.deviceKey(),{action:'read_rows'}).then(()=>false,e=>e.status===403))")
        secret=invitation["url"].split("#pair=")[1]
        assert js("return import('./device.js').then(m=>m.pair("+json.dumps(secret)+",'Revoked invite').then(()=>false,e=>e.status===403))")
        print(
            "PASS mobile pairing, live stream, ask and question, steer, stop, child controls, new session, resume and revocation",
            flush=True,
        )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("harness", type=Path)
    parser.add_argument("--screenshot", type=Path)
    args = parser.parse_args()
    run(args.harness.resolve(), args.screenshot)
