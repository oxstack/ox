"""Pair a second service and transfer reviewed keys using the web controls."""
from contextlib import ExitStack
from pathlib import Path
import json
import os
import shutil
import ssl
import subprocess


def repo(path, origin):
    path.mkdir(parents=True)
    subprocess.run(["git", "init", "--quiet", str(path)], check=True)
    subprocess.run(["git", "-C", str(path), "remote", "add", "origin", origin], check=True)


def receiver(d, stack):
    from native_client import certificate, session_host
    from browser import Forward, ThreadedTCP, admin, managed_server, wait
    root = d / "receiver"
    root.mkdir()
    home = root / "home"
    home.mkdir()
    package = root / "bin"
    package.mkdir()
    for var, name in [("OX_TEST_HOST_BIN", "ox-host"), ("OX_TEST_SETTINGS_OX", "ox")]:
        shutil.copy2(os.environ[var], package / name)
    env = dict(HOME=str(home), CODEX_HOME=str(home / ".codex"), XDG_STATE_HOME=str(home / "state"),
               PATH=str(package), OX_NO_BACKGROUND_UPDATE="1", SHELL="/bin/sh")
    (package / "git").symlink_to(shutil.which("git"))
    if codex := os.environ.get("CODEOX_TEST_CODEX_BIN"):
        from http.server import ThreadingHTTPServer
        from stock_session import Fixture
        vendor = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        vendor.mode = "linked-worker"
        vendor.project = root
        managed_server(stack, vendor)
        (package / "codex").symlink_to(Path(codex).resolve())
        env.update(PATH=str(package) + ":/usr/bin:/bin", OX_LINK_FIXTURE_KEY="fixture-only", SHELL="/bin/bash")
        for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            env[name] = env[name.lower()] = "http://127.0.0.1:1"
        env["NO_PROXY"] = env["no_proxy"] = "127.0.0.1,localhost"
        config = Path(env["CODEX_HOME"])
        config.mkdir()
        (config / "codeox.toml").write_text('[providers.fixture]\nname="Fixture"\nwire_api="chat_completions"\n'
            f'base_url="http://127.0.0.1:{vendor.server_port}/v1"\nenv_key="OX_LINK_FIXTURE_KEY"\ndiscovery=false\n'
            '[[providers.fixture.models]]\nid="demo"\ncontext_window=131072\n')
    project = root / "build-checkout"
    repo(project, "git@code.example.test:owner/ox.git")
    (project / "base.bin").write_bytes(os.urandom(2 * 1024 * 1024))
    (project / "nested").mkdir()
    (project / "nested/tracked").write_text("target base")
    (project / ".gitignore").write_text(".env\n")
    subprocess.run(["git", "-C", project, "add", "."], check=True)
    subprocess.run(["git", "-C", project, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test",
                    "-c", "commit.gpgsign=false", "commit", "-qm", "shared base"], check=True)
    process=stack.enter_context(ExitStack())
    process.enter_context(session_host(package / "ox-host", root, env))
    state = Path(env["OX_HOST_STATE_DIR"])
    wait(lambda: (state / "remote-admin.sock").exists())
    listed = subprocess.check_output([package / "ox", "host", "project", "add", project], env=env, text=True)
    assert listed.strip() == str(project)
    assert not (home / "state/ox/desktop/workspace.json").exists()
    public = ThreadedTCP(("127.0.0.1", 0), Forward)
    port = public.server_address[1]
    name = "build.fixture.ts.net"
    origin = f"https://{name}:{port}"
    def job_fault(first, body):
        if first.startswith(b"GET /api/worker-result ") and (root / "hold-result").exists():
            return b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        if not first.startswith(b"POST /api/worker-job "):
            return None
        job=json.loads(json.loads(body)["request"])
        if (root / "hold-start").exists() and job["operation"]["operation"]=="start":
            (root / "start-rejected").touch()
            return b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        if (root / "malformed-notices").exists():
            reply=json.dumps({"Ok":{"job_id":job["job"],"status":"running","linked_notices":"broken"}}).encode()
            return f"HTTP/1.1 200 OK\r\nContent-Length: {len(reply)}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n".encode()+reply
    public.job_fault = job_fault
    public.backend = admin(state, {"type": "enable", "port": 0, "origin": origin})["port"]
    invite = admin(state, {"type": "pair", "login": "phone@example.test"})["pairing"]["url"]
    certificate(root, name)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.num_tickets = 0
    tls.load_cert_chain(root / "server.pem", root / "server.key")
    public.socket = tls.wrap_socket(public.socket, server_side=True)
    managed_server(stack, public)
    # The source checks the peer owner against its local Tailscale status.
    status = {"BackendState": "Running", "Self": {"UserID": 1}, "User": {"1": {"LoginName": "phone@example.test"}},
              "Peer": {"receiver": {"UserID": 1, "DNSName": name + "."},
                       "unrelated": {"UserID": 2, "DNSName": "other.fixture.ts.net."}}}
    ts_bin = root / "source-tools"
    ts_bin.mkdir()
    tool = ts_bin / "tailscale"
    tool.write_text("#!/bin/sh\ncat <<'STATUS'\n" + json.dumps(status) + "\nSTATUS\n")
    tool.chmod(0o700)
    def restart():
        process.close()
        process.enter_context(session_host(package / "ox-host",root,env))
        public.backend=wait(lambda: admin(state,{"type":"status"}).get("port"))
    return (dict(PATH=str(ts_bin) + os.pathsep + os.environ["PATH"], OX_TEST_ORIGIN=origin, OX_TEST_NATIVE_DIR=str(root),
                 OX_TEST_LOOPBACK=f"127.0.0.1:{port}"), invite, home / ".codex/ox-api-keys.json", state, d, restart)


