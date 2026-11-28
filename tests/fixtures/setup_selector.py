"""Exercise the release binary's terminal-only setup entry point with stub clients."""

import fcntl
import http.server
import json
import os
import pathlib
import pty
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import urllib.request

binary = pathlib.Path(os.environ.get("OX_TEST_BINARY", "target/release/ox")).resolve()


UUID_V4 = __import__("re").compile(r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}")


def drop_session(argv):
    """Remove the `--session-id <uuidv4>` ox gives a new Claude session; return the ID."""
    if "--session-id" not in argv:
        return None
    i = argv.index("--session-id")
    session = argv[i + 1]
    assert UUID_V4.fullmatch(session), argv
    del argv[i:i + 2]
    return session


def run(
    args, choices=None, signed_in=False, claude_signed_in=False,
    config=None, prompt="Choose 1–5".encode(), saved=None, model=None,
    favorites=None, expected_favorites=None, expected_lineup=None,
    expected_context=None, expected_classifier=None,
    persist_model=False,
):
    with tempfile.TemporaryDirectory(prefix="ox-selector-") as tmp:
        home = pathlib.Path(tmp)
        bindir = home / "bin"
        bindir.mkdir()
        trace = home / "trace"
        context_trace = home / "context"
        classifier_trace = home / "classifier"
        lineup_trace = home / "lineup"
        original = 'model_provider = "openai"\n'
        (home / "config.toml").write_text(original)
        native_claude = '{"model":"claude-sonnet-5","permissions":{"deny":["Bash"]}}\n'
        if persist_model:
            (home / "settings.json").write_text(native_claude)
        if signed_in:
            (home / "auth.json").write_text(
                '{"auth_mode":"chatgpt","tokens":{"access_token":"fixture-token"}}\n'
            )
        if claude_signed_in:
            (home / ".claude.json").write_text(
                '{"oauthAccount":{"accountUuid":"fixture-account"}}\n'
            )
        if config is not None:
            (home / "codeox.toml").write_text(config)
        if favorites is not None:
            (home / "ox-favorites.json").write_text(json.dumps(favorites))
        for client in ("codex", "claude"):
            fake = bindir / client
            fake.write_text('''#!/bin/sh
if [ "$1" = debug ] && [ "$2" = models ]; then
    printf '{"models":[]}'
    exit 0
fi
if [ "$1" = auth ] && [ "$2" = status ]; then
    touch "$CLAUDE_CONFIG_DIR/unexpected-auth-probe"
    exit 1
fi
if [ "$3" = login ] && [ "$4" = status ]; then
    touch "$CODEX_HOME/unexpected-auth-probe"
    exit 1
fi
if env | grep -q '^CUSTOM_.*_API_KEY='; then
    exit 9
fi
if [ "$FIXTURE_PERSIST_MODEL" = yes ]; then
    "$FIXTURE_PYTHON" - "$(basename "$0")" <<'PYTEST'
import json, os, pathlib, sys
if sys.argv[1] == "claude":
    path = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"]) / "settings.json"
    settings = json.loads(path.read_text())
    assert settings["model"] == "claude-sonnet-5"
    assert settings["permissions"]["deny"] == ["Bash"]
    settings["model"] = "alibaba/qwen-fixture"
    path.write_text(json.dumps(settings))
else:
    path = pathlib.Path(os.environ["CODEX_HOME"]) / "config.toml"
    path.write_text(path.read_text() + 'model = "alibaba/qwen-fixture"\\n')
PYTEST
    [ "$?" = 0 ] || exit 10
fi
printf '%s\\n' "$(basename "$0")" "$@" > "$FIXTURE_TRACE"
if [ "$(basename "$0")" = claude ]; then
    printf '%s' "${CLAUDE_CODE_MAX_CONTEXT_TOKENS:-}" > "$FIXTURE_CONTEXT"
    printf '%s\\n' "${CLAUDE_CODE_AUTO_MODE_SERVER:-}" "${CLAUDE_CODE_AUTO_MODE_MODEL:-}" > "$FIXTURE_CLASSIFIER"
    # API routes pass a settings file; subscription launches pass inline JSON.
    if [ "$1" = --settings ] && [ -f "$2" ]; then
        "$FIXTURE_PYTHON" -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1])).get("modelPicker")))' "$2" > "$FIXTURE_LINEUP"
    fi
fi
''')
            fake.chmod(0o755)
        env = {
            "HOME": tmp, "CODEX_HOME": tmp, "CLAUDE_CONFIG_DIR": tmp,
            "PATH": f"{bindir}:/usr/bin:/bin", "FIXTURE_TRACE": str(trace),
            "TERM": "xterm-256color", "FIXTURE_CONTEXT": str(context_trace),
            "FIXTURE_CLASSIFIER": str(classifier_trace),
            "FIXTURE_PYTHON": sys.executable, "FIXTURE_LINEUP": str(lineup_trace),
            "FIXTURE_PERSIST_MODEL": "yes" if persist_model else "no",
            "OPENROUTER_API_KEY": "dummy-fixture-credential",
            "ANTHROPIC_API_KEY": "dummy-anthropic-credential",
            "ALIBABA_API_KEY": "dummy-alibaba-credential",
        }
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        proc = subprocess.Popen([binary, *args], env=env, stdin=slave, stdout=slave, stderr=slave)
        # This process keeps its end of the terminal open until the output has
        # been read. With it closed, a program that prints and exits at once
        # closes the last writer before the first read, and macOS may then
        # discard what was printed.
        capture = bytearray()

        def read(wait=.05):
            if not select.select([master], [], [], wait)[0]:
                return False
            capture.extend(os.read(master, 8192))
            return True

        def drain():
            while read(0):
                pass

        try:
            steps = [(prompt, choices)] if isinstance(choices, bytes) else choices or []
            previous = 0
            for marker, answer in steps:
                # Each prompt proves progress. Separate setup steps can each
                # wait for durable writes to the provider config and key file.
                deadline = time.monotonic() + 8
                while marker not in capture[previous:] and time.monotonic() < deadline:
                    if proc.poll() is not None:
                        drain()
                        assert marker in capture[previous:], \
                            f"ox exited before prompt {marker!r}: {capture.decode(errors='replace')}"
                    else:
                        read()
                assert marker in capture[previous:], f"Missing prompt {marker!r}: {capture.decode(errors='replace')}"
                previous = len(capture)
                os.write(master, answer)
            deadline = time.monotonic() + 8
            while proc.poll() is None and time.monotonic() < deadline:
                read()
            assert proc.wait(timeout=2) == 0, capture.decode()
            drain()
            text = capture.decode()
            assert "Provider key:" not in text
            assert "dummy-fixture-credential" not in text
            assert (home / "config.toml").read_text() == original
            if persist_model:
                assert (home / "settings.json").read_text() == native_claude
                if args[0] == "codex":
                    updated = home / "ox-clients" / "codex" / "config.toml"
                    assert 'model = "alibaba/qwen-fixture"' in updated.read_text()
                else:
                    updated = home / "ox-clients" / "claude" / "settings.json"
                    assert json.loads(updated.read_text())["model"] == "alibaba/qwen-fixture"
            registry = (home / "codeox.toml").read_text() if (home / "codeox.toml").exists() else None
            if saved:
                secret_file = home / "ox-api-keys.json"
                assert secret_file.stat().st_mode & 0o777 == 0o600
                keys = json.loads(secret_file.read_text())
                for name, (scope, key) in saved.items():
                    assert keys.get(name) == {"scope": scope, "value": key}, name
                    assert key not in text and key not in registry, "Key exposed during setup"
                doctor = subprocess.run(
                    [binary, "doctor"], env=env, capture_output=True, text=True, timeout=5,
                )
                assert doctor.returncode == 0, doctor.stdout
                for name in saved:
                    assert f"{name} is available to ox (value hidden)" in doctor.stdout
            if model:
                for client in ("codex", "claude"):
                    launched = subprocess.run(
                        [binary, client, "--model", model], env=env,
                        capture_output=True, text=True, timeout=5,
                    )
                    assert launched.returncode == 0, launched.stderr
                    argv = trace.read_text().splitlines()
                    assert argv[0] == client and argv[-2:] == ["--model", model], argv
                assert context_trace.read_text() == "123456"
                with socket.socket() as sock:
                    sock.bind(("127.0.0.1", 0))
                    port = sock.getsockname()[1]
                gateway_key = "fixture-gateway-key-not-an-upstream-key"
                with subprocess.Popen(
                    [binary, "serve", "--port", str(port)],
                    env={**env, "OX_GATEWAY_KEY": gateway_key},
                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
                ) as daemon:
                    try:
                        assert select.select([daemon.stderr], [], [], 5)[0], "ox serve timed out"
                        assert daemon.stderr.readline().startswith("ox: serving on ")
                        req = urllib.request.Request(
                            f"http://127.0.0.1:{port}/v1/models",
                            headers={"Authorization": f"Bearer {gateway_key}"},
                        )
                        http = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                        with http.open(req, timeout=5) as response:
                            limits = {item["id"]: item.get("max_input_tokens") for item in json.load(response)["data"]}
                            assert limits.get(model) == 123456
                    finally:
                        daemon.terminate()
                        daemon.wait(timeout=5)
                (home / "codeox.toml").write_text(registry.replace("/v1", "/different/v1"))
                doctor = subprocess.run(
                    [binary, "doctor"], env=env, capture_output=True, text=True, timeout=5,
                )
                assert doctor.returncode != 0 and "is not set" in doctor.stdout
            if expected_favorites is not None:
                persisted = json.loads((home / "ox-favorites.json").read_text())
                assert persisted == expected_favorites
            if expected_lineup is not None:
                lineup = json.loads(lineup_trace.read_text())
                assert lineup["replaceBuiltInOptions"] is True
                assert [row["model"] for row in lineup["options"]] == expected_lineup
            if expected_context is not None:
                assert context_trace.read_text() == expected_context
            if expected_classifier is not None:
                classifier = classifier_trace.read_text().splitlines()
                assert classifier == list(expected_classifier), (classifier, expected_classifier)
            assert not (home / "unexpected-auth-probe").exists()
            return text, trace.read_text().splitlines() if trace.exists() else [], registry
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            os.close(slave)
            os.close(master)


