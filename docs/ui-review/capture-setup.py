#!/usr/bin/env python3
"""Capture setup PTY prompts, installer output, and a chained error.

Run from the repository root:
  python3 docs/ui-review/capture-setup.py BEFORE_BINARY AFTER_BINARY [OUTPUT_DIR]

Requires pexpect and Pillow. Compile tests/fixtures/fake_codex.rs as
/tmp/crow-capture-tools/codex first, as described in scripts/capture-cli.py.
Every command uses a temporary HOME and CROW_HOME with fake GitHub/Codex tools.
Setup captures stop at EOF before registration or starting services.
"""
import importlib.util, io, json, os, re, subprocess, sys, tempfile
from pathlib import Path
import pexpect
root=Path.cwd()
spec=importlib.util.spec_from_file_location('captures',root/'scripts/capture-cli.py')
c=importlib.util.module_from_spec(spec); spec.loader.exec_module(c)
before, after = sys.argv[1:3]
out=Path(sys.argv[3]).resolve() if len(sys.argv) > 3 else root/'docs/ui-review'
out.mkdir(parents=True, exist_ok=True)
def normalized(text, tmp):
    text=text.replace('\r\n','\n').replace(tmp,'/home/alice')
    return re.sub(r'\x1b\[[0-9;]*[mK]','',text)
def save(name, side, cmd, output):
    (out/f'{name}-{side}.txt').write_text('$ '+cmd+'\n\n'+output)
    c.picture(out/f'{name}-{side}.png',side.title(),cmd,output)
for side,binary in [('before',str(Path(before).resolve())),('after',str(Path(after).resolve()))]:
  for name in ['setup-choices','setup-models','install','error-context']:
    with tempfile.TemporaryDirectory(prefix='crow-setup-ui-') as tmp:
      home=Path(tmp); state=home/'.local/share/crow'; state.mkdir(parents=True)
      bins=home/'fixtures'; bins.mkdir()
      for program,body in {'gh': 'case "$*" in *token*) echo fixture-github-token;; *"api user"*) echo \'{"login":"alice"}\';; *) echo gh;; esac', 'git':'echo git version 2.0', 'systemctl':'echo "Stopped before starting a service in the screenshot fixture." >&2; exit 1'}.items():
        f=bins/program; f.write_text('#!/bin/sh\n'+body+'\n'); f.chmod(0o755)
      (bins/'codex').symlink_to('/tmp/crow-capture-tools/codex')
      env=dict(os.environ,HOME=tmp,CROW_HOME=str(state),CROW_BIN_DIR=str(home/'.local/bin'),PATH=str(bins)+':'+os.environ['PATH'],NO_COLOR='1',TERM='dumb',COLUMNS='100')
      cfg={'version':1,'role':'both','operator':None,'publicUrl':None,'port':8787,'bind':'127.0.0.1','adminToken':'fixture-admin-token-'*3,'serviceUrl':'http://127.0.0.1:8787','worker':dict(c.SETTINGS,id='review-worker',token='fixture-worker-token-'*3,concurrency=3,codex='/tmp/crow-capture-tools/codex',codexHome=str(state/'codex')),'catchUp':{'enabled':True,'threshold':10},'auditIntervalMs':3600000,'retentionDays':7,'ingress':{'type':'funnel'},'app':None}
      (state/'config.json').write_text(json.dumps(cfg))
      if name=='install':
        run=subprocess.run([binary,'install','--no-setup'],env=env,capture_output=True,text=True,timeout=30)
        assert run.returncode==0,run.stderr
        save(name,side,'crow install --no-setup',normalized(run.stdout+run.stderr,tmp)); continue
      if name=='error-context':
        (state/'config.json').write_text('{')
        run=subprocess.run([binary,'status'],env=env,capture_output=True,text=True,timeout=30)
        save(name,side,'crow status',normalized(run.stdout+run.stderr,tmp)); continue
      log=io.StringIO()
      child=pexpect.spawn(binary,['setup'],env=env,encoding='utf-8',timeout=30,dimensions=(50,100))
      child.logfile_read=log
      try:
        child.expect(r'\[both\]: '); child.sendline('both' if name=='setup-choices' else 'worker')
        if name=='setup-choices':
          child.expect(r'\[funnel\]: '); child.sendline('existing')
          child.expect(r'Public HTTPS[^\r\n]*: '); child.sendline('https://crow.example.com')
          child.expect(r'\[personal\]: '); child.sendline('personal')
          child.expect(r'\[public\]: '); child.sendline('public')
          child.expect(r'GitHub App name[^\r\n]*: ')
        else:
          child.expect(r'Connection-service HTTPS URL: '); child.sendline('https://crow.example.com')
          child.expect(r'Worker ID from crow pair[^\r\n]*: '); child.sendline('review-worker')
          child.expect(r'Worker token from crow pair: '); child.sendline('fixture-pairing-token-with-32-characters')
          child.expect(r'\[provider-default\]: '); child.sendline('provider-default')
          child.expect(r'\[medium\]: ')
        child.sendeof(); child.expect(pexpect.EOF)
      except Exception:
        print(log.getvalue()); child.close(force=True); raise
      save(name,side,'crow setup',normalized(log.getvalue(),tmp))
      print(side,name,child.exitstatus)
