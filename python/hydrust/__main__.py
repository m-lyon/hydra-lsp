"""Run `hydrust` as `python -m hydrust`."""

import os
import sys

from hydrust import find_hydrust_bin

if __name__ == "__main__":
    hydrust = os.fsdecode(find_hydrust_bin())
    if sys.platform == "win32":
        import subprocess

        # `execvp` on Windows spawns a child and exits immediately, which
        # breaks the exit code and interleaves output with the shell prompt.
        completed_process = subprocess.run([hydrust, *sys.argv[1:]])
        sys.exit(completed_process.returncode)
    else:
        os.execvp(hydrust, [hydrust, *sys.argv[1:]])
