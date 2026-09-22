#!/usr/bin/env python3
"""Standard-library end-to-end coverage for agent-tunnel.

This test intentionally launches the binary with argv lists only.  It owns all
processes and temporary files created during a run and does not use a shell.
"""

from __future__ import annotations

import argparse
import base64
import collections
import json
import os
import pathlib
import select
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any, Callable, Iterable, Optional


ROOT = pathlib.Path(__file__).resolve().parents[1]
MAX_TOTAL_JOBS = 16
TERMINAL_STATES = {"exited", "failed", "timed_out", "cancelled"}


class E2EFailure(AssertionError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise E2EFailure(message)


def mode(path: pathlib.Path) -> int:
    return stat.S_IMODE(path.stat().st_mode)


def text(data: bytes | str) -> str:
    if isinstance(data, bytes):
        return data.decode("utf-8", "replace")
    return data


def short_output(value: str, limit: int = 4000) -> str:
    if len(value) <= limit:
        return value
    return value[:limit] + f"\n... ({len(value) - limit} bytes omitted)"


def pick_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def wait_until(
    predicate: Callable[[], Any],
    timeout: float,
    description: str,
    interval: float = 0.05,
) -> Any:
    deadline = time.monotonic() + timeout
    last_error: Optional[BaseException] = None
    while time.monotonic() < deadline:
        try:
            result = predicate()
            if result:
                return result
        except BaseException as exc:  # diagnostics for a retryable readiness check
            last_error = exc
        time.sleep(interval)
    suffix = f"; last error: {last_error}" if last_error else ""
    raise E2EFailure(f"timed out waiting for {description}{suffix}")


def parse_json_line(output: bytes, context: str) -> dict[str, Any]:
    raw = text(output)
    lines = [line.strip() for line in raw.splitlines() if line.strip()]
    require(bool(lines), f"{context}: expected JSON output, got empty stdout")
    try:
        value = json.loads(lines[-1])
    except json.JSONDecodeError as exc:
        raise E2EFailure(
            f"{context}: invalid JSON stdout: {exc}; stdout={short_output(raw)!r}"
        ) from exc
    require(isinstance(value, dict), f"{context}: JSON result is not an object")
    return value


def run_cli(
    binary: pathlib.Path,
    args: Iterable[str],
    *,
    timeout: float = 30.0,
    cwd: pathlib.Path = ROOT,
) -> subprocess.CompletedProcess[bytes]:
    argv = [str(binary), *[str(arg) for arg in args]]
    try:
        return subprocess.run(
            argv,
            cwd=str(cwd),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
            shell=False,
        )
    except subprocess.TimeoutExpired as exc:
        raise E2EFailure(
            f"CLI timed out after {timeout}s: argv={argv!r}; "
            f"stdout={short_output(text(exc.stdout or b''))!r}; "
            f"stderr={short_output(text(exc.stderr or b''))!r}"
        ) from exc


class ManagedProcess:
    """Popen wrapper with process-group cleanup and bounded stderr capture."""

    def __init__(
        self,
        argv: list[str],
        name: str,
        *,
        interactive: bool = False,
        cwd: pathlib.Path = ROOT,
    ) -> None:
        self.argv = argv
        self.name = name
        self.process = subprocess.Popen(
            argv,
            cwd=str(cwd),
            stdin=subprocess.PIPE if interactive else subprocess.DEVNULL,
            stdout=subprocess.PIPE if interactive else subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            bufsize=0,
            start_new_session=True,
            shell=False,
        )
        self._stderr: collections.deque[str] = collections.deque(maxlen=200)
        self._stderr_lock = threading.Lock()
        self._stderr_thread = threading.Thread(
            target=self._capture_stderr,
            name=f"{name}-stderr",
            daemon=True,
        )
        self._stderr_thread.start()

    def _capture_stderr(self) -> None:
        stream = self.process.stderr
        if stream is None:
            return
        try:
            for line in iter(stream.readline, b""):
                with self._stderr_lock:
                    self._stderr.append(text(line).rstrip("\n"))
        finally:
            try:
                stream.close()
            except OSError:
                pass

    def poll(self) -> Optional[int]:
        return self.process.poll()

    def logs(self) -> str:
        with self._stderr_lock:
            return "\n".join(self._stderr)

    def wait_for_log(self, needle: str, timeout: float) -> None:
        def found() -> bool:
            if needle in self.logs():
                return True
            if self.poll() is not None:
                raise E2EFailure(
                    f"{self.name} exited with status {self.poll()} before log {needle!r}; "
                    f"stderr:\n{self.logs()}"
                )
            return False

        wait_until(found, timeout, f"{self.name} log {needle!r}")

    def read_stdout_line(self, timeout: float) -> bytes:
        stream = self.process.stdout
        require(stream is not None, f"{self.name} has no stdout pipe")
        ready, _, _ = select.select([stream], [], [], timeout)
        require(bool(ready), f"timed out waiting for {self.name} stdout")
        line = stream.readline()
        require(line != b"", f"{self.name} closed stdout; stderr:\n{self.logs()}")
        return line

    def terminate(self, timeout: float = 5.0) -> None:
        if self.poll() is not None:
            return
        try:
            os.killpg(self.process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.process.wait(timeout=timeout)
            return
        except subprocess.TimeoutExpired:
            pass
        try:
            os.killpg(self.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            self.process.wait(timeout=timeout)
        except subprocess.TimeoutExpired as exc:
            raise E2EFailure(
                f"could not clean up {self.name} process group; argv={self.argv!r}"
            ) from exc

    def close_stdin(self) -> None:
        if self.process.stdin is not None:
            try:
                self.process.stdin.close()
            except OSError:
                pass


class FaultProxy:
    """A raw TCP forwarder that can cut existing connections and pause accepts."""

    def __init__(self, listen_port: int, target_port: int) -> None:
        self.listen_port = listen_port
        self.target_port = target_port
        self._closed = threading.Event()
        self._blocked = threading.Event()
        self._lock = threading.Lock()
        self._connections: set[tuple[socket.socket, socket.socket]] = set()
        self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._listener.bind(("127.0.0.1", listen_port))
        self._listener.listen(64)
        self._listener.settimeout(0.2)
        self._thread = threading.Thread(target=self._serve, name="fault-proxy", daemon=True)
        self._thread.start()

    def _serve(self) -> None:
        while not self._closed.is_set():
            try:
                client, _ = self._listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            if self._blocked.is_set():
                self._close_socket(client)
                continue
            try:
                upstream = socket.create_connection(("127.0.0.1", self.target_port), timeout=3.0)
                upstream.settimeout(None)
            except OSError:
                self._close_socket(client)
                continue
            pair = (client, upstream)
            with self._lock:
                self._connections.add(pair)
            threading.Thread(
                target=self._pump,
                args=(pair, client, upstream),
                name="fault-proxy-c2s",
                daemon=True,
            ).start()
            threading.Thread(
                target=self._pump,
                args=(pair, upstream, client),
                name="fault-proxy-s2c",
                daemon=True,
            ).start()

    @staticmethod
    def _close_socket(sock: socket.socket) -> None:
        try:
            sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            sock.close()
        except OSError:
            pass

    def _close_pair(self, pair: tuple[socket.socket, socket.socket]) -> None:
        with self._lock:
            self._connections.discard(pair)
        self._close_socket(pair[0])
        self._close_socket(pair[1])

    def _pump(
        self,
        pair: tuple[socket.socket, socket.socket],
        source: socket.socket,
        destination: socket.socket,
    ) -> None:
        try:
            while not self._closed.is_set():
                data = source.recv(65536)
                if not data:
                    break
                destination.sendall(data)
        except OSError:
            pass
        finally:
            self._close_pair(pair)

    def active_count(self) -> int:
        with self._lock:
            return len(self._connections)

    def drop_and_block(self) -> None:
        self._blocked.set()
        with self._lock:
            pairs = list(self._connections)
        for pair in pairs:
            self._close_pair(pair)

    def release(self) -> None:
        self._blocked.clear()

    def close(self) -> None:
        self._blocked.set()
        self._closed.set()
        try:
            self._listener.close()
        except OSError:
            pass
        with self._lock:
            pairs = list(self._connections)
        for pair in pairs:
            self._close_pair(pair)
        self._thread.join(timeout=2.0)


class Session:
    def __init__(
        self,
        binary: pathlib.Path,
        root: pathlib.Path,
        name: str,
        relay_url: str,
        lease_secs: int,
    ) -> None:
        self.binary = binary
        self.root = root
        self.name = name
        self.relay_url = relay_url
        self.lease_secs = lease_secs
        self.relay_file = root / "relay.json"
        self.connector_file = root / "connector.json"
        self.controller_file = root / "controller.json"
        self.state_dir = root / "connector-state"
        self.socket_path = root / "controller.sock"
        self.connector: Optional[ManagedProcess] = None
        self.controller: Optional[ManagedProcess] = None
        self.session_id = ""
        self.target_id = ""
        self.connector_token = ""
        self.controller_token = ""
        self.incarnation = ""

    def initialize(self) -> None:
        cp = run_cli(
            self.binary,
            [
                "init",
                "--dir",
                str(self.root),
                "--relay",
                self.relay_url,
                "--name",
                self.name,
                "--ttl-secs",
                "300",
            ],
        )
        require(
            cp.returncode == 0,
            f"{self.name}: init failed with {cp.returncode}; "
            f"stdout={text(cp.stdout)!r}; stderr={text(cp.stderr)!r}",
        )
        output = text(cp.stdout)
        require('"token"' not in output, f"{self.name}: init printed a token")
        try:
            summary = json.loads(output)
        except json.JSONDecodeError as exc:
            raise E2EFailure(f"{self.name}: init summary is not JSON: {output!r}") from exc
        require(summary.get("directory") == str(self.root), f"{self.name}: init directory summary mismatch")
        require(mode(self.root) == 0o700, f"{self.name}: session directory mode is {oct(mode(self.root))}, expected 0700")
        for path in (self.relay_file, self.connector_file, self.controller_file):
            require(path.is_file(), f"{self.name}: missing {path.name}")
            require(mode(path) == 0o600, f"{self.name}: {path.name} mode is {oct(mode(path))}, expected 0600")
            require(path.stat().st_uid == os.geteuid(), f"{self.name}: {path.name} owner mismatch")
        relay = json.loads(self.relay_file.read_text())
        connector = json.loads(self.connector_file.read_text())
        controller = json.loads(self.controller_file.read_text())
        require(connector["role"] == "connector", f"{self.name}: connector role not connector")
        require(controller["role"] == "controller", f"{self.name}: controller role not controller")
        require(relay["session_id"] == connector["session_id"] == controller["session_id"], f"{self.name}: session IDs differ")
        require(connector["target_id"] == controller["target_id"], f"{self.name}: target IDs differ")
        self.session_id = connector["session_id"]
        self.target_id = connector["target_id"]
        self.connector_token = connector["token"]
        self.controller_token = controller["token"]

    def start(self) -> None:
        self.connector = ManagedProcess(
            [
                str(self.binary),
                "connect",
                "--config",
                str(self.connector_file),
                "--state-dir",
                str(self.state_dir),
                "--allow-exec",
                "--lease-secs",
                str(self.lease_secs),
            ],
            f"{self.name}-connector",
        )
        wait_until(lambda: self.state_dir.is_dir(), 5.0, f"{self.name} state directory")
        self.controller = ManagedProcess(
            [
                str(self.binary),
                "local",
                "--config",
                str(self.controller_file),
                "--socket",
                str(self.socket_path),
                "--accept-session-risk",
            ],
            f"{self.name}-controller",
        )
        wait_until(lambda: self.socket_path.exists(), 5.0, f"{self.name} controller socket")
        require(mode(self.socket_path) == 0o600, f"{self.name}: controller socket mode is {oct(mode(self.socket_path))}, expected 0600")
        wait_until(lambda: self.try_info()[0] is not None, 20.0, f"{self.name} controller/connector connection")
        info = self.info()
        self.incarnation = str(info["incarnation"])
        require(info["session_id"] == self.session_id, f"{self.name}: info session mismatch")
        require(info["target_id"] == self.target_id, f"{self.name}: info target mismatch")
        require(info["protocol"] == 1, f"{self.name}: unexpected protocol {info.get('protocol')!r}")
        require(info["approval"] == "session-approved", f"{self.name}: approval metadata mismatch")
        require(info["end_to_end_encrypted"] is False, f"{self.name}: unexpected E2EE metadata")

    def try_info(self) -> tuple[Optional[dict[str, Any]], Optional[dict[str, Any]]]:
        cp = run_cli(self.binary, ["info", "--socket", str(self.socket_path)], timeout=10.0)
        if not cp.stdout.strip():
            return None, None
        payload = parse_json_line(cp.stdout, f"{self.name} info")
        if payload.get("error") is not None:
            return None, payload
        return payload.get("result"), payload

    def info(self) -> dict[str, Any]:
        cp = run_cli(self.binary, ["info", "--socket", str(self.socket_path)], timeout=15.0)
        payload = parse_json_line(cp.stdout, f"{self.name} info")
        require(cp.returncode == 0, f"{self.name}: info failed: {payload}; stderr={text(cp.stderr)!r}")
        require(payload.get("error") is None and isinstance(payload.get("result"), dict), f"{self.name}: info reply error: {payload}")
        return payload["result"]

    def _call(self, args: list[str], context: str, timeout: float = 20.0) -> dict[str, Any]:
        cp = run_cli(self.binary, args, timeout=timeout)
        payload = parse_json_line(cp.stdout, context)
        if cp.returncode != 0 and payload.get("error") is None:
            raise E2EFailure(f"{context}: CLI status {cp.returncode}; stderr={text(cp.stderr)!r}; payload={payload}")
        return payload

    def exec(
        self,
        request_id: str,
        argv: list[str],
        *,
        cwd: pathlib.Path,
        timeout_ms: int = 60000,
        env: Optional[dict[str, str]] = None,
        incarnation: Optional[str] = None,
        timeout: float = 20.0,
    ) -> dict[str, Any]:
        args = [
            "exec",
            "--socket",
            str(self.socket_path),
            "--incarnation",
            incarnation if incarnation is not None else self.incarnation,
            "--cwd",
            str(cwd),
            "--request-id",
            request_id,
            "--timeout-ms",
            str(timeout_ms),
        ]
        for key, value in (env or {}).items():
            args.extend(["--env", f"{key}={value}"])
        args.extend(["--", *argv])
        return self._call(args, f"{self.name} exec {request_id}", timeout=timeout)

    def read(self, job_id: str, cursor: int = 0) -> dict[str, Any]:
        return self._call(
            ["read", "--socket", str(self.socket_path), "--job", job_id, "--cursor", str(cursor)],
            f"{self.name} read {job_id} cursor={cursor}",
            timeout=20.0,
        )

    def cancel(self, job_id: str) -> dict[str, Any]:
        return self._call(
            ["cancel", "--socket", str(self.socket_path), "--job", job_id],
            f"{self.name} cancel {job_id}",
            timeout=20.0,
        )

    def stop_controller(self) -> None:
        if self.controller is not None:
            self.controller.terminate()
            self.controller = None

    def stop_connector(self) -> None:
        if self.connector is not None:
            self.connector.terminate()
            self.connector = None

    def stop(self) -> None:
        self.stop_controller()
        self.stop_connector()


class Suite:
    def __init__(self, binary: pathlib.Path, lease_secs: int) -> None:
        self.binary = binary
        self.lease_secs = lease_secs
        self.jobs_started = 0
        self._known_jobs: set[str] = set()
        self.notes: list[str] = []

    def register_reply(self, payload: dict[str, Any], context: str) -> dict[str, Any]:
        require(payload.get("error") is None, f"{context}: unexpected error {payload.get('error')}")
        result = payload.get("result")
        require(isinstance(result, dict), f"{context}: expected object result, got {result!r}")
        job_id = result.get("job_id")
        if isinstance(job_id, str) and job_id not in self._known_jobs:
            self._known_jobs.add(job_id)
            self.jobs_started += 1
            require(self.jobs_started <= MAX_TOTAL_JOBS, f"test exceeded {MAX_TOTAL_JOBS} total started jobs")
        return result

    def expect_error(self, payload: dict[str, Any], code: str, context: str) -> None:
        error = payload.get("error")
        require(isinstance(error, dict), f"{context}: expected error {code}, got {payload!r}")
        require(error.get("code") == code, f"{context}: expected error {code}, got {error}")

    def read_to_terminal(
        self,
        session: Session,
        job_id: str,
        *,
        timeout: float = 20.0,
        min_state: Optional[str] = None,
    ) -> tuple[dict[str, Any], list[dict[str, Any]]]:
        deadline = time.monotonic() + timeout
        cursor = 0
        events: list[dict[str, Any]] = []
        last: Optional[dict[str, Any]] = None
        terminal_cursor: Optional[int] = None
        while time.monotonic() < deadline:
            payload = session.read(job_id, cursor)
            view = self.register_read(payload, f"{session.name} read {job_id}")
            last = view
            batch = view.get("events")
            require(isinstance(batch, list), f"{session.name}: read events is not a list")
            events.extend(batch)
            next_cursor = view.get("next_cursor")
            require(isinstance(next_cursor, int) and next_cursor >= cursor, f"{session.name}: cursor regressed {cursor}->{next_cursor}")
            for event in batch:
                require(isinstance(event.get("seq"), int), f"{session.name}: malformed output event {event!r}")
                require(event["seq"] >= cursor, f"{session.name}: cursor returned an old event {event!r}")
            progressed = next_cursor > cursor
            cursor = next_cursor
            state = view.get("state")
            if state in TERMINAL_STATES and (min_state is None or state == min_state):
                if terminal_cursor == cursor and not progressed:
                    return view, events
                terminal_cursor = cursor
            time.sleep(0.05)
        raise E2EFailure(f"{session.name}: job {job_id} did not reach terminal state; last={last}")

    @staticmethod
    def register_read(payload: dict[str, Any], context: str) -> dict[str, Any]:
        require(payload.get("error") is None, f"{context}: unexpected error {payload.get('error')}")
        result = payload.get("result")
        require(isinstance(result, dict), f"{context}: expected JobView result, got {result!r}")
        return result


def assert_job_identity(view: dict[str, Any], expected: dict[str, Any], context: str) -> None:
    for key in ("job_id", "pid", "pgid", "incarnation"):
        require(view.get(key) == expected.get(key), f"{context}: {key} changed: {view.get(key)!r} != {expected.get(key)!r}")


def process_exists(pid: int) -> bool:
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def load_json(path: pathlib.Path) -> dict[str, Any]:
    value = json.loads(path.read_text())
    require(isinstance(value, dict), f"{path}: expected JSON object")
    return value


def websocket_handshake(
    host: str,
    port: int,
    path: str,
    token: str,
    instance: str,
    *,
    origin: Optional[str] = None,
    timeout: float = 4.0,
) -> tuple[int, str]:
    key = base64.b64encode(os.urandom(16)).decode("ascii")
    headers = [
        f"GET {path} HTTP/1.1",
        f"Host: {host}:{port}",
        "Upgrade: websocket",
        "Connection: Upgrade",
        "Sec-WebSocket-Version: 13",
        f"Sec-WebSocket-Key: {key}",
        f"Authorization: Bearer {token}",
        f"X-Agent-Tunnel-Instance: {instance}",
    ]
    if origin is not None:
        headers.append(f"Origin: {origin}")
    request = ("\r\n".join(headers) + "\r\n\r\n").encode("ascii")
    with socket.create_connection((host, port), timeout=timeout) as sock:
        sock.sendall(request)
        response = bytearray()
        while b"\r\n\r\n" not in response and len(response) < 65536:
            chunk = sock.recv(4096)
            if not chunk:
                break
            response.extend(chunk)
    raw = bytes(response)
    header = raw.split(b"\r\n\r\n", 1)[0]
    first = header.split(b"\r\n", 1)[0].decode("ascii", "replace")
    pieces = first.split()
    require(len(pieces) >= 2, f"malformed WebSocket HTTP response: {first!r}")
    try:
        status = int(pieces[1])
    except ValueError as exc:
        raise E2EFailure(f"malformed WebSocket HTTP status: {first!r}") from exc
    return status, text(raw)


def test_init_and_role_isolation(
    suite: Suite,
    session: Session,
    proxy_port: int,
) -> None:
    # Existing-directory protection is checked before the relay starts, while the
    # created session's modes and token non-disclosure are checked by initialize().
    existing = session.root.parent / "already-exists"
    existing.mkdir(mode=0o700)
    marker = existing / "do-not-overwrite"
    marker.write_text("keep")
    cp = run_cli(
        suite.binary,
        [
            "init",
            "--dir",
            str(existing),
            "--relay",
            session.relay_url,
            "--ttl-secs",
            "300",
        ],
    )
    require(cp.returncode != 0, "init unexpectedly overwrote an existing directory")
    require(marker.read_text() == "keep", "existing init directory marker changed")
    require(not (existing / "controller.json").exists(), "failed init wrote controller.json")

    # Loading a credential under the wrong role must fail before networking.
    wrong_connector = run_cli(
        suite.binary,
        [
            "connect",
            "--config",
            str(session.controller_file),
            "--allow-exec",
            "--lease-secs",
            str(suite.lease_secs),
        ],
    )
    require(wrong_connector.returncode != 0, "connector accepted controller credentials")
    require("role mismatch" in text(wrong_connector.stderr), f"wrong connector role error changed: {text(wrong_connector.stderr)!r}")
    wrong_controller = run_cli(
        suite.binary,
        [
            "local",
            "--config",
            str(session.connector_file),
            "--socket",
            str(session.root / "wrong.sock"),
            "--accept-session-risk",
        ],
    )
    require(wrong_controller.returncode != 0, "controller accepted connector credentials")
    require("role mismatch" in text(wrong_controller.stderr), f"wrong controller role error changed: {text(wrong_controller.stderr)!r}")

    base = f"/v1/connect/{session.session_id}"
    control = f"/v1/control/{session.session_id}"
    status, _ = websocket_handshake("127.0.0.1", proxy_port, base, "0" * 64, "raw-bad-token")
    require(status == 401, f"wrong connector token returned HTTP {status}, expected 401")
    status, _ = websocket_handshake("127.0.0.1", proxy_port, base, session.controller_token, "raw-wrong-role")
    require(status == 401, f"controller token on connector route returned HTTP {status}, expected 401")
    status, _ = websocket_handshake("127.0.0.1", proxy_port, control, session.connector_token, "raw-wrong-role-2")
    require(status == 401, f"connector token on control route returned HTTP {status}, expected 401")
    status, _ = websocket_handshake(
        "127.0.0.1",
        proxy_port,
        base,
        session.connector_token,
        "raw-origin",
        origin="https://untrusted.example",
    )
    require(status == 403, f"Origin-bearing connector handshake returned HTTP {status}, expected 403")
    status, _ = websocket_handshake("127.0.0.1", proxy_port, base, session.connector_token, "raw-connector-binding")
    require(status == 409, f"second connector instance returned HTTP {status}, expected 409")
    status, _ = websocket_handshake("127.0.0.1", proxy_port, control, session.controller_token, "raw-controller-binding")
    require(status == 409, f"second controller instance returned HTTP {status}, expected 409")


def test_primary_operations(suite: Suite, session: Session, workdir: pathlib.Path) -> None:
    info = session.info()
    require(info["cwd"], "info did not report connector cwd")
    require(isinstance(info["uid"], int) and isinstance(info["gid"], int), "info uid/gid metadata malformed")

    # cwd, env, stdout, stderr, non-zero exit, and stdin EOF in one direct argv job.
    basic_code = (
        "import os,sys; "
        "data=sys.stdin.read(); "
        "print('E2E_STDOUT|cwd='+os.getcwd()+'|env='+os.getenv('E2E_ENV','missing')+'|stdin_eof='+str(data=='').lower(), flush=True); "
        "print('E2E_STDERR|separate-stream', file=sys.stderr, flush=True); "
        "sys.exit(7)"
    )
    basic = suite.register_reply(
        session.exec(
            "basic-stdout-stderr",
            [sys.executable, "-c", basic_code],
            cwd=workdir,
            env={"E2E_ENV": "present"},
            timeout_ms=10000,
        ),
        "basic job",
    )
    basic_view, basic_events = suite.read_to_terminal(session, basic["job_id"], timeout=15.0)
    output = "".join(event["text"] for event in basic_events)
    require("E2E_STDOUT|cwd=" + str(workdir) in output, f"cwd was not applied: {output!r}")
    require("|env=present|stdin_eof=true" in output, f"env/default stdin EOF was not applied: {output!r}")
    require("E2E_STDERR|separate-stream" in output, f"stderr was not captured: {output!r}")
    require(basic_view["state"] == "exited", f"basic job state is {basic_view['state']!r}")
    require(basic_view["exit_code"] == 7, f"basic job exit code is {basic_view['exit_code']!r}")
    streams = {event["stream"] for event in basic_events}
    require({"stdout", "stderr"}.issubset(streams), f"stdout/stderr stream labels missing: {streams!r}")
    first_read = session.read(basic["job_id"], 0)
    first_view = suite.register_read(first_read, "basic cursor first read")
    next_cursor = first_view["next_cursor"]
    second_read = session.read(basic["job_id"], next_cursor)
    second_view = suite.register_read(second_read, "basic cursor continuation")
    require(all(event["seq"] >= next_cursor for event in second_view["events"]), "cursor returned an old event")

    # Incarnation binding is checked before any process is spawned.
    mismatch = session.exec(
        "bad-incarnation",
        [sys.executable, "-c", "print('must-not-run')"],
        cwd=workdir,
        incarnation="not-the-current-incarnation",
    )
    suite.expect_error(mismatch, "TARGET_MISMATCH", "incarnation mismatch")

    timeout = suite.register_reply(
        session.exec(
            "timeout-job",
            [sys.executable, "-c", "import time; print('before-timeout', flush=True); time.sleep(10)"],
            cwd=workdir,
            timeout_ms=250,
        ),
        "timeout job",
    )
    timeout_view, timeout_events = suite.read_to_terminal(session, timeout["job_id"], timeout=10.0)
    require(timeout_view["state"] == "timed_out", f"timeout state is {timeout_view['state']!r}; events={timeout_events!r}")
    require(timeout_view["termination_reason"] == "timed_out", f"timeout reason is {timeout_view['termination_reason']!r}")

    cancel = suite.register_reply(
        session.exec(
            "cancel-job",
            [sys.executable, "-c", "import time; print('cancel-ready', flush=True); time.sleep(30)"],
            cwd=workdir,
            timeout_ms=60000,
        ),
        "cancel job",
    )
    cancel_reply = session.cancel(cancel["job_id"])
    cancel_result = suite.register_reply(cancel_reply, "cancel request")
    require(cancel_result.get("job_id") == cancel["job_id"], "cancel response job mismatch")
    cancel_view, _ = suite.read_to_terminal(session, cancel["job_id"], timeout=10.0)
    require(cancel_view["state"] == "cancelled", f"cancel state is {cancel_view['state']!r}")
    require(cancel_view["termination_reason"] == "cancelled", f"cancel reason is {cancel_view['termination_reason']!r}")

    big_size = 1_100_000
    big_code = f"import sys; sys.stdout.write('B'*{big_size}); sys.stdout.flush()"
    big = suite.register_reply(
        session.exec("large-output", [sys.executable, "-c", big_code], cwd=workdir, timeout_ms=30000),
        "large output job",
    )
    big_view, big_events = suite.read_to_terminal(session, big["job_id"], timeout=25.0)
    require(big_view["state"] == "exited", f"large-output state is {big_view['state']!r}")
    require(big_view["output_truncated"] is True, "large output did not report truncation")
    require(big_view["dropped_before_cursor"] is not None, "large output omitted dropped cursor metadata")
    big_text = "".join(event["text"] for event in big_events)
    require(len(big_text) > 0 and set(big_text) <= {"B"}, "large output was altered unexpectedly")
    require(all(len(event["text"]) <= 32768 for event in big_events), "read exceeded MAX_READ boundary")

    # Four jobs may run concurrently; the fifth must be rejected without a fifth process.
    concurrent: list[dict[str, Any]] = []
    for index in range(4):
        concurrent.append(
            suite.register_reply(
                session.exec(
                    f"concurrent-{index}",
                    [sys.executable, "-c", "import time; time.sleep(3)"],
                    cwd=workdir,
                    timeout_ms=10000,
                ),
                f"concurrent job {index}",
            )
        )
    over_limit = session.exec(
        "concurrent-over-limit",
        [sys.executable, "-c", "print('must-not-run')"],
        cwd=workdir,
        timeout_ms=10000,
    )
    suite.expect_error(over_limit, "RESOURCE_LIMIT", "fifth concurrent job")
    for job in concurrent:
        view, _ = suite.read_to_terminal(session, job["job_id"], timeout=15.0)
        require(view["state"] == "exited", f"concurrent job did not exit: {view}")


def test_transport_reconnect(suite: Suite, session: Session, proxy: FaultProxy, workdir: pathlib.Path) -> None:
    marker = workdir / "reconnect-count.txt"
    reconnect_code = (
        "import pathlib,time; "
        f"p=pathlib.Path({str(marker)!r}); "
        "n=int(p.read_text() or '0')+1 if p.exists() else 1; p.write_text(str(n)); "
        "print('RECONNECT_START', flush=True); time.sleep(4); print('RECONNECT_END', flush=True)"
    )
    original = suite.register_reply(
        session.exec(
            "reconnect-idempotent",
            [sys.executable, "-c", reconnect_code],
            cwd=workdir,
            timeout_ms=30000,
        ),
        "reconnect job",
    )
    wait_until(lambda: marker.exists() and marker.read_text() == "1", 5.0, "reconnect job start marker")
    wait_until(lambda: proxy.active_count() >= 2, 5.0, "two established WebSocket transports")
    proxy.drop_and_block()
    wait_until(lambda: proxy.active_count() == 0, 5.0, "fault proxy to close established transports")
    proxy.release()

    # Poll the same request ID through reconnect.  A new ID would be unsafe and
    # would be a test failure, not a retry strategy.
    def same_request_reply() -> Optional[dict[str, Any]]:
        payload = session.exec(
            "reconnect-idempotent",
            [sys.executable, "-c", reconnect_code],
            cwd=workdir,
            timeout_ms=30000,
            timeout=10.0,
        )
        if payload.get("error") is not None:
            code = payload["error"].get("code")
            if code in {"TARGET_OFFLINE", "EXECUTION_UNKNOWN"}:
                return None
            raise E2EFailure(f"same request ID after reconnect returned {payload}")
        return payload

    repeated_payload = wait_until(same_request_reply, 12.0, "same request ID after transport reconnect")
    repeated = suite.register_reply(repeated_payload, "reconnect idempotent replay")
    assert_job_identity(repeated, original, "reconnect idempotency")
    require(marker.read_text() == "1", f"reconnect retry reran the command: marker={marker.read_text()!r}")
    final_view, events = suite.read_to_terminal(session, original["job_id"], timeout=15.0)
    require(final_view["state"] == "exited", f"reconnect job state is {final_view['state']!r}")
    captured = "".join(event["text"] for event in events)
    require("RECONNECT_START" in captured and "RECONNECT_END" in captured, f"read after reconnect lost output: {captured!r}")

    conflict = session.exec(
        "reconnect-idempotent",
        [sys.executable, "-c", "print('different arguments')"],
        cwd=workdir,
        timeout_ms=30000,
    )
    suite.expect_error(conflict, "REQUEST_CONFLICT", "same request ID with different arguments")


def inspect_state(suite: Suite, session: Session) -> list[dict[str, Any]]:
    cp = run_cli(suite.binary, ["inspect", "--state-dir", str(session.state_dir)], timeout=15.0)
    payload = parse_json_line(cp.stdout, f"{session.name} inspect")
    require(cp.returncode == 0, f"{session.name}: inspect failed: {payload}; stderr={text(cp.stderr)!r}")
    records = payload.get("records")
    require(isinstance(records, list), f"{session.name}: inspect records is not a list")
    return records


def wait_for_record(
    suite: Suite,
    session: Session,
    job_id: str,
    timeout: float,
) -> dict[str, Any]:
    def find() -> Optional[dict[str, Any]]:
        for entry in inspect_state(suite, session):
            if isinstance(entry, dict) and isinstance(entry.get("record"), dict) and entry["record"].get("job_id") == job_id:
                return entry
        return None

    return wait_until(find, timeout, f"state record {job_id}")


def test_controller_death_lease(suite: Suite, session: Session, workdir: pathlib.Path) -> None:
    job = suite.register_reply(
        session.exec(
            "controller-death-lease",
            [sys.executable, "-c", "import time; print('lease-running', flush=True); time.sleep(60)"],
            cwd=workdir,
            timeout_ms=60000,
        ),
        "controller death lease job",
    )
    pid = int(job["pid"])
    wait_until(lambda: process_exists(pid), 3.0, "controller-death child process")
    session.stop_controller()
    entry = wait_for_record(suite, session, job["job_id"], suite.lease_secs + 8.0)
    record = entry["record"]
    for key in ("session_id", "target_id", "incarnation", "job_id", "request_id", "pid", "pgid", "updated_at"):
        require(key in record, f"lease state record omitted {key}")
    require(record["job_id"] == job["job_id"], "lease state record job mismatch")
    require(record["request_id"] == "controller-death-lease", "lease state record request ID mismatch")
    require(record["state"] == "cancelled", f"controller death did not cancel job: {record}")
    require(record["termination_reason"] == "cancelled", f"controller death termination reason: {record}")
    require(not process_exists(pid), f"controller death left job PID {pid} running")
    require(session.connector is not None, "lease test lost connector handle")
    wait_until(lambda: session.connector is not None and session.connector.poll() is not None, 5.0, "connector exit after lease expiry")
    require(mode(session.state_dir) == 0o700, "lease state directory permissions changed")
    for path in session.state_dir.glob("*.json"):
        require(mode(path) == 0o600, f"lease state record {path.name} mode is {oct(mode(path))}")


def test_connector_exit_cleanup(suite: Suite, session: Session, workdir: pathlib.Path) -> None:
    job = suite.register_reply(
        session.exec(
            "connector-exit-cleanup",
            [sys.executable, "-c", "import time; print('connector-running', flush=True); time.sleep(60)"],
            cwd=workdir,
            timeout_ms=60000,
        ),
        "connector exit cleanup job",
    )
    pid = int(job["pid"])
    wait_until(lambda: process_exists(pid), 3.0, "connector-exit child process")
    session.stop_connector()
    entry = wait_for_record(suite, session, job["job_id"], 8.0)
    record = entry["record"]
    require(record["state"] == "cancelled", f"connector exit did not cancel job: {record}")
    require(record["termination_reason"] == "cancelled", f"connector exit reason: {record}")
    require(record["request_id"] == "connector-exit-cleanup", "connector exit record request mismatch")
    require(not process_exists(pid), f"connector exit left job PID {pid} running")
    require(session.controller is not None and session.controller.poll() is None, "controller died while testing connector exit")


class McpProbe:
    def __init__(self, binary: pathlib.Path, socket_path: pathlib.Path, version: str) -> None:
        self.version = version
        self.process = ManagedProcess(
            [str(binary), "mcp", "--socket", str(socket_path)],
            f"mcp-{version}",
            interactive=True,
        )

    def send(self, value: dict[str, Any]) -> None:
        stream = self.process.process.stdin
        require(stream is not None, "MCP stdin pipe missing")
        stream.write((json.dumps(value, separators=(",", ":")) + "\n").encode("utf-8"))
        stream.flush()

    def receive(self, timeout: float = 8.0) -> dict[str, Any]:
        raw = self.process.read_stdout_line(timeout)
        try:
            value = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise E2EFailure(f"MCP {self.version}: invalid JSON response {raw!r}") from exc
        require(isinstance(value, dict), f"MCP {self.version}: response is not an object")
        return value

    def request(self, request_id: int, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        return self.receive()

    def close(self) -> None:
        self.process.close_stdin()
        self.process.terminate()


def test_mcp(suite: Suite, session: Session) -> None:
    failures: list[str] = []
    for version in ("2025-06-18", "2025-11-25"):
        probe = McpProbe(suite.binary, session.socket_path, version)
        try:
            initialize = probe.request(
                1,
                "initialize",
                {
                    "protocolVersion": version,
                    "capabilities": {},
                    "clientInfo": {"name": "agent-tunnel-e2e", "version": "1"},
                },
            )
            if "error" in initialize:
                failures.append(f"{version} initialize error: {initialize['error']}")
                continue
            result = initialize.get("result")
            require(isinstance(result, dict), f"MCP {version}: initialize result missing")
            negotiated = result.get("protocolVersion")
            require(isinstance(negotiated, str), f"MCP {version}: negotiated protocol missing")
            require(negotiated in {"2025-06-18", "2025-11-25"}, f"MCP negotiated unsupported version {negotiated!r}")
            probe.send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
            tools = probe.request(2, "tools/list", {})
            require("error" not in tools, f"MCP {version}: tools/list error {tools}")
            tool_items = tools.get("result", {}).get("tools", [])
            names = {item.get("name") for item in tool_items if isinstance(item, dict)}
            require({"remote_info", "remote_exec", "remote_read", "remote_cancel"}.issubset(names), f"MCP tools missing: {names!r}")
            call = probe.request(3, "tools/call", {"name": "remote_info", "arguments": {}})
            require("error" not in call, f"MCP {version}: tools/call JSON-RPC error {call}")
            call_result = call.get("result")
            require(isinstance(call_result, dict), f"MCP {version}: tools/call result missing")
            structured = call_result.get("structuredContent")
            require(isinstance(structured, dict), f"MCP {version}: structuredContent missing")
            require(structured.get("error") is None, f"MCP {version}: remote_info returned error {structured}")
            suite.notes.append(f"MCP compatible with {version} (negotiated {negotiated})")
            return
        except (E2EFailure, OSError, subprocess.SubprocessError) as exc:
            failures.append(f"{version} probe exception: {exc}")
        finally:
            probe.close()
    raise E2EFailure("MCP compatibility failure: " + " | ".join(failures))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    parser.add_argument(
        "--lease-secs",
        type=int,
        default=10,
        help="Connector lease used by death tests (5..60; default: 10)",
    )
    args = parser.parse_args()
    binary = args.binary.expanduser().resolve()
    require(binary.is_file() and os.access(binary, os.X_OK), f"binary is not executable: {binary}")
    require(5 <= args.lease_secs <= 60, "--lease-secs must be between 5 and 60")
    require(sys.platform.startswith("linux"), "these e2e tests require Linux /proc and Unix process groups")

    suite = Suite(binary, args.lease_secs)
    managed_sessions: list[Session] = []
    proxy: Optional[FaultProxy] = None
    temp: Optional[tempfile.TemporaryDirectory[str]] = None
    try:
        temp = tempfile.TemporaryDirectory(prefix="agent-tunnel-e2e-")
        root = pathlib.Path(temp.name)
        relay_port = pick_port()
        proxy_port = pick_port()
        relay_url = f"ws://127.0.0.1:{proxy_port}/"
        primary = Session(binary, root / "primary", "e2e-primary", relay_url, args.lease_secs)
        lease = Session(binary, root / "lease", "e2e-lease", relay_url, args.lease_secs)
        connector_exit = Session(binary, root / "connector-exit", "e2e-connector-exit", relay_url, args.lease_secs)
        managed_sessions.extend([primary, lease, connector_exit])
        for session in managed_sessions:
            session.initialize()

        proxy = FaultProxy(proxy_port, relay_port)
        relay_args = [str(binary), "relay", "--listen", f"127.0.0.1:{relay_port}"]
        for session in managed_sessions:
            relay_args.extend(["--session-file", str(session.relay_file)])
        relay = ManagedProcess(relay_args, "relay")
        try:
            relay.wait_for_log("relay listening on", 10.0)
            primary.start()
            test_init_and_role_isolation(suite, primary, proxy_port)

            workdir = root / "work"
            workdir.mkdir(mode=0o700)
            test_primary_operations(suite, primary, workdir)
            test_transport_reconnect(suite, primary, proxy, workdir)
            test_mcp(suite, primary)

            lease.start()
            test_controller_death_lease(suite, lease, workdir)

            connector_exit.start()
            test_connector_exit_cleanup(suite, connector_exit, workdir)

            print(f"PASS: standard-library end-to-end suite; started_jobs={suite.jobs_started}/{MAX_TOTAL_JOBS}")
            for note in suite.notes:
                print(f"NOTE: {note}")
            return 0
        finally:
            # The relay is intentionally cleaned after endpoint processes so it
            # can close their transport sockets deterministically.
            for session in reversed(managed_sessions):
                session.stop()
            relay.terminate()
    except E2EFailure as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        if suite.notes:
            for note in suite.notes:
                print(f"NOTE: {note}", file=sys.stderr)
        return 1
    except Exception as exc:
        print(f"FAIL: unexpected {type(exc).__name__}: {exc}", file=sys.stderr)
        traceback = __import__("traceback")
        traceback.print_exc()
        return 1
    finally:
        if proxy is not None:
            proxy.close()
        if temp is not None:
            temp.cleanup()


if __name__ == "__main__":
    raise SystemExit(main())
