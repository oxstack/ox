"""Run with python3 tests/fixtures/stock_opencode.py /path/to/ox /path/to/opencode."""

import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading

ox, cli = (Path(arg).resolve(strict=True) for arg in sys.argv[1:])
repo = Path(__file__).resolve().parents[2]
fixtures = repo / "tests/fixtures/shared_gateway"
responses = (fixtures / "responses.sse").read_bytes().replace(b'"OK"', b'"Responses fixture"')
chat = (
    b'data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant",'
    b'"content":"Chat Completions fixture"},"finish_reason":null}]}\n\n'
    b'data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],'
    b'"usage":{"prompt_tokens":20,"completion_tokens":5,"total_tokens":25}}\n\n'
    b'data: [DONE]\n\n'
)
provider_keys = {
    "messages": "fixture-messages-secret",
    "responses": "fixture-responses-secret",
    "chat": "fixture-chat-secret",
}
tool_output = "fixture.txt confirms safe reading"
hits = []
interrupt_started = threading.Event()
interrupt_released = threading.Event()


def sse(items):
    return "".join(f"event: {event}\ndata: {json.dumps({'type': event, **item})}\n\n"
                   for event, item in items).encode()


def messages_stream(part, reason):
    is_tool = part["type"] == "tool_use"
    block = {**part, "input": {}} if is_tool else {"type": "text", "text": ""}
    delta = {"type": "input_json_delta", "partial_json": json.dumps(part["input"])} if is_tool else {
        "type": "text_delta", "text": part["text"]
    }
    return sse([
        ("message_start", {"message": {
            "id": f"msg_{len(hits)}", "type": "message", "role": "assistant",
            "model": "messages-fixture", "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 20, "output_tokens": 0}
        }}),
        ("content_block_start", {"index": 0, "content_block": block}),
        ("content_block_delta", {"index": 0, "delta": delta}),
        ("content_block_stop", {"index": 0}),
        ("message_delta", {"delta": {"stop_reason": reason, "stop_sequence": None},
                           "usage": {"output_tokens": 8}}),
        ("message_stop", {}),
    ])


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        model = payload["model"]
        route = model.removesuffix("-fixture")
        key = self.headers.get("x-api-key") if route == "messages" else self.headers.get(
            "authorization", "").removeprefix("Bearer ")
        if provider_keys.get(route) != key:
            self.send_error(401)
            return
        hits.append((self.path, route, payload))
        if self.path.endswith("/count_tokens"):
            data, kind = b'{"input_tokens":64}', "application/json"
        elif route == "responses":
            data, kind = responses, "text/event-stream"
        elif route == "chat":
            data, kind = chat, "text/event-stream"
        elif route == "messages":
            body = json.dumps(payload["messages"])
            if "OX CASE HOLD" in body:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.flush()
                interrupt_started.set()
                interrupt_released.wait(timeout=15)
                return
            if any(
                item["type"] == "tool_result" and tool_output in json.dumps(item)
                for message in payload["messages"] if isinstance(message["content"], list)
                for item in message["content"]
            ) and "OX CASE RESUME" in body:
                data = messages_stream({"type": "text", "text": "Saved context resumed"}, "end_turn")
            elif any(
                item["type"] == "tool_result" and tool_output in json.dumps(item)
                for message in payload["messages"] if isinstance(message["content"], list)
                for item in message["content"]
            ):
                data = messages_stream({"type": "text", "text": "Read tool worked"}, "end_turn")
            elif "OX CASE READ" in body and any(tool["name"] == "read" for tool in payload.get("tools", [])):
                data = messages_stream({
                    "type": "tool_use", "id": "toolu_fixture", "name": "read",
                    "input": {"filePath": str(self.server.project / "fixture.txt")},
                }, "tool_use")
            else:
                data = messages_stream({"type": "text", "text": "Messages fixture"}, "end_turn")
            kind = "text/event-stream"
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


