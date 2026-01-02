"""Test module for diagnostics."""


class DataLoader:
    def __init__(self, batch_size: int, shuffle: bool = False):
        self.batch_size = batch_size
        self.shuffle = shuffle


def create_model(input_dim: int, output_dim: int, hidden_dim: int = 128):
    return None


class Config:
    def __init__(self, name: str, value: float):
        self.name = name
        self.value = value
