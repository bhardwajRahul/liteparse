from .parser import LiteParse
from .types import (
    # Enums
    OutputFormat,
    ImageFormat,
    # Results
    ParseResult,
    BatchResult,
    ParsedPage,
    TextItem,
    BoundingBox,
    ScreenshotResult,
    ScreenshotBatchResult,
    # Errors
    ParseError,
    CLINotFoundError,
)

__version__ = "1.2.1"
__all__ = [
    # Main class
    "LiteParse",
    # Enums
    "OutputFormat",
    "ImageFormat",
    # Results
    "ParseResult",
    "BatchResult",
    "ParsedPage",
    "TextItem",
    "BoundingBox",
    "ScreenshotResult",
    "ScreenshotBatchResult",
    # Errors
    "ParseError",
    "CLINotFoundError",
]
