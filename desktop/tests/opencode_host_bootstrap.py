"""Run the real stock OpenCode child through the private OX launch protocol.

python3 desktop/tests/opencode_host_bootstrap.py /path/to/opencode /path/to/ox
"""

import argparse
from contextlib import closing, contextmanager
import json
import os
from pathlib import Path
import secrets
import socket
import stat
import struct
import subprocess
import tempfile
import threading
from http.server import ThreadingHTTPServer
from urllib.parse import urlsplit

from host_fixture import descendants, wait_gone
from opencode_host_contract import Api, Events, MODEL, Provider, process, report_version, wait


class CredentialProvider(Provider):
    def stream(self, case, filename):
        if case not in ("credential check", "teardown check"):
            return super().stream(case, filename)
        self.write_event("message_start", message={
            "id": "msg_keys", "type": "message", "role": "assistant", "model": "local",
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 12, "output_tokens": 0},
        })
        self.write_event("content_block_start", index=0, content_block={
            "type": "tool_use", "id": "toolu_keys", "name": "bash", "input": {},
        })
        if case == "credential check":
            command = (
                'if [ -z "${FIXTURE_PROVIDER_KEY+x}" ] && [ -z "${SECOND_PROVIDER_KEY+x}" ] '
                '&& [ -z "${CODEOX_TOKEN+x}" ]; '
                "then printf clean; else printf leaked; fi > credential-check.txt"
            )
        else:
            command = 'printf "%s" "$$" > active-shell.pid; exec sleep 8'
        self.write_event("content_block_delta", index=0, delta={
            "type": "input_json_delta",
            "partial_json": json.dumps({"command": command, "description": "Check the child process", "timeout": 12000}),
        })
        self.write_event("content_block_stop", index=0)
        self.write_event("message_delta", delta={"stop_reason": "tool_use", "stop_sequence": None},
                         usage={"output_tokens": 5})
        self.write_event("message_stop")


def read_exact(control, size):
    data = bytearray()
    while len(data) < size:
        chunk = control.recv(size - len(data))
        if not chunk:
            raise EOFError("The managed OpenCode helper closed its startup socket")
        data.extend(chunk)
    return data


def launch(control, profile):
    packet = json.dumps({"profile": str(profile), "model": MODEL}).encode()
    control.sendall(struct.pack("!I", len(packet)) + packet)
    size, = struct.unpack("!I", read_exact(control, 4))
    assert 0 < size <= 16 * 1024
    return json.loads(read_exact(control, size))


@contextmanager
def managed(ox, env, project):
    with tempfile.TemporaryFile() as errors:
        control, peer = socket.socketpair()
        with control, process(
            [ox, "--ox-opencode-host-v1"], cwd=project, env=env,
            stdin=peer, stdout=subprocess.DEVNULL, stderr=errors, start_new_session=True,
        ) as child:
            peer.close()
            control.settimeout(30)
            try:
                yield control, child
            except Exception:
                errors.seek(0)
                print(errors.read().decode(errors="replace")[-2500:])
                raise


