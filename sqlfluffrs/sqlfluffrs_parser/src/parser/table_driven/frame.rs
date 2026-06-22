use smallvec::SmallVec;
use sqlfluffrs_types::{GrammarId, GrammarVariant, ParseMode};
use std::sync::Arc;

use crate::parser::{FrameContext, FrameState, MatchResult};

/// Result of frame processing - either finished or needs to push frame back
pub(crate) enum TableFrameResult {
    /// Frame processing is complete, don't push back
    Done,
    /// Frame needs to be pushed back with updated state
    Push(TableParseFrame),
}

/// Stack structure for managing ParseFrames and related state
pub(crate) struct TableParseFrameStack {
    stack: Vec<TableParseFrame>,
    /// Completed-child results, keyed by `frame_id`.
    ///
    /// This is the hand-off channel between a child and its parent: when a child
    /// frame reaches `Complete`, the loop writes its
    /// `(Arc<MatchResult>, end_pos, element_key)` here; the parent — parked in
    /// `WaitingForChild` — reclaims it by looking up its own
    /// `last_child_frame_id`. `Arc` keeps the hand-off clone-free.
    ///
    /// - `end_pos`: token position just past the child's match (the parent's new
    ///   `working_idx`/`matched_idx`).
    /// - `element_key`: optional per-element identity (set by OneOf, consumed by
    ///   AnyNumberOf for `max_times_per_element` accounting); `None` otherwise.
    pub(crate) results: hashbrown::HashMap<usize, (Arc<MatchResult>, usize, Option<u64>)>,
    pub(crate) frame_id_counter: usize,
    // Add any additional state fields here as needed
}

impl Default for TableParseFrameStack {
    fn default() -> Self {
        Self::new()
    }
}

impl TableParseFrameStack {
    pub(crate) fn new() -> Self {
        TableParseFrameStack {
            stack: Vec::new(),
            results: hashbrown::HashMap::new(),
            frame_id_counter: 0,
        }
    }

    pub(crate) fn push(&mut self, frame: TableParseFrame) {
        self.stack.push(frame);
    }

    pub(crate) fn pop(&mut self) -> Option<TableParseFrame> {
        self.stack.pop()
    }

    pub(crate) fn len(&self) -> usize {
        self.stack.len()
    }

    pub(crate) fn last_mut(&mut self) -> Option<&mut TableParseFrame> {
        self.stack.last_mut()
    }

