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
import shlex
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
MAX_TOTAL_JOBS = 64
MAX_JOBS_PER_SESSION = 16
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


class WebSocketCapture:
    """Decode WebSocket text messages without changing the forwarded bytes.

    The relay terminates WebSocket framing but must not terminate Noise.  This
    parser gives the test the same view of the outer application frames that a
    relay would have, including unmasking client-to-server frames.  It does
    not parse or decrypt the JSON carried by a ``data`` frame.
    """

    def __init__(self) -> None:
        self.buffer = bytearray()
        self.handshake_done = False
        self.fragment_opcode: Optional[int] = None
        self.fragment = bytearray()

    def feed(self, data: bytes) -> list[bytes]:
        self.buffer.extend(data)
        messages: list[bytes] = []
        if not self.handshake_done:
            marker = self.buffer.find(b"\r\n\r\n")
            if marker < 0:
                # HTTP headers are bounded by the server/client handshake;
                # avoid retaining arbitrary data if a peer is malformed.
                if len(self.buffer) > 64 * 1024:
                    self.buffer.clear()
                return messages
            del self.buffer[: marker + 4]
            self.handshake_done = True

        while True:
            if len(self.buffer) < 2:
                return messages
            first, second = self.buffer[0], self.buffer[1]
            opcode = first & 0x0F
            masked = bool(second & 0x80)
            length = second & 0x7F
            header_len = 2
            if length == 126:
                if len(self.buffer) < 4:
                    return messages
                length = int.from_bytes(self.buffer[2:4], "big")
                header_len = 4
            elif length == 127:
                if len(self.buffer) < 10:
                    return messages
                length = int.from_bytes(self.buffer[2:10], "big")
                header_len = 10
            mask_len = 4 if masked else 0
            total = header_len + mask_len + length
            if total > 64 * 1024 * 1024:
                self.buffer.clear()
                return messages
            if len(self.buffer) < total:
                return messages

            frame = bytes(self.buffer[:total])
            del self.buffer[:total]
            payload_start = header_len + mask_len
            payload = bytearray(frame[payload_start:])
            if masked:
                mask = frame[header_len : header_len + 4]
                for index in range(len(payload)):
                    payload[index] ^= mask[index % 4]

            final = bool(first & 0x80)
            if opcode == 1:  # text
                if final:
                    messages.append(bytes(payload))
                else:
                    self.fragment_opcode = opcode
                    self.fragment = payload
            elif opcode == 0 and self.fragment_opcode == 1:
                self.fragment.extend(payload)
                if final:
                    messages.append(bytes(self.fragment))
                    self.fragment_opcode = None
                    self.fragment.clear()
            # Ping/Pong/Close and binary frames are deliberately ignored.


