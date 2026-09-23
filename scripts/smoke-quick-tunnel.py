#!/usr/bin/env python3
"""Opt-in live Cloudflare smoke test; creates a short-lived, encrypted test session.
Runs only a harmless printf on the local test Connector. Never targets production.
"""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True)
parser.add_argument('--proxy-command', help='optional executable prefix, e.g. mgraftcp')
parser.add_argument('--connect-ip', help='optional operator-approved Cloudflare edge IP; TLS still validates hostname')
args = parser.parse_args()
if args.connect_ip: ipaddress.ip_address(args.connect_ip)
binary = str(Path(args.binary).resolve())
cloudflared = shutil.which('cloudflared')
if not cloudflared: raise SystemExit('cloudflared is not installed')
prefix = [args.proxy_command] if args.proxy_command else []
with tempfile.TemporaryDirectory(prefix='agent-tunnel-cloudflare-') as tmp:
    root = Path(tmp)
    processes = []; logs = []
    # Do not inherit model API keys, Cloudflare named-tunnel tokens or other
    # unrelated operator credentials into temporary public test processes.
    env_keys = {'PATH', 'HOME', 'LANG', 'LC_ALL', 'SSL_CERT_FILE', 'SSL_CERT_DIR',
                'HTTP_PROXY', 'HTTPS_PROXY', 'NO_PROXY', 'http_proxy', 'https_proxy', 'no_proxy'}
    safe_env = {k: v for k, v in os.environ.items() if k in env_keys or (args.proxy_command and k.startswith('MGRAFTCP_'))}
    safe_env['HOME'] = tmp
    if args.connect_ip:
        safe_env['AGENT_TUNNEL_CONNECT_IP'] = args.connect_ip
    def start(name, command, env=None):
        path = root/(name+'.log'); log = open(path, 'wb'); logs.append(log)
        proc = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log, stderr=log, env=env or safe_env, start_new_session=True)
        processes.append(proc); return proc, path
    def cli(*argv, allow_error=False):
        p = subprocess.run([binary, *map(str, argv)], capture_output=True, text=True, timeout=20)
        if p.returncode and not allow_error: raise RuntimeError(p.stderr[-2000:])
        if p.stdout.strip(): return json.loads(p.stdout)
        return None
    try:
        with socket.socket() as port:
            port.bind(('127.0.0.1', 0)); relay_port = port.getsockname()[1]
        # HOME override prevents modifying/using the operator's cloudflared config.
        cf_env = dict(safe_env)
        tunnel, tunnel_log = start('cloudflared', [*prefix, cloudflared, 'tunnel', '--no-autoupdate', '--protocol', 'http2', '--url', f'http://127.0.0.1:{relay_port}'], cf_env)
        deadline = time.monotonic()+90; url = None
        while time.monotonic() < deadline:
            content = tunnel_log.read_text(errors='replace')
            match = re.search(r'https://[a-z0-9-]+\.trycloudflare\.com', content)
            if match: url = match.group(); break
            if tunnel.poll() is not None: break
            time.sleep(0.2)
        if not url: raise RuntimeError('Quick Tunnel URL unavailable: '+tunnel_log.read_text(errors='replace')[-3000:])
        session = root/'session'
        cli('init', '--dir', session, '--relay', url, '--name', 'quick-tunnel-smoke', '--ttl-secs', '300')
        start('relay', [binary, 'relay', '--listen', f'127.0.0.1:{relay_port}', '--session-file', str(session/'relay.json')])
        for _ in range(50):
            if (session/'relay-admin.sock').exists(): break
            time.sleep(0.1)
        controller, _ = start('controller', [*prefix, binary, 'local', '--config', str(session/'controller.json'), '--socket', str(session/'controller.sock'), '--accept-session-risk'])
        connector, _ = start('connector', [*prefix, binary, 'connect', '--config', str(session/'connector.json'), '--allow-exec'])
        info = None; deadline = time.monotonic()+55
        while time.monotonic() < deadline:
            reply = cli('info', '--socket', session/'controller.sock', allow_error=True)
            if reply and reply.get('result'):
                info = reply['result']; break
            if controller.poll() is not None or connector.poll() is not None: break
            time.sleep(0.3)
        if not info: raise RuntimeError('Both endpoints could not establish the public WSS/Noise path')
        if not info.get('end_to_end_encrypted'): raise RuntimeError('E2EE is not enabled')
        marker = 'agent-tunnel-public-wss-noise-ok'
        job = cli('exec', '--socket', session/'controller.sock', '--incarnation', info['incarnation'], '--request-id', 'public-wss-smoke', '--cwd', tmp, '--', '/bin/sh', '-c', f"printf '%s\\n' '{marker}'")['result']
        output = ''; cursor = 0; deadline = time.monotonic()+10
        while time.monotonic() < deadline:
            view = cli('read', '--socket', session/'controller.sock', '--job', job['job_id'], '--cursor', cursor)['result']
            output += ''.join(e['text'] for e in view['events']); cursor = view['next_cursor']
            if view['state'] != 'running': break
            time.sleep(0.1)
        if marker not in output or view['exit_code'] != 0: raise RuntimeError('Public path exec/read verification failed')
        revoked = cli('revoke', '--admin-socket', session/'relay-admin.sock', '--session', info['session_id'])
        if not revoked.get('persisted'): raise RuntimeError('Revocation not persisted')
        print(json.dumps({'provider':'Cloudflare Quick Tunnel', 'both_endpoints_via_public_wss':True, 'noise_e2ee':True, 'exec_read_exit_code':0, 'durable_revoke':True, 'production_target':False}))
    except Exception:
        # Generated tokens/PSKs are never printed. Application logs intentionally omit them.
        for path in root.glob('*.log'):
            print(f'--- {path.name} ---\n{path.read_text(errors="replace")[-3000:]}', file=__import__('sys').stderr)
        raise
    finally:
        import signal
        for proc in reversed(processes):
            if proc.poll() is None:
                try: os.killpg(proc.pid, signal.SIGTERM)
                except ProcessLookupError: pass
        for proc in reversed(processes):
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try: os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError: pass
                proc.wait(timeout=5)
        for log in logs: log.close()
