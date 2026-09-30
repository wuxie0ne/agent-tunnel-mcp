#!/usr/bin/env python3
"""Local cross-process relay/target test. SDK approvals are simulated, not G0."""
import argparse, hashlib, json, os, pathlib, queue, signal, socket, subprocess, tempfile, threading, time

def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1',0)); return s.getsockname()[1]

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--binary',required=True);parser.add_argument('--client',required=True)
    parser.add_argument('--command', default='printf local-smoke');parser.add_argument('--target-timeout',default='2m');parser.add_argument('--wait',default='6m');parser.add_argument('--keep',action='store_true')
    a=parser.parse_args();root=pathlib.Path(tempfile.mkdtemp(prefix='agent-tunnel-local-'))
    env=os.environ.copy();env['AGENT_TUNNEL_REGISTRATION_KEY']=os.urandom(32).hex()
    binary=str(pathlib.Path(a.binary).resolve());port=free_port();procs=[]
    report={'test':'local_relay_cross_process_simulated_sdk_confirmation','real_agent_acceptance':False,'evidence_dir':str(root),'binary_sha256':hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest()}
    try:
        relay=subprocess.Popen([binary,'relay','--listen',f'127.0.0.1:{port}'],env=env,stdout=subprocess.PIPE,stderr=(root/'relay.stderr').open('w'),text=True,start_new_session=True);procs.append(relay)
        relay.stdout.readline()
        node=subprocess.Popen([binary,'target','--transport','relay','--relay-url',f'http://127.0.0.1:{port}','--mode','review','--ttl','30m','--command-timeout',a.target_timeout,'--log-dir',str(root/'history')],env=env,stdout=subprocess.PIPE,stderr=(root/'target.stderr').open('w'),text=True,start_new_session=True);procs.append(node)
        lines=queue.Queue()
        def reader():
            for line in node.stdout:
                try:
                    if json.loads(line).get('event')=='ready':lines.put(line)
                except ValueError:pass
            lines.put(None)
        threading.Thread(target=reader,daemon=True).start()
        line=lines.get(timeout=35)
        if line is None:raise RuntimeError('target failed before ready')
        ready=root/'ready.json';ready.write_text(line);ready.chmod(0o600)
        started=time.monotonic();result=subprocess.run([str(pathlib.Path(a.client).resolve()),'--url-file',str(ready),'--confirm','accept','--command',a.command,'--wait',a.wait],text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        (root/'result.json').write_text(result.stdout);(root/'client.stderr').write_text(result.stderr)
        report.update(status='passed' if result.returncode==0 else 'failed',elapsed_seconds=round(time.monotonic()-started,3),client_exit=result.returncode)
        return result.returncode
    finally:
        for proc in reversed(procs):
            if proc.poll() is None:proc.send_signal(signal.SIGTERM)
            try:proc.wait(timeout=15)
            except subprocess.TimeoutExpired:os.killpg(proc.pid,signal.SIGKILL);proc.wait();report['forced_cleanup']=True
        ready=root/'ready.json'
        if ready.exists():ready.unlink()
        (root/'report.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))
        if not a.keep:
            import shutil;shutil.rmtree(root)
if __name__=='__main__':raise SystemExit(main())
