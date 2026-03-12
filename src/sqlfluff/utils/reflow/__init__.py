"""Reflow utilities for sqlfluff rules."""

from sqlfluff.utils.reflow.rust_bridge import (
    RustReflowSequence,
    has_rust_reflow,
)
from sqlfluff.utils.reflow.sequence import ReflowSequence

__all__ = (
    "ReflowSequence",
    # Drop-in Rust-accelerated replacement for ReflowSequence
    "RustReflowSequence",
    # Utilities for custom Rust integration
    "has_rust_reflow",
)
