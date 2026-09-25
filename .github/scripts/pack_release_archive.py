"""Pack the binary from a built wheel into the archive dist would have made.

    python .github/scripts/pack_release_archive.py <wheel> <target> <out-dir>

`build-wheels.yml` compiles each target once, and this turns that wheel into
the GitHub Release asset, so the two ship the same binary. dist no longer
builds the archives (`build-local-artifacts = false`), so this script has to
match its output, because the VS Code extension depends on it:

- `hydrust-<target>.zip` on `x86_64-pc-windows-msvc`, `hydrust-<target>.tar.xz`
  everywhere else.
- A tarball has a top-level `hydrust-<target>/` directory; a zip has none.
- A `<archive>.sha256` next to it, in `sha256sum --binary` format. dist ends
  the line with a blank one, so this does too.
- The binary is mode 0755, the other files 0644.

Checked against the dist-built v0.5.0 assets.
"""

import hashlib
import io
import re
import sys
import tarfile
import time
import zipfile
from pathlib import Path

NAME = "hydrust"
# Shipped alongside the binary, as dist does by default.
EXTRA_FILES = ["README.md", "CHANGELOG.md", "LICENSE"]


def binary_from_wheel(wheel: Path, exe: str) -> bytes:
    with zipfile.ZipFile(wheel) as zf:
        matches = [n for n in zf.namelist() if n.endswith(f".data/scripts/{exe}")]
        if len(matches) != 1:
            sys.exit(f"expected one {exe} in {wheel}, found {matches}")
        return zf.read(matches[0])


def pack_tar_xz(path: Path, top: str, binary: bytes, exe: str) -> None:
    def add(name: str, data: bytes, mode: int) -> None:
        info = tarfile.TarInfo(f"{top}/{name}")
        info.size = len(data)
        info.mode = mode
        info.mtime = now
        tf.addfile(info, io.BytesIO(data))

    # Extraction keeps these, and a 1970 mtime looks stale to anything that ages
    # files, so use the build time as dist does.
    now = int(time.time())
    with tarfile.open(path, "w:xz", format=tarfile.GNU_FORMAT) as tf:
        info = tarfile.TarInfo(top)
        info.type = tarfile.DIRTYPE
        info.mode = 0o755
        info.mtime = now
        tf.addfile(info)
        for name in EXTRA_FILES:
            add(name, Path(name).read_bytes(), 0o644)
        add(exe, binary, 0o755)


def pack_zip(path: Path, binary: bytes, exe: str) -> None:
    def add(name: str, data: bytes, mode: int) -> None:
        info = zipfile.ZipInfo(name, date_time=time.localtime()[:6])
        # Packed on Windows, where ZipInfo would claim an MS-DOS host and Unix
        # extractors would drop the mode bits.
        info.create_system = 3
        info.external_attr = mode << 16
        info.compress_type = zipfile.ZIP_DEFLATED
        zf.writestr(info, data)

    with zipfile.ZipFile(path, "w") as zf:
        for name in EXTRA_FILES:
            add(name, Path(name).read_bytes(), 0o100644)
        add(exe, binary, 0o100755)


def main() -> None:
    if len(sys.argv) != 4:
        sys.exit(f"usage: {sys.argv[0]} <wheel> <target> <out-dir>, got {sys.argv[1:]}")
    wheel, target, out_dir = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    if not re.fullmatch(r"[\w.]+(-[\w.]+){2,3}", target):
        sys.exit(f"{target} is not a target triple")
    windows = target.endswith("-windows-msvc")
    exe = f"{NAME}.exe" if windows else NAME
    top = f"{NAME}-{target}"
    archive = out_dir / f"{top}.{'zip' if windows else 'tar.xz'}"

    out_dir.mkdir(parents=True, exist_ok=True)
    binary = binary_from_wheel(wheel, exe)
    if windows:
        pack_zip(archive, binary, exe)
    else:
        pack_tar_xz(archive, top, binary, exe)

    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksum = archive.with_name(f"{archive.name}.sha256")
    checksum.write_text(f"{digest} *{archive.name}\n\n", newline="\n")
    print(checksum.read_text().strip())


if __name__ == "__main__":
    main()
