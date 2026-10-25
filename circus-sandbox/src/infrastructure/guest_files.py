"""Private Linux/Python 3 file protocol. Exit statuses are consumed by engine.rs."""
import errno
import os
import stat
import sys

UNSAFE, LIMIT, INVALID, PROTOCOL, IO_ERROR, NOT_FOUND, PERMISSION_DENIED = 81, 82, 83, 84, 85, 86, 87
ENTRY_LIMIT = 88
ALPHA = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"


def fail(code):
    raise SystemExit(code)


def drive_prefix(component):
    return len(component) >= 2 and component[0] in ALPHA and component[1] == ":"


def components(path, absolute=False):
    if "\\" in path or "\x00" in path or (absolute != path.startswith("/")):
        fail(INVALID)
    parts = path[1:].split("/") if absolute else path.split("/")
    if any(p == ".." or drive_prefix(p) for p in parts):
        fail(INVALID)
    return [p for p in parts if p not in ("", ".")]


def open_directory(parent, name, create=False):
    if create:
        try:
            os.mkdir(name, 0o755, dir_fd=parent)
        except FileExistsError:
            pass
    return os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                   dir_fd=parent)


def walk(fd, parts, create=False):
    try:
        for part in parts:
            child = open_directory(fd, part, create)
            os.close(fd)
            fd = child
        return fd
    except BaseException:
        os.close(fd)
        raise


def entry_name(name):
    """Checks one listed name and returns its UTF-8 bytes, or fails INVALID."""
    try:
        raw = name.encode("utf-8")
    except UnicodeEncodeError:
        # Surrogate escapes mean the guest name is not UTF-8.
        fail(INVALID)
    else:
        if not raw or name in (".", "..") or "/" in name or "\\" in name:
            fail(INVALID)
        if drive_prefix(name):
            fail(INVALID)
        return raw


def scan(root_parts, max_entries):
    """Walks the workspace without following links into manifest records.

    Each directory is opened by re-walking from `/` component by component, so
    every component is opened with O_NOFOLLOW and at most one directory
    descriptor is held at a time, however wide the tree is.
    """
    if max_entries < 0:
        fail(INVALID)
    found = []
    pending = [()]
    while pending:
        parts = pending.pop()
        fd = walk(os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC),
                  root_parts + list(parts))
        try:
            with os.scandir(fd) as entries:
                for entry in sorted(entries, key=lambda item: item.name):
                    raw = entry_name(entry.name)
                    rel = b"/".join(parts + (raw,))
                    if entry.is_symlink():
                        fail(UNSAFE)
                    if len(found) + 1 > max_entries:
                        fail(ENTRY_LIMIT)
                    if entry.is_dir(follow_symlinks=False):
                        found.append((rel, True, 0))
                        pending.append(parts + (raw,))
                    elif entry.is_file(follow_symlinks=False):
                        meta = entry.stat(follow_symlinks=False)
                        # A second link to the inode would alias two snapshot paths.
                        if meta.st_nlink != 1 or meta.st_size < 0:
                            fail(UNSAFE)
                        found.append((rel, False, meta.st_size))
                    else:
                        fail(UNSAFE)
        finally:
            os.close(fd)
    return found


def emit(found, max_bytes):
    """Writes the sorted NUL-delimited manifest, failing before it exceeds the bound."""
    found.sort(key=lambda item: item[0])
    out = bytearray()
    for rel, is_dir, size in found:
        record = (b"d\x00" if is_dir else b"f\x00") + rel + b"\x00"
        if not is_dir:
            record += str(size).encode("ascii") + b"\x00"
        if len(out) + len(record) > max_bytes:
            fail(LIMIT)
        out += record
    sys.stdout.buffer.write(out)
    sys.stdout.buffer.flush()


def main():
    args = sys.argv[1:]
    if len(args) not in (4, 5):
        fail(PROTOCOL)
    mode, workspace, relative, bound_text = args[:4]
    entries_text = args[4] if len(args) == 5 else None
    bound = int(bound_text)
    if bound < 0:
        fail(INVALID)
    root_parts = components(workspace, True)
    if mode == "list":
        # The workspace root itself is the listing root, so it is never listed.
        if relative != "" or entries_text is None:
            fail(PROTOCOL)
        emit(scan(root_parts, int(entries_text)), bound)
        return
    if entries_text is not None:
        fail(PROTOCOL)
    parts = components(relative)
    root = walk(os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC), root_parts)
    if not parts:
        os.close(root)
        if mode == "mkdir":
            return
        fail(UNSAFE)
    parent = walk(root, parts[:-1], mode == "mkdir")
    try:
        name = parts[-1]
        if mode == "mkdir":
            os.close(open_directory(parent, name, True))
        elif mode == "unlink":
            # unlinkat never follows the final entry, even during concurrent replacement.
            os.unlink(name, dir_fd=parent)
        elif mode in ("read", "write"):
            flags = os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
            flags |= os.O_RDONLY if mode == "read" else os.O_WRONLY | os.O_CREAT
            fd = os.open(name, flags, 0o644, dir_fd=parent)
            try:
                if not stat.S_ISREG(os.fstat(fd).st_mode):
                    fail(UNSAFE)
                if mode == "read":
                    remaining = bound + 1
                    while remaining:
                        chunk = os.read(fd, min(65536, remaining))
                        if not chunk:
                            break
                        sys.stdout.buffer.write(chunk)
                        remaining -= len(chunk)
                    sys.stdout.buffer.flush()
                else:
                    # No truncation until the opened inode has been verified regular.
                    os.ftruncate(fd, 0)
                    remaining = bound
                    while remaining:
                        chunk = sys.stdin.buffer.read(min(65536, remaining))
                        if not chunk:
                            fail(PROTOCOL)
                        view = memoryview(chunk)
                        while view:
                            written = os.write(fd, view)
                            view = view[written:]
                        remaining -= len(chunk)
                    # The host frames stdin with this exact validated byte count.
                    # Do not require EOF: Windows named pipes cannot half-close.
            finally:
                os.close(fd)
        else:
            fail(PROTOCOL)
    finally:
        os.close(parent)


try:
    main()
except OSError as error:
    if error.errno in (errno.ELOOP, errno.ENOTDIR, errno.EISDIR, errno.ENXIO):
        fail(UNSAFE)
    if error.errno == errno.ENOENT:
        fail(NOT_FOUND)
    if error.errno in (errno.EACCES, errno.EPERM):
        fail(PERMISSION_DENIED)
    fail(IO_ERROR)
except (ValueError, IndexError):
    fail(PROTOCOL)
