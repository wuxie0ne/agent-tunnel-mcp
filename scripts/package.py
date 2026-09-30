#!/usr/bin/env python3
"""Build Linux amd64 developer packages. Never downloads anything at runtime."""
import argparse, hashlib, json, os, pathlib, re, shutil, subprocess, tarfile, tempfile, time
ROOT=pathlib.Path(__file__).resolve().parents[1]
def run(args,**kw):return subprocess.run(args,cwd=ROOT,check=True,**kw)
def sha(path):return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()
def modules():
    text=run(['go','list','-deps','-json','./cmd/agent-tunnel'],capture_output=True,text=True).stdout;decoder=json.JSONDecoder();values={}
    while text.strip():
        text=text.lstrip();item,index=decoder.raw_decode(text);module=item.get("Module");
        if module:values[module["Path"]]=module
        text=text[index:]
    return list(values.values())

def licenses(directory):
    dest=directory/'licenses';dest.mkdir()
    goroot=run(['go','env','GOROOT'],capture_output=True,text=True).stdout.strip()
    shutil.copyfile(pathlib.Path(goroot)/'LICENSE',dest/'Go-LICENSE')
    found=[]
    for module in modules():
        if module.get('Main'):continue
        base=pathlib.Path(module.get('Dir',''))
        files=[p for p in base.iterdir() if p.is_file() and p.name.upper() in ('LICENSE','LICENSE.TXT','LICENSE.MD','COPYING','NOTICE','NOTICE.TXT')]
        if not files:raise RuntimeError('missing third-party license: '+module['Path'])
        label=re.sub(r'[^a-zA-Z0-9_.-]','_',module['Path'])
        for file in files:shutil.copyfile(file,dest/(label+'-'+file.name))
        found.append({'module':module['Path'],'version':module.get('Version'),'sum':module.get('Sum')})
    return found

def main():
    p=argparse.ArgumentParser();p.add_argument('--version',default='0.1.0-dev');p.add_argument('--cloudflared');p.add_argument('--official-sha256');p.add_argument('--cloudflared-version');p.add_argument('--cloudflared-license');p.add_argument('--force',action='store_true');a=p.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*',a.version):p.error('unsafe version')
    if a.cloudflared and not (a.official_sha256 and re.fullmatch('[a-f0-9]{64}',a.official_sha256) and a.cloudflared_version and re.fullmatch(r'\d{4}\.\d+\.\d+',a.cloudflared_version) and a.cloudflared_license):p.error('full package requires official checksum, upstream version, and upstream LICENSE')
    if a.cloudflared and sha(a.cloudflared)!=a.official_sha256:raise RuntimeError('cloudflared does not match official input checksum')
    dist=ROOT/'dist';dist.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='agent-tunnel-package-') as tmp:
        work=pathlib.Path(tmp);binary=work/'agent-tunnel';env=os.environ.copy()
        env.update(CGO_ENABLED='0',GOOS='linux',GOARCH='amd64',GOAMD64='v1',GOTOOLCHAIN='local')
        run(['go','build','-trimpath','-ldflags','-s -w -X main.version='+a.version,'-o',str(binary),'./cmd/agent-tunnel'],env=env)
        source=run(['git','rev-parse','HEAD'],capture_output=True,text=True).stdout.strip();dirty=bool(run(['git','status','--porcelain'],capture_output=True,text=True).stdout.strip())
        metadata={'project_version':a.version,'platform':'linux-amd64','source_commit':source,'source_dirty':dirty,'go':run(['go','version'],capture_output=True,text=True).stdout.strip(),'build_options':['CGO_ENABLED=0','GOAMD64=v1','-trimpath','-s -w'],'binary_sha256':sha(binary),'acceptance':'developer candidate; see implementation-status, not production or complete P0 certification'}
        for flavor in ('slim','full') if a.cloudflared else ('slim',):
            name=f'agent-tunnel-{a.version}-linux-amd64-{flavor}';directory=work/name;directory.mkdir();shutil.copy2(binary,directory/'agent-tunnel');shutil.copy2(ROOT/'README.md',directory/'README.md')
            docs=directory/'docs';docs.mkdir()
            for document in ('go-requirements.md','go-design-plan.md','implementation-status.md'):shutil.copy2(ROOT/'docs'/document,docs/document)
            m=dict(metadata);m['dependencies']=licenses(directory)
            if flavor=='full':
                cf=directory/'cloudflared';shutil.copyfile(a.cloudflared,cf);cf.chmod(0o755)
                if sha(cf)!=a.official_sha256:raise RuntimeError("copied cloudflared changed during packaging")
                original_size=cf.stat().st_size
                run(['strip','--strip-debug',str(cf)])
                run(['upx','--best','--lzma',str(cf)],stdout=subprocess.DEVNULL)
                run(['upx','-t',str(cf)],stdout=subprocess.DEVNULL)
                actual=run([str(cf),'--version'],capture_output=True,text=True).stdout.strip()
                if a.cloudflared_version not in actual:raise RuntimeError('processed cloudflared version mismatch')
                shutil.copyfile(a.cloudflared_license,directory/'licenses/cloudflared-LICENSE')
                (directory/'licenses/UPX-LICENSE.txt').write_text(run(['upx','--license'],capture_output=True,text=True).stdout)
                (directory/'licenses/cloudflared-build-modules.txt').write_text(run(['go','version','-m',a.cloudflared],capture_output=True,text=True).stdout)
                m['cloudflared']={'version':a.cloudflared_version,'source_url':f'https://github.com/cloudflare/cloudflared/releases/download/{a.cloudflared_version}/cloudflared-linux-amd64','original_sha256':a.official_sha256,'processed_sha256':sha(cf),'original_bytes':original_size,'processed_bytes':cf.stat().st_size,'processing':['strip --strip-debug','upx --best --lzma'],'upx':run(['upx','--version'],capture_output=True,text=True).stdout.splitlines()[0],'note':'Modified packaging of official binary; version/UPX check alone is NOT functional validation'}
            (directory/'BUILD-INFO.json').write_text(json.dumps(m,ensure_ascii=False,indent=2)+'\n')
            (directory/'THIRD-PARTY.md').write_text('Third-party licenses are retained in licenses/. Cloudflared is separately licensed; the full package modifies its binary using strip/UPX, not its source. The cloudflared module list is provenance, not a legal completeness certification. No project-source license has been assigned by this tool.\n')
            checks=[f'{sha(file)}  {file.relative_to(directory)}' for file in sorted(directory.rglob('*')) if file.is_file()]
            (directory/'SHA256SUMS').write_text('\n'.join(checks)+'\n')
            archive=dist/(name+'.tar.gz')
            if archive.exists() and not a.force:raise RuntimeError('archive exists; use --force for this named build')
            with tarfile.open(archive,'w:gz',compresslevel=9) as tar:tar.add(directory,arcname=name)
            print(json.dumps({'artifact':str(archive.relative_to(ROOT)),'sha256':sha(archive),'bytes':archive.stat().st_size,'source_commit':source,'source_dirty':dirty}))
        artifacts=[f'{sha(file)}  {file.name}' for file in sorted(dist.glob('*.tar.gz'))]
        (dist/'SHA256SUMS').write_text('\n'.join(artifacts)+'\n')
if __name__=='__main__':main()