    pub(crate) fn iter(&'_ self) -> std::slice::Iter<'_, TableParseFrame> {
        self.stack.iter()
    }

    pub(crate) fn increment_frame_id_counter(&mut self) {
        self.frame_id_counter += 1;
    }

    #[inline]
    pub(crate) fn insert_empty_result(&mut self, frame_id: usize, pos: usize) {
        self.results
            .insert(frame_id, (Arc::new(MatchResult::empty_at(pos)), pos, None));
    }

    #[inline]
    pub(crate) fn insert_result(
        &mut self,
        frame_id: usize,
        match_result: MatchResult,
        end_pos: usize,
    ) {
        self.results
            .insert(frame_id, (Arc::new(match_result), end_pos, None));
    }

    #[inline]
    pub(crate) fn insert_arc_result(
        &mut self,
        frame_id: usize,
        match_result: Arc<MatchResult>,
        end_pos: usize,
    ) {
        self.results.insert(frame_id, (match_result, end_pos, None));
    }

    #[inline]
    pub(crate) fn insert_arc_result_with_key(
        &mut self,
        frame_id: usize,
        match_result: Arc<MatchResult>,
        end_pos: usize,
        element_key: Option<u64>,
    ) {
        self.results
            .insert(frame_id, (match_result, end_pos, element_key));
    }

    /// Push child frame and update parent to wait for it
    #[inline]
    pub(crate) fn push_child_and_wait(
        &mut self,
        mut parent: TableParseFrame,
        child: TableParseFrame,
    ) -> TableFrameResult {
        parent.state = FrameState::WaitingForChild;
        self.push(parent);
        self.push(child);
        self.increment_frame_id_counter();
        TableFrameResult::Done
    }

    #[inline]
    pub(crate) fn transition_to_combining(
        &mut self,
        mut frame: TableParseFrame,
        end_pos: Option<usize>,
    ) -> TableFrameResult {
        frame.transition_to_combining(end_pos);
        self.push(frame);
        TableFrameResult::Done
    }

    /// Complete a frame and insert into results map
    #[inline]
    pub(crate) fn complete_frame(
        &mut self,
        mut frame: TableParseFrame,
        result: Arc<MatchResult>,
    ) -> TableFrameResult {
        let pos = result.end();
        frame.end_pos = Some(pos);
        frame.state = FrameState::Complete(result);
        self.push(frame);
        // self.insert_result(frame.frame_id, result, pos);
        TableFrameResult::Done
    }

    /// Complete a frame with empty result
    #[inline]
    pub(crate) fn complete_frame_empty(&mut self, frame: &TableParseFrame) -> TableFrameResult {
        self.insert_empty_result(frame.frame_id, frame.pos);
        TableFrameResult::Done
    }

    /// Complete a frame with empty result
    #[inline]
    pub(crate) fn complete_frame_empty_at_pos(
        &mut self,
        frame: &TableParseFrame,
        pos: usize,
    ) -> TableFrameResult {
        self.insert_empty_result(frame.frame_id, pos);
        TableFrameResult::Done
    }

    /// Update the last_child_frame_id for the parent frame on the stack
    /// Returns true if the update succeeded, false if parent wasn't found or had wrong context type
    pub(crate) fn update_parent_last_child_id(
        &mut self,
        context_type: GrammarVariant,
        child_frame_id: usize,
    ) -> bool {
        match self.last_mut() {
            Some(parent_frame) => parent_frame
                .context
                .set_last_child_id(context_type, child_frame_id),
            None => false,
        }
    }

    /// Push a child frame onto the stack and update parent's last_child_frame_id
    /// Also pushes the parent frame back onto the stack first (for use in WaitingForChild handlers)
    /// Returns the new frame_id_counter value
    pub(crate) fn push_child_and_update_parent(
        &mut self,
        mut parent_frame: TableParseFrame,
        child_frame: TableParseFrame,
        parent_context_type: GrammarVariant,
    ) {
        let child_id = child_frame.frame_id;

        // Update parent's last_child_frame_id BEFORE pushing
        parent_frame
            .context
            .set_last_child_id(parent_context_type, child_id);

        // Push parent and child
        self.push(parent_frame);
        self.increment_frame_id_counter();
        self.push(child_frame);
    }

    /// Update Sequence parent on stack and push child (for WaitingForChild state)
    /// Assumes parent is already on the stack
    pub(crate) fn update_sequence_parent_and_push_child(
        &mut self,
        child_frame: TableParseFrame,
        next_element_idx: usize,
    ) -> TableFrameResult {
        let child_id = child_frame.frame_id;

        // Update parent's last_child_frame_id, current_element_idx, AND state
        if let Some(parent_frame) = self.last_mut() {
            if let FrameContext::Sequence(state) = &mut parent_frame.context {
                state.last_child_frame_id = Some(child_id);
                state.current_element_idx = next_element_idx;
            }
            // CRITICAL: Set parent state to WaitingForChild so it knows to process child result
            parent_frame.state = FrameState::WaitingForChild;
        }

        // Increment counter and push child
        self.increment_frame_id_counter();
        self.push(child_frame);
        TableFrameResult::Done
    }

    // Add more helper methods as needed for dispatch or state management
}

/// A parse frame represents a single parsing task in the iterative parser.
///
/// Instead of using recursion, the parser maintains a stack of frames,
/// where each frame represents parsing a particular grammar element at a
/// specific position in the token stream.
#[derive(Debug, Clone)]
pub(crate) struct TableParseFrame {
    /// Unique ID for this frame
    pub(crate) frame_id: usize,
    /// Table-driven grammar ID (for gradual migration to table-based parsing)
    pub(crate) grammar_id: GrammarId,
    /// Position in token stream
    pub(crate) pos: usize,
    /// When Some, this frame uses table-driven parsing
    /// Table-driven terminators (parallel to terminators field)
    /// SmallVec avoids heap allocation for common case of 0-4 terminators
    pub(crate) table_terminators: SmallVec<[GrammarId; 4]>,
    /// Current state of this frame
    pub(crate) state: FrameState,
    /// Additional context depending on grammar type
    pub(crate) context: FrameContext,
    /// The ceiling inherited from the parent (simulates Python's
    /// `segments[:max_idx]` slicing): if `Some(n)`, this frame may not match
    /// past `n`. This is the *input* bound, set when the frame is created.
    pub(crate) parent_max_idx: Option<usize>,
    /// The frame's *own* effective ceiling, computed in `Initial` from its
    /// terminators, parse mode, and `parent_max_idx` (see
    /// [`crate::parser::helpers::Parser::calculate_max_idx`]).
    /// `None` until the handler computes it. This — not `parent_max_idx` — is
    /// the authoritative bound used during matching and as part of the cache
    /// key, so cache checks and stores stay consistent.
    pub(crate) calculated_max_idx: Option<usize>,
    /// Where this frame's match ended, set when transitioning to `Complete`.
    /// Authoritative result extent; mirrored into the `results` map's `end_pos`.
    pub(crate) end_pos: Option<usize>,
    /// Element key for this match (used by AnyNumberOf to track per-element counts)
    /// Set by OneOf when storing its result, propagated to parent via results map
    pub(crate) element_key: Option<u64>,
    /// Parse mode override for this frame. When Some, this overrides the grammar's native parse_mode.
    /// Used by Bracketed to force content to use GREEDY mode when the Bracketed itself is GREEDY.
    /// This matches Python behavior where Bracketed(parse_mode=GREEDY) inherits from Sequence
    /// and passes its parse_mode to all content elements.
    pub(crate) parse_mode_override: Option<ParseMode>,
}

impl TableParseFrame {
    /// Create a new table-driven child frame
    pub(crate) fn new_child(
        frame_id: usize,
        grammar_id: GrammarId,
        pos: usize,
        table_terminators: &[GrammarId],
        parent_max_idx: Option<usize>,
    ) -> Self {
        TableParseFrame {
            frame_id,
            grammar_id,
            pos,
            table_terminators: SmallVec::from_slice(table_terminators),
            state: FrameState::Initial,
            context: FrameContext::None,
            parent_max_idx,
            calculated_max_idx: None,
            end_pos: None,
            element_key: None,
            parse_mode_override: None,
        }
    }

    fn transition_to_combining(&mut self, end_pos: Option<usize>) {
        if let Some(pos) = end_pos {
            self.end_pos = Some(pos);
        }
        self.state = FrameState::Combining;
    }
}
