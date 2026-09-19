#!/usr/bin/python3
"""Deterministic blocked Git read for MCP cancellation tests."""
import os
from pathlib import Path
import sys
import time

assert 'ls-tree' in sys.argv, sys.argv
Path(__file__).parent.joinpath('git-started').write_text(str(os.getpid()))
time.sleep(60)
raise AssertionError('Discovery Git read was not cancelled')
