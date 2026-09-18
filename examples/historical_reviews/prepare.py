"""Fetch immutable historical PR comparisons without running repository code."""
import argparse
import json
import os
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('output', type=Path)
parser.add_argument('--case', action='append', default=[], help='Prepare only these case IDs')
args = parser.parse_args()
root = args.output.resolve()
root.mkdir(parents=True, exist_ok=True)
cases = json.loads(Path(__file__).with_name('cases.json').read_text())
env = dict(os.environ, GIT_TERMINAL_PROMPT='0', GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_NOSYSTEM='1')

def run(argv, **kwargs):
    return subprocess.check_output(argv, env=env, text=True, **kwargs).strip()

for case in cases:
    if args.case and case['id'] not in args.case:
        continue
    directory = root / case['id']
    directory.mkdir(exist_ok=False)
    repo, number = case['repo'], case['number']
    # Keep the full metadata for evaluation, but supply only title/body to Crow.
    pr = json.loads(run(['gh', 'api', f'repos/{repo}/pulls/{number}']))
    if pr['base']['sha'] != case['baseTip'] or pr['head']['sha'] != case['head']:
        raise RuntimeError(f'{case["id"]}: GitHub comparison changed since this evaluation was pinned')
    files = json.loads(run(['gh', 'api', f'repos/{repo}/pulls/{number}/files?per_page=100']))
    (directory / 'pr.json').write_text(json.dumps(pr, indent=2) + '\n')
    (directory / 'files.json').write_text(json.dumps(files, indent=2) + '\n')
    bare = root / 'repositories' / (repo.replace('/', '--') + '.git')
    bare.parent.mkdir(exist_ok=True)
    if not bare.exists():
        run(['git', 'init', '--bare', str(bare)])
        run(['git', '-C', str(bare), 'remote', 'add', 'origin', f'https://github.com/{repo}.git'])
    base_tip, head = pr['base']['sha'], pr['head']['sha']
    run(['git', '-C', str(bare), 'fetch', '--no-tags', '--depth=200', 'origin', base_tip, head])
    try:
        base = run(['git', '-C', str(bare), 'merge-base', base_tip, head])
    except subprocess.CalledProcessError:
        run(['git', '-C', str(bare), 'fetch', '--no-tags', '--deepen=500', 'origin', base_tip, head])
        base = run(['git', '-C', str(bare), 'merge-base', base_tip, head])
    actual = set(run(['git', '-C', str(bare), 'diff', '--name-only', base, head]).splitlines())
    expected = {f['filename'] for f in files}
    if actual != expected:
        raise RuntimeError(f'{case["id"]}: local PR paths differ from GitHub: {actual ^ expected}')
    context = {'title': pr['title'], 'body': pr['body'] or ''}
    if case.get('reverse'):
        base, head = head, base
        context = {'title': 'Adjust byte-range suffix handling', 'body': 'Review the change to HTTP Range parsing.'}
    source = {'dir': str(bare), 'base': base, 'head': head, 'target': pr['base']['ref'], 'targetSha': base,
              'repo': repo, 'number': number, 'jobId': case['id'], 'prContext': context}
    (directory / 'source.json').write_text(json.dumps(source, indent=2) + '\n')
    (directory / 'case.json').write_text(json.dumps(dict(case, url=pr['html_url'], base=base, head=head), indent=2) + '\n')
    print(f'{case["id"]}: {base[:12]} -> {head[:12]}, {len(actual)} files', flush=True)
