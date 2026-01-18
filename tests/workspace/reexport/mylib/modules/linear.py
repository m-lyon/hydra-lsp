# mylib/modules/linear.py - Module where Linear is actually defined


class Linear:
    """A linear transformation class (like torch.nn.Linear)."""

    def __init__(self, in_features: int, out_features: int, bias: bool = True):
        """Initialize the linear layer.

        Args:
            in_features: Size of input features
            out_features: Size of output features
            bias: Whether to include a bias term
        """
        self.in_features = in_features
        self.out_features = out_features
        self.bias = bias


class DirectClass:
    """A class that is directly accessed without re-export."""

    def __init__(self, value: int):
        """Initialize with a value."""
        self.value = value


class OriginalClass:
    """A class that is re-exported with an alias."""

    def __init__(self, param: str):
        """Initialize with a parameter."""
        self.param = param