with tempfile.TemporaryDirectory(prefix="ox-stock-opencode-") as name, ThreadingHTTPServer(
    ("127.0.0.1", 0), Provider
) as server:
    threading.Thread(target=server.serve_forever, daemon=True).start()
    root = Path(name)
    for directory in ("bin", "home", "native-config", "native-data", "native-cache",
                      "native-state", "project", "ox"):
        (root / directory).mkdir(mode=0o700)
    (root / "bin/opencode").symlink_to(cli)
    (root / "native-data/keep").write_text("stock profile untouched")
    project = root / "project"
    (project / "fixture.txt").write_text(tool_output)
    (project / "opencode.json").write_text(json.dumps({
        "model": "non-ox/missing",
        "disabled_providers": ["ox"],
        "provider": {"ox": {
            "options": {"baseURL": "http://127.0.0.1:1/v1", "apiKey": "project-fixture"},
            "models": {
                "foreign/model": {"name": "Foreign model"},
                "messages/messages-fixture": {
                    "id": "foreign/model",
                    "provider": {"npm": "@ai-sdk/openai-compatible"},
                    "headers": {"x-api-key": "project-fixture"},
                },
            },
        }},
    }))
    server.project = project
    env = {
        "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
        "HOME": str(root / "home"),
        "CODEX_HOME": str(root / "ox"),
        "OPENCODE_DISABLE_AUTOUPDATE": "1",
        "OPENCODE_DISABLE_MODELS_FETCH": "1",
        "OPENCODE_DB": str(root / "native-data/native.db"),
        "XDG_CONFIG_HOME": str(root / "native-config"),
        "XDG_DATA_HOME": str(root / "native-data"),
        "XDG_CACHE_HOME": str(root / "native-cache"),
        "XDG_STATE_HOME": str(root / "native-state"),
    }
    config = ""
    for route, wire in [("messages", "anthropic_messages"), ("responses", "responses"),
                        ("chat", "chat_completions")]:
        key_name = f"FIXTURE_{route.upper()}_KEY"
        env[key_name] = provider_keys[route]
        config += f"""
[providers.{route}]
base_url = "http://127.0.0.1:{server.server_port}/v1"
wire_api = "{wire}"
env_key = "{key_name}"
discovery = false
[[providers.{route}.models]]
id = "{route}-fixture"
context_window = 65536
"""
    (root / "ox/codeox.toml").write_text(config)

    def run(*args, prompt=None, route="messages"):
        launch = [str(ox), "opencode", *args]
        if prompt is not None:
            launch += ["--format", "json", "--model", f"ox/{route}/{route}-fixture", prompt]
        result = subprocess.run(launch, cwd=project, env=env, text=True, capture_output=True, timeout=25)
        if result.returncode:
            raise AssertionError(f"{result.returncode}: {result.stdout}\n{result.stderr}")
        return result.stdout

    models = run("models")
    assert sorted(models.split()) == ["ox/chat/chat-fixture", "ox/messages/messages-fixture",
                                      "ox/responses/responses-fixture"], models
    stock_models = run("models", "ox")
    assert sorted(stock_models.split()) == sorted(models.split()), stock_models
    default = run("run", "--format", "json", "Show the OX default")
    assert "Chat Completions fixture" in default, default
    ran = run("run", prompt="OX CASE READ read fixture.txt")
    assert "Read tool worked" in ran, ran
    assert '"tool":"read"' in ran, ran
    session = next(json.loads(line)["sessionID"] for line in ran.splitlines() if line.startswith("{"))
    resumed = run("run", "--continue", "--format", "json", "OX CASE RESUME show the result")
    assert "Saved context resumed" in resumed, resumed
    assert all(json.loads(line)["sessionID"] == session for line in resumed.splitlines() if line.startswith("{"))
    for route, expected in [("responses", "Responses fixture"), ("chat", "Chat Completions fixture")]:
        output = run("run", prompt="Show the fixture response", route=route)
        assert expected in output, output
    for stop, send in [(signal.SIGINT, os.killpg), (signal.SIGTERM, os.kill)]:
        interrupt_started.clear()
        interrupt_released.clear()
        held = subprocess.Popen(
            [str(ox), "opencode", "run", "--model", "ox/messages/messages-fixture", "OX CASE HOLD"],
            cwd=project, env=env, text=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            assert interrupt_started.wait(15), "OpenCode did not reach the stalled provider"
            send(held.pid, stop)
            assert held.wait(timeout=8) in (-stop, 128 + stop), held.returncode
            processes = subprocess.check_output(["ps", "-Ao", "pgid=,stat="], text=True)
            assert all(
                group != str(held.pid) or state.startswith("Z")
                for group, state in (line.split() for line in processes.splitlines())
            ), "a live process remained after interrupting the launch"
        finally:
            interrupt_released.set()
            try:
                os.killpg(held.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            held.wait()
    assert sorted({(path, route) for path, route, _ in hits}) == [
        ("/v1/chat/completions", "chat"), ("/v1/messages", "messages"), ("/v1/responses", "responses")
    ], hits
    assert not (root / "native-data/native.db").exists()
    assert not list((root / "native-config").iterdir())
    assert (root / "ox/ox-clients/opencode/data/opencode/opencode.db").exists()
    assert not list((root / "ox/ox-clients/opencode/config").glob(".ox-open-*"))
    print("OpenCode: three wires, authoritative model list, read tool, resume, SIGINT, wrapper-only SIGTERM")
