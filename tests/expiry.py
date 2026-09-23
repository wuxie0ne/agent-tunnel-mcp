#!/usr/bin/env python3
"""Isolated, standard-library regression for an expired lease or session TTL.

Usage: python3 tests/expiry.py --binary target/debug/agent-tunnel

Each scenario generates fresh credentials, starts its own loopback relay and
Unix-socket controller, and executes only Python commands writing into its
private temporary directory.  A stopped connector has already authenticated;
controller Lease frames and an Exec request can queue while it cannot read.
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
from typing import Iterator

from e2e import (
    E2EFailure,
    ManagedProcess,
    Session,
    Suite,
    parse_json_line,
    pick_port,
    require,
    run_cli,
    text,
    wait_until,
)


def initialize(session: Session, ttl_secs: int) -> int:
    """Use the real init CLI, rather than Session.initialize's fixed 300s TTL."""
    cp = run_cli(
        session.binary,
        [
            "init", "--dir", str(session.root), "--relay", session.relay_url,
            "--name", session.name, "--ttl-secs", str(ttl_secs),
        ],
        timeout=5,
    )
    require(cp.returncode == 0, f"init failed: {text(cp.stderr)}")
    summary = parse_json_line(cp.stdout, "init")
    relay = json.loads(session.relay_file.read_text())
    connector = json.loads(session.connector_file.read_text())
    controller = json.loads(session.controller_file.read_text())
    expiry = summary.get("expires_at")
    require(isinstance(expiry, int), f"init did not report an expiry: {summary}")
    require(
        expiry == relay["expires_at"] == connector["expires_at"] == controller["expires_at"],
        "role credentials have inconsistent expiries",
    )
    require(relay["session_id"] == connector["session_id"] == controller["session_id"],
            "role credentials have inconsistent session IDs")
    session.session_id = connector["session_id"]
    session.target_id = connector["target_id"]
    return expiry


@contextmanager
def live_session(binary: Path, root: Path, *, ttl_secs: int, lease_secs: int) -> Iterator[tuple[Session, int]]:
    session = Session(binary, root / "session", f"expiry-{ttl_secs}-{lease_secs}",
                      f"ws://127.0.0.1:{pick_port()}/", lease_secs)
    expires_at = initialize(session, ttl_secs)
    relay: ManagedProcess | None = None
    try:
        relay = ManagedProcess(
            [str(binary), "relay", "--listen", session.relay_url.removeprefix("ws://").rstrip("/"),
             "--session-file", str(session.relay_file)],
            "expiry-relay",
        )
        relay.wait_for_log("relay listening on", 5)
        session.start()  # Includes authenticated Info over the Unix socket.
        require(session.controller is not None, "controller was not started")
        session.controller.wait_for_log("end-to-end channel authenticated", 3)
        yield session, expires_at
    finally:
        # Always release a stopped connector before ManagedProcess.terminate;
        # SIGTERM alone cannot be handled while SIGSTOP is in effect.
        if session.connector is not None and session.connector.poll() is None:
            try:
                os.killpg(session.connector.process.pid, signal.SIGCONT)
            except ProcessLookupError:
                pass
        try:
            session.stop()
        finally:
            if relay is not None:
                relay.terminate()


def marker_argv(path: Path) -> list[str]:
    return [sys.executable, "-c",
            "from pathlib import Path; import sys; Path(sys.argv[1]).write_text('ran')",
            str(path)]


def preflight(session: Session, workdir: Path, suite: Suite) -> None:
    """Prevent an offline target or broken exec path from passing a negative test."""
    marker = workdir / "preflight"
    payload = session.exec("expiry-preflight", marker_argv(marker), cwd=workdir, timeout_ms=3000)
    result = suite.register_reply(payload, "preflight exec", session=session)
    view, _ = suite.read_to_terminal(session, result["job_id"], timeout=5)
    require(view["state"] == "exited" and view["exit_code"] == 0,
            f"preflight command failed: {view}")
    require(marker.read_text() == "ran", "preflight command did not create its marker")


def stop_connector(session: Session) -> float:
    connector = session.connector
    require(connector is not None and connector.poll() is None, "connector already exited")
    os.killpg(connector.process.pid, signal.SIGSTOP)

    def stopped() -> bool:
        status = Path(f"/proc/{connector.process.pid}/status").read_text()
        return any(line.startswith("State:\tT") for line in status.splitlines())

    wait_until(stopped, 2, "connector SIGSTOP to take effect")
    return time.monotonic()


@contextmanager
def queued_exec(session: Session, workdir: Path, marker: Path) -> Iterator[subprocess.Popen[bytes]]:
    """Submit while stopped without blocking this test on the controller reply."""
    argv = [
        str(session.binary), "exec", "--socket", str(session.socket_path),
        "--incarnation", session.incarnation, "--cwd", str(workdir),
        "--request-id", f"queued-{session.name}", "--timeout-ms", "3000",
        "--", *marker_argv(marker),
    ]
    proc = subprocess.Popen(argv, cwd=str(workdir), stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            start_new_session=True, shell=False)
    try:
        yield proc
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGTERM)
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait(timeout=2)
        if proc.stdout is not None:
            proc.stdout.close()
        if proc.stderr is not None:
            proc.stderr.close()