def exercise(js, wait, invite, saved, receiver_state, source):
    from browser import admin
    def go(label):
        wait(lambda: js("""const btn = [...document.querySelectorAll('#settings button')]
            .find(b => b.textContent.trim() === arguments[0] && b.getClientRects().length && !b.disabled);
            if (!btn) return false; btn.click(); return true;""", label))

    def select_keys():
        js("""for(const label of document.querySelectorAll('#settings label')) {
            const box=label.querySelector('input[type=checkbox]');
            if(box) box.checked=/Brave Search ·|ALIBABA_API_KEY/.test(label.textContent);
        } document.querySelector('#settings form').dispatchEvent(new Event('change',{bubbles:true}));""")

    js("""window.originalTransferFetch = window.fetch;
        window.transferExposedValue = false;
        window.observedKeySend = false;
        window.fetch = async function(resource, init) {
            const response = await originalTransferFetch.call(this, resource, init);
            if (new URL(resource, location.href).pathname === '/api/settings') {
                const operation = JSON.parse(JSON.parse(init.body).request);
                if (operation.action === 'send_key_transfer') window.observedKeySend = true;
                if (/fixture-transfer-(brave|alibaba)/.test(String(init?.body)) ||
                    /fixture-transfer-(brave|alibaba)/.test(await response.clone().text()))
                    window.transferExposedValue = true;
            }
            return response;
        };""")
    js("document.getElementById('open-phone-settings').click()")
    go("API keys")
    go("Copy keys to another host…")
    wait(lambda: js("return !!document.querySelector('#settings input[name=destination]')"))
    field = "document.querySelector('#settings input[name=destination]').value=arguments[0]"
    js(field, invite.replace("build.fixture.ts.net", "other.fixture.ts.net"))
    go("Pair destination")
    wait(lambda: js("return document.querySelector('#settings').innerText.includes('owned by your Tailscale login')"))
    js(field, invite)
    go("Pair destination")
    try:
        wait(lambda: js("return [...document.querySelectorAll('#settings label')].some(l=>l.textContent.includes('Brave Search ·'))"))
    except AssertionError as exc:
        raise AssertionError(js("return document.querySelector('#settings').innerText")) from exc
    assert js("return document.querySelector('#settings').innerText.includes('incur charges')")
    select_keys()
    # A key rotation between the review and send must leave the destination untouched.
    request_key = """const query=arguments[0]; return import('./device.js').then(async m=>
        m.settingsRequest(await m.deviceKey(),query))"""
    snapshot = js(request_key, {"action": "read_rows"})
    revision = snapshot["revisions"]["keys"]
    garden = next(k for k in snapshot["credentials"] if k["name"] == "GARDEN_TOKEN")
    js(request_key, {"action": "edit_key", "name": garden["name"], "scope": garden["scope"],
                     "value": "fixture-rotated", "revision": revision})
    go("Send selected keys")
    wait(lambda: js("return document.querySelector('#settings').innerText.includes('changed. Review the transfer again.')"))
    assert not saved.exists()
    go("Refresh review")
    wait(lambda: js("return !!document.querySelector('#settings input[type=checkbox]')"))
    select_keys()
    go("Send selected keys")
    wait(lambda: js("return document.querySelector('#settings').innerText.includes('Sent 2 keys to ')"))
    assert js("return document.querySelector('#settings').innerText.includes('1 key still needs endpoint setup')")
    config = saved.with_name("codeox.toml").read_text()
    assert '[providers.alibaba]' in config and 'fixture-transfer-alibaba' not in config
    content = saved.read_text()
    assert "fixture-transfer-alibaba" in content
    assert "fixture-transfer-brave" in content and "GARDEN_TOKEN" not in content
    assert not js("return [document.body.textContent,JSON.stringify(localStorage),JSON.stringify(sessionStorage)].some(t=>t.includes('fixture-transfer-brave'))")
    go("Copy keys to another host…")
    wait(lambda: js("return [...document.querySelectorAll('#settings button')].some(b=>b.textContent.includes('build.fixture.ts.net'))"))
    js("[...document.querySelectorAll('#settings button')].find(b=>b.textContent.includes('build.fixture.ts.net')).click()")
    wait(lambda: js("return [...document.querySelectorAll('#settings label')].some(l=>l.textContent.includes('Replace key'))"))
    assert js("return ![...document.querySelectorAll('#settings label')].find(l=>l.textContent.includes('Brave Search ·')).querySelector('input').checked")
    # The desktop directory has the same on-disk schema. Move the existing
    # working registration into it to exercise reuse and independent web repair.
    web = source / "key-destinations.json"
    desktop = source / "home/state/ox/desktop/hosts.json"
    assert not desktop.exists()
    web.rename(desktop)
    original = desktop.read_bytes()
    host = json.loads(original)[0]["id"]
    review_query = {"action": "review_key_transfer", "host": host}
    old = js(request_key, review_query)["pairing"]
    fresh = admin(receiver_state, {"type": "pair", "login": "phone@example.test"})["pairing"]["url"]
    js(request_key, {"action": "pair_key_destination", "address": fresh, "code": ""})
    assert js(request_key, review_query)["pairing"] != old
    assert desktop.read_bytes() == original
    assert len(admin(receiver_state, {"type": "status"})["devices"]) == 2
    web.unlink()
    assert js(request_key, review_query)["pairing"] == old
    assert js("window.fetch = window.originalTransferFetch; return window.observedKeySend && !window.transferExposedValue")
    print("PASS web UI copies selected keys, configures Alibaba on the destination, rejects stale reviews and foreign-owned peers, and preserves desktop pairings", flush=True)


