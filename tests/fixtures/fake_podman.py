#!/usr/bin/env python3
"""Fake rootless Podman for runtime experiment tests. Containers are directories
under $FAKE_PODMAN_STATE; commands run on the host in that directory."""
import json
import os
import subprocess
import sys
import tarfile

state = os.environ["FAKE_PODMAN_STATE"]
args = sys.argv[1:]
with open(os.path.join(state, "calls.jsonl"), "a") as log:
    log.write(json.dumps(args) + "\n")


def workspace(name):
    return os.path.join(state, "containers", name)


def extract(target):
    with tarfile.open(fileobj=sys.stdin.buffer, mode="r|") as archive:
        archive.extractall(target, filter="data")


command = args[0]
if command == "image" and args[1] == "exists":
    sys.exit(0 if os.path.exists(os.path.join(state, "image")) else 1)
if command == "run":
    name = next(a[len("--name="):] for a in args if a.startswith("--name="))
    os.makedirs(workspace(name))
    print(name)
    sys.exit(0)
if command in ("rm", "ps"):
    sys.exit(0)
if command == "exec":
    rest = [a for a in args[1:] if a != "--interactive"]
    target, program = workspace(rest[0]), rest[1:]
    if program[0] == "python3":  # restore.py: pinned source over the workspace
        extract(target)
        sys.exit(0)
    if program[0] == "tar":  # snapshot export
        with tarfile.open(fileobj=sys.stdout.buffer, mode="w|") as archive:
            archive.add(target, arcname=".")
        sys.exit(0)
    script = program[2]
    if "tar -xf -" in script:  # limits check and unpack
        extract(target)
        sys.exit(0)
    script = script.split("# Crow setup command\n", 1)[-1]
    sys.exit(subprocess.run(["/bin/sh", "-c", script], cwd=target).returncode)
sys.exit(f"fake podman: unsupported {args}")
