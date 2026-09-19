#!/usr/bin/python3
"""Infrastructure-only MCP cancellation fixture; never runs reviewed commands."""
import json
import os
from pathlib import Path
import sys
import time

root = Path(__file__).parent
args = sys.argv[1:]
if args[0] == 'info':
    print(json.dumps({'host': {'security': {'rootless': True, 'seccompEnabled': True},
                             'serviceIsRemote': False, 'cgroupVersion': 'v2'}}))
elif args[0] == 'run':
    (root / 'created').write_text('container created')
elif args[0] == 'rm':
    (root / 'removing-pid').write_text(str(os.getpid()))
    (root / 'removing').write_text('container cleanup started')
    while (root / 'hold-remove').exists():
        time.sleep(0.02)
    if (root / 'fail-remove').exists():
        print('fixture removal failed', file=sys.stderr)
        sys.exit(125)
    with (root / 'removed-names').open('a') as removed:
        removed.write(args[-1] + '\n')
    (root / 'removed').write_text('container removed')
elif args[0] == 'exec' and args[1] == '--interactive':
    sys.stdin.buffer.read()
elif args[0] == 'exec' and args[2:4] == ['/bin/sh', '-c']:
    command = args[4]
    if command.startswith('read memory '):
        sys.exit(0)
    assert command == 'fixture-wait', args
    (root / 'active').write_text('command started')
    time.sleep(60)
else:
    raise AssertionError(args)