class FaultProxy:
    """A raw TCP forwarder that can cut connections and record outer WS frames."""

    def __init__(self, listen_port: int, target_port: int) -> None:
        self.listen_port = listen_port
        self.target_port = target_port
        self._closed = threading.Event()
        self._blocked = threading.Event()
        self._lock = threading.Lock()
        self._connections: set[tuple[socket.socket, socket.socket]] = set()
        self._captured_frames: list[tuple[str, bytes]] = []
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
                args=(pair, client, upstream, "client->relay", WebSocketCapture()),
                name="fault-proxy-c2s",
                daemon=True,
            ).start()
            threading.Thread(
                target=self._pump,
                args=(pair, upstream, client, "relay->client", WebSocketCapture()),
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
        direction: str,
        capture: WebSocketCapture,
    ) -> None:
        try:
            while not self._closed.is_set():
                data = source.recv(65536)
                if not data:
                    break
                frames = capture.feed(data)
                if frames:
                    with self._lock:
                        self._captured_frames.extend((direction, frame) for frame in frames)
                destination.sendall(data)
        except OSError:
            pass
        finally:
            self._close_pair(pair)

    def active_count(self) -> int:
        with self._lock:
            return len(self._connections)

    def clear_captured_frames(self) -> None:
        with self._lock:
            self._captured_frames.clear()

    def captured_application_frames(self) -> list[tuple[str, bytes]]:
        with self._lock:
            return list(self._captured_frames)

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
        require(isinstance(connector.get("channel_key"), str) and len(connector["channel_key"]) == 64, f"{self.name}: connector channel key missing")
        require(connector["channel_key"] == controller.get("channel_key"), f"{self.name}: endpoint channel keys differ")
        require("channel_key" not in relay, f"{self.name}: relay credential contains the end-to-end key")
        self.session_id = connector["session_id"]
        self.target_id = connector["target_id"]
        self.connector_token = connector["token"]
        self.controller_token = controller["token"]

    def start_connector(self) -> None:
        require(self.connector is None, f"{self.name}: connector already started")
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

    def start_controller(self, *, wait_for_connection: bool = True) -> None:
        require(self.controller is None, f"{self.name}: controller already started")
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
        if not wait_for_connection:
            return
        wait_until(lambda: self.try_info()[0] is not None, 20.0, f"{self.name} controller/connector connection")
        info = self.info()
        self.incarnation = str(info["incarnation"])
        require(info["session_id"] == self.session_id, f"{self.name}: info session mismatch")
        require(info["target_id"] == self.target_id, f"{self.name}: info target mismatch")
        require(info["protocol"] == 1, f"{self.name}: unexpected protocol {info.get('protocol')!r}")
        require(info["approval"] == "session-approved", f"{self.name}: approval metadata mismatch")
        require(info["end_to_end_encrypted"] is True, f"{self.name}: unexpected E2EE metadata")

    def start(self) -> None:
        self.start_connector()
        self.start_controller()

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
        stdin: bool = False,
        pty: bool = False,
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
        if stdin:
            args.append("--stdin")
        if pty:
            args.append("--pty")
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

    def write(self, job_id: str, request_id: str, data: str = "", *, eof: bool = False) -> dict[str, Any]:
        args = [
            "write",
            "--socket",
            str(self.socket_path),
            "--job",
            job_id,
            "--request-id",
            request_id,
            "--data",
            data,
        ]
        if eof:
            args.append("--eof")
        return self._call(args, f"{self.name} write {request_id}", timeout=20.0)

    def resize(self, job_id: str, rows: int, cols: int) -> dict[str, Any]:
        return self._call(
            [
                "resize",
                "--socket",
                str(self.socket_path),
                "--job",
                job_id,
                "--rows",
                str(rows),
                "--cols",
                str(cols),
            ],
            f"{self.name} resize {job_id} {rows}x{cols}",
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
        self._known_jobs_by_session: dict[str, set[str]] = collections.defaultdict(set)
        self.notes: list[str] = []

    def register_reply(
        self,
        payload: dict[str, Any],
        context: str,
        *,
        session: Optional[Session] = None,
    ) -> dict[str, Any]:
        require(payload.get("error") is None, f"{context}: unexpected error {payload.get('error')}")
        result = payload.get("result")
        require(isinstance(result, dict), f"{context}: expected object result, got {result!r}")
        job_id = result.get("job_id")
        if isinstance(job_id, str) and job_id not in self._known_jobs:
            self._known_jobs.add(job_id)
            self.jobs_started += 1
            require(self.jobs_started <= MAX_TOTAL_JOBS, f"test exceeded {MAX_TOTAL_JOBS} total started jobs")
        if session is not None and isinstance(job_id, str):
            jobs = self._known_jobs_by_session[session.name]
            if job_id not in jobs:
                jobs.add(job_id)
                require(
                    len(jobs) <= MAX_JOBS_PER_SESSION,
                    f"{session.name}: test observed more than {MAX_JOBS_PER_SESSION} jobs in one session",
                )
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


def read_until_text(
    suite: Suite,
    session: Session,
    job_id: str,
    marker: str,
    *,
    cursor: int = 0,
    timeout: float = 20.0,
) -> tuple[dict[str, Any], list[dict[str, Any]], int]:
    deadline = time.monotonic() + timeout
    events: list[dict[str, Any]] = []
    last: Optional[dict[str, Any]] = None
    while time.monotonic() < deadline:
        payload = session.read(job_id, cursor)
        view = suite.register_read(payload, f"{session.name} read {job_id} while waiting for {marker}")
        last = view
        batch = view.get("events")
        require(isinstance(batch, list), f"{session.name}: read events is not a list")
        events.extend(batch)
        next_cursor = view.get("next_cursor")
        require(isinstance(next_cursor, int) and next_cursor >= cursor, f"{session.name}: cursor regressed {cursor}->{next_cursor}")
        cursor = next_cursor
        output = "".join(event.get("text", "") for event in events)
        if marker in output:
            return view, events, cursor
        if view.get("state") in TERMINAL_STATES:
            break
        time.sleep(0.05)
    raise E2EFailure(f"{session.name}: job {job_id} did not emit {marker!r}; last={last}; events={events!r}")


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


def replace_channel_key(path: pathlib.Path) -> None:
    """Replace an endpoint key atomically while retaining its private mode."""
    value = load_json(path)
    key = value.get("channel_key")
    require(isinstance(key, str) and len(key) == 64, f"{path}: cannot mutate missing channel key")
    replacement = ("0" if key[0] != "0" else "1") + key[1:]
    value["channel_key"] = replacement
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)
    require(mode(path) == 0o600, f"{path}: mutated credential mode is not 0600")