text, argv, registry = run([], b"1\n5\n")
assert argv == ["codex", "-c", 'model_provider="openai"', "login"], argv
assert "ox codex --chatgpt" in text and registry is None, text
print("bare ox -> ChatGPT: launched native Codex sign-in, printed the next command")
text, argv, registry = run(["setup"], b"1\n5\n", signed_in=True)
assert argv == [] and "ox codex --chatgpt" in text and registry is None, text
print("ox setup -> already signed in to ChatGPT: no second sign-in")
text, argv, registry = run(["setup"], b"2\n5\n")
assert argv == ["claude", "auth", "login", "--claudeai"], argv
assert "ox claude --native" in text and registry is None, text
print("ox setup -> Claude: launched native Claude Code sign-in, printed the next command")
menu = "Choose 1–5".encode()
hidden = b"API key (hidden): "
go_key = "dummy-go-pasted-key"
brave_key = "dummy-brave-pasted-key"
text, argv, registry = run(
    ["setup"], [
        (menu, b"nope\n3\n"),
        (b"Service number:", b"1\n"),
        (hidden, f"{go_key}\n".encode()),
        (menu, b"4\n"),
        (hidden, f"{brave_key}\n".encode()),
        (menu, b"5\n"),
    ],
    saved={
        "OPENCODE_API_KEY": ("https://opencode.ai/inference/go/openai/v1", go_key),
        "BRAVE_SEARCH_API_KEY": ("https://api.search.brave.com/", brave_key),
    },
)
assert "enter a number from 1 to 5" in text
assert registry is not None
for expected in (
    "[providers.opencode-go]",
    'base_url = "https://opencode.ai/inference/go/openai/v1"',
    'models_url = "https://opencode.ai/inference/go/v1/models"',
    'wire_api = "chat_completions"',
    'env_key = "OPENCODE_API_KEY"',
    "[search]",
    'env_key = "BRAVE_SEARCH_API_KEY"',
):
    assert expected in registry.splitlines(), registry
