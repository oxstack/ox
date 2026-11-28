"""Close the actual desktop during a stock tool approval, approve from a paired PWA and reopen."""
import argparse
from contextlib import contextmanager, ExitStack
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time

from claude_stock_session import setup
from host_fixture import session_host
from mixed_runtime_window import Window

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ox-host-web/tests"))
from browser import admin, answer_from_phone, paired_browser, wait


@contextmanager
def desktop_window(gui, root, env, project, name):
    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as probe:
        probe.bind(str(root / f"probe-{name}"))
        probe.settimeout(1)
        env = dict(env, OX_DESK_STATS_SOCKET=str(root / f"stats-{name}"))
        with subprocess.Popen([gui, "--desk-stats", project], env=env) as app:
            try:
                window = Window(root, app, env["OX_DESK_STATS_SOCKET"], probe)
                window.wait(lambda s: s["connected"], "desktop subscribed to session host")
                yield window
            finally:
                app.terminate()
                app.wait(timeout=8)


def run_reviewed_fixture(window, marker, expected):
    app_window = window.id
    window.id = window.command("xdotool", "search", "--sync", "--onlyvisible", "--all", "--pid",
                               str(window.app.pid), "--name", "^OX Terminal$").splitlines()[-1]
    try:
        window.command("xdotool", "windowfocus", window.id)
        window.word(expected[-2])
        assert not marker.exists(), "Original request or unconfirmed terminal review executed"
        # Run is the terminal review's first focusable control.
        window.key("Tab")
        window.key("Return")
        window.word("Exited")
        assert marker.read_text().splitlines() == expected
        window.word("Close", click=True)
    finally:
        window.id = app_window
        window.command("xdotool", "windowfocus", window.id)


def capture(window, name):
    """Save the app's view and a screenshot for the Linux gate artifact."""
    print(f"desktop state at {name}:", json.dumps(window.snapshot()), flush=True)
    if temp := os.environ.get("RUNNER_TEMP"):
        window.command("import", "-window", window.id, str(Path(temp) / f"continuity-{name}.png"))


def open_transcript_find(window):
    """Open transcript find and wait for its bar before typing a query.

    Wait for the bar's Previous button: OCR on ubuntu-24.04-arm does not read
    the grey "Find in transcript" placeholder even while the field is open
    and focused, which a screenshot from that runner showed.
    """
    window.key("ctrl+f")
    try:
        window.word("Previous")
    except AssertionError:
        capture(window, "find")
        raise


def local_tool_terminal(window, provider, root, project):
    import shlex
    program = root / "bin/sudo"
    # Exercise stock Bash's real no-TTY failure without touching the system sudo.
    program.write_text("#!/bin/sh\n" +
                       'if ! test -t 0 || ! test -t 1; then echo "sudo: a terminal is required to read the password" >&2; exit 91; fi\n' +
                       '{ pwd -P; printf "%s\\n" "$@"; } >> terminal-witness\n' +
                       'echo PrivateTerminalOutput\n')
    program.chmod(0o700)
    marker = project / "terminal-witness"
    provider.review_command = "sudo -- fixture-check 'two words'"
    window.prompt("TERMINAL-REVIEW manual command")
    window.wait(lambda s: s["selected_requests"], "stock command approval")
    window.word("Decline", same_row="review", click=True)
    run_reviewed_fixture(window, marker, [str(project.resolve()), "--", "fixture-check", "two words"])
    window.wait(lambda s: not s["selected_requests"] and not s["visible_turn"], "original native request denied")
    marker.unlink()
    provider.review_command = f"{shlex.quote(str(program))} -- fixture-failed 'two words'"
    window.prompt("TERMINAL-REVIEW failed command")
    window.wait(lambda s:s["selected_requests"], "stock non-PTY command request")
    window.key("ctrl+alt+1")
    window.wait(lambda s:not s["selected_requests"] and not s["visible_turn"], "stock shell returned its terminal-required diagnostic")
    assert not marker.exists()
    # Native transcript search expands the exact matching tool and scrolls it into view.
    # Earlier approvals can have another identically titled Terminal header on screen.
    open_transcript_find(window)
    window.text("fixture-failed")
    try:
        window.press("Run in OX terminal")
    except AssertionError:
        capture(window, "run")
        raise
    run_reviewed_fixture(window, marker, [str(project.resolve()), "--", "fixture-failed", "two words"])
    assert "PrivateTerminalOutput" not in json.dumps(provider.requests)
    print("PASS real approval and failed tool handoffs, tty, argv, cwd, exactly one manual execution, private output.", flush=True)


