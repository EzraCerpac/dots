"""Validate paths before handing a P-07 fixture file or process to a tool."""

import os
import stat
import sys


def open_inside(base, path, *, required=False, directory=False):
    if not os.path.isabs(path) or ".." in path.split("/"):
        raise ValueError("path is not absolute and confined")
    if os.path.commonpath([base, os.path.normpath(path)]) != base:
        raise ValueError("path is outside World")
    relative = os.path.relpath(path, base)
    parts = [] if relative == "." else relative.split("/")
    protected = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC
    fd = os.open(base, protected | os.O_DIRECTORY)
    try:
        owner = os.fstat(fd).st_uid
        for index, part in enumerate(parts):
            flags = protected
            if index < len(parts) - 1 or directory:
                flags |= os.O_DIRECTORY
            else:
                flags |= os.O_NONBLOCK
            try:
                child = os.open(part, flags, dir_fd=fd)
            except FileNotFoundError:
                if required:
                    raise
                return None
            os.close(fd)
            fd = child
            if os.fstat(fd).st_uid != owner:
                raise ValueError("path has another owner")
        result, fd = fd, None
        return result
    finally:
        if fd is not None:
            os.close(fd)


def check_paths(base, args):
    check_directory(base, os.getcwd())
    for index, arg in enumerate(args):
        if arg in ("-C", "--cd"):
            check_directory(base, args[index + 1])
        elif arg.startswith("--cd="):
            check_directory(base, arg.split("=", 1)[1])
        elif arg.startswith("-C") and len(arg) > 2:
            check_directory(base, arg[2:])

    required = (
        "HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
        "XDG_CACHE_HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR", "TMPDIR",
        "MISE_CONFIG_DIR", "MISE_DATA_DIR", "MISE_STATE_DIR",
        "MISE_CACHE_DIR", "MISE_TMP_DIR", "MISE_SYSTEM_DIR",
        "MISE_SYSTEM_CONFIG_DIR", "MISE_SYSTEM_CONFIG_FILE",
        "MISE_SYSTEM_DATA_DIR", "MISE_SYSTEM_INSTALLS_DIR", "MISE_SYSTEM_SHIMS_DIR",
        "MISE_INSTALLS_DIR", "MISE_INSTALL_STORE_DIR", "MISE_DOWNLOADS_DIR",
        "MISE_PLUGINS_DIR", "MISE_SHIMS_DIR",
    )
    optional = (
        "DOTS_ROOT", "XDG_RUNTIME_DIR", "MISE_CONFIG_FILE", "MISE_GLOBAL_CONFIG_FILE",
        "MISE_GLOBAL_CONFIG_ROOT", "MISE_SKILLS_DIR", "BASH_ENV", "ENV",
        "MISE_LOCAL_CONFIG_FILE", "MISE_LOG_FILE", "MISE_SOPS_AGE_KEY_FILE",
        "SOPS_AGE_KEY_FILE", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM", "GIT_TEMPLATE_DIR",
        "GH_CONFIG_DIR",
    )
    lists = (
        "MISE_CEILING_PATHS", "MISE_TRUSTED_CONFIG_PATHS",
        "MISE_SHARED_INSTALL_DIRS", "MISE_SYSTEM_PLUGINS_DIRS",
        "XDG_CONFIG_DIRS", "XDG_DATA_DIRS", "GIT_CEILING_DIRECTORIES",
    )
    paths = {key: [os.environ[key]] for key in required}
    paths.update((key, [os.environ[key]]) for key in optional if os.environ.get(key))
    paths.update((key, value.split(os.pathsep))
                 for key in lists if (value := os.environ.get(key)))
    for key, values in paths.items():
        for value in values:
            try:
                fd = open_inside(base, value)
                if fd is not None:
                    os.close(fd)
            except (OSError, ValueError) as error:
                raise ValueError(f"{key} points outside approved fixture paths") from error


def check_directory(base, path):
    os.close(open_inside(base, path, required=True, directory=True))


def main():
    base, mode, *args = sys.argv[1:]
    if mode == "read":
        path, = args
        fd = open_inside(base, os.path.join(os.getcwd(), path), required=True)
        with os.fdopen(fd, "rb") as source:
            info = os.fstat(source.fileno())
            if not stat.S_ISREG(info.st_mode):
                raise ValueError("input is not a regular file")
            sys.stdout.buffer.write(source.read())
    elif mode == "mise":
        executable, *argv = args
        check_paths(base, argv)
        os.execv(executable, [executable, *argv])
    else:
        raise ValueError("unknown fixture boundary operation")


if __name__ == "__main__":
    try:
        main()
    except (KeyError, IndexError, OSError, ValueError) as error:
        sys.exit(f"P-07 fixture boundary refused: {error}")
