"""Test module for Hydra LSP integration tests."""


class DataLoader:
    """A simple data loader for testing.

    Args:
        batch_size: The size of each batch
        shuffle: Whether to shuffle the data
        num_workers: Number of worker processes (default: 0)
    """

    def __init__(self, batch_size: int, shuffle: bool = False, num_workers: int = 0):
        self.batch_size = batch_size
        self.shuffle = shuffle
        self.num_workers = num_workers


def create_model(input_dim: int, output_dim: int, hidden_dim: int = 128):
    """Create a simple model.

    Args:
        input_dim: Input dimension
        output_dim: Output dimension
        hidden_dim: Hidden layer dimension (default: 128)

    Returns:
        A model instance
    """
    return None


class Config:
    """Configuration class.

    Args:
        name: Configuration name
        value: Configuration value
    """

    def __init__(self, name: str, value: float):
        self.name = name
        self.value = value


def simple_function():
    """A simple function with no parameters."""
    pass


def function_with_params(arg1, arg2: int, arg3: str = "default"):
    """Function with various parameter types.

    Args:
        arg1: First argument without type
        arg2: Integer argument
        arg3: String argument with default
    """
    pass


def function_with_return(x: int, y: int) -> int:
    """Function with return type annotation."""
    return x + y


def variadic_function(*args, **kwargs):
    """Function with variadic parameters."""
    pass


def complex_function(
    pos_only, /, regular, *args, keyword_only, another_kw=None, **kwargs
) -> dict[str, int]:
    """Function with all parameter types."""
    raise NotImplementedError


class SimpleClass:
    """A simple class."""

    pass


class ClassWithInit:
    """A class with __init__ method."""

    def __init__(self, name: str, value: int = 0):
        """Initialize the class.

        Args:
            name: Name parameter
            value: Value with default
        """
        self.name = name
        self.value = value


class ComplexClass:
    """A more complex class with multiple methods."""

    def __init__(self, *args, **kwargs):
        """Initialize with variadic parameters."""
        pass

    def method(self):
        """A method."""
        pass
