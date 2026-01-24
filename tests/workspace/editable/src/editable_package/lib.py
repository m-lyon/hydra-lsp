"""Editable package library module for testing .pth file resolution."""


class EditableModel:
    """A model class in an editable package.

    This class is used to test that hydra-lsp can resolve modules
    installed via pixi-style editable installs (using .pth files).
    """

    def __init__(self, input_size: int, output_size: int) -> None:
        """Initialize the editable model.

        Args:
            input_size: The input dimension.
            output_size: The output dimension.
        """
        self.input_size = input_size
        self.output_size = output_size