def phone_send(js, text):
    value=json.dumps(text)
    wait(lambda: js(f"""const d=document.querySelector('#draft'); d.value={value};
        d.dispatchEvent(new Event('input')); if(document.querySelector('#send').disabled)return false;
        document.querySelector('#composer').requestSubmit();return true;"""), 25)


def live_session(window, provider, env, root):
    import threading
    beta = root / "Beta"
    marker = beta / "approved.txt"
    provider.release_stream = threading.Event()
    with tempfile.TemporaryDirectory() as tmp, ExitStack() as resources:
        resources.callback(provider.release_stream.set)
        js, _, _ = paired_browser(Path(tmp), Path(env["OX_HOST_STATE_DIR"]), resources)
        js("document.querySelector('#new').click()")
        selected = js(f"""const p=document.querySelector('#project'),r=document.querySelector('#route');
          const project=[...p.options].find(o=>o.textContent==={json.dumps(str(beta))});
          const route=[...r.options].find(o=>o.textContent==='Claude · fixture/demo');
          if(!project||!route)return [Array.from(p.options,o=>o.textContent),Array.from(r.options,o=>o.textContent)];
          p.value=project.value;r.value=route.value;document.querySelector('#new-form').requestSubmit();return true;""")
        assert selected is True, selected
        phone_send(js,"Marigold APPROVAL shared")
        wait(lambda: js("return document.querySelector('#asks .ask')?.innerText.includes('approved.txt')"), 30)
        # Opening the rendered sidebar row must work while native-history discovery stays idle.
        window.word("Marigold", click=True)
        s=window.wait(lambda s:s["selected_requests"]==1 and s["visible_turn"], "phone-created Beta thread with the same approvable tool")
        thread=s["selected_thread"]
        assert not marker.exists()
        window.key("ctrl+alt+1")
        wait(lambda: marker.exists() and js("return !document.querySelector('#asks .ask')"), 30)
        assert marker.read_text().strip()=="accepted"
        wait(lambda: js("return document.querySelector('#timeline').innerText.includes('Fixture completed.')"),30)
        window.wait(lambda s:s["selected_thread"]==thread and not s["visible_turn"], "completed phone thread")
        window.prompt("LIVE-SPLIT desktop message")
        wait(lambda: js("return document.querySelector('#timeline').innerText.includes('LIVE-SPLIT desktop message') && document.querySelector('#timeline').innerText.includes('Copperstream')"),30)
        window.word("Copperstream")
        assert not js("return document.querySelector('#timeline').innerText.includes('Sunbeam')")
        provider.release_stream.set()
        wait(lambda: js("return document.querySelector('#timeline').innerText.includes('Copperstream Sunbeam')"),30)
        window.wait(lambda s:s["selected_thread"]==thread and not s["visible_turn"], "both clients received the final chunk")
        window.word("Sunbeam")
        js("document.querySelector('#access-mode').click()")
        wait(lambda: js("""const p=document.querySelector('#permission');const option=[...p.options].find(o=>o.textContent.includes('YOLO'));
            if(!option)return false;p.value=option.value;p.dispatchEvent(new Event('change'));document.querySelector('#apply-permission').click();return true;"""))
        wait(lambda: js("return document.querySelector('#access-mode').dataset.bypass==='true'"),30)
        window.word("YOLO", bottom=True)
        marker.unlink()
        phone_send(js,"APPROVAL verify confirmed full access")
        wait(marker.exists,30)
        assert marker.read_text().strip()=="accepted"
        window.wait(lambda s:s["selected_thread"]==thread and not s["visible_turn"] and not s["selected_requests"], "confirmed YOLO allowed native tool")
        window.word("YOLO", click=True, bottom=True)
        window.word("Default", click=True, bottom=True)
        wait(lambda: js("return document.querySelector('#access-mode').textContent.trim()==='Default'"),30)
        marker.unlink()
        phone_send(js,"APPROVAL manual stop")
        window.wait(lambda s:s["selected_thread"]==thread and s["selected_requests"], "baseline permission restored")
        wait(lambda: js("return !!document.querySelector('#asks .ask')"), 30)
        js("document.querySelector('#stop').click()")
        wait(lambda: js("return !document.querySelector('#asks .ask') && document.querySelector('#stop').disabled"),30)
        window.wait(lambda s:s["selected_thread"]==thread and not s["visible_turn"] and not s["selected_requests"], "phone Stop reached stock")
        assert not marker.exists(), "Stopped tool ran"
        host_state=Path(env["OX_HOST_STATE_DIR"])
        device=next(d for d in admin(host_state,{"type":"status"})["devices"] if d["label"]=="Fixture phone")
        admin(host_state, {"type":"terminal_grant","id":device["id"],"enabled":True})
        wait(lambda:js("const b=document.querySelector('#operator-session');if(b.hidden)return false;b.click();return true;"))
        wait(lambda:js("return !document.querySelector('#operator-run').hidden && document.querySelector('#operator-review').textContent.includes('--resume')"))
        preview=js("return document.querySelector('#operator-review').textContent")
        assert str(beta) in preview and "fixture/demo" in preview and thread.split(":")[-1] in preview,preview
        js("document.querySelector('#operator-run').click()")
        wait(lambda:js("return !document.querySelector('#operator-tools').hidden"),30)
        wait(lambda:js("return document.querySelector('#operator-screen').textContent.trim().length>0"),30)
        js("document.querySelector('#operator-close').click()")
        phone_send(js,"Back from native terminal")
        wait(lambda:js("return document.querySelector('#timeline').innerText.includes('Back from native terminal')"),30)
        window.wait(lambda s:s["selected_thread"]==thread and not s["visible_turn"],"host owns original thread after operator terminal exit")
        wait(lambda:any("Back from native terminal" in json.dumps(entry["messages"]) for entry in provider.requests),30)
        print("PASS signed native terminal handoff and return to the same stock Claude billing route.",flush=True)

    print("PASS cross-project session appears live, bidirectional prompt and stream, approval, confirmed YOLO/restore, and phone Stop.",flush=True)


