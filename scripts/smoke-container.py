#!/usr/bin/env python3
"""Offline rootless container smoke: static scratch Relay with host-side clients.

Uses one newly built image, an isolated session/port, an explicitly writable
private admin mount and read-only role config. Never contacts a public server.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tests'))
from e2e import Session, Suite, pick_port, require, wait_until


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--binary', required=True)
    p.add_argument('--image', default='localhost/agent-tunnel-relay:smoke')
    args = p.parse_args()
    binary = Path(args.binary).resolve()
    require(binary.is_file(), f'binary unavailable: {binary}')
    with tempfile.TemporaryDirectory(prefix='agent-tunnel-container-') as dirname:
        root = Path(dirname)
        admin = root / 'admin'
        admin.mkdir(mode=0o700)
        port = pick_port()
        name = f'agent-tunnel-smoke-{os.getpid()}'
        session = Session(binary, root / 'session', 'scratch-container', f'ws://127.0.0.1:{port}/', 10)
        session.initialize()
        socket_file = admin / 'admin.sock'
        cid_file = root / 'container.cid'
        command = ['podman', 'run', '--rm', '--cidfile', str(cid_file), '--name', name, '--network', 'host',
                   '--userns', 'keep-id', '--user', f'{os.getuid()}:{os.getgid()}',
                   '--mount', f'type=bind,src={session.root},dst=/run/session,ro=true',
                   '--mount', f'type=bind,src={admin},dst=/run/admin',
                   args.image, 'relay', '--listen', f'127.0.0.1:{port}',
                   '--session-file', '/run/session/relay.json', '--admin-socket', '/run/admin/admin.sock']
        container = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     start_new_session=True)
        try:
            def ready():
                if container.poll() is not None:
                    out, err = container.communicate(timeout=1)
                    raise RuntimeError(f'container failed (exit {container.returncode}): {err.decode(errors="replace")[-2000:]}')
                try:
                    with urlopen(f'http://127.0.0.1:{port}/healthz', timeout=.3) as r:
                        return r.status == 200 and r.read() == b'ok\n'
                except (OSError, TimeoutError):
                    return False
            wait_until(ready, 12, 'scratch Relay HTTP healthz')
            require(socket_file.is_socket(), 'private mounted Relay admin socket unavailable')
            probe = subprocess.run([str(binary), 'sessions', '--admin-socket', str(socket_file)],
                                   text=True, capture_output=True, timeout=5)
            require(probe.returncode == 0, f'admin query failed: {probe.stderr[-600:]}')
            result = json.loads(probe.stdout)
            require(any(s['session_id'] == session.session_id for s in result['sessions']), 'Relay did not load mounted role file')
            session.start()
            suite = Suite(binary, 10)
            marker = root / 'container-result'
            code = f'from pathlib import Path; Path({str(marker)!r}).write_text("container-relay-ok")'
            job = suite.register_reply(session.exec('container-smoke', [sys.executable, '-c', code], cwd=root,
                                                   timeout_ms=5000), 'container relay job')
            view, _ = suite.read_to_terminal(session, job['job_id'], timeout=10)
            require(view['exit_code'] == 0 and marker.read_text() == 'container-relay-ok',
                    'container relay failed to route Noise exec/read')
            print('PASS: scratch Relay rootless, loopback-only, read-only config, private writable admin, Noise exec/read')
        finally:
            session.stop()
            if cid_file.is_file():
                cid = cid_file.read_text().strip()
                if cid and all(ch in '0123456789abcdef' for ch in cid):
                    subprocess.run(['podman', 'stop', '--time', '4', cid], stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, timeout=8, check=False)
            try: container.wait(timeout=8)
            except subprocess.TimeoutExpired:
                container.terminate()
                try: container.wait(timeout=4)
                except subprocess.TimeoutExpired: container.kill(); container.wait(timeout=4)

if __name__ == '__main__': main()
