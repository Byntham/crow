"""Replay prepared comparisons through Crow locally, without posting to GitHub."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import hashlib
from pathlib import Path
import shutil
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('output', type=Path)
parser.add_argument('--podman', required=True, type=Path)
parser.add_argument('--reviewer', type=Path, default=Path('target/debug/examples/local_review'))
parser.add_argument('--case', action='append', default=[])
parser.add_argument('--attempt', default='baseline')
parser.add_argument('--resume-from', help='Copy an incomplete attempt and resume its saved session')
parser.add_argument('--jobs', type=int, default=2)
args = parser.parse_args()
if not args.attempt.replace('-', '').replace('_', '').isalnum():
    parser.error('attempt must be a simple directory name')
if args.resume_from and not args.resume_from.replace('-', '').replace('_', '').isalnum():
    parser.error('resume-from must be a simple directory name')
cases = json.loads(Path(__file__).with_name('cases.json').read_text())
# MCP helpers launch the current executable. Pin it so rebuilding Crow while
# evaluating cannot change the implementation halfway through a review.
with args.reviewer.open('rb') as binary:
    digest = hashlib.file_digest(binary, 'sha256').hexdigest()
reviewer = args.output.resolve() / f'reviewer-{digest}'
if not reviewer.exists():
    shutil.copy2(args.reviewer, reviewer)


def review(case):
    directory = args.output.resolve() / case['id']
    state = directory / args.attempt
    # Never overwrite an earlier attempt or its evidence.
    if state.exists():
        print(f'{case["id"]}: already has {args.attempt}, skipped', flush=True)
        return
    if args.resume_from:
        previous = directory / args.resume_from
        if (previous / 'result.json').exists():
            raise ValueError(f'{case["id"]}: previous review is already complete')
        if not (previous / 'reviews' / case['id'] / 'session.json').exists():
            raise ValueError(f'{case["id"]}: no saved session to resume')
        shutil.copytree(previous, state)
        for name in ['error.txt', 'replay.json']:
            (state / name).unlink(missing_ok=True)
    else:
        state.mkdir()
    started = time.time()
    with (directory / f'{args.attempt}.log').open('x') as log:
        result = subprocess.run([
            str(reviewer), str(directory / 'source.json'),
            str(state), 'auto', str(args.podman.resolve()),
        ], stdout=log, stderr=subprocess.STDOUT)
    receipt = {'exitCode': result.returncode, 'elapsedSeconds': round(time.time() - started, 2),
               'reviewerSha256': digest, 'resumedFrom': args.resume_from}
    (state / 'replay.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(f'{case["id"]}: {receipt}', flush=True)


with ThreadPoolExecutor(max_workers=args.jobs) as pool:
    list(pool.map(review, [case for case in cases if not args.case or case['id'] in args.case]))
