#!/usr/bin/env python3
"""Real official Inspector web UI, automated buttons (not a human-click claim).
Requires separately installed Inspector 2.8.0 and Python Playwright. Neither is
part of the delivered Go runtime. All profiles, credentials and servers are local.
"""
import argparse, hashlib, json, os, pathlib, queue, re, secrets, shlex, signal, socket, subprocess, tempfile, threading, time
from playwright.sync_api import sync_playwright

def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1',0)); return s.getsockname()[1]

def main():
    p=argparse.ArgumentParser();p.add_argument('--binary',required=True);p.add_argument('--inspector',required=True,help='official package clients/launcher/build/index.js');p.add_argument('--browser',default='/usr/bin/google-chrome');p.add_argument('--transport',choices=['relay','quick'],default='relay');p.add_argument('--cloudflared',default='/usr/local/bin/cloudflared');p.add_argument('--proxy-command',default='');p.add_argument('--keep',action='store_true');a=p.parse_args()
    root=pathlib.Path(tempfile.mkdtemp(prefix='agent-tunnel-inspector-proof-'));procs=[];binary=str(pathlib.Path(a.binary).resolve());env=os.environ.copy();env['AGENT_TUNNEL_REGISTRATION_KEY']=secrets.token_hex(32)
    report={'test':'official_Inspector_2_8_0_real_web_UI','transport':a.transport,'binary_sha256':hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest(),'approval_button_automated':True,'human_click_claim':False,'evidence_dir':str(root),'status':'failed'}
    try:
        extra=[]
        if a.transport=='relay':
            relay_port=port();relay=subprocess.Popen([binary,'relay','--listen',f'127.0.0.1:{relay_port}'],env=env,stdout=subprocess.PIPE,stderr=(root/'relay.stderr').open('w'),text=True,start_new_session=True);procs.append(relay);relay.stdout.readline();extra=['--relay-url',f'http://127.0.0.1:{relay_port}']
        else:
            cf=pathlib.Path(a.cloudflared).resolve()
            if a.proxy_command:
                wrapper=root/'cloudflared-wrapper';wrapper.write_text('#!/bin/sh\nexec '+shlex.join(shlex.split(a.proxy_command)+[str(cf)])+' "$@"\n');wrapper.chmod(0o700);cf=wrapper
            extra=['--cloudflared',str(cf)]
        node=subprocess.Popen([binary,'target','--transport',a.transport,'--mode','review','--log-dir',str(root/'history')]+extra,env=env,stdout=subprocess.PIPE,stderr=(root/'target.stderr').open('w'),text=True,start_new_session=True);procs.append(node)
        lines=queue.Queue()
        def reader():
            for line in node.stdout:
                try:
                    if json.loads(line).get('event')=='ready':lines.put(line)
                except ValueError:pass
            lines.put(None)
        threading.Thread(target=reader,daemon=True).start();line=lines.get(timeout=35)
        if not line:raise RuntimeError('target not ready')
        endpoint=json.loads(line)['mcp_url'];config=root/'config.json';config.write_text(json.dumps({'mcpServers':{'proof':{'type':'streamable-http','url':endpoint,'protocolEra':'modern'}}}));config.chmod(0o600)
        ui_port=port();home=root/'home';home.mkdir();env.update(HOME=str(home),MCP_STORAGE_DIR=str(root/'storage'),MCP_INSPECTOR_SECRET_STORE='memory',MCP_AUTO_OPEN_ENABLED='false',CLIENT_PORT=str(ui_port),MCP_SANDBOX_PORT='0',MCP_APP_ORIGIN_PORT='0',HOST='127.0.0.1',MCP_INSPECTOR_API_TOKEN=secrets.token_hex(32))
        inspector=subprocess.Popen(['node',str(pathlib.Path(a.inspector).resolve()),'--web','--config',str(config),'--server','proof'],env=env,stdout=(root/'inspector.stdout').open('w'),stderr=(root/'inspector.stderr').open('w'),start_new_session=True);procs.append(inspector)
        with sync_playwright() as pw:
            browser=pw.chromium.launch(executable_path=a.browser,headless=True,args=['--no-sandbox']);page=browser.new_page();page.set_default_timeout(10000)
            ui=f'http://127.0.0.1:{ui_port}'
            for attempt in range(15):
                try:page.goto(ui);break
                except Exception:
                    if attempt==14:raise
                    time.sleep(.5)
            switch=page.get_by_role('switch',name='Connect or disconnect "proof"');switch.locator('..').click() if not switch.is_checked() else None;page.get_by_text('Tools',exact=True).first.click();page.get_by_role('button',name='exec',exact=True).click()
            def request(file):
                page.get_by_role('button',name='target_info',exact=True).click();page.get_by_role('button',name='exec',exact=True).click();edit=page.get_by_role('switch').last;edit.locator('..').click() if not edit.is_checked() else None
                editor=page.get_by_role('textbox',name=re.compile('^Arguments JSON'));editor.focus();editor.press('Control+A');page.keyboard.insert_text(json.dumps({'shell_command':"printf 'x\\n' >> "+shlex.quote(str(file))}));page.get_by_role('button',name='Execute Tool',exact=True).click();dialog=page.get_by_role('dialog');dialog.wait_for();assert not file.exists();text=dialog.inner_text();assert str(file) in text and 'instance' in text and 'cwd' in text and 'timeout_ns' in text;return dialog
            approved=root/'approved';declined=root/'declined';dialog=request(approved);dialog.get_by_role('checkbox',name='confirm').check(force=True);dialog.get_by_role('button',name='Submit',exact=True).click();page.wait_for_timeout(700);assert approved.read_text()=='x\n'
            dialog=request(declined);dialog.get_by_role('button',name='Decline',exact=True).click();page.wait_for_timeout(700);assert not declined.exists()
            report.update(status='passed',protocol='2026-07-28',full_command_displayed=True,no_effect_before_approval=True,approval_runs_once=True,decline_no_effect=True);browser.close()
        return 0
    finally:
        for proc in reversed(procs):
            if proc.poll() is None:proc.send_signal(signal.SIGTERM)
            try:proc.wait(timeout=15)
            except subprocess.TimeoutExpired:os.killpg(proc.pid,signal.SIGKILL);proc.wait();report['forced_cleanup']=True
        config=root/'config.json'
        if config.exists():config.unlink()
        # Inspector diagnostics can contain full endpoints. Keep history/report,
        # not credentials or third-party storage/browser profiles.
        for name in ('inspector.stdout','inspector.stderr'):
            f=root/name
            if f.exists():f.unlink()
        (root/'report.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))
        import shutil
        for name in ('home','storage'):
            if (root/name).exists():shutil.rmtree(root/name)
        if not a.keep:shutil.rmtree(root)
if __name__=='__main__':raise SystemExit(main())
