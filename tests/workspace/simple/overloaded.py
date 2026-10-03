"""Overloaded callables, for the overload-narrowing tests."""

from typing import overload


@overload
def load(path: str) -> str: ...
@overload
def load(path: str, *, binary: bool) -> bytes: ...
def load(path, *, binary=False):
    """Load a file as text, or as bytes when `binary` is set."""


class Reader:
    @overload
    def __init__(self, path: str) -> None: ...
    @overload
    def __init__(self, path: str, *, encoding: str) -> None: ...
    def __init__(self, path, *, encoding="utf-8"):
        """Open a reader."""