def relay_admin(
    suite: Suite,
    admin_socket: pathlib.Path,
    *,
    session_id: Optional[str] = None,
) -> dict[str, Any]:
    args = ["sessions", "--admin-socket", str(admin_socket)] if session_id is None else [
        "revoke",
        "--admin-socket",
        str(admin_socket),
        "--session",
        session_id,
    ]
    cp = run_cli(suite.binary, args, timeout=15.0)
    payload = parse_json_line(cp.stdout, f"relay admin {'status' if session_id is None else 'revoke'}")
    require(cp.returncode == 0, f"relay admin failed: {payload}; stderr={text(cp.stderr)!r}")
    return payload


class RawWebSocket:
    """Minimal client used only for relay/Noise negative-path tests."""

    def __init__(self, sock: socket.socket) -> None:
        self.sock = sock

    @classmethod
    def connect(
        cls,
        host: str,
        port: int,
        path: str,
        token: str,
        instance: str,
        *,
        timeout: float = 4.0,
    ) -> "RawWebSocket":
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (
            "\r\n".join(
                [
                    f"GET {path} HTTP/1.1",
                    f"Host: {host}:{port}",
                    "Upgrade: websocket",
                    "Connection: Upgrade",
                    "Sec-WebSocket-Version: 13",
                    f"Sec-WebSocket-Key: {key}",
                    f"Authorization: Bearer {token}",
                    f"X-Agent-Tunnel-Instance: {instance}",
                ]
            )
            + "\r\n\r\n"
        ).encode("ascii")
        sock = socket.create_connection((host, port), timeout=timeout)
        try:
            sock.sendall(request)
            response = bytearray()
            while b"\r\n\r\n" not in response and len(response) < 65536:
                chunk = sock.recv(4096)
                if not chunk:
                    break
                response.extend(chunk)
            header = bytes(response).split(b"\r\n\r\n", 1)[0]
            first = header.split(b"\r\n", 1)[0].decode("ascii", "replace")
            pieces = first.split()
            require(len(pieces) >= 2, f"malformed raw WebSocket response: {first!r}")
            status = int(pieces[1])
            require(status == 101, f"raw WebSocket upgrade returned HTTP {status}: {first!r}")
            sock.settimeout(timeout)
            return cls(sock)
        except BaseException:
            try:
                sock.close()
            except OSError:
                pass
            raise

    def send_text(self, value: bytes | str) -> None:
        payload = value.encode("utf-8") if isinstance(value, str) else value
        mask = os.urandom(4)
        if len(payload) < 126:
            header = bytes((0x81, 0x80 | len(payload)))
        elif len(payload) < 65536:
            header = bytes((0x81, 0xFE)) + len(payload).to_bytes(2, "big")
        else:
            header = bytes((0x81, 0xFF)) + len(payload).to_bytes(8, "big")
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def close(self) -> None:
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass


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
    # dropped_before_cursor describes THIS requested cursor, not global history.
    # The draining helper finishes at the current cursor, so explicitly request
    # the now-stale zero cursor to verify the gap and its continuation boundary.
    stale = suite.register_read(session.read(big["job_id"], 0), "stale large-output cursor")
    require(isinstance(stale["dropped_before_cursor"], int) and stale["dropped_before_cursor"] > 0,
            "stale large-output cursor omitted dropped range metadata")
    require(stale["events"] and stale["events"][0]["seq"] == stale["dropped_before_cursor"],
            "gap metadata did not identify the first retained output event")
    require(sum(len(e["text"].encode()) for e in stale["events"]) <= 32768,
            "aggregate read exceeded MAX_READ")
    continued = suite.register_read(session.read(big["job_id"], stale["next_cursor"]), "continued retained output")
    require(continued["dropped_before_cursor"] is None, "valid retained cursor falsely reported a gap")
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


