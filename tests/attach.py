#!/usr/bin/env python3
"""Human-only PTY ownership regression (standard library, isolated loopback)."""
from __future__ import annotations

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import socket
import subprocess
import sys
import tempfile
import termios
import time
import uuid

from e2e import E2EFailure, ManagedProcess, Session, pick_port, require, run_cli, wait_until


def ipc(session: Session, op: str, **args: object) -> dict:
    req_id = str(args.pop("id", uuid.uuid4().hex))
    request = {"id": req_id, "op": op, **args}
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(15)
        connection.connect(str(session.socket_path))
        connection.sendall(json.dumps(request, separators=(",", ":")).encode() + b"\n")
        buf = bytearray()
        while b"\n" not in buf and len(buf) < 256 * 1024:
            chunk = connection.recv(4096)
            require(chunk, f"IPC closed without reply for {op}")
            buf.extend(chunk)
    reply = json.loads(buf.split(b"\n", 1)[0])
    require(reply["id"] == req_id, f"mismatched IPC reply for {op}")
    return reply


def error(reply: dict, code: str) -> None:
    actual = reply.get("error") or {}
    require(actual.get("code") == code, f"expected {code}, got {actual.get('code')}")


def result(reply: dict) -> dict:
    require(reply.get("error") is None, f"unexpected {reply.get('error')}")
    value = reply.get("result")
    require(isinstance(value, dict), "missing JSON object result")
    return value


def takeover(session: Session, job: str) -> tuple[str, int]:
    r = result(ipc(session, "takeover", job_id=job, expected_incarnation=session.incarnation))
    token = r.get("owner_token")
    cursor = r.get("next_cursor")
    require(isinstance(token, str) and len(token) == 64 and isinstance(cursor, int),
            "no valid private takeover token/cursor")
    return token, cursor


def wait_clean_read(session: Session, job: str) -> dict:
    def read() -> dict | None:
        reply = session.read(job, 0)
        if (reply.get("error") or {}).get("code") == "HANDOFF_DRAINING":
            return None
        return result(reply)
    return wait_until(read, 5, "handoff quiet window then agent read")


def wait_lines(path: Path, minimum: int) -> list[str] | None:
    if not path.exists():
        return None
    lines = path.read_text().splitlines()
    return lines if len(lines) >= minimum else None


def operator_cli(binary: Path, session: Session, job: str) -> tuple[subprocess.Popen[bytes], int, int, list]:
    master, slave = pty.openpty()
    observer = os.dup(slave)
    saved = termios.tcgetattr(observer)
    try:
        proc = subprocess.Popen(
            [str(binary), "attach", "--socket", str(session.socket_path),
             "--job", job, "--incarnation", session.incarnation],
            stdin=slave, stdout=slave, stderr=slave,
            start_new_session=True,
            preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
            cwd=str(session.root), shell=False, close_fds=True,
        )
    except BaseException:
        os.close(master)
        os.close(observer)
        raise
    finally:
        os.close(slave)
    return proc, master, observer, saved


def wait_header(proc: subprocess.Popen[bytes], master: int, label: str) -> bytes:
    output = bytearray()
    def ready() -> bool:
        output.extend(collect_tty(master, 0.1))
        return b"operator attach" in output or proc.poll() is not None
    wait_until(ready, 5, label)
    require(proc.poll() is None, f"{label}: attach exited before operator takeover")
    return bytes(output)


def stop_operator(proc: subprocess.Popen[bytes], master: int, observer: int) -> None:
    try:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGTERM)
            try:
                proc.wait(timeout=4)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait(timeout=4)
    finally:
        os.close(observer)
        os.close(master)


def collect_tty(master: int, timeout: float = 0.1) -> bytes:
    if not select.select([master], [], [], timeout)[0]:
        return b""
    try:
        return os.read(master, 64 * 1024)
    except OSError:  # EIO when the child closes its controlling TTY
        return b""


