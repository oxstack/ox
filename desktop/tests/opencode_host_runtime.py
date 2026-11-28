"""Verify the host-owned controller with stock OpenCode and a local fixture.

Build `cargo test --manifest-path desktop/Cargo.toml -p ox-host --test opencode_stock_runtime --no-run`.
Run `python3 desktop/tests/opencode_host_runtime.py STOCK OX TEST_BINARY` with its reported executable.
"""

import argparse
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
import threading
import time

from opencode_host_contract import MODEL, Provider, report_version


class RuntimeProvider(Provider):
    def respond(self, case):
        if case != "provider error":
            return super().respond(case)
        body = json.dumps({"type": "error", "error": {
            "type": "invalid_request_error", "message": "Fixture rejected the turn",
        }}).encode()
        self.send_response(400)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def stream(self, case, filename):
        # Stock coalesces adjacent user messages into one upstream message.
        # An immediate Stop can leave its acknowledged user message without
        # an assistant, so the next explicit prompt follows it in this case.
        if case == "cancel approval next":
            case = "next"
        if case != "read tool":
            return super().stream(case, filename)
        self.write_event("message_start", message={
            "id": "msg_read", "type": "message", "role": "assistant", "model": "local",
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 12, "output_tokens": 0},
        })
        self.write_event("content_block_start", index=0, content_block={
            "type": "tool_use", "id": "toolu_read", "name": "read", "input": {},
        })
        self.write_event("content_block_delta", index=0, delta={
            "type": "input_json_delta", "partial_json": json.dumps({"filePath": str(self.server.read_file)}),
        })
        self.write_event("content_block_stop", index=0)
        self.write_event("message_delta", delta={"stop_reason": "tool_use", "stop_sequence": None},
                         usage={"output_tokens": 5})
        self.write_event("message_stop")


def run(stock, ox, test):
    report_version(stock)
    with tempfile.TemporaryDirectory(prefix="ox-opencode-runtime-") as temp, \
            ThreadingHTTPServer(("127.0.0.1", 0), RuntimeProvider) as provider:
        root = Path(temp).resolve()
        bin_dir, home, project, other, state = (root / name for name in ("bin", "home", "project", "other", "state"))
        for path in (bin_dir, home, project, other, state):
            path.mkdir(mode=0o700)
        (bin_dir / "opencode").symlink_to(stock)
        provider.read_file = project / "test-read.txt"
        provider.read_file.write_text("The fixture read this file.\n")
        (project / "opencode.json").write_text(json.dumps({
            "autoupdate": False, "share": "disabled", "permission": {"*": "allow", "bash": "allow"},
            "agent": {"build": {"permission": {"bash": "allow"}}},
        }))
        alias = MODEL.split("/", 1)[1]
        (home / "codeox.toml").write_text(
            '[providers.fixture]\n'
            f'base_url = "http://127.0.0.1:{provider.server_port}/v1"\n'
            'wire_api = "anthropic_messages"\nenv_key = "FIXTURE_PROVIDER_KEY"\ndiscovery = false\n'
            f'[[providers.fixture.models]]\nid = "{alias}"\n'
        )
        provider.key = secrets.token_urlsafe(24)
        provider.gates = {name: threading.Event() for name in ("reconnect", "cancel stream")}
        watching = threading.Event()

        def release_stream():
            while not watching.is_set():
                if (project / "allow-stream").exists():
                    provider.gates["reconnect"].set()
                    return
                time.sleep(0.025)

        service = threading.Thread(target=provider.serve_forever, daemon=True)
        watcher = threading.Thread(target=release_stream, daemon=True)
        service.start()
        watcher.start()
        try:
            subprocess.run([test, "--ignored", "--nocapture"], check=True, timeout=80, env={
                "PATH": os.pathsep.join((str(bin_dir), os.defpath)), "HOME": str(home), "CODEX_HOME": str(home),
                "FIXTURE_PROVIDER_KEY": provider.key,
                "OX_TEST_OPENCODE_GATEWAY": str(ox), "OX_TEST_OPENCODE_PROJECT": str(project),
                "OX_TEST_OPENCODE_STATE": str(state),
            })
        finally:
            watching.set()
            watcher.join()
            provider.shutdown()
            service.join()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("stock", "ox", "test"):
        parser.add_argument(arg, type=Path)
    args = parser.parse_args()
    run(*(path.resolve(strict=True) for path in (args.stock, args.ox, args.test)))