def test_pipe_stdin(suite: Suite, session: Session, workdir: pathlib.Path) -> None:
    marker = "PIPE_INPUT_UNIQUE_E2E"
    job = suite.register_reply(
        session.exec(
            "pipe-cat",
            ["/bin/cat"],
            cwd=workdir,
            timeout_ms=30000,
            stdin=True,
        ),
        "pipe cat job",
        session=session,
    )

    first = session.write(job["job_id"], "pipe-write", marker + "\n")
    require(first.get("error") is None, f"pipe first write failed: {first}")
    duplicate = session.write(job["job_id"], "pipe-write", marker + "\n")
    require(duplicate.get("error") is None, f"duplicate pipe write failed: {duplicate}")
    eof = session.write(job["job_id"], "pipe-eof", eof=True)
    require(eof.get("error") is None, f"pipe EOF failed: {eof}")

    view, events = suite.read_to_terminal(session, job["job_id"], timeout=15.0)
    output = "".join(event["text"] for event in events)
    require(output.count(marker) == 1, f"duplicate write was delivered more than once: {output!r}")
    require(view["state"] == "exited", f"pipe cat state is {view['state']!r}")


def test_pty_shell(suite: Suite, session: Session, workdir: pathlib.Path) -> None:
    nested = workdir / "pty-nested"
    nested.mkdir(mode=0o700)
    state_marker = "PTY_STATE_UNIQUE_E2E"
    resize_marker = "PTY_RESIZE_UNIQUE_E2E"
    job = suite.register_reply(
        session.exec(
            "pty-shell",
            ["/bin/sh"],
            cwd=workdir,
            timeout_ms=60000,
            stdin=True,
            pty=True,
        ),
        "PTY shell job",
        session=session,
    )

    state_command = (
        f"cd {shlex.quote(str(nested))}; "
        "export E2E_PTY_ENV=present; "
        f"printf '{state_marker}|cwd=%s|env=%s\\n' \"$PWD\" \"$E2E_PTY_ENV\""
    )
    session.write(job["job_id"], "pty-state", state_command + "\n")
    _, state_events, cursor = read_until_text(suite, session, job["job_id"], state_marker, timeout=15.0)
    state_output = "".join(event["text"] for event in state_events)
    require(
        f"{state_marker}|cwd={nested}|env=present" in state_output,
        f"PTY did not preserve cd/export state: {state_output!r}",
    )

    resized = session.resize(job["job_id"], rows=40, cols=100)
    require(resized.get("error") is None, f"PTY resize failed: {resized}")
    session.write(
        job["job_id"],
        "pty-size",
        f"printf '{resize_marker}|'; stty size\n",
    )
    _, resize_events, cursor = read_until_text(
        suite,
        session,
        job["job_id"],
        resize_marker + "|40 100",
        cursor=cursor,
        timeout=15.0,
    )
    resize_output = "".join(event["text"] for event in resize_events)
    require(resize_marker + "|40 100" in resize_output, f"PTY size output did not reflect resize: {resize_output!r}")

    session.write(job["job_id"], "pty-cancel-command", "sleep 30\n")
    cancel = session.cancel(job["job_id"])
    require(cancel.get("error") is None, f"PTY cancel failed: {cancel}")
    terminal, _ = suite.read_to_terminal(session, job["job_id"], timeout=15.0)
    require(terminal["state"] == "cancelled", f"PTY shell state is {terminal['state']!r}")
    require(terminal["termination_reason"] == "cancelled", f"PTY shell reason is {terminal['termination_reason']!r}")


