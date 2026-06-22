// Keep the crate's public surface honest: flag any `pub` item that isn't actually reachable
// from outside the crate so it can be `pub(crate)` instead.
#![warn(unreachable_pub)]

pub mod parser;

#[cfg(feature = "python")]
pub use parser::{PyMatchResult, PyNode, PyParser, RsParseError};