def scenario(binary: Path, root: Path) -> None:
    session = Session(binary, root / "session", "human-attach", f"ws://127.0.0.1:{pick_port()}/", 60)
    session.initialize()  # Fresh, private 300s config; no existing environment.
    relay = ManagedProcess([
        str(binary), "relay", "--listen", session.relay_url.removeprefix("ws://").rstrip("/"),
        "--session-file", str(session.relay_file),
    ], "attach-relay")
    try:
        relay.wait_for_log("relay listening on", 5)
        session.start()
        work = root / "work"
        work.mkdir(mode=0o700)
        record = work / "human-input"
        code = (
            "import sys; from pathlib import Path; p=Path(sys.argv[1]); "
            "print('ready',flush=True);\n"
            "for line in sys.stdin:\n"
            " with p.open('a') as f: f.write(line)\n"
            " print('seen:'+line+'\\x1b[31m',flush=True)\n"
        )
        job = result(session.exec("attach-pty", [sys.executable, "-u", "-c", code, str(record)],
                                  cwd=work, timeout_ms=120000, pty=True))["job_id"]
        wait_until(lambda: "ready" in "".join(e["text"] for e in result(session.read(job))["events"]),
                   5, "PTY preflight output")
        no_tty = run_cli(binary, ["attach", "--socket", str(session.socket_path),
                                  "--job", job, "--incarnation", session.incarnation], timeout=3)
        require(no_tty.returncode != 0 and b"TTY" in no_tty.stderr, "non-TTY attach was accepted")
        error(ipc(session, "takeover", job_id=job, expected_incarnation="wrong-incarnation"), "TARGET_MISMATCH")
        token, cursor = takeover(session, job)
        require(token not in json.dumps(session.info()), "operator token leaked into Info")
        error(session.read(job), "TAKEOVER_ACTIVE")
        error(session.cancel(job), "TAKEOVER_ACTIVE")
        error(session.resize(job, 30, 100), "TAKEOVER_ACTIVE")
        error(session.write(job, "model-write", "agent-injection\n"), "TAKEOVER_ACTIVE")
        require(not record.exists(), "agent wrote while operator owned PTY")
        error(ipc(session, "read", job_id=job, cursor=cursor, owner_token="0" * 64), "TAKEOVER_ACTIVE")
        renewed = result(ipc(session, "takeover", job_id=job,
                             expected_incarnation=session.incarnation, owner_token=token))
        require(renewed.get("renewed") is True and "owner_token" not in renewed,
                "token renewal failed or returned secret")
        request_id = uuid.uuid4().hex
        write = {"id": request_id, "job_id": job, "data": "manual-secret\n", "eof": False,
                 "owner_token": token}
        result(ipc(session, "write", **write))
        result(ipc(session, "write", **write))  # Same ID/args must NOT replay stdin.
        require(wait_until(lambda: wait_lines(record, 1), 5, "operator stdin side effect") == ["manual-secret"],
                "input replayed or changed")
        time.sleep(0.2)
        require(record.read_text() == "manual-secret\n", "same request ID replayed remote stdin")
        owner_view = result(ipc(session, "read", job_id=job, cursor=cursor, owner_token=token))
        require(any("manual-secret" in e["text"] for e in owner_view["events"]),
                "operator cannot read owned PTY echo")
        error(ipc(session, "release", job_id=job, expected_incarnation="wrong", owner_token=token),
              "TARGET_MISMATCH")
        result(ipc(session, "release", job_id=job, expected_incarnation=session.incarnation, owner_token=token))
        clean = wait_clean_read(session, job)
        require(clean["dropped_before_cursor"] is not None and not any(
            "manual-secret" in e["text"] for e in clean["events"]), "secret ring not scrubbed / cursor gap missing")
        result(session.write(job, "agent-after-release", "public\n"))
        require(wait_until(lambda: wait_lines(record, 2), 5, "agent input after release") ==
                ["manual-secret", "public"], "agent input not restored after release")
        # A real controlling TTY can attach without any token in argv/stdout.
        proc, master, observer, saved = operator_cli(binary, session, job)
        try:
            wait_header(proc, master, "attach UI")
            error(session.read(job), "TAKEOVER_ACTIVE")
            os.write(master, b"typed-by-human\n")
            require(wait_until(lambda: wait_lines(record, 3), 5, "TTY input") ==
                    ["manual-secret", "public", "typed-by-human"], "real operator input lost")
            displayed = bytearray()
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline and b"\\u{1b}" not in displayed:
                displayed.extend(collect_tty(master, 0.1))
            require(b"\x1b" not in displayed and b"\\u{1b}" in displayed,
                    "operator output was not inertly escaped")
            require(token.encode() not in displayed, "operator token leaked to UI")
            os.write(master, b"\x1d")  # Ctrl+] detaches.
            require(proc.wait(timeout=4) == 0, "Ctrl+] did not detach cleanly")
            require(termios.tcgetattr(observer) == saved, "Ctrl+] did not restore operator TTY")
        finally:
            stop_operator(proc, master, observer)
        clean = wait_clean_read(session, job)
        require(clean["dropped_before_cursor"] is not None and not any(
            "typed-by-human" in e["text"] for e in clean["events"]), "CLI handoff leaked PTY echo")
        # SIGINT also releases best-effort and restores raw mode via RAII.
        proc, master, observer, saved = operator_cli(binary, session, job)
        try:
            wait_header(proc, master, "SIGINT attach")
            os.kill(proc.pid, signal.SIGINT)
            require(proc.wait(timeout=4) == 0, "SIGINT attach failed to cleanly detach")
            require(termios.tcgetattr(observer) == saved, "SIGINT did not restore operator TTY")
        finally:
            stop_operator(proc, master, observer)
        wait_clean_read(session, job)
        # No renewal for >30s: automatic release must clear the ring, then
        # previously minted token must be unusable (not silently reacquired).
        token, cursor = takeover(session, job)
        result(ipc(session, "write", job_id=job, data="expiry-secret\n", eof=False,
                   owner_token=token))
        require(wait_until(lambda: wait_lines(record, 4), 5, "expiry secret written")[-1] ==
                "expiry-secret", "expiry input failed")
        time.sleep(31.2)
        clean = wait_clean_read(session, job)
        require(clean["dropped_before_cursor"] is not None and not any(
            "expiry-secret" in e["text"] for e in clean["events"]), "expired ownership leaked output")
        error(ipc(session, "takeover", job_id=job, expected_incarnation=session.incarnation,
                  owner_token=token), "OWNER_EXPIRED")
        result(session.cancel(job))
        print("PASS: PTY ownership, scrub/gaps, TTL, real TTY, SIGINT, no replay", flush=True)
    finally:
        try:
            session.stop()
        finally:
            relay.terminate()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.expanduser().resolve()
    require(binary.is_file() and os.access(binary, os.X_OK), f"missing executable: {binary}")
    require(sys.platform.startswith("linux"), "real TTY attach test requires Linux")
    with tempfile.TemporaryDirectory(prefix="agent-tunnel-attach-") as name:
        scenario(binary, Path(name))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except E2EFailure as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        raise SystemExit(1) from exc
