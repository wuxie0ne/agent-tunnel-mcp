#!/usr/bin/env python3
"""Load the real installed pi extension offline without making any model request."""
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import tempfile
import time

root = Path(__file__).resolve().parents[1]
pi = shutil.which("pi")
if not pi:
    raise SystemExit("pi is not installed; extension runtime check cannot be performed")
with tempfile.TemporaryDirectory(prefix="agent-tunnel-pi-load-") as tmp:
    probe = Path(tmp) / "probe.ts"
    probe.write_text('export default function(pi) { pi.on("session_start", async () => { process.stderr.write("AGENT_TUNNEL_TOOLS="+JSON.stringify(pi.getAllTools().map(t=>t.name))+"\\n"); }); }\n')
    env = {k: v for k, v in os.environ.items() if k in ("PATH", "HOME", "TERM", "LANG", "LD_LIBRARY_PATH")}
    env.update(PI_CODING_AGENT_DIR=str(Path(tmp)/"pi"), PI_OFFLINE="1")
    command = [pi, "--offline", "--no-session", "--no-extensions", "--no-skills", "--no-prompt-templates", "--no-themes", "--no-context-files", "--no-approve", "--mode", "rpc", "-e", str(root/"integrations/pi/index.ts"), "-e", str(probe)]
    proc = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=tmp)
    try:
        proc.stdin.write(b'{"id":"state","type":"get_state"}\n'); proc.stdin.flush()
        sel = selectors.DefaultSelector(); buffers = {}
        for stream in (proc.stdout, proc.stderr):
            sel.register(stream, selectors.EVENT_READ); buffers[stream] = b""
        deadline = time.monotonic()+25; tools = None; state = False; errors = []
        while time.monotonic() < deadline and not (tools and state):
            for key, _ in sel.select(0.5):
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    sel.unregister(key.fileobj); continue
                buffers[key.fileobj] += chunk
                while b"\n" in buffers[key.fileobj]:
                    line, buffers[key.fileobj] = buffers[key.fileobj].split(b"\n", 1)
                    text = line.decode(errors="replace")
                    if text.startswith("AGENT_TUNNEL_TOOLS="):
                        tools = json.loads(text.split("=",1)[1])
                    elif key.fileobj is proc.stdout:
                        try:
                            value = json.loads(text)
                            if value.get("command") == "get_state" and value.get("success"): state = True
                        except json.JSONDecodeError: pass
                    else:
                        errors.append(text)
            if proc.poll() is not None: break
        expected = {"remote_info", "remote_exec", "remote_read", "remote_write", "remote_resize", "remote_cancel"}
        if not state or not tools or not expected.issubset(tools):
            raise SystemExit("pi extension runtime check failed: " + "\n".join(errors)[-5000:] + f"\ntools={tools} state={state}")
        version = subprocess.check_output([pi, "--version"], env=env, text=True).strip()
        print(json.dumps({"pi_version":version,"registered_tools":sorted(expected),"rpc_startup":True,"model_requests":0}))
    finally:
        proc.terminate()
        try: proc.wait(timeout=5)
        except subprocess.TimeoutExpired: proc.kill(); proc.wait(timeout=5)