assert argv == []
assert "Run `ox doctor` to check setup" in text
print("ox setup: guided Go and Brave setup with hidden keys usable after restart")


class Catalog(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        ok = self.path == "/v1/models" and self.headers.get("Authorization") == "Bearer fixture-custom-key"
        self.server.calls.append(ok)
        self.send_response(200 if ok else 401)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"data":[{"id":"fixture","context_window":123456,"max_output":16384}]}')

    def log_message(self, *args):
        pass


with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Catalog) as vendor:
    vendor.calls = []
    thread = threading.Thread(target=vendor.serve_forever)
    thread.start()
    port = vendor.server_port
    base = f"http://127.0.0.1:{port}/v1"
    try:
        text, argv, registry = run(
            ["provider", "add"], [
                (b"Service number:", b"6\n"),
                (b"Paste the API base URL", f"{base}\n".encode()),
                (hidden, b"fixture-custom-key\n"),
                (b"Add another API endpoint", b"\n"),
            ],
            saved={f"CUSTOM_127_0_0_1_{port}_API_KEY": (base, "fixture-custom-key")},
            model=f"custom-127-0-0-1-{port}/fixture",
        )
        assert "Using OpenAI Responses" in text
        assert vendor.calls == [True, True, True], vendor.calls
    finally:
        vendor.shutdown()
        thread.join()