def run(gui, claude, root, capture=None):
    with ExitStack() as stack:
        provider, env = setup(root, claude, stack)
        project = root / "Alpha"
        state = root / "state"
        workspace = state / "ox/desktop/workspace.json"
        workspace.parent.mkdir(parents=True)
        workspace.write_text(json.dumps({"project": str(project), "recent": [
            {"project": str(project), "runtime": "claude", "api_model": "fixture/demo"},
            {"project": str(root / "Beta"), "runtime": "claude", "api_model": "fixture/demo"}
        ]}))
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        env.update(DISPLAY=os.environ["DISPLAY"], XDG_SESSION_TYPE="x11", LANG="C.UTF-8",
                   XDG_STATE_HOME=str(state), XDG_RUNTIME_DIR=str(runtime), OX_DESK_STATS_SOCKET=str(root / "stats"))
        for key in ("XAUTHORITY","VK_DRIVER_FILES","LD_LIBRARY_PATH"):
            if key in os.environ:
                env[key] = os.environ[key]
        if node := shutil.which("node"):
            (root / "bin/node").symlink_to(node)
        with session_host(gui.with_name("ox-host"), root, env):
            with desktop_window(gui,root,env,project,"first") as window:
                window.prompt("APPROVAL Help me water the garden")
                snap = window.wait(lambda s: s["selected_requests"] and s["visible_turn"], "stock Claude approval")
                thread = snap["selected_thread"]
                if capture:
                    window.command("import","-window",window.id,str(capture))
            marker = project / "approved.txt"
            assert not marker.exists()
            answer_from_phone(Path(env["OX_HOST_SOCKET"]), Path(env["OX_HOST_STATE_DIR"]), thread, env)
            deadline = time.monotonic() + 15
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(.05)
            assert marker.read_text().strip() == "accepted"
            with desktop_window(gui,root,env,project,"reopened") as restored:
                snap = restored.snapshot()
                if snap["selected_thread"] != thread:
                    restored.choose("Help me water the garden")
                restored.wait(lambda s: s["selected_thread"]==thread and not s["visible_turn"] and not s["selected_requests"],
                              "same completed thread after reopening")
                local_tool_terminal(restored,provider,root,project)
                live_session(restored,provider,env,root)
            assert not provider.errors, provider.errors
            print("PASS stock Claude, desktop exit, paired browser approval, original-project write, and desktop reopen.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("gui", type=Path)
    parser.add_argument("claude", type=Path)
    parser.add_argument("--capture", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ox-phone-") as tmp:
        run(args.gui.resolve(strict=True), args.claude.resolve(strict=True), Path(tmp),args.capture)