def test_wire_confidentiality(
    suite: Suite,
    session: Session,
    proxy: FaultProxy,
    workdir: pathlib.Path,
) -> None:
    marker = "E2EE_OUTER_WIRE_UNIQUE_OUTPUT_MARKER"
    proxy.clear_captured_frames()
    # Capture a NEW handshake, not just already-established encrypted data.
    proxy.drop_and_block()
    wait_until(lambda: proxy.active_count() == 0, 5.0, "confidentiality probe disconnect")
    proxy.release()
    wait_until(lambda: session.try_info()[0] is not None, 20.0, "confidentiality probe re-handshake")
    job = suite.register_reply(
        session.exec(
            "e2ee-wire-marker",
            [sys.executable, "-c", f"print({marker!r}, flush=True)"],
            cwd=workdir,
            timeout_ms=10000,
        ),
        "E2EE wire marker job",
        session=session,
    )
    view, events = suite.read_to_terminal(session, job["job_id"], timeout=15.0)
    output = "".join(event["text"] for event in events)
    require(marker in output, f"E2EE marker did not reach the controller: {output!r}")
    require(view["state"] == "exited", f"E2EE wire marker state is {view['state']!r}")

    wait_until(
        lambda: len(proxy.captured_application_frames()) >= 4,
        5.0,
        "captured Noise/WebSocket application frames",
    )
    frames = proxy.captured_application_frames()
    frame_types: set[str] = set()
    for direction, payload in frames:
        try:
            frame = json.loads(payload)
        except json.JSONDecodeError as exc:
            raise E2EFailure(f"captured {direction} WebSocket payload is not JSON: {payload!r}") from exc
        require(isinstance(frame, dict), f"captured {direction} frame is not an object: {frame!r}")
        frame_type = frame.get("type")
        require(isinstance(frame_type, str), f"captured frame has no type: {frame!r}")
        require(frame_type in {"init", "response", "data", "relay_event"}, f"unexpected outer frame type: {frame!r}")
        frame_types.add(frame_type)
        require(marker.encode() not in payload, f"outer frame exposed the output marker: {direction} {payload!r}")
        if frame_type == "data":
            encoded = frame.get("data")
            require(isinstance(encoded, str), f"captured data frame has invalid ciphertext: {frame!r}")
            try:
                ciphertext = base64.b64decode(encoded, validate=True)
            except (ValueError, base64.binascii.Error) as exc:
                raise E2EFailure(f"captured data frame is not base64: {frame!r}") from exc
            require(marker.encode() not in ciphertext, "captured Noise data decoded to the plaintext output marker")
    require({"init", "response", "data"}.issubset(frame_types), f"Noise frame types not observed: {frame_types!r}")
    suite.notes.append("relay-side WebSocket capture observed only Noise outer frames; command output marker stayed confidential")


def test_bad_channel_key(
    suite: Suite,
    session: Session,
    proxy: FaultProxy,
    workdir: pathlib.Path,
) -> None:
    replace_channel_key(session.controller_file)
    session.start_connector()
    session.start_controller(wait_for_connection=False)
    marker = workdir / "bad-channel-key-marker"
    proxy.clear_captured_frames()

    def channel_error() -> Optional[dict[str, Any]]:
        _, payload = session.try_info()
        if payload and isinstance(payload.get("error"), dict):
            return payload
        return None

    info_error = wait_until(channel_error, 15.0, "bad channel key to reject info")
    suite.expect_error(info_error, "TARGET_OFFLINE", "bad channel key info")
    wait_until(
        # NNpsk0 authenticates the initiating message, so a wrong PSK may
        # be rejected by the responder before any response reaches Controller.
        lambda: session.connector is not None and "end-to-end authentication failed" in session.connector.logs(),
        8.0,
        "bad channel key rejected during authenticated handshake",
    )

    attempted = session.exec(
        "bad-channel-key-exec",
        [sys.executable, "-c", f"open({str(marker)!r}, 'w').write('must-not-run')"],
        cwd=workdir,
        timeout_ms=10000,
    )
    suite.expect_error(attempted, "TARGET_OFFLINE", "bad channel key exec")
    require(not marker.exists(), "bad channel key unexpectedly fell back to plaintext execution")
    for _, payload in proxy.captured_application_frames():
        require(marker.name.encode() not in payload, "bad-key outer frame exposed the command marker")
        try:
            frame = json.loads(payload)
        except json.JSONDecodeError:
            continue
        if isinstance(frame, dict) and frame.get("type") == "data" and isinstance(frame.get("data"), str):
            try:
                decoded = base64.b64decode(frame["data"], validate=True)
            except (ValueError, base64.binascii.Error):
                continue
            require(str(marker).encode() not in decoded, "bad-key frame contained an unencrypted command")

    session.stop()


def test_session_job_quota(
    suite: Suite,
    quota: Session,
    peer: Session,
    workdir: pathlib.Path,
) -> None:
    for index in range(MAX_JOBS_PER_SESSION):
        job = suite.register_reply(
            quota.exec(
                f"quota-{index:02d}",
                ["/bin/true"],
                cwd=workdir,
                timeout_ms=10000,
            ),
            f"quota job {index}",
            session=quota,
        )
        view, _ = suite.read_to_terminal(quota, job["job_id"], timeout=10.0)
        require(view["state"] == "exited", f"quota job {index} state is {view['state']!r}")

    over_limit = quota.exec(
        "quota-over-limit",
        ["/bin/echo", "must-not-run"],
        cwd=workdir,
        timeout_ms=10000,
    )
    suite.expect_error(over_limit, "RESOURCE_LIMIT", "seventeenth-session-job quota")

    # Filling one Connector's durable request table must not consume another
    # session's independent 16-job budget.
    peer_job = suite.register_reply(
        peer.exec("other-session-after-quota", ["/bin/true"], cwd=workdir, timeout_ms=10000),
        "other session after quota",
        session=peer,
    )
    peer_view, _ = suite.read_to_terminal(peer, peer_job["job_id"], timeout=10.0)
    require(peer_view["state"] == "exited", f"other session was affected by quota session: {peer_view}")


