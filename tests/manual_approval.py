#!/usr/bin/env python3
"""Exercise a real independent controlling TTY, without model calls or production targets."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import sys
import tempfile
import time
import termios

from e2e import Session, Suite, ManagedProcess, pick_port, require, wait_until


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True)
    args = parser.parse_args()
    binary = Path(args.binary).resolve()
    with tempfile.TemporaryDirectory(prefix='agent-tunnel-approval-') as tmp:
        root = Path(tmp); port = pick_port()
        session = Session(binary, root/'session', 'manual-gate', f'ws://127.0.0.1:{port}', 60)
        session.initialize(); suite = Suite(binary, 60)
        relay = ManagedProcess([str(binary), 'relay', '--listen', f'127.0.0.1:{port}', '--session-file', str(session.relay_file)], 'manual-relay')
        operator = None; master = None
        try:
            relay.wait_for_log('relay listening on', 10)
            command = [str(binary), 'local', '--config', str(session.controller_file), '--socket', str(session.socket_path)]
            denied = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True, timeout=5)
            require(denied.returncode != 0 and b'/dev/tty' in denied.stderr, 'non-TTY manual controller did not fail closed')
            require(not session.socket_path.exists(), 'non-TTY controller left a live IPC socket')
            session.start_connector()
            master, slave = pty.openpty()
            operator = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
                                        preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0), close_fds=True)
            os.close(slave)
            wait_until(lambda: session.socket_path.exists(), 5, 'manual controller socket')
            wait_until(lambda: session.try_info()[0] is not None, 20, 'manual controller authenticated connection')
            session.incarnation = session.info()['incarnation']
            terminal_buffer = bytearray()

            def prompt(request_id):
                deadline = time.monotonic()+8
                while time.monotonic() < deadline:
                    while b'\n' in terminal_buffer:
                        line, _, remaining = terminal_buffer.partition(b'\n'); terminal_buffer[:] = remaining
                        try: value = json.loads(line)
                        except (json.JSONDecodeError, UnicodeDecodeError): continue
                        if value.get('event') == 'approval_required' and value.get('request_id') == request_id:
                            return value
                    if select.select([master], [], [], 0.2)[0]: terminal_buffer.extend(os.read(master, 65536))
                raise AssertionError('independent TTY did not receive the expected approval request')

            marker = root/'approved-once'
            argv = [sys.executable, '-c', f"from pathlib import Path; p=Path({str(marker)!r}); p.write_text(p.read_text()+'x' if p.exists() else 'x')"]
            pending = session.exec('manual-exec', argv, cwd=root)
            suite.expect_error(pending, 'APPROVAL_REQUIRED', 'first manual exec')
            require(not marker.exists(), 'command executed before operator approval')
            notice = prompt('manual-exec')
            require(notice['approval_code'] not in json.dumps(pending), 'approval code leaked through tool reply')
            require(notice['command']['argv'] == argv, 'TTY did not show the actual argv')
            os.write(master, ('allow '+notice['approval_code']+'\n').encode())
            accepted = wait_until(lambda: (r if (r := session.exec('manual-exec', argv, cwd=root)).get('result') else None), 5, 'operator approval propagation')
            job = suite.register_reply(accepted, 'approved command')
            suite.read_to_terminal(session, job['job_id'])
            require(marker.read_text() == 'x', 'approved command did not execute exactly once')
            duplicate = suite.register_reply(session.exec('manual-exec', argv, cwd=root), 'approved duplicate')
            require(duplicate['job_id'] == job['job_id'] and marker.read_text() == 'x', 'approved retry repeated execution')
            suite.expect_error(session.exec('manual-exec', ['/bin/echo', 'changed'], cwd=root), 'REQUEST_CONFLICT', 'approval argument binding')

            refused = session.exec('manual-deny', ['/bin/echo', 'denied-command'], cwd=root)
            suite.expect_error(refused, 'APPROVAL_REQUIRED', 'denied request first call')
            notice = prompt('manual-deny'); os.write(master, ('deny '+notice['approval_code']+'\n').encode())
            wait_until(lambda: session.exec('manual-deny', ['/bin/echo', 'denied-command'], cwd=root).get('error', {}).get('code') == 'DENIED', 5, 'operator denial propagation')

            args_cat = ['/bin/cat']
            suite.expect_error(session.exec('manual-cat', args_cat, cwd=root, stdin=True), 'APPROVAL_REQUIRED', 'manual stdin exec')
            notice = prompt('manual-cat'); os.write(master, ('allow '+notice['approval_code']+'\n').encode())
            cat_reply = wait_until(lambda: (r if (r := session.exec('manual-cat', args_cat, cwd=root, stdin=True)).get('result') else None), 5, 'cat approval')
            cat = suite.register_reply(cat_reply, 'manual cat')
            text = 'operator-approved-input\n'
            suite.expect_error(session.write(cat['job_id'], 'manual-write', text, eof=True), 'APPROVAL_REQUIRED', 'manual write approval')
            notice = prompt('manual-write'); os.write(master, ('allow '+notice['approval_code']+'\n').encode())
            wait_until(lambda: (r if (r := session.write(cat['job_id'], 'manual-write', text, eof=True)).get('result') else None), 5, 'write approval')
            view, events = suite.read_to_terminal(session, cat['job_id'])
            require(view['exit_code'] == 0 and ''.join(e['text'] for e in events) == text, 'approved stdin/EOF was not delivered')
            print('PASS: real TTY manual exec/write approval, denial, exact argument binding, dedup, and non-TTY fail-closed')
        finally:
            if operator and operator.poll() is None:
                os.killpg(operator.pid, signal.SIGTERM)
                try: operator.wait(timeout=5)
                except subprocess.TimeoutExpired: os.killpg(operator.pid, signal.SIGKILL); operator.wait(timeout=5)
            if master is not None: os.close(master)
            session.stop(); relay.terminate()

if __name__ == '__main__': main()