print("Other: plain /v1 defaults to Responses and both launchers and ox serve discover the model")

for client in ("codex", "claude"):
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Catalog) as vendor:
        vendor.calls = []
        thread = threading.Thread(target=vendor.serve_forever)
        thread.start()
        port = vendor.server_port
        base = f"http://127.0.0.1:{port}/v1"
        alias = f"custom-127-0-0-1-{port}/fixture"
        try:
            text, argv, registry = run(
                [client], [
                    (menu, b"3\n"),
                    (b"Service number:", b"6\n"),
                    (b"Paste the API base URL", f"{base}\n".encode()),
                    (hidden, b"fixture-custom-key\n"),
                    (b"Add another API endpoint", b"\n"),
                ],
                saved={f"CUSTOM_127_0_0_1_{port}_API_KEY": (base, "fixture-custom-key")},
            )
            assert "Welcome to ox" in text and "Setup is complete" in text, text
            assert argv[0] == client and argv[-2:] == ["--model", alias], argv
            assert "Run `ox doctor` to check setup" not in text
            assert vendor.calls == [True], vendor.calls
        finally:
            vendor.shutdown()
            thread.join()
print("first run: ox codex and ox claude finish guided setup and launch the client in one go")

partial = '[providers.openrouter]\nbase_url = "http://127.0.0.1:9/v1"\nenv_key = "FIXTURE_MISSING_KEY"\ndiscovery = false\n\n[[providers.openrouter.models]]\nid = "fixture"\n'
text, argv, registry = run(
    ["codex"], [(hidden, b"fixture-late-key\n")], config=partial,
    saved={"FIXTURE_MISSING_KEY": ("http://127.0.0.1:9/v1", "fixture-late-key")},
)
assert "has no API key yet" in text and registry == partial
assert argv[0] == "codex" and argv[-2:] == ["--model", "openrouter/fixture"], argv
print("launch with a missing key: asks for it once, saves it, and continues")