def test_role_token_theft(
    suite: Suite,
    session: Session,
    proxy_port: int,
    workdir: pathlib.Path,
) -> None:
    del suite  # This test must not obtain an authenticated application reply.
    session.start_connector()
    require(session.connector is not None, f"{session.name}: connector process missing")
    session.connector.wait_for_log("connector transport connected", 10.0)
    forged_marker = workdir / "role-token-forged-marker"
    raw: Optional[RawWebSocket] = None
    try:
        raw = RawWebSocket.connect(
            "127.0.0.1",
            proxy_port,
            f"/v1/control/{session.session_id}",
            session.controller_token,
            "stolen-controller-token-instance",
        )
        forged_request = {
            "type": "request",
            "version": 1,
            "request": {
                "id": "forged-without-psk",
                "op": "exec",
                "expected_incarnation": "forged-incarnation",
                "argv": [
                    "/bin/sh",
                    "-c",
                    f"printf {shlex.quote('ROLE_TOKEN_FORGED')} > {shlex.quote(str(forged_marker))}",
                ],
                "cwd": str(workdir),
                "env": {},
                "timeout_ms": 10000,
                "stdin": False,
                "pty": False,
            },
        }
        # The outer frame is syntactically a Noise data frame, but its payload
        # is deliberately not a Noise ciphertext.  Do not send a plaintext
        # Request as if the relay routed it directly.
        fake_ciphertext = base64.b64encode(json.dumps(forged_request, separators=(",", ":")).encode()).decode()
        raw.send_text(json.dumps({"type": "data", "data": fake_ciphertext}, separators=(",", ":")))
        time.sleep(0.5)
        require(not forged_marker.exists(), "a stolen role token forged a command without the channel PSK")

        session.start_controller(wait_for_connection=False)

        def legitimate_controller_blocked() -> Optional[dict[str, Any]]:
            _, payload = session.try_info()
            if payload and isinstance(payload.get("error"), dict):
                return payload
            return None

        blocked = wait_until(legitimate_controller_blocked, 15.0, "role-token theft DoS")
        require(blocked["error"]["code"] == "TARGET_OFFLINE", f"stolen token did not only cause DoS: {blocked}")
    finally:
        if raw is not None:
            raw.close()
        session.stop()


def test_transport_reconnect(suite: Suite, session: Session, proxy: FaultProxy, workdir: pathlib.Path) -> None:
    marker = workdir / "reconnect-count.txt"
    proxy.clear_captured_frames()
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
    reconnect_records = [
        entry.get("record")
        for entry in inspect_state(suite, session)
        if isinstance(entry, dict)
        and isinstance(entry.get("record"), dict)
        and entry["record"].get("request_id") == "reconnect-idempotent"
    ]
    require(len(reconnect_records) == 1, f"reconnect created an implicit second request/job: {reconnect_records!r}")

    wait_until(
        lambda: len(proxy.captured_application_frames()) >= 8,
        5.0,
        "Noise re-handshake frames after transport reconnect",
    )
    reconnect_types = {
        json.loads(payload).get("type")
        for _, payload in proxy.captured_application_frames()
        if payload.startswith(b"{")
    }
    require({"init", "response", "data"}.issubset(reconnect_types), f"reconnect did not carry Noise frames: {reconnect_types!r}")

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
    *,
    state: Optional[str] = None,
) -> dict[str, Any]:
    def find() -> Optional[dict[str, Any]]:
        for entry in inspect_state(suite, session):
            if (
                isinstance(entry, dict)
                and isinstance(entry.get("record"), dict)
                and entry["record"].get("job_id") == job_id
                and (state is None or entry["record"].get("state") == state)
            ):
                return entry
        return None

    return wait_until(find, timeout, f"state record {job_id} state={state or 'any'}")


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
    entry = wait_for_record(suite, session, job["job_id"], suite.lease_secs + 8.0, state="cancelled")
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
    entry = wait_for_record(suite, session, job["job_id"], 8.0, state="cancelled")
    record = entry["record"]
    require(record["state"] == "cancelled", f"connector exit did not cancel job: {record}")
    require(record["termination_reason"] == "cancelled", f"connector exit reason: {record}")
    require(record["request_id"] == "connector-exit-cleanup", "connector exit record request mismatch")
    require(not process_exists(pid), f"connector exit left job PID {pid} running")
    require(session.controller is not None and session.controller.poll() is None, "controller died while testing connector exit")


