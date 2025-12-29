"""Test module for python_analyzer tests."""


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