for endpoint, wire in (
    ("https://chat.example/v1/chat/completions", "chat_completions"),
    ("https://messages.example/v1/messages", "anthropic_messages"),
):
    text, argv, registry = run(
        ["provider", "add"], [
            (b"Service number:", b"6\n"),
            (b"Paste the API base URL", f"{endpoint}\n".encode()),
            (hidden, b"\n"),
            (b"Add another API endpoint", b"\n"),
        ],
    )
    assert f'wire_api = "{wire}"' in registry
    assert f'base_url = "{endpoint.split("/v1")[0]}/v1"' in registry
    assert "needs a key" in text
print("Other: full Chat Completions and Messages endpoints select their own format")

text, argv, registry = run(["provider", "add"], [
    (b"Service number:", b"6\n"),
    (b"Paste the API base URL", b"https://fixture.example/v1/responses\n"),
    (hidden, b"fixture-responses-key\n"),
    (b"Add another API endpoint", b"y\n"),
    (b"Paste the full inference URL", b"https://fixture.example/v1/messages\n"),
    (hidden, b"fixture-messages-key\n"),
    (b"Add another API endpoint", b"\n"),
], saved={
    "CUSTOM_FIXTURE_EXAMPLE_API_KEY": ("https://fixture.example/v1", "fixture-responses-key"),
    "CUSTOM_FIXTURE_EXAMPLE_MESSAGES_API_KEY": ("https://fixture.example/v1", "fixture-messages-key"),
})
assert "One endpoint works with both" in text
assert "[providers.custom-fixture-example]" in registry
assert "[providers.custom-fixture-example-messages]" in registry
assert 'wire_api = "anthropic_messages"' in registry
print("Other: add Responses and Messages endpoints together with separate credentials")


for path, base, wire in (
    ("/compatible-mode/v1/chat/completions", "/compatible-mode/v1", "chat_completions"),
    ("/apps/anthropic", "/apps/anthropic/v1", "anthropic_messages"),
):
    host = "https://fixture-workspace.maas.aliyuncs.com"
    text, argv, registry = run(
        ["provider", "add"], [
            (b"Service number:", b"5\n"),
            (b"Paste the Alibaba Cloud API URL", f"{host}{path}\n".encode()),
        ],
    )
    assert "5. Alibaba Cloud" in text
    assert "[providers.alibaba]" in registry
    assert f'base_url = "{host}{base}"' in registry
    assert f'wire_api = "{wire}"' in registry
    assert 'env_key = "ALIBABA_API_KEY"' in registry
    assert "discovery = false" not in registry
print("Alibaba Cloud: prompt uses the supplied account URL and infers its API format")

host = "https://fixture-workspace.maas.aliyuncs.com"
alibaba_chat = f'''[providers.alibaba]
base_url = "{host}/compatible-mode/v1"
wire_api = "chat_completions"
env_key = "ALIBABA_API_KEY"
'''
text, argv, registry = run(["provider", "add"], [
    (b"Service number:", b"5\n"),
    (b"Paste the Alibaba Cloud API URL", f"{host}/apps/anthropic\n".encode()),
    (hidden, b"fixture-alibaba-messages-key\n"),
], config=alibaba_chat, saved={
    "ALIBABA_MESSAGES_API_KEY": (f"{host}/apps/anthropic/v1", "fixture-alibaba-messages-key"),
})
assert registry.startswith(alibaba_chat)
assert "[providers.alibaba-messages]" in registry
assert "discovery = false" not in registry
print("Alibaba Cloud: adding Messages preserves the existing Chat Completions route")


text, argv, registry = run([
    "provider", "add", "openrouter",
    "--base-url", "https://openrouter.ai/api/v1",
    "--wire", "responses",
    "--no-discovery", "--model", "fixture-model",
])
assert "Display name" not in text
assert "API key variable name" not in text
assert registry is not None
assert 'env_key = "OPENROUTER_API_KEY"' in registry.splitlines()
assert argv == []
print("flag-driven ox provider add in a terminal: applied defaults without prompting")


