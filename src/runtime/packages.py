"""Transfer verified package downloads only, inside the runtime sandbox.

Usage: python3 -I -c SCRIPT export|import PINS_JSON [WORKSPACE_ROOT] [IMPORT_BYTES]
No package metadata, installed code, credentials, or build outputs are retained.
"""
import base64
import hashlib
import io
import json
import os
import re
import secrets
import stat
import sys
import tarfile
import zipfile

LIMIT = 512 * 1024 * 1024
FILE_LIMIT = 128 * 1024 * 1024
COUNT_LIMIT = 10000
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
NPM = ('.crow-home', '.npm', '_cacache', 'content-v2', 'sha512')
CARGO = ('.crow-home', '.cargo', 'registry', 'cache')
GO = ('.crow-home', 'go', 'pkg', 'mod', 'cache', 'download')


def digest_h1(data, is_zip):
    digest = hashlib.sha256()
    if not is_zip:
        digest.update((hashlib.sha256(data).hexdigest() + '  go.mod\n').encode())
    else:
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            entries = archive.infolist()
            if (len(entries) > COUNT_LIMIT or sum(entry.file_size for entry in entries) > LIMIT
                    or len({entry.filename for entry in entries}) != len(entries)):
                raise ValueError('Invalid or oversized module zip')
            # Go dirhash.HashZip hashes every ZIP entry, including empty directory
            # entries. Omitting directories would accept bytes Go rejects for a pin.
            for entry in sorted(entries, key=lambda entry: entry.filename):
                if '\n' in entry.filename:
                    raise ValueError('Invalid module zip filename')
                content_hash = hashlib.sha256()
                if entry.is_dir():
                    # archive/zip.File.Open rejects directories with nonzero data.
                    if entry.file_size != 0:
                        raise ValueError('Invalid module zip directory')
                else:
                    with archive.open(entry) as source:
                        while chunk := source.read(65536):
                            content_hash.update(chunk)
                digest.update((content_hash.hexdigest() + '  ' + entry.filename + '\n').encode())
    return 'h1:' + base64.b64encode(digest.digest()).decode()


def permitted(parts, pins):
    if parts[:len(NPM)] == NPM and pins.get('npm') is True:
        suffix = parts[len(NPM):]
        if (len(suffix) == 3 and [len(part) for part in suffix] == [2, 2, 124]
                and re.fullmatch('[0-9a-f]{128}', ''.join(suffix))):
            return ('npm', ''.join(suffix))
    if parts[:len(CARGO)] == CARGO:
        suffix = parts[len(CARGO):]
        if len(suffix) == 2 and suffix[1] in pins.get('cargo', {}):
            return ('cargo', pins['cargo'][suffix[1]])
    if parts[:len(GO)] == GO:
        name = '/'.join(parts[len(GO):])
        if name in pins.get('go', {}) and name.endswith(('.zip', '.mod')):
            return ('gozip' if name.endswith('.zip') else 'gomod', pins['go'][name])
    return None


def verified(data, specification):
    kind, expected = specification
    if kind == 'npm':
        return hashlib.sha512(data).hexdigest() == expected
    if kind == 'cargo':
        return hashlib.sha256(data).hexdigest() in expected
    return digest_h1(data, kind == 'gozip') == expected


def parent_fd(root, parts, create=False):
    current = os.dup(root)
    try:
        for part in parts:
            if create:
                try:
                    os.mkdir(part, mode=0o700, dir_fd=current)
                except FileExistsError:
                    pass
            child = os.open(part, DIR_FLAGS, dir_fd=current)
            os.close(current)
            current = child
        return current
    except BaseException:
        os.close(current)
        raise


def walk(directory, prefix, counter):
    for name in sorted(os.listdir(directory)):
        counter[0] += 1
        if counter[0] > COUNT_LIMIT:
            raise ValueError('Too many package cache entries')
        entry = os.stat(name, dir_fd=directory, follow_symlinks=False)
        if stat.S_ISDIR(entry.st_mode):
            child = os.open(name, DIR_FLAGS, dir_fd=directory)
            try:
                yield from walk(child, prefix + (name,), counter)
            finally:
                os.close(child)
        elif stat.S_ISREG(entry.st_mode) and entry.st_nlink == 1:
            yield directory, name, prefix + (name,)


