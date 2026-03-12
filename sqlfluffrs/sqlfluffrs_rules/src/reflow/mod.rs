//! Reflow module for spacing analysis (LT01 rule).
//!
//! This module implements the reflow/respace logic from Python's
//! `sqlfluff.utils.reflow` in Rust. It walks a parsed `Node` tree,
//! splits raw segments into alternating blocks (code) and points
//! (whitespace/newlines/indents), then analyzes spacing constraints
//! to produce lint violations.

pub mod config;
pub mod depthmap;
pub mod elements;
#[cfg(feature = "python")]
pub mod python;
pub mod respace;
pub mod sequence;

pub use config::{BlockConfig, ReflowConfig};
pub use depthmap::{DepthInfo, DepthMap, StackPosition};
pub use elements::{ReflowBlock, ReflowElement, ReflowPoint};
pub use respace::LintViolation;
pub use sequence::ReflowSequence;
