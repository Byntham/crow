"""Replay prepared comparisons through Crow locally, without posting to GitHub."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import hashlib
from pathlib import Path
import shutil
import signal
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
if args.jobs < 1:
    parser.error('jobs must be positive')
if not args.attempt.replace('-', '').replace('_', '').isalnum():
    parser.error('attempt must be a simple directory name')
if args.resume_from and not args.resume_from.replace('-', '').replace('_', '').isalnum():
    parser.error('resume-from must be a simple directory name')
cases = json.loads(Path(__file__).with_name('cases.json').read_text())
interruption = None


def interrupt(signum, _frame):
    # Signal handlers run on the main thread. Do not take locks or wait here:
    # review threads forward cancellation and await their children's cleanup.
    global interruption
    if interruption is None:
        interruption = signum


signal.signal(signal.SIGTERM, interrupt)
signal.signal(signal.SIGINT, interrupt)
# MCP helpers launch the current executable. Pin it so rebuilding Crow while
# evaluating cannot change the implementation halfway through a review.
with args.reviewer.open('rb') as binary:
    digest = hashlib.file_digest(binary, 'sha256').hexdigest()
reviewer = args.output.resolve() / f'reviewer-{digest}'
if not reviewer.exists():
    shutil.copy2(args.reviewer, reviewer)


def review(case):
    if interruption is not None:
        return
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
    exit_code = None
    interrupted = None
    try:
        with (directory / f'{args.attempt}.log').open('x') as log:
            if interruption is not None:
                interrupted = interruption
            else:
                child = subprocess.Popen([
                    str(reviewer), str(directory / 'source.json'),
                    str(state), 'auto', str(args.podman.resolve()),
                ], stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                while True:
                    if interruption is not None and interrupted is None:
                        interrupted = interruption
                        try:
                            child.send_signal(interrupted)
                        except ProcessLookupError:
                            pass
                    try:
                        exit_code = child.wait(timeout=0.1)
                        break
                    except subprocess.TimeoutExpired:
                        pass
    except OSError as error:
        (state / 'error.txt').write_text(f'Replay could not launch the reviewer: {error}\n')
    receipt = {'exitCode': exit_code, 'elapsedSeconds': round(time.time() - started, 2),
               'reviewerSha256': digest, 'resumedFrom': args.resume_from,
               'interrupted': interrupted is not None,
               'signal': signal.Signals(interrupted).name if interrupted else None}
    (state / 'replay.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(f'{case["id"]}: {receipt}', flush=True)


with ThreadPoolExecutor(max_workers=args.jobs) as pool:
    list(pool.map(review, [case for case in cases if not args.case or case['id'] in args.case]))
if interruption is not None:
    raise SystemExit(128 + interruption)
