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