def test_revocation_and_restart_marker(
    suite: Suite,
    session: Session,
    proxy: FaultProxy,
    proxy_port: int,
    admin_socket: pathlib.Path,
    relay: ManagedProcess,
    relay_args: list[str],
    workdir: pathlib.Path,
) -> None:
    status = relay_admin(suite, admin_socket)
    sessions = status.get("sessions")
    require(isinstance(sessions, list), f"relay sessions result is not a list: {status}")
    for item in sessions:
        require(isinstance(item, dict), f"relay status contains malformed session metadata: {item!r}")
        require("token" not in item and "channel_key" not in item, f"relay status disclosed credential material: {item!r}")
    metadata = next((item for item in sessions if isinstance(item, dict) and item.get("session_id") == session.session_id), None)
    require(isinstance(metadata, dict), f"relay status omitted revoke test session: {status}")
    require(metadata.get("revoked") is False, f"fresh session unexpectedly revoked: {metadata}")
    require(metadata.get("connector_connected") is True, f"revoke test connector is not connected: {metadata}")
    require(metadata.get("controller_connected") is True, f"revoke test controller is not connected: {metadata}")

    job = suite.register_reply(
        session.exec(
            "revocation-running-job",
            [sys.executable, "-c", "import time; print('revoke-running', flush=True); time.sleep(60)"],
            cwd=workdir,
            timeout_ms=60000,
        ),
        "revocation running job",
        session=session,
    )
    pid = int(job["pid"])
    wait_until(lambda: process_exists(pid), 3.0, "revocation test child process")

    revoked = relay_admin(suite, admin_socket, session_id=session.session_id)
    require(revoked.get("session_id") == session.session_id, f"revoke session mismatch: {revoked}")
    require(revoked.get("revoked") is True, f"revoke did not return revoked=true: {revoked}")
    require(revoked.get("persisted") is True, f"revoke did not persist the marker: {revoked}")

    marker = pathlib.Path(str(session.relay_file) + ".revoked")
    require(marker.is_file(), f"revocation marker was not created: {marker}")
    require(mode(marker) == 0o600, f"revocation marker mode is {oct(mode(marker))}, expected 0600")
    require(marker.read_text() == session.session_id, f"revocation marker content mismatch: {marker.read_text()!r}")

    status_after = relay_admin(suite, admin_socket)
    sessions_after = status_after.get("sessions")
    require(isinstance(sessions_after, list), f"relay sessions after revoke is not a list: {status_after}")
    metadata_after = next((item for item in sessions_after if isinstance(item, dict) and item.get("session_id") == session.session_id), None)
    require(isinstance(metadata_after, dict) and metadata_after.get("revoked") is True, f"relay status did not retain revoked=true: {status_after}")

    for route, token, instance in (
        (f"/v1/connect/{session.session_id}", session.connector_token, "revoked-connector-retry"),
        (f"/v1/control/{session.session_id}", session.controller_token, "revoked-controller-retry"),
    ):
        http_status, _ = websocket_handshake("127.0.0.1", proxy_port, route, token, instance)
        require(http_status == 401, f"revoked session accepted {route} retry with HTTP {http_status}")

    # Revoke closes the transports, but the Connector owns the child until its
    # remaining authenticated controller lease expires.  Verify cleanup within
    # that bounded window rather than assuming an instantaneous kill.
    entry = wait_for_record(suite, session, job["job_id"], suite.lease_secs + 8.0, state="cancelled")
    record = entry["record"]
    require(record["state"] == "cancelled", f"revoked running job state is {record}")
    require(record["termination_reason"] == "cancelled", f"revoked running job reason is {record}")
    require(not process_exists(pid), f"revocation left child PID {pid} running")
    session.stop()

    # A restarted relay must reject every old session-file set containing the
    # durable marker, rather than silently forgetting the revocation.
    relay.terminate()
    restarted = ManagedProcess(relay_args, "relay-restart-revoked")
    try:
        wait_until(
            lambda: restarted.poll() is not None,
            5.0,
            "relay restart to reject revoked session file",
        )
        require(restarted.poll() != 0, "relay restarted successfully with a revoked session file")
        wait_until(
            lambda: "durable revocation marker" in restarted.logs(),
            2.0,
            "relay restart durable-marker diagnostic",
        )
        require(
            "durable revocation marker" in restarted.logs(),
            f"relay restart did not report the durable marker refusal: {restarted.logs()}",
        )
    finally:
        restarted.terminate()


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
            require(
                {"remote_info", "remote_exec", "remote_read", "remote_cancel", "remote_write", "remote_resize"}.issubset(names),
                f"MCP tools missing: {names!r}",
            )
            remote_exec = next(item for item in tool_items if item.get("name") == "remote_exec")
            exec_schema = remote_exec.get("inputSchema", {})
            exec_properties = exec_schema.get("properties", {})
            require(exec_properties.get("stdin", {}).get("default") is False, f"MCP remote_exec stdin default changed: {exec_schema!r}")
            require(exec_properties.get("pty", {}).get("default") is False, f"MCP remote_exec pty default changed: {exec_schema!r}")
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
        bad_key = Session(binary, root / "bad-key", "e2e-bad-key", relay_url, args.lease_secs)
        quota = Session(binary, root / "quota", "e2e-quota", relay_url, args.lease_secs)
        token_theft = Session(binary, root / "token-theft", "e2e-token-theft", relay_url, args.lease_secs)
        lease = Session(binary, root / "lease", "e2e-lease", relay_url, args.lease_secs)
        connector_exit = Session(binary, root / "connector-exit", "e2e-connector-exit", relay_url, args.lease_secs)
        revoke = Session(binary, root / "revoke", "e2e-revoke", relay_url, args.lease_secs)
        managed_sessions.extend([primary, bad_key, quota, token_theft, lease, connector_exit, revoke])
        for session in managed_sessions:
            session.initialize()

        proxy = FaultProxy(proxy_port, relay_port)
        admin_socket = root / "relay-admin.sock"
        relay_args = [str(binary), "relay", "--listen", f"127.0.0.1:{relay_port}"]
        for session in managed_sessions:
            relay_args.extend(["--session-file", str(session.relay_file)])
        relay_args.extend(["--admin-socket", str(admin_socket)])
        relay = ManagedProcess(relay_args, "relay")
        try:
            relay.wait_for_log("relay listening on", 10.0)
            wait_until(lambda: admin_socket.is_socket(), 5.0, "relay admin socket")
            require(mode(admin_socket) == 0o600, f"relay admin socket mode is {oct(mode(admin_socket))}, expected 0600")
            primary.start()
            print("RUN: test_init_and_role_isolation", flush=True)
            test_init_and_role_isolation(suite, primary, proxy_port)

            workdir = root / "work"
            workdir.mkdir(mode=0o700)
            print("RUN: test_primary_operations", flush=True)
            test_primary_operations(suite, primary, workdir)
            print("RUN: test_pipe_stdin", flush=True)
            test_pipe_stdin(suite, primary, workdir)
            print("RUN: test_pty_shell", flush=True)
            test_pty_shell(suite, primary, workdir)
            print("RUN: test_wire_confidentiality", flush=True)
            test_wire_confidentiality(suite, primary, proxy, workdir)
            print("RUN: test_transport_reconnect", flush=True)
            test_transport_reconnect(suite, primary, proxy, workdir)
            print("RUN: test_mcp", flush=True)
            test_mcp(suite, primary)
            print("RUN: test_bad_channel_key", flush=True)
            test_bad_channel_key(suite, bad_key, proxy, workdir)

            quota.start()
            print("RUN: test_session_job_quota", flush=True)
            test_session_job_quota(suite, quota, primary, workdir)

            print("RUN: test_role_token_theft", flush=True)
            test_role_token_theft(suite, token_theft, proxy_port, workdir)

            lease.start()
            print("RUN: test_controller_death_lease", flush=True)
            test_controller_death_lease(suite, lease, workdir)

            connector_exit.start()
            print("RUN: test_connector_exit_cleanup", flush=True)
            test_connector_exit_cleanup(suite, connector_exit, workdir)

            revoke.start()
            print("RUN: test_revocation_and_restart_marker", flush=True)
            test_revocation_and_restart_marker(
                suite,
                revoke,
                proxy,
                proxy_port,
                admin_socket,
                relay,
                relay_args,
                workdir,
            )

            print(
                "PASS: standard-library end-to-end suite; "
                f"started_jobs={suite.jobs_started}/{MAX_TOTAL_JOBS}; "
                f"per_session_quota={MAX_JOBS_PER_SESSION}"
            )
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
