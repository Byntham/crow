#!/usr/bin/env python3
"""Extract the setup browser pages from the original and current source.

Run from the repository root:
  python3 docs/ui-review/capture-browser-fixtures.py BEFORE_SETUP_RS OUTPUT_DIR

Creates OUTPUT_DIR/before and OUTPUT_DIR/after HTML. BEFORE_SETUP_RS is the
original src/setup.rs, available from git show BASE_COMMIT:src/setup.rs.
The two original error pages were text/plain; their HTML fixtures use the
browser's default preformatted text styling. Original text is also saved.
No external account or service is contacted.
"""
import re, json, html, sys
from pathlib import Path
root=Path.cwd()
destination=Path(sys.argv[2]).resolve()
before=destination/'before'; before.mkdir(parents=True,exist_ok=True)
after=destination/'after'; after.mkdir(parents=True,exist_ok=True)
lit=r'"(?:[^"\\]|\\.)*"'
source=(root/'src/setup.rs').read_text()
old=Path(sys.argv[1]).read_text()
def decode(s): return json.loads(s)
action='https://github.com/settings/apps/new?state=example'
manifest=html.escape(json.dumps({'name':'Crow example'}), quote=True)
install='https://github.com/apps/crow-example/installations/new'
def format_html(body, args):
    for a in args: body=body.replace('{}', a, 1)
    return body
original_pages=[decode(m) for m in re.findall(r'"<!doctype html>(?:[^"\\]|\\.)*"',old)]
assert len(original_pages) == 2, len(original_pages)
for name,body,args in zip(['connect','success'], original_pages, [[action,manifest],[install]]):
    (before/f'{name}.html').write_text(format_html(body,args))
for name,text in [('invalid-link','Invalid or already used setup callback. Return to your terminal.'),('failed','Setup failed. See the Crow terminal for details.')]:
    (before/f'{name}.html').write_text('<!doctype html><html><body><pre style="white-space: pre-wrap; word-wrap: break-word;">'+html.escape(text)+'</pre></body></html>')
    (before/f'{name}.txt').write_text(text)
template=(root/'src/setup_page.html').read_text()
expr=r'setup_page\(\s*('+lit+r'),\s*('+lit+r'),\s*(?:&format!\(\s*)?('+lit+r')'
pages=re.findall(expr, source)
assert len(pages)==4, len(pages)
for name,page,args in zip(['connect','invalid-link','success','failed'], pages, [[action,manifest],[],[install],[]]):
    title,step,body=map(decode,page)
    document=template.replace('__TITLE__',html.escape(title,quote=True)).replace('__STEP__',html.escape(step,quote=True)).replace('__BODY__',format_html(body,args))
    (after/f'{name}.html').write_text(document)
print(f'Created 4 before/after browser page pairs from source in {destination}')