for args, sign_in, expected in (
    (["codex"], {"signed_in": True}, ["codex", "-c", 'model_provider="openai"']),
    (["claude"], {"claude_signed_in": True}, ["claude"]),
    ([], {"signed_in": True}, ["codex", "-c", 'model_provider="openai"']),
):
    text, argv, registry = run(args, **sign_in)
    session = drop_session(argv)
    # A new Claude session gets an ID ox knows before the first turn.
    assert (session is not None) == (expected[0] == "claude"), argv
    if expected[0] == "claude":
        # Subscription launches carry ox's inline billing overlay, so a user or
        # project settings file cannot redirect the session to another API origin.
        i = argv.index("--settings")
        overlay = json.loads(argv[i + 1])
        assert overlay["apiKeyHelper"] == "", overlay
        for key in ("ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"):
            assert overlay["env"][key] == "", (key, overlay)
        del argv[i:i + 2]
    assert argv == expected and registry is None, (text, argv)
    assert "What would you like to use?" not in text
print("plain commands: launch signed-in stock clients without reopening setup")

text, argv, registry = run([], b"5\n", claude_signed_in=True)
assert argv == [] and registry is None, (text, argv)
text, argv, registry = run([], b"2\n", claude_signed_in=True)
assert argv[0] == "claude" and registry is None, (text, argv)
print("bare ox with only a Claude account starts Claude only after selection")

text, argv, registry = run(["setup"], b"1\n2\n5\n", signed_in=True, claude_signed_in=True)
assert "has a saved Claude account" in text and "has a saved ChatGPT sign-in" in text
assert argv == [] and registry is None
print("setup: checks both existing native sign-ins in one pass")

text, argv, registry = run(["doctor"], signed_in=True, claude_signed_in=True)
assert "API gateway providers: 0" in text and "saved ChatGPT sign-in found" in text and "saved Claude account found" in text, text
assert argv == [] and registry is None
print("doctor: zero API providers and both native accounts detected")

shared = """
[providers.anthropic]
base_url = "https://api.anthropic.com/v1"
wire_api = "anthropic_messages"
env_key = "ANTHROPIC_API_KEY"
discovery = false

[[providers.anthropic.models]]
id = "claude-fixture"

[providers.alibaba]
base_url = "https://alibaba.example/v1"
wire_api = "chat_completions"
env_key = "ALIBABA_API_KEY"
discovery = false

[[providers.alibaba.models]]
id = "qwen-fixture"

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
env_key = "OPENROUTER_API_KEY"
discovery = false

""" + "\n".join(f'[[providers.openrouter.models]]\nid = "model-{i}"\n' for i in range(14))

for main_model, options, config, expected in (
    ("anthropic/claude-fixture", [], shared, ("", "")),
    ("alibaba/qwen-fixture", [], shared, ("0", "")),
    ("openrouter/model-1", [], shared, ("0", "")),
    ("alibaba/qwen-fixture", ["--client-classifier=auto"], shared, ("0", "")),
    ("alibaba/qwen-fixture", ["--client-classifier", "auto"],
     shared.replace('id = "claude-fixture"', 'id = "claude-sonnet-5"'),
     ("0", "anthropic/claude-sonnet-5")),
):
    text, argv, registry = run(
        ["claude", "--model", main_model, *options], config=config,
        expected_classifier=expected,
    )
    assert drop_session(argv) is not None, argv
    assert argv[0] == "claude" and argv[3:] == ["--model", main_model], argv
print("Claude launch: native review default, translated review fallback, and configured Sonnet classifier")

alibaba_snapshot = shared.replace(
    "https://alibaba.example/v1", "https://catalog-fixture.maas.aliyuncs.com/v1",
).replace('id = "qwen-fixture"', 'id = "deepseek-v4.1-flash-1017"')
run(["claude", "--model", "alibaba/deepseek-v4.1-flash-1017"],
    config=alibaba_snapshot, expected_context="1000000")
print("Claude launch: Alibaba's model context reaches the client without discovery metadata")

for client in ("codex", "claude"):
    run([client, "--model", "alibaba/qwen-fixture"], config=shared, persist_model=True)
