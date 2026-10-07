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
with open(os.path.join(state, "homes"), "a") as log:
    log.write(os.environ.get("HOME", "") + "\n")


def workspace(name):
    return os.path.join(state, "containers", name)


def extract(target, strip=False):
    with tarfile.open(fileobj=sys.stdin.buffer, mode="r|") as archive:
        for member in archive:
            if strip:  # --strip-components=1 skips the top entry itself
                if "/" not in member.name:
                    continue
                member.name = member.name.split("/", 1)[1]
            archive.extract(member, target, filter="data")


command = args[0]
if command == "info":  # the default seccomp profile Crow derives its own from
    profile = os.path.join(state, "seccomp.json")
    with open(profile, "w") as out:
        json.dump({"defaultAction": "SCMP_ACT_ERRNO", "syscalls": [
            {"names": ["read", "clone", "clone3", "unshare"], "action": "SCMP_ACT_ALLOW"}]}, out)
    print(json.dumps({"host": {"security": {"rootless": True, "seccompProfilePath": profile}}}))
    sys.exit(0)
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
            archive.add(target, arcname="workspace")
        sys.exit(0)
    script = program[2]
    if "tar -xf -" in script:  # sandbox checks and unpack
        extract(target, strip="--strip-components=1" in script)
        sys.exit(0)
    script = script.split("# Crow setup command\n", 1)[-1]
    sys.exit(subprocess.run(["/bin/sh", "-c", script], cwd=target).returncode)
sys.exit(f"fake podman: unsupported {args}")
