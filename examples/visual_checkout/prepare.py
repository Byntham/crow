from pathlib import Path
import argparse, json, shutil, struct, subprocess, zlib
parser = argparse.ArgumentParser(description='Create a two-commit visual regression fixture.')
parser.add_argument('output', type=Path, help='A new directory for generated files')
parser.add_argument('--autonomous', action='store_true', help='Use ordinary npm setup with no Crow guidance')
parser.add_argument('--clean', action='store_true', help='Change decoration without hiding the label')
args = parser.parse_args()
root = args.output.resolve()
root.mkdir(parents=True, exist_ok=False)
fixture = root / 'repository'
fixture.mkdir()
template = Path(__file__).resolve().parents[2] / 'tests/fixtures/visual_checkout'
shutil.copytree(template, fixture, dirs_exist_ok=True)
if args.autonomous:
    shutil.rmtree(fixture / '.crow')
    (fixture / 'package.json').write_text(json.dumps({'name':'fieldwork-checkout','version':'1.0.0','private':True,'type':'module','scripts':{'start':'node server.mjs','test':'node smoke.mjs'},'dependencies':{'mime-types':'2.1.35'}}, indent=2))
    server = (fixture / 'server.mjs').read_text()
    (fixture / 'server.mjs').write_text("import mime from 'mime-types';\n" + server.replace("file[1]", "mime.lookup(file[0]) || file[1]"))
    (fixture / 'README.md').write_text('# Fieldwork checkout\n\nA local demo checkout. No real payments.\n\nInstall with `npm install`, start with `npm start` on port 8080, and run the browser smoke test with `npm test`.\n')
def png(broken, clean_change=False):
    w,h=600,90
    rows=[]
    for y in range(h):
        row=bytearray()
        for x in range(w):
            alpha=255 if broken and 14 <= y <= 76 else ((20 if clean_change else 10) if x > 430+y else 0)
            row.extend((41,61,49,alpha))
        rows.append(b'\0'+bytes(row))
    def chunk(t,d): return struct.pack('>I',len(d))+t+d+struct.pack('>I',zlib.crc32(t+d)&0xffffffff)
    return b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',w,h,8,6,0,0,0))+chunk(b'IDAT',zlib.compress(b''.join(rows)))+chunk(b'IEND',b'')
def git(*args):
    return subprocess.check_output(['git','-C',str(fixture),'-c','user.name=Visual test','-c','user.email=visual-test@example.invalid',*args],text=True).strip()
git('init','-b','main')
(fixture/'button-finish.png').write_bytes(png(False))
git('add','.');git('commit','-m','Checkout page and browser smoke test')
base=git('rev-parse','HEAD')
(fixture/'button-finish.png').write_bytes(png(not args.clean, args.clean))
git('add','.');git('commit','-m','Refresh the checkout button finish asset')
head=git('rev-parse','HEAD')
source={'dir':str(fixture),'head':head,'base':base,'target':'main','targetSha':base}
(root/'source.json').write_text(json.dumps(source,indent=2))
for label,rev in [('base',base),('head',head)]:
    dest=root/label;dest.mkdir(exist_ok=True)
    data=subprocess.check_output(['git','-C',str(fixture),'archive','--format=tar',rev])
    (root/(label+'.tar')).write_bytes(data)
    subprocess.run(['tar','-xf','-','-C',str(dest)],input=data,check=True)
print(json.dumps(source,indent=2))
print(git('diff','--stat',base,head))