def export_cache(root, pins):
    total = 0
    count = [0]
    with tarfile.open(fileobj=sys.stdout.buffer, mode='w|') as archive:
        for prefix in (NPM, CARGO, GO):
            try:
                directory = parent_fd(root, prefix)
            except OSError:
                continue
            try:
                for directory_fd, name, parts in walk(directory, prefix, count):
                    specification = permitted(parts, pins)
                    if specification is None:
                        continue
                    try:
                        fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                                     dir_fd=directory_fd)
                        with os.fdopen(fd, 'rb') as source:
                            entry = os.fstat(source.fileno())
                            if (not stat.S_ISREG(entry.st_mode) or entry.st_nlink != 1
                                    or entry.st_size > FILE_LIMIT):
                                continue
                            data = source.read(FILE_LIMIT + 1)
                        if len(data) > FILE_LIMIT or not verified(data, specification):
                            continue
                    except (OSError, ValueError, zipfile.BadZipFile):
                        continue
                    if total + len(data) > LIMIT:
                        continue
                    total += len(data)
                    member = tarfile.TarInfo('/'.join(parts))
                    member.size = len(data)
                    member.mode = 0o600
                    archive.addfile(member, io.BytesIO(data))
            finally:
                os.close(directory)


def import_cache(root, pins, budget=128 * 1024 * 1024):
    if budget < 0 or budget > LIMIT:
        raise ValueError('Invalid package cache import budget')
    total = 0
    imported_bytes = 0
    imported_files = 0
    skipped = 0
    count = 0
    with tarfile.open(fileobj=sys.stdin.buffer, mode='r|') as archive:
        for member in archive:
            count += 1
            total += member.size
            parts = tuple(member.name.split('/'))
            if (count > COUNT_LIMIT or member.size < 0 or member.size > FILE_LIMIT
                    or total > LIMIT or not member.isfile()
                    or any(part in ('', '.', '..') for part in parts)):
                raise ValueError('Invalid package cache archive entry')
            specification = permitted(parts, pins)
            if specification is None:
                raise ValueError('Unexpected package cache path')
            with archive.extractfile(member) as source:
                data = source.read(FILE_LIMIT + 1)
            if len(data) != member.size or not verified(data, specification):
                raise ValueError('Package cache integrity mismatch')
            if imported_bytes + len(data) > budget:
                skipped += 1
                continue
            directory = parent_fd(root, parts[:-1], create=True)
            temporary = '.crow-cache-' + secrets.token_hex(16)
            try:
                fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                             0o600, dir_fd=directory)
                with os.fdopen(fd, 'wb') as destination:
                    destination.write(data)
                # Atomic replacement does not follow an existing symlink or hardlink.
                os.replace(temporary, parts[-1], src_dir_fd=directory, dst_dir_fd=directory)
                imported_bytes += len(data)
                imported_files += 1
            finally:
                try:
                    os.unlink(temporary, dir_fd=directory)
                except FileNotFoundError:
                    pass
                os.close(directory)

    return {'files': imported_files, 'bytes': imported_bytes, 'skipped': skipped}


if __name__ == '__main__':
    operation, encoded_pins = sys.argv[1:3]
    pins = json.loads(encoded_pins)
    root = os.open(sys.argv[3] if len(sys.argv) > 3 else '/workspace', DIR_FLAGS)
    try:
        if operation == 'export':
            export_cache(root, pins)
        elif operation == 'import':
            print(json.dumps(import_cache(root, pins, int(sys.argv[4]) if len(sys.argv) > 4 else 128 * 1024 * 1024)))
        else:
            raise ValueError('Unknown package cache operation')
    finally:
        os.close(root)
