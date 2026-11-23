from dataclasses import dataclass
from typing import Callable, Literal


class Encoder:
    def __init__(self, width: int, layers: list[int] | None = None) -> None: ...


@dataclass
class Point:
    x: float
    y: float = 0.0


def make_point(x: float) -> Point: ...


def build(factory: Callable[[], Encoder], scale: float = 1.0) -> Encoder: ...


def train(
    encoder: Encoder,
    lr: float,
    optimizer: Literal["adam", "sgd"] = "adam",
    verbose: bool = False,
) -> None: ...