print("Relay client model saves are isolated from both native settings files")

search_prompt = b"Search as you type"
for client, search, expected in (
    ("codex", "anthropic", "anthropic/claude-fixture"),
    ("claude", "alibaba", "alibaba/qwen-fixture"),
):
    text, argv, registry = run(
        [client], [
            (search_prompt, search.encode()),
            (f"Search as you type  {search}▏".encode(), b"\r"),
        ], config=shared, signed_in=True, claude_signed_in=True,
    )
    assert registry == shared and argv[0] == client and argv[-2:] == ["--model", expected], argv
    assert "All models · 1 matching" in text
    assert ">   " + expected in text
print("live picker: both clients narrow the list while typing and Enter launches the highlight")

text, argv, registry = run(
    ["claude"], [
        (search_prompt, b"\x1b[6~"),
        (b">   openrouter/model-10", b"\x1b"),
    ], config=shared,
)
assert "\x1b[?1049l" in text and argv == [] and registry == shared
print("live picker: page through a large catalog and restore the terminal when canceled")

favorites = ["openrouter/model-1", "openrouter/model-10"]
text, argv, registry = run(
    ["claude"], [
        (search_prompt, b"model-1"),
        ("Search as you type  model-1▏".encode(), b"\t\t"),
        (b"2 starred", b"\x06"),
        ("Starred only · 2 matching".encode(), b"\x1b"),
    ], config=shared, expected_favorites=favorites,
)
assert all(f"★ {alias}" in text for alias in favorites)
assert argv == [] and registry == shared

text, argv, registry = run(
    ["codex"], [
        ("> ★ openrouter/model-1".encode(), b"\x06"),
        ("Starred only · 2 matching".encode(), b"\t"),
        ("Starred only · 1 matching".encode(), b"\r"),
    ], config=shared, favorites=favorites, expected_favorites=favorites[1:],
)
assert argv[-2:] == ["--model", favorites[1]] and registry == shared
print("live picker: stars persist, appear first in Codex, and can be removed in the starred view")


before = ["openrouter/model-13", "alibaba/qwen-fixture", "anthropic/claude-fixture"]
after = ["alibaba/qwen-fixture", "openrouter/model-13", "anthropic/claude-fixture"]
text, argv, registry = run(
    ["claude"], [
        (b"> " + "★ openrouter/model-13".encode(), b"\x1b[1;3B"),
        (b"  " + "★ alibaba/qwen-fixture".encode(), b"\r"),
    ], config=shared, favorites=before, expected_favorites=after, expected_lineup=after,
)
assert argv[-2:] == ["--model", "openrouter/model-13"] and registry == shared
print("live picker: reorder favorites and pass the same lineup to Claude Code")


previous = shared + '\n[search]\nenabled = false # manual opt-out\nenv_key = "CUSTOM_BRAVE_KEY"\nmax_results = 3\n'
text, argv, registry = run(["setup"], [(menu, b"4\n"), (hidden, b"\n"), (menu, b"4\n"), (hidden, b"\n"), (menu, b"5\n")], config=previous)
assert registry == previous.replace("false # manual opt-out", "true # manual opt-out")
assert "Brave search needs a key" in text and argv == []
print("setup: re-enable an existing Brave configuration without rewriting other settings")


for client, plan, expected in (
    ("codex", "ChatGPT", ["codex", "-c", 'model_provider="openai"']),
    ("claude", "Claude", ["claude"]),
):
    text, argv, registry = run(
        [client], [
            (search_prompt, b"\x1b[A"),
            (f">   {plan} subscription  [native]".encode(), b"\r"),
        ], config=shared, signed_in=True, claude_signed_in=True,
    )
    assert f"{plan} subscription  [native]" in text
    assert "anthropic/claude-fixture" in text
    assert argv[:len(expected)] == expected and registry == shared, argv
    assert "ox_workers" in " ".join(argv)
    assert "API workers enabled" in text
print("shared model picker: arrows select the matching subscription alongside API choices")