def worker_links(js, wait, state, source, restart, harness, source_env):
    from browser import admin
    # The source web service uses a mock engine in this fixture.
    source_id = (source / "host-id").read_text().strip()
    invite = admin(state, {"type": "worker_invitation", "login": "phone@example.test"})["pairing"]["url"]

    def click(label):
        wait(lambda: js("""const button=[...document.querySelectorAll('#settings button')]
            .find(b => b.textContent.trim()===arguments[0] && b.getClientRects().length && !b.disabled);
            if (!button) return false; button.click(); return true;""", label))

    # Reopen Settings at its top-level menu.
    js("document.getElementById('open-phone-settings').click()")
    click("Host updates")
    form = "#settings select[name="
    wait(lambda: js("return document.querySelector(arguments[0])?.value",form+"mode]") == "automatic")
    def select(name, value):
        js("""const s=document.querySelector(arguments[0]); s.value=arguments[1];
            s.dispatchEvent(new Event('change'));""",form+name+"]",value)
        wait(lambda: js("const e=document.querySelector(arguments[0]); return e && !e.disabled && e.value===arguments[1]",
                        form+name+"]",value))
    select("channel", "dev")
    select("mode", "manual")
    click("Update when idle")
    click("Cancel scheduled update")
    wait(lambda: js("return document.querySelector('#settings').innerText.includes('Update when idle')"))
    prefs = json.loads((source / "managed-host-fixture/update-preferences.json").read_text())
    assert (prefs["mode"],prefs["channel"],prefs["scheduled"]) == ("manual","dev",False)
    select("mode", "download")
    select("channel", "stable")
    click("← Settings")
    click("Remote access")
    click("Worker host links")
    wait(lambda: js("return !!document.querySelector('input[name=worker_address]')"))
    js("document.querySelector('input[name=worker_address]').value=arguments[0]", invite)
    click("Link worker host")
    try:
        wait(lambda: js("return document.querySelector('#settings').innerText.includes('Remove outgoing link')"))
    except AssertionError as exc:
        raise AssertionError(js("return document.querySelector('#settings').innerText")) from exc
    click("← Remote access")
    click("← Settings")
    click("← Sessions")
    js("document.getElementById('open-hosts').click()")
    wait(lambda: js("""const host=document.querySelector('#settings .phone-settings-host-open');
        return host?.href===new URL(arguments[0]).origin+'/' && host.target==='_blank' &&
            host.rel.includes('noopener') && document.getElementById('settings').innerText.includes('Pair this phone there');""", invite))
    click("← Sessions")
    js("""window.fixtureFetch=window.fetch;
        window.fetch=(url,...options)=>String(url).endsWith('/api/settings')
            ? Promise.reject(new TypeError('Host unavailable'))
            : window.fixtureFetch(url,...options);""")
    try:
        js("document.getElementById('open-hosts').click()")
        wait(lambda: js("""const host=document.querySelector('#settings .phone-settings-host-open');
            return host?.href===new URL(arguments[0]).origin+'/' &&
                document.getElementById('settings').innerText.includes('Saved address');""", invite))
    finally:
        js("window.fetch=window.fixtureFetch;delete window.fixtureFetch;")
    click("Retry host list")
    click("Manage worker hosts")
    wait(lambda: js("return document.querySelector('#settings .back').textContent==='← Hosts'"))
    report = admin(state, {"type": "status"})
    rows = report["devices"]
    link, = report["worker_hosts"]
    assert link["worker"] == {"source": source_id, "may_launch": True, "ceiling": "yolo"}
    assert not link["terminal"]
    local_view = admin(state, {"type": "worker_links", "command": {"action": "worker_links"}})["worker_links"]
    assert [d["id"] for d in local_view["incoming"]] == [link["id"]]
    edited = admin(state, {"type": "worker_access", "id": link["id"], "before": link["worker"],
                          "may_launch": False, "ceiling": "read"})
    assert edited["worker_hosts"][0]["worker"] == dict(link["worker"],may_launch=False,ceiling="read")
    repo(source / "inventory-source/ox", "https://user:fixture-secret@code.example.test/owner/ox")
    repo(source / "another/ox", "https://code.example.test/different/ox")
    subprocess.run([harness, "--exact", "inventory::tests::paired_inventory", "--ignored"],
                   env=source_env, check=True, timeout=45)
    target = source / "receiver/build-checkout"
    parent = source / "inventory-source/ox"
    def git(path, *args):
        return subprocess.check_output(["git", "-C", path, *args])
    git(parent, "fetch", "--quiet", target, "HEAD")
    git(parent, "checkout", "--quiet", "-B", "laptop", "FETCH_HEAD")
    (parent / "nested/tracked").write_text("staged")
    git(parent, "add", "nested/tracked")
    (parent / "nested/tracked").write_text("uncommitted on laptop")
    private = os.urandom(600_000)
    (parent / "nested/input").write_bytes(private)
    (parent / ".env").write_text("ignored")
    def state_of(path):
        return (git(path, "status", "--porcelain=v1"), git(path, "diff", "--cached"), git(path, "show-ref"))
    before = (state_of(parent), state_of(target))
    def transfer(env):
        subprocess.run([harness,"--exact","worker_checkout::tests::linked_checkout","--ignored"],
                       env=env, check=True, timeout=60)
    transfer(dict(source_env, OX_TEST_DENIED_LINK="1"))
    assert not (state / "linked-work/jobs").exists()
    admin(state, {"type": "worker_access", "id": link["id"], "before": edited["worker_hosts"][0]["worker"],
                  "may_launch": True, "ceiling": "read"})
    transfer(source_env)
    job = state / "linked-work/jobs" / source_id / "42"
    work = job / "checkout"
    assert (job.parent / "45/checkout/nested/tracked").read_text()=="uncommitted on laptop"
    receipt = json.loads((job / "receipt.json").read_text())
    assert bytes(receipt["cwd"]) == b"nested"
    assert receipt["receipt"]["commit"] == git(work, "rev-parse", "HEAD").decode().strip()
    assert git(work,"branch","--show-current").strip() == b"ox/42"
    assert git(work,"remote","get-url","origin").strip() == b"git@code.example.test:owner/ox.git"
    assert (work / "nested/tracked").read_text() == "uncommitted on laptop"
    assert (work / "nested/input").read_bytes() == private and not (work / ".env").exists()
    git(work, "add", "-A")
    assert git(work,"diff","--cached","--name-only") == b""
    assert before == (state_of(parent),state_of(target))
    if os.environ.get("CODEOX_TEST_CODEX_BIN"):
        subprocess.run([harness, "--exact", "linked_jobs::tests::read_worker", "--ignored", "--nocapture"],
                       env=source_env, check=True, timeout=110)
        from linked_workers import parent_restart
        parent_restart(source / "real-parent", parent / "nested", state, source_env, restart)
    enabled = next(r["worker"] for r in admin(state, {"type":"status"})["worker_hosts"] if r["id"] == link["id"])
    with subprocess.Popen([harness, "--exact", "worker_checkout::sender::tests::interrupted_checkout", "--ignored"],
                          env=source_env) as upload:
        wait(lambda: (source / "upload-started").exists() and any(job.parent.glob(".incoming-*")))
        admin(state, {"type": "worker_access", "id": link["id"], "before": enabled,
                      "may_launch": False, "ceiling": "read"})
        assert upload.wait(timeout=12) == 0
    wait(lambda: not any(job.parent.glob(".incoming-*")))
    assert not (job.parent / "44").exists()
    current = next(r["worker"] for r in admin(state, {"type": "status"})["worker_hosts"] if r["id"] == link["id"])
    granted = admin(state, {"type": "worker_access", "id": link["id"], "before": current,
                            "may_launch": current["may_launch"], "ceiling": current["ceiling"], "may_sync_memory": True})
    assert granted["worker_hosts"][0]["worker"]["may_sync_memory"] is True
    ungranted = admin(state, {"type": "worker_access", "id": link["id"], "before": granted["worker_hosts"][0]["worker"],
                              "may_launch": current["may_launch"], "ceiling": current["ceiling"], "may_sync_memory": False})
    assert "may_sync_memory" not in ungranted["worker_hosts"][0]["worker"]
    print("PASS signed checkout transfer, shared-base negotiation, dirty and untracked inputs, retry and link admission", flush=True)
    click("Remove outgoing link")
    wait(lambda: not admin(state, {"type":"status"}).get("worker_hosts"))
    assert {row["id"] for row in rows if row["class"] == "desktop"} == {row["id"] for row in admin(state, {"type":"status"})["devices"]}
    known = json.loads((source / "home/state/ox/desktop/hosts.json").read_text())[0]
    click("Link " + known["name"])
    wait(lambda: bool(admin(state, {"type":"status"}).get("worker_hosts")))
    click("Remove outgoing link")
    wait(lambda: not admin(state, {"type":"status"}).get("worker_hosts"))
    print("PASS web update modes and channels, idle scheduling and cancellation, worker link invitations, one-click pairing, local grant editing and independent revocation", flush=True)
