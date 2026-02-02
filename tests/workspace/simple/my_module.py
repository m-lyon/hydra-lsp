"""Test module for Hydra LSP integration tests."""


class DataLoader:
    """A simple data loader for testing."""

    def __init__(self, batch_size: int, shuffle: bool = False, num_workers: int = 0):
        """Initialize the data loader.

        Args:
            batch_size: Size of each batch
            shuffle: Whether to shuffle the data (default: False)
            num_workers: Number of worker threads (default: 0)
        """
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


def my_long_func(
    param1: int, param2: str = "default", param3: float = 3.14, param4=True
) -> None:
    """A function with a really long signature to test line wrapping in documentation.

    Args:
        param1: An integer parameter
        param2: A string parameter with default
        param3: A float parameter with default
        param4: A boolean parameter with default
    """
    pass


class MyBaseClass:
    pass


class MyReallyLongClassNameToTestLineWrappingInDocumentation(MyBaseClass):
    pass


class MyReallyReallyLongClassNameToTestLine(
    MyReallyLongClassNameToTestLineWrappingInDocumentation
):
    """A class with a really long name to test line wrapping in documentation."""

    def __init__(
        self, some_parameter: str, another_parameter: int = 42, flag: bool = True
    ) -> None:
        """Initialize the class.

        Args:
            some_parameter: A parameter for initialization
            another_parameter: Another parameter with default
            flag: A boolean flag with default
        """
        self.some_parameter = some_parameter
        self.another_parameter = another_parameter
        self.flag = flag


class ParentWithInit:
    """Parent class with __init__ for testing inheritance."""

    def __init__(self, name: str, value: int = 0):
        """Initialize parent.

        Args:
            name: The name parameter
            value: Optional value (default: 0)
        """
        self.name = name
        self.value = value


class ChildWithoutInit(ParentWithInit):
    """Child class that inherits __init__ from parent."""

    pass


class GrandchildWithoutInit(ChildWithoutInit):
    """Grandchild class that inherits __init__ from grandparent through parent."""

    pass


class ChildWithOwnInit(ParentWithInit):
    """Child class that overrides __init__."""

    def __init__(self, name: str, extra: bool = False):
        """Initialize child with different signature.

        Args:
            name: The name parameter
            extra: An extra boolean flag
        """
        super().__init__(name)
        self.extra = extra
