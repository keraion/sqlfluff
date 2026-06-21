//! Compound-grammar dispatch contract.
//!
//! A *compound* grammar (OneOf, Sequence, Delimited, Bracketed, AnyNumberOf, Ref) drives
//! one or more child parses and therefore moves through the full frame lifecycle:
//! `Initial -> WaitingForChild -> Combining`. Each compound variant implements those three
//! lifecycle steps; the terminal/leaf variants (StringParser, Token, Meta, ...) match
//! synchronously and never reach `WaitingForChild`/`Combining`.
//!
//! [`CompoundGrammar`] makes that contract explicit and is implemented by one zero-sized
//! marker per compound variant ([`markers`]). The impls are thin forwarders to the existing
//! `Parser::handle_<variant>_<phase>` methods, so behaviour is unchanged and — being
//! `#[inline(always)]` static dispatch over concrete types — they compile to the same code as a
//! direct method call.
//!
//! The three lifecycle dispatchers in `iterative.rs` each forward to these markers from a single
//! `match` (on `GrammarVariant` for `Initial`/`Combining`, on the in-hand `FrameContext` tag for
//! `WaitingForChild`) — the same dispatch shape as the per-variant handlers had before, so there
//! is no added branching on any hot path. Adding a compound variant means: its own handler file,
//! a marker here, and one arm in each of the three dispatchers.

use std::sync::Arc;

use crate::parser::table_driven::frame::{TableFrameResult, TableParseFrame, TableParseFrameStack};
use crate::parser::{MatchResult, ParseError, Parser};

/// The lifecycle steps every compound grammar must implement.
///
/// Methods take `p: &mut Parser` rather than `&mut self` because the implementers are
/// zero-sized markers — all state lives on the `Parser` and the frame.
pub(crate) trait CompoundGrammar {
    /// First visit: inspect the grammar and push the first child frame(s).
    fn initial(
        p: &mut Parser,
        frame: TableParseFrame,
        stack: &mut TableParseFrameStack,
    ) -> Result<TableFrameResult, ParseError>;

    /// A child parse just completed; fold its result in and decide what to do next.
    fn waiting_for_child(
        p: &mut Parser,
        frame: TableParseFrame,
        child_match: &Arc<MatchResult>,
        child_end_pos: &usize,
        stack: &mut TableParseFrameStack,
    ) -> Result<TableFrameResult, ParseError>;

    /// All children done: assemble this frame's `MatchResult`.
    fn combining(
        p: &mut Parser,
        frame: TableParseFrame,
        stack: &mut TableParseFrameStack,
    ) -> Result<TableFrameResult, ParseError>;
}

/// One zero-sized marker per compound variant. Each forwards to the variant's existing handlers.
pub(crate) mod markers {
    use super::*;

    /// Generate a marker type whose `CompoundGrammar` impl forwards to the given handler methods.
    ///
    /// `waiting`/`combining` take an optional `(stack)` suffix because `Ref`'s handlers don't
    /// take the frame stack; the marker absorbs that asymmetry so the trait stays uniform.
    macro_rules! marker {
        (
            $name:ident,
            initial = $initial:ident,
            waiting = $waiting:ident ($($w_stack:tt)?),
            combining = $combining:ident ($($c_stack:tt)?)
        ) => {
            pub(crate) struct $name;
            impl CompoundGrammar for $name {
                #[inline(always)]
                fn initial(
                    p: &mut Parser,
                    frame: TableParseFrame,
                    stack: &mut TableParseFrameStack,
                ) -> Result<TableFrameResult, ParseError> {
                    p.$initial(frame, stack)
                }
                #[inline(always)]
                fn waiting_for_child(
                    p: &mut Parser,
                    frame: TableParseFrame,
                    child_match: &Arc<MatchResult>,
                    child_end_pos: &usize,
                    #[allow(unused_variables)] stack: &mut TableParseFrameStack,
                ) -> Result<TableFrameResult, ParseError> {
                    p.$waiting(frame, child_match, child_end_pos $(, marker!(@stack $w_stack stack))?)
                }
                #[inline(always)]
                fn combining(
                    p: &mut Parser,
                    frame: TableParseFrame,
                    #[allow(unused_variables)] stack: &mut TableParseFrameStack,
                ) -> Result<TableFrameResult, ParseError> {
                    p.$combining(frame $(, marker!(@stack $c_stack stack))?)
                }
            }
        };
        // Expand `(stack)` suffix to the `stack` argument; absence expands to nothing.
        (@stack stack $stack:ident) => { $stack };
    }

    marker!(OneOf, initial = handle_oneof_initial,
        waiting = handle_oneof_waiting_for_child (stack),
        combining = handle_oneof_combining (stack));
    marker!(Sequence, initial = handle_sequence_initial,
        waiting = handle_sequence_waiting_for_child (stack),
        combining = handle_sequence_combining (stack));
    marker!(Delimited, initial = handle_delimited_initial,
        waiting = handle_delimited_waiting_for_child (stack),
        combining = handle_delimited_combining (stack));
    marker!(Bracketed, initial = handle_bracketed_initial,
        waiting = handle_bracketed_waiting_for_child (stack),
        combining = handle_bracketed_combining (stack));
    marker!(AnyNumberOf, initial = handle_anynumberof_initial,
        waiting = handle_anynumberof_waiting_for_child (stack),
        combining = handle_anynumberof_combining (stack));
    // Ref's waiting/combining handlers don't take the frame stack.
    marker!(Ref, initial = handle_ref_initial,
        waiting = handle_ref_waiting_for_child (),
        combining = handle_ref_combining ());
}
