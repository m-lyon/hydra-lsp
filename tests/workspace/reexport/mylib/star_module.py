# mylib/star_module.py - Module with __all__ that gets star-imported

__all__ = ["StarExportedClass"]  # Only export this class


class StarExportedClass:
    """A class exported via star import."""

    def __init__(self, size: int):
        """Initialize with size."""
        self.size = size


class _PrivateClass:
    """A private class that should NOT be exported."""

    pass
