"""Test module for diagnostics."""


class DataLoader:
    def __init__(self, batch_size: int, shuffle: bool = False):
        self.batch_size = batch_size
        self.shuffle = shuffle

    @classmethod
    def from_config(cls, config_path: str, batch_size: int = 32):
        return cls(batch_size=batch_size)


def create_model(input_dim: int, output_dim: int, hidden_dim: int = 128):
    return None


class Config:
    def __init__(self, name: str, value: float):
        self.name = name
        self.value = value


def my_func(arg1, arg2, *args, **kwargs):
    pass


def strict_func(arg1, arg2):
    pass


def mixed_func(arg1, *args, kw_only):
    pass
