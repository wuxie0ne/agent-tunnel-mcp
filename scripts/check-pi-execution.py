#!/usr/bin/env python3
"""Exercise actual pi tool code against a fake private IPC socket, without an LLM.

The fake Controller only records structured requests and never executes argv.
This checks the TypeScript extension entrypoint and its pi 0.86.0 tool schema
path rather than substituting a mock implementation of the transport.
"""
import json
import os
from pathlib import Path
import selectors
import shutil
import socket
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
pi = shutil.which('pi')
if not pi:
    raise SystemExit('pi CLI is unavailable')
with tempfile.TemporaryDirectory(prefix='agent-tunnel-pi-call-') as tmp:
    root = Path(tmp)
    path = root / 'controller.sock'
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(path))
    os.chmod(path, 0o600)
    server.listen(8)
    server.settimeout(.2)
    received = []
    shutdown = threading.Event()

    def serve():
        while not shutdown.is_set():
            try:
                conn, _ = server.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with conn:
                conn.settimeout(4)
                buf = b''
                try:
                    while b'\n' not in buf:
                        chunk = conn.recv(65536)
                        if not chunk or len(buf) + len(chunk) > 256 * 1024:
                            break
                        buf += chunk
                    if not buf.endswith(b'\n'):
                        continue
                    request = json.loads(buf)
                    received.append(request)
                    result = {'name': 'stub-target', 'incarnation': 'stub-incarnation'} if request['op'] == 'info' else {'job_id':'stub-job'}
                    conn.sendall(json.dumps({'id':request['id'],'result':result,'error':None}).encode()+b'\n')
                except (OSError, ValueError, KeyError):
                    pass

    worker = threading.Thread(target=serve, daemon=True)
    worker.start()
    probe = root / 'probe.ts'
    extension = ROOT / 'integrations/pi/index.ts'
    probe.write_text('''
import remote from ''' + json.dumps(str(extension)) + ''';
export default function(pi) {
  const definitions = new Map();
  remote({ registerTool: tool => definitions.set(tool.name, tool) });
  pi.on('session_start', async () => {
    const signal = new AbortController().signal;
    try {
      const result = await definitions.get('remote_info').execute('stub-call',
        { op: 'exec', argv: ['must-not-execute'], expected_incarnation: 'fake' },
        signal, undefined, { hasUI: false });
      if (result.details.result.name !== 'stub-target') throw Error('Info result mismatch');
      let refused = false;
      try { await definitions.get('remote_exec').execute('stub-call',
        { expected_incarnation: 'fake', argv: ['must-not-execute'], cwd:'/tmp' },
        signal, undefined, { hasUI: false }); }
      catch { refused = true; }
      if (!refused) throw Error('Exec omitted required request_id');
      const e = await definitions.get('remote_exec').execute('stub-call',
        { request_id:'pi-request-id', expected_incarnation:'stub-incarnation',
          argv:['/bin/echo','not-actually-run'], cwd:'/tmp', op:'info' },
        signal, undefined, { hasUI:false });
      if (e.details.result.job_id !== 'stub-job') throw Error('Exec result mismatch');
      process.stderr.write('AGENT_TUNNEL_CALLS=ok\\n');
    } catch (error) { process.stderr.write('AGENT_TUNNEL_CALLS=failed:'+String(error)+'\\n'); }
  });
}
''')
    env = {k:v for k,v in os.environ.items() if k in ('PATH','HOME','TERM','LANG','LD_LIBRARY_PATH')}
    env.update(PI_CODING_AGENT_DIR=str(root/'pi'),PI_OFFLINE='1',AGENT_TUNNEL_SOCKET=str(path))
    command = [pi,'--offline','--no-session','--no-extensions','--no-skills',
               '--no-prompt-templates','--no-themes','--no-context-files','--no-approve',
               '--mode','rpc','-e',str(extension),'-e',str(probe)]
    process = subprocess.Popen(command,stdin=subprocess.PIPE,stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE,env=env,cwd=root)
    try:
        process.stdin.write(b'{"id":"pi-load","type":"get_state"}\n')
        process.stdin.flush()
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        selector.register(process.stderr, selectors.EVENT_READ)
        streams = {process.stdout:b'', process.stderr:b''}
        startup = executed = False
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline and not(startup and executed):
            for key,_ in selector.select(.5):
                chunk = os.read(key.fileobj.fileno(),65536)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                streams[key.fileobj] += chunk
                while b'\n' in streams[key.fileobj]:
                    line,streams[key.fileobj] = streams[key.fileobj].split(b'\n',1)
                    text=line.decode(errors='replace')
                    if text.startswith('AGENT_TUNNEL_CALLS='):
                        if text!='AGENT_TUNNEL_CALLS=ok':
                            raise AssertionError(text)
                        executed=True
                    elif key.fileobj is process.stdout:
                        try:
                            response=json.loads(text)
                            startup |= response.get('command')=='get_state' and response.get('success') is True
                        except json.JSONDecodeError:
                            pass
            if process.poll() is not None:
                break
        assert startup and executed, f'pi RPC startup/tool result missing: startup={startup} executed={executed}'
        assert len(received)==2, f'unexpected number of IPC requests: {len(received)}'
        assert received[0]['op']=='info', 'read-only tool was coerced to exec'
        assert received[1]['op']=='exec' and received[1]['id']=='pi-request-id', 'exec ID/op not preserved'
        assert received[1]['argv']==['/bin/echo','not-actually-run']
        print(json.dumps({'pi_offline_tool_execute':True,'schema_registration':True,
                          'ipc_requests':len(received),'read_only_op_forced':True,'model_requests':0}))
    finally:
        process.terminate()
        try: process.wait(timeout=5)
        except subprocess.TimeoutExpired: process.kill();process.wait(timeout=5)
        shutdown.set()
        server.close()
        worker.join(timeout=5)
