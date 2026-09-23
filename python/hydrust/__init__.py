"""Locate the `hydrust` executable installed alongside this package."""

from __future__ import annotations

import os
import sys
import sysconfig

__all__ = ["find_hydrust_bin"]


def find_hydrust_bin() -> str:
    """Return the path to the `hydrust` binary installed with this package."""

    hydrust_exe = "hydrust" + sysconfig.get_config_var("EXE")

    # The scripts directory of the environment this package is installed into.
    scripts_path = os.path.join(sysconfig.get_path("scripts"), hydrust_exe)
    if os.path.isfile(scripts_path):
        return scripts_path

    # A `pip install --user` install.
    if sys.version_info >= (3, 10):
        user_scheme = sysconfig.get_preferred_scheme("user")
    elif os.name == "nt":
        user_scheme = "nt_user"
    elif sys.platform == "darwin" and getattr(sys, "_framework", ""):
        user_scheme = "osx_framework_user"
    else:
        user_scheme = "posix_user"

    user_path = os.path.join(
        sysconfig.get_path("scripts", scheme=user_scheme), hydrust_exe
    )
    if os.path.isfile(user_path):
        return user_path

    # A `pip install --target` install, where the scripts land in `bin/` next
    # to the package directory.
    pkg_root = os.path.dirname(os.path.dirname(__file__))
    target_path = os.path.join(pkg_root, "bin", hydrust_exe)
    if os.path.isfile(target_path):
        return target_path

    # An isolated build environment created by pip, e.g. when `hydrust` is a
    # build requirement. pip puts `<tmp>/pip-build-env-<rand>/overlay/bin` and
    # `<tmp>/pip-build-env-<rand>/normal/bin` at the front of PATH, and the
    # binary lives in the first. See:
    # https://github.com/pypa/pip/blob/102d8187a1f5a4cd5de7a549fd8a9af34e89a54f/src/pip/_internal/build_env.py#L87
    paths = os.environ.get("PATH", "").split(os.pathsep)
    if len(paths) >= 2:

        def get_last_three_path_parts(path: str) -> list[str]:
            """Return a list of up to the last three parts of a path."""
            parts = []
            while len(parts) < 3:
                head, tail = os.path.split(path)
                if tail or head != path:
                    parts.append(tail)
                    path = head
                else:
                    parts.append(path)
                    break
            return parts

        maybe_overlay = get_last_three_path_parts(paths[0])
        maybe_normal = get_last_three_path_parts(paths[1])
        if (
            len(maybe_normal) >= 3
            and maybe_normal[-1].startswith("pip-build-env-")
            and maybe_normal[-2] == "normal"
            and len(maybe_overlay) >= 3
            and maybe_overlay[-1].startswith("pip-build-env-")
            and maybe_overlay[-2] == "overlay"
        ):
            # The overlay must contain the hydrust binary.
            candidate = os.path.join(paths[0], hydrust_exe)
            if os.path.isfile(candidate):
                return candidate

    raise FileNotFoundError(scripts_path)
