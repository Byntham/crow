"""Restore pinned Git source over prepared dependencies, inside the sandbox only."""
import os
from pathlib import Path, PurePosixPath
import shutil
import sys
import tarfile

root = Path('/workspace')
directories = []

def remove(path):
    if path.is_symlink() or not path.is_dir():
        path.unlink(missing_ok=True)
    else:
        shutil.rmtree(path)

def parents(parts):
    path = root
    for part in parts:
        path = path / part
        if path.is_symlink() or (path.exists() and not path.is_dir()):
            remove(path)
        path.mkdir(exist_ok=True)
    return path

with tarfile.open(fileobj=sys.stdin.buffer, mode='r|') as archive:
    for member in archive:
        relative = PurePosixPath(member.name)
        if relative.is_absolute() or '..' in relative.parts or not relative.parts:
            raise ValueError('Invalid pinned source path')
        parent = parents(relative.parts[:-1])
        path = parent / relative.name
        if member.isdir():
            parents(relative.parts)
            directories.append((path, member.mode & 0o777))
        elif member.isfile():
            # Unlink first so a setup hook cannot keep two tracked files hardlinked.
            remove(path)
            with archive.extractfile(member) as source, path.open('wb') as target:
                shutil.copyfileobj(source, target)
            path.chmod(member.mode & 0o777)
        elif member.issym():
            remove(path)
            path.symlink_to(member.linkname)
        else:
            raise ValueError('Unsupported pinned source entry')
for path, mode in reversed(directories):
    path.chmod(mode)