def require_pending(proc: subprocess.Popen[bytes]) -> None:
    time.sleep(0.3)  # Not an expiry boundary: confirm this was not a fast offline rejection.
    if proc.poll() is not None:
        out, err = proc.communicate()
        raise E2EFailure(f"stopped connector did not hold the Exec request: "
                         f"exit={proc.returncode}, stdout={text(out)!r}, stderr={text(err)!r}")


def resume_and_check(session: Session, proc: subprocess.Popen[bytes], marker: Path,
                     *, require_json: bool) -> None:
    connector = session.connector
    require(connector is not None and connector.poll() is None,
            "stopped connector disappeared before SIGCONT")
    require(not marker.exists(), "queued command ran while connector was SIGSTOPped")
    os.killpg(connector.process.pid, signal.SIGCONT)
    # Once past the deadline, queued Lease frames cannot renew the old lease.
    wait_until(lambda: connector.poll() is not None, 4, "connector to exit after SIGCONT")
    try:
        out, err = proc.communicate(timeout=15)
    except subprocess.TimeoutExpired as exc:
        raise E2EFailure("queued exec CLI failed to return after connector expiry") from exc
    if out.strip():
        payload = parse_json_line(out, "queued exec")
        require(payload.get("result") is None and isinstance(payload.get("error"), dict),
                f"expired connector accepted queued Exec: {payload}")
        require(payload["error"].get("code") in
                {"LEASE_EXPIRED", "EXECUTION_UNKNOWN", "TARGET_OFFLINE"},
                f"unexpected queued exec reply: {payload}")
    else:
        require(not require_json and proc.returncode != 0,
                f"queued exec returned without a rejection: exit={proc.returncode}, stderr={text(err)!r}")
    time.sleep(0.3)  # Catch late side effects from a spawned but not yet finished child.
    require(not marker.exists(), f"expired connector EXECUTED queued command: {marker}")
    connector.wait_for_log("session TTL or controller lease expired", 2)


def test_lease(binary: Path, root: Path, iteration: int) -> None:
    with live_session(binary, root, ttl_secs=45, lease_secs=5) as (session, expiry):
        workdir = root / "work"
        workdir.mkdir(mode=0o700)
        preflight(session, workdir, Suite(binary, 5))
        require(expiry - time.time() > 15, "lease fixture TTL is too close")
        stopped_at = stop_connector(session)
        # Allow at least two controller Lease ticks to queue ahead of Exec.
        time.sleep(2.2)
        marker = workdir / "must-not-run"
        with queued_exec(session, workdir, marker) as pending:
            require_pending(pending)
            time.sleep(max(0, stopped_at + 7.2 - time.monotonic()))
            require(session.controller is not None and session.controller.poll() is None,
                    "controller exited before the lease test could resume")
            require(pending.poll() is None, "controller returned before the queued request was tested")
            resume_and_check(session, pending, marker, require_json=True)
    print(f"PASS: expired 5s lease rejects queued Lease/Exec (trial {iteration})", flush=True)


def test_ttl(binary: Path, root: Path) -> None:
    with live_session(binary, root, ttl_secs=10, lease_secs=60) as (session, expiry):
        workdir = root / "work"
        workdir.mkdir(mode=0o700)
        preflight(session, workdir, Suite(binary, 60))
        require(expiry - time.time() > 5, "TTL fixture too slow: cannot queue before expiry")
        stop_connector(session)
        marker = workdir / "must-not-run"
        with queued_exec(session, workdir, marker) as pending:
            require_pending(pending)
            # Wall-time expiry comes from init credentials. Add generous slack:
            # no assertion depends on a subsecond SIGCONT/timer scheduling race.
            time.sleep(max(0, expiry - time.time() + 1.5))
            resume_and_check(session, pending, marker, require_json=False)
        # A fresh request after SIGCONT must also fail, whether the local
        # controller has already closed its Unix socket or is still exiting.
        late_marker = workdir / "must-not-run-after-ttl"
        late = run_cli(
            binary,
            ["exec", "--socket", str(session.socket_path), "--incarnation", session.incarnation,
             "--cwd", str(workdir), "--request-id", "after-ttl", "--timeout-ms", "3000",
             "--", *marker_argv(late_marker)],
            timeout=8,
            cwd=workdir,
        )
        require(late.returncode != 0, f"post-TTL command succeeded: {text(late.stdout)!r}")
        if late.stdout.strip():
            response = parse_json_line(late.stdout, "post-TTL exec")
            require(response.get("result") is None and isinstance(response.get("error"), dict),
                    f"post-TTL command was accepted: {response}")
        require(not late_marker.exists(), "post-TTL command executed")
    print("PASS: expired 10s session TTL rejects queued and new Exec", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.expanduser().resolve()
    require(sys.platform.startswith("linux"), "SIGSTOP regression requires Linux /proc")
    require(binary.is_file() and os.access(binary, os.X_OK), f"binary is not executable: {binary}")
    with tempfile.TemporaryDirectory(prefix="agent-tunnel-expiry-") as name:
        root = Path(name)
        for i in range(2):
            trial = root / f"lease-{i}"
            trial.mkdir(mode=0o700)
            test_lease(binary, trial, i + 1)
        ttl = root / "ttl"
        ttl.mkdir(mode=0o700)
        test_ttl(binary, ttl)
    print("PASS: expiration regressions (positive preflight and negative queued commands)", flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except E2EFailure as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        raise SystemExit(1) from exc