def run(ox, opencode, root, provider):
    bin_dir, home, project, profile = (root / name for name in ("bin", "home", "project", "profile"))
    for directory in (bin_dir, home, project, profile):
        directory.mkdir(mode=0o700)
    (bin_dir / "opencode").symlink_to(opencode)
    (home / "codeox.toml").write_text(
        '[providers.fixture]\n'
        f'base_url = "http://127.0.0.1:{provider.server_port}/v1"\n'
        'wire_api = "anthropic_messages"\nenv_key = "FIXTURE_PROVIDER_KEY"\ndiscovery = false\n'
        '[[providers.fixture.models]]\nid = "local"\n'
        '[[providers.fixture.models]]\nid = "wrong-route"\n'
        '[providers.second]\n'
        f'base_url = "http://127.0.0.1:{provider.server_port}/v1"\n'
        'wire_api = "anthropic_messages"\nenv_key = "SECOND_PROVIDER_KEY"\ndiscovery = false\n'
        '[[providers.second.models]]\nid = "local"\n'
    )
    (project / "opencode.json").write_text(json.dumps({
        "autoupdate": False, "share": "disabled", "permission": {"*": "deny", "bash": "ask"},
    }))
    env = {
        "PATH": os.pathsep.join((str(bin_dir), os.defpath)), "HOME": str(home), "CODEX_HOME": str(home),
        "LANG": "C", "FIXTURE_PROVIDER_KEY": provider.key, "SECOND_PROVIDER_KEY": secrets.token_urlsafe(24),
        "CODEOX_TOKEN": "previous-launch-token",
        # Overrides from a previous stock launch must not redirect hosted data, config, or HTTP auth.
        "OPENCODE_SERVER_USERNAME": "old-user", "OPENCODE_SERVER_PASSWORD": "old-password",
        "OPENCODE_DB": str(root / "wrong.db"), "OPENCODE_CONFIG_CONTENT": '{"model":"wrong/model"}',
    }
    # Occupy the native default to prove the helper uses the address actually
    # assigned by the child. If occupied already, that service stays untouched.
    with socket.socket() as default:
        import errno
        try:
            default.bind(("127.0.0.1", 4096))
            default.listen(1)
        except OSError as error:
            if error.errno != errno.EADDRINUSE:
                raise
        with managed(ox, env, project) as (control, child):
            ready = launch(control, profile)
            endpoint = urlsplit(f"http://{ready['address']}")
            assert endpoint.hostname == "127.0.0.1" and endpoint.port not in (None, 0, 4096)
            assert ready["password"] != env["OPENCODE_SERVER_PASSWORD"]
            api = Api(endpoint.geturl(), ready["password"], project)
            assert api.read("/global/health", authenticate=False, status=401) is None
            assert Api(endpoint.geturl(), "old-password", project).read("/global/health", status=401) is None
            assert api.read("/global/health")["healthy"] is True
            paths = api.read("/path")
            assert paths["directory"] == str(project)
            assert paths["config"] == str(profile / "config/opencode")
            assert paths["state"] == str(profile / "state/opencode")
            launch_config = next((profile / "config").glob(".ox-open-*/opencode.json"))
            config = launch_config.read_text()
            provider_config = json.loads(config)["provider"]["ox"]
            assert set(provider_config["models"]) == {MODEL}
            assert provider_config["env"] == [] and provider_config["options"]["apiKey"] == "{file:.ox-relay-token}"
            assert stat.S_IMODE(launch_config.with_name(".ox-relay-token").stat().st_mode) == 0o600
            assert provider.key not in config and env["SECOND_PROVIDER_KEY"] not in config

            with managed(ox, env, project) as (duplicate, contender):
                try:
                    launch(duplicate, profile)
                except EOFError:
                    assert contender.wait(timeout=4) != 0
                else:
                    raise AssertionError("A duplicate process acquired the same profile")

            session = api.read("/session", {"title": "test clean provider secret boundary"})["id"]
            assert (profile / "data/opencode/opencode.db").exists()
            assert not (root / "wrong.db").exists()
            with api.open("/event") as stream:
                events = Events(stream)
                events.until("server.connected")
                api.prompt(session, "credential check")
                ask = events.until("permission.asked", lambda p: p["sessionID"] == session)
                request = ask["properties"]["id"]
                assert api.read(f"/permission/{request}/reply", {"reply": "once"}) is True
                events.until("session.idle", lambda p: p["sessionID"] == session)
            assert (project / "credential-check.txt").read_text() == "clean"
            with api.open("/event") as stream:
                events = Events(stream)
                events.until("server.connected")
                api.prompt(session, "teardown check")
                ask = events.until("permission.asked", lambda p: p["sessionID"] == session)
                assert api.read(f"/permission/{ask['properties']['id']}/reply", {"reply": "once"}) is True
            marker = project / "active-shell.pid"
            shell = wait(lambda: int(marker.read_text()) if marker.exists() and marker.read_text() else 0,
                         "running stock tool")
            stock = descendants(child.pid)
            assert shell in stock and os.getpgid(shell) != child.pid
            assert any(os.getpgid(pid) == child.pid for pid in stock)
            # The private control stream is the owning process' lifetime.
            control.shutdown(socket.SHUT_RDWR)
            child.wait(timeout=5)
            assert not wait_gone(stock | {child.pid}, seconds=2)
            with closing(socket.socket()) as probe:
                assert probe.connect_ex(("127.0.0.1", endpoint.port)) != 0
            assert not list((profile / "config").glob(".ox-open-*"))
        print("Verified assigned loopback, exact profile and model, provider and relay key isolation, exclusive ownership, and teardown with a detached tool still running.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("opencode", type=Path)
    parser.add_argument("ox", type=Path)
    args = parser.parse_args()
    opencode, ox = (path.resolve(strict=True) for path in (args.opencode, args.ox))
    report_version(opencode)
    with tempfile.TemporaryDirectory(prefix="ox-opencode-bootstrap-") as temp, \
            ThreadingHTTPServer(("127.0.0.1", 0), CredentialProvider) as provider:
        provider.key = secrets.token_urlsafe(24)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        try:
            run(ox, opencode, Path(temp).resolve(), provider)
        finally:
            provider.shutdown()
            thread.join()


if __name__ == "__main__":
    main()
