#!/usr/bin/env python3
"""Public Quick Tunnel test, with simulated SDK confirmation (NOT G0 acceptance)."""
import argparse, hashlib, json, os, pathlib, queue, shlex, signal, subprocess, tempfile, threading, time

def main():
    p = argparse.ArgumentParser()
    p.add_argument('--binary', required=True)
    p.add_argument('--client', required=True)
    p.add_argument('--cloudflared', default='/usr/local/bin/cloudflared')
    p.add_argument('--proxy-command', default='')
    p.add_argument('--client-proxy-command',default='')
    p.add_argument('--resolver',default='')
    p.add_argument('--command', default='printf quick-smoke')
    p.add_argument('--target-timeout', default='2m')
    p.add_argument('--wait', default='6m')
    p.add_argument('--settle-seconds', type=float, default=0, help='after client error, wait for local command history result before stopping')
    p.add_argument('--keep', action='store_true', help='keep private evidence directory')
    a = p.parse_args()
    root = pathlib.Path(tempfile.mkdtemp(prefix='agent-tunnel-quick-'))
    executable = pathlib.Path(a.cloudflared).resolve()
    if a.proxy_command:
        wrapper = root/'cloudflared-wrapper'
        wrapper.write_text('#!/bin/sh\nexec '+shlex.join(shlex.split(a.proxy_command)+[str(executable)])+' "$@"\n')
        wrapper.chmod(0o700)
        executable = wrapper
    ready_lines = queue.Queue()
    proc = subprocess.Popen([str(pathlib.Path(a.binary).resolve()), 'target', '--transport', 'quick', '--mode', 'review', '--ttl', '30m', '--command-timeout', a.target_timeout, '--log-dir', str(root/'history'), '--cloudflared', str(executable)], stdout=subprocess.PIPE, stderr=(root/'target.stderr').open('w'), text=True, start_new_session=True)
    def reader():
        for line in proc.stdout:
            try:
                data = json.loads(line)
                if data.get('event') == 'ready': ready_lines.put(line)
            except ValueError: pass
        ready_lines.put(None)
    thread = threading.Thread(target=reader, daemon=True); thread.start()
    report = {'test': 'public_quick_with_simulated_sdk_confirmation', 'real_agent_acceptance': False, 'evidence_dir': str(root), 'command_timeout': a.target_timeout, 'binary_sha256': hashlib.sha256(pathlib.Path(a.binary).read_bytes()).hexdigest(), 'cloudflared_sha256':hashlib.sha256(pathlib.Path(a.cloudflared).read_bytes()).hexdigest(), 'client_proxy':a.client_proxy_command}
    try:
        try: line = ready_lines.get(timeout=40)
        except queue.Empty: line = None
        if line is None:
            report.update(status='access_not_ready', target_exit=proc.poll())
            return 1
        endpoint = root/'ready.json'; endpoint.write_text(line); endpoint.chmod(0o600)
        started = time.monotonic()
        result = subprocess.run(shlex.split(a.client_proxy_command)+[str(pathlib.Path(a.client).resolve()), '--url-file', str(endpoint), '--confirm', 'accept', '--command', a.command, '--wait', a.wait, '--resolver',a.resolver], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        (root/'result.json').write_text(result.stdout)
        (root/'client.stderr').write_text(result.stderr)
        report.update(status='passed' if result.returncode == 0 else 'call_failed_or_tool_error', client_exit=result.returncode, elapsed_seconds=round(time.monotonic()-started, 3))
        if result.stdout:
            try:
                r=json.loads(result.stdout).get('structuredContent', {})
                report.update(command_status=r.get('status'), history_status=r.get('history_status'), started=r.get('started'), stdout_bytes=len(r.get('stdout','').encode()))
            except ValueError: pass
        if result.returncode and a.settle_seconds and "MCP connection failed" not in result.stderr:
            deadline=time.monotonic()+a.settle_seconds
            settled=False
            while time.monotonic()<deadline and not settled:
                for file in (root/'history').glob('*/commands*.jsonl'):
                    for line in file.read_text().splitlines():
                        try: event=json.loads(line)
                        except ValueError: continue
                        if event.get('event')=='execution_result':
                            data=event.get('data', {})
                            report.update(local_completion=data.get('status'), local_duration_ms=data.get('duration_ms'))
                            settled=True
                if not settled: time.sleep(1)
            report['local_result_found']=settled
        return result.returncode
    finally:
        proc.send_signal(signal.SIGTERM) if proc.poll() is None else None
        try: proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL); proc.wait()
            report['forced_cleanup']=True
        endpoint = root/'ready.json'
        if endpoint.exists(): endpoint.unlink()  # never keep access token in evidence
        report['target_exit']=proc.returncode
        (root/'report.json').write_text(json.dumps(report, indent=2)+'\n')
        print(json.dumps(report))
        if not a.keep:
            import shutil; shutil.rmtree(root)

if __name__=='__main__': raise SystemExit(main())
