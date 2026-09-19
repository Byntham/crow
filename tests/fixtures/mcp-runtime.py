#!/usr/bin/python3
"""Infrastructure-only MCP cancellation fixture; never runs reviewed commands."""
import json
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
