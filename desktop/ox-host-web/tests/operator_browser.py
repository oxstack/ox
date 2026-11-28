"""A real paired mobile browser with a real PTY and an isolated project."""
import json
import os
from pathlib import Path
import time

GOOGLE_LOGIN_URL = (
    "https://accounts.google.com/o/oauth2/auth?access_type=offline"
    "&client_id=fixture.apps.googleusercontent.com&code_challenge=fixture&code_challenge_method=S256"
    "&redirect_uri=https%3A%2F%2Fantigravity.google%2Foauth-callback&response_type=code"
    "&scope=" + "+".join(["https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.email"] * 8)
    + "&state=fixture%2Bstate%2Fvalue"
)

def install_login_fixture(root):
    executable = root / "agy-fixture-bin" / "agy"
    executable.parent.mkdir()
    go = str(root / "continue-agy-login")
    # Hold the second write until the browser has received a partial screen.
    executable.write_text(f"""#!/usr/bin/env python3
from pathlib import Path
import sys, time
assert sys.argv[1:] == ["acp", "--login"]
url = {GOOGLE_LOGIN_URL!r}
print("Open this link in a browser to log in with Google:\\n", flush=True)
print("  " + url[:310], end="", flush=True)
while not Path({go!r}).exists():
    time.sleep(0.02)
print(url[310:] + "\\n\\nEnter the authorization code: ", end="", flush=True)
assert input() == "fixture-code"
""")
    executable.chmod(0o700)
    return executable.parent

def check_terminal(js, admin, wait, root, host):
    device = admin(root, {"type":"status"})["devices"][0]
    assert device["terminal"] is False
    project=Path(host.scope["project"])
    def open_panel():
        assert js("return import('/operator.js').then(m=>{m.openPhoneTerminal('shell',arguments[0]);return true;})",str(project))
    def text(id):
        return js("return document.getElementById(arguments[0]).textContent", id)
    def line(value):
        js("""const field=document.querySelector('#operator-input');field.value=arguments[0];document.querySelector('#operator-send').click();""",value)
    open_panel()
    wait(lambda:"must be enabled" in text("operator-state"))
    js("document.querySelector('#operator-close').click()")
    def grant(enabled):
        admin(root,{"type":"terminal_grant","id":device["id"],"enabled":enabled})
    grant(True)
    try:
        # A sign-in review can be cancelled without starting a native login.
        for preset, command in (("codex-login", "login chatgpt --device-auth"), ("claude-login", "login claude")):
            js("return import('/operator.js').then(m=>m.openPhoneTerminal(arguments[0]))", preset)
            wait(lambda:command in text("operator-review"))
            assert js("return !document.querySelector('#operator-run').hidden && document.querySelector('#operator-tools').hidden")
            js("document.querySelector('#operator-close').click()")
        js("return import('/operator.js').then(m=>m.openPhoneTerminal('antigravity-login'))")
        wait(lambda: "agy acp --login" in text("operator-review"))
        js("document.querySelector('#operator-run').click()")
        wait(lambda: "https://accounts.google.com/o/oauth2/auth" in text("operator-screen"))
        assert js("return document.getElementById('operator-link').hidden"), "Sign-in link appeared before the CLI finished writing it"
        (root / "continue-agy-login").touch()
        wait(lambda: js("return !document.getElementById('operator-link').hidden"))
        assert js("return document.getElementById('operator-link').getAttribute('href')") == GOOGLE_LOGIN_URL
        line("fixture-code")
        wait(lambda: "exited with status 0" in text("operator-state"))
        assert js("return document.getElementById('operator-link').hidden")
        js("document.querySelector('#operator-close').click()")
        print("PASS Antigravity sign-in waits for the complete OAuth URL and preserves its bytes.", flush=True)
        # A normal session challenge must not authorize an operator socket.
        assert js("""return (async()=>{
          const {proof,deviceKey}=await import('/device.js'),auth=await proof(await deviceKey(),'connect');
          const ws=new WebSocket('wss://'+location.host+'/api/operator');
          return await new Promise(resolve=>{ws.onopen=()=>ws.send(JSON.stringify({proof:auth.proof,request:JSON.stringify({preset:'shell'})}));
          ws.onmessage=()=>{ws.close();resolve(false)};ws.onclose=()=>resolve(true);});})()""")
        open_panel()
        wait(lambda:"/bin/sh" in text("operator-review"))
        assert str(project) in text("operator-review") and js("return document.querySelector('#operator-tools').hidden")
        assert js("return document.querySelector('#operator-run').getBoundingClientRect().height>=44")
        js("document.querySelector('#operator-run').click()")
        wait(lambda:js("return !document.querySelector('#operator-tools').hidden"))
        def input_type():
            return js("return document.getElementById('operator-input').type")
        def visibility():
            js("document.getElementById('operator-visibility').click()")
        wait(lambda:input_type()=="text")
        visibility()
        assert input_type()=="password"
        visibility()
        assert input_type()=="text"
        # Simulate the visual viewport available above a phone's on-screen keyboard.
        assert js("""const panel=document.querySelector('#operator-panel');
            panel.style.setProperty('--terminal-height','330px');
            try {
              const bounds=key=>document.querySelector(key).getBoundingClientRect();
              const sheet=bounds('#operator-panel'), close=bounds('#operator-close'), send=bounds('#operator-send');
              return close.top>=sheet.top && close.height>=44 && send.height>=44 && send.bottom<=sheet.bottom
                && sheet.bottom<=331 && sheet.top>=-1 && panel.scrollWidth<=panel.clientWidth+1
                && bounds('#operator-screen').height>40
                && [...panel.querySelectorAll('[data-key]')].every(b=>b.getBoundingClientRect().width>=44);
            } finally {panel.style.removeProperty('--terminal-height')}""")
        # The marker starts only after Run; the entered canary must never appear on the noecho screen.
        line("test -t 0 && test -t 1 && pwd -P > cwd; echo $$ > pid; stty -echo; printf 'MarkerInput> '; read -r value; stty echo; test -n \"$value\" && touch received")
        wait(lambda:"MarkerInput>" in text("operator-screen") and (project/"cwd").exists())
        assert (project/"cwd").read_text().strip()==str(project.resolve())
        secret="private-fixture-input-7ad"
        wait(lambda:input_type()=="password")
        visibility()
        assert input_type()=="text"
        visibility()
        assert input_type()=="password"
        line(secret)
        wait((project/"received").exists)
        wait(lambda:input_type()=="text")
        assert secret not in text("operator-screen") and secret not in json.dumps(host.actions)
        pid=int((project/"pid").read_text())
        grant(False)
        wait(lambda:"disconnected" in text("operator-state").lower())
        assert not text("operator-screen")
        def gone():
            try:os.kill(pid,0)
            except ProcessLookupError:return True
            return False
        wait(gone)
        js("document.querySelector('#operator-close').click()")
        open_panel()
        wait(lambda:"must be enabled" in text("operator-state"))
        js("document.querySelector('#operator-close').click()")
        for f in root.glob("*.json"):
            assert secret not in f.read_text()
    finally:
        grant(False)
    print("PASS per-device terminal grant, distinct signed proof, touch review, private input, PTY/cwd, revoke and process cleanup.",flush=True)
