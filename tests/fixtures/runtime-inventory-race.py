#!/usr/bin/env python3
"""Attempt a later review's execution while cleanup removes an earlier container."""
import fcntl
import json
import os
from pathlib import Path
import sys

root = Path(os.environ['MOCK_DIR'])
a = 'a' * 32
b = 'b' * 32
if sys.argv[1] == 'ps':
    print(json.dumps([{'Names': [f'crow-experiment-{a}'], 'State': 'exited'}]))
elif sys.argv[1:] == ['rm', '--ignore', f'crow-experiment-{a}']:
    experiments = root / 'reviews/b/experiments'
    with (experiments / 'execution.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            (root / 'race').write_text('blocked')
        else:
            (root / 'race').write_text('acquired')
            (root / 'late-container').touch()
            (experiments / f'{b}.json').write_text(json.dumps({
                'id': b, 'status': 'passed', 'containerStarted': True,
                'cleanupError': 'Removal failed after the inventory was taken',
            }))
else:
    sys.exit(7)
