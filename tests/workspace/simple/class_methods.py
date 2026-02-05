"""Test module for classmethod and staticmethod support."""


class ModelFactory:
    """A factory class with classmethods and staticmethods."""

    default_dim: int = 128

    def __init__(self, input_dim: int, output_dim: int):
        """Initialize the factory.

        Args:
            input_dim: Input dimension
            output_dim: Output dimension
        """
        self.input_dim = input_dim
        self.output_dim = output_dim

    @classmethod
    def from_config(cls, config: dict) -> "ModelFactory":
        """Create a factory from a config dict.

        Args:
            config: Configuration dictionary

        Returns:
            A new ModelFactory instance
        """
        return cls(config["input_dim"], config["output_dim"])

    @classmethod
    def with_defaults(cls, output_dim: int = 10) -> "ModelFactory":
        """Create a factory with default input dimension.

        Args:
            output_dim: Output dimension (default: 10)

        Returns:
            A new ModelFactory instance
        """
        return cls(cls.default_dim, output_dim)

    @staticmethod
    def compute_size(dim1: int, dim2: int) -> int:
        """Compute the size of a matrix.

        Args:
            dim1: First dimension
            dim2: Second dimension

        Returns:
            The product of dimensions
        """
        return dim1 * dim2

    @staticmethod
    def validate_dims(input_dim: int, output_dim: int) -> bool:
        """Validate dimensions are positive.

        Args:
            input_dim: Input dimension
            output_dim: Output dimension

        Returns:
            True if both dimensions are positive
        """
        return input_dim > 0 and output_dim > 0


class DataProcessor:
    """Another class with class methods for testing."""

    @classmethod
    def create(cls, name: str, **kwargs) -> "DataProcessor":
        """Create a new processor.

        Args:
            name: Processor name
            **kwargs: Additional configuration

        Returns:
            A new DataProcessor instance
        """
        instance = cls()
        instance.name = name
        return instance

    @staticmethod
    def preprocess(data: list, normalize: bool = True) -> list:
        """Preprocess the data.

        Args:
            data: Input data list
            normalize: Whether to normalize (default: True)

        Returns:
            Preprocessed data
        """
        return data


class InheritedFactory(ModelFactory):
    """A factory that inherits from ModelFactory without adding new methods."""


class NestedExample:
    """A class with nested classmethods and staticmethods."""

    nested_class = ModelFactory
    inherited_nested_class = InheritedFactory


class NestedTwice:
    """A class with twice nested classmethods and staticmethods."""

    nested = NestedExample


class InheritedNested(NestedTwice):
    """A class that inherits from a class with nested classmethods and staticmethods."""
