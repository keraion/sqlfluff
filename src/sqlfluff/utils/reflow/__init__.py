"""Reflow utilities for sqlfluff rules."""

from sqlfluff.utils.reflow.rust_bridge import (
    RustReflowSequence,
    convert_rust_violations,
    has_rust_reflow,
    reflow_rebreak,
    reflow_rebreak_around_target,
    reflow_reindent,
    reflow_respace,
)
from sqlfluff.utils.reflow.sequence import ReflowSequence

__all__ = (
    "ReflowSequence",
    # Drop-in Rust-accelerated replacement for ReflowSequence
    "RustReflowSequence",
    # High-level reflow operation wrappers (Rust-accelerated when available)
    "reflow_respace",
    "reflow_reindent",
    "reflow_rebreak",
    "reflow_rebreak_around_target",
    # Utilities for custom Rust integration
    "has_rust_reflow",
    "convert_rust_violations",
)
