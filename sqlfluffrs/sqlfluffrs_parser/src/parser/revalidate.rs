//! Rust-native segment re-validation via re-parse.
//!
//! This is the Rust-side counterpart of Python's
//! `BaseSegment.validate_segment_with_reparse`
//! (`src/sqlfluff/core/parser/segments/base.py:1249`), which the fix loop
//! (`src/sqlfluff/core/linter/fix.py`) uses to re-check a *fixed* segment before
//! accepting an edit.
//!
//! The key native behaviour we reproduce: a fixed container is re-matched by
//! running its *own* `match_grammar` against its *own current, already-TYPED
//! leaf segments* — NOT against re-lexed text.  Because the leaves are already
//! typed, a leaf typed `naked_identifier` fed into a grammar slot that expects a
//! `function_name_identifier` (a `TypedParser("word")`) fails to match.  That is
//! exactly the corruption we want to catch (e.g. RF06 renaming a function name
//! to a bare identifier): the rematch is incomplete, so the fix is rejected.
//!
//! Faithful to `base.py:1261-1295`:
//!   1. resolve the container's grammar (skip if it has none — mirrors Python
//!      skipping segments without a `match_grammar`);
//!   2. gather descendant leaves, drop metas, trim non-code from both ends;
//!   3. re-match the grammar against synthetic tokens built from those leaves;
//!   4. valid iff the match covers the whole slice AND introduces no new
//!      unparsables.
//!
//! Phase 1 scope: these are standalone primitives with unit tests only — not
//! yet wired into the fix façade or the PyO3 bindings.

use sqlfluffrs_dialects::Dialect;
use sqlfluffrs_types::Token;

use super::arena::{Arena, NodeId};
use super::Parser;

/// The verdict of a single container re-validation.
///
/// `Skipped` mirrors Python's behaviour for a segment whose class has no
/// resolvable `match_grammar`: there is nothing to re-check, so the caller
/// should treat it as "no opinion / valid" rather than a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevalidateOutcome {
    /// The container's grammar re-matched its own leaves completely and cleanly.
    Valid,
    /// The re-match was incomplete or introduced new unparsables — the fix that
    /// produced this state corrupted the segment.
    Invalid,
    /// The container has no resolvable grammar (mirrors Python skipping a
    /// segment without a `match_grammar`); treated as valid by callers.
    Skipped,
}

/// A single ordered leaf's re-validation-relevant payload, sourced either from a
/// committed arena leaf (`revalidate_container`) or from a staged plan's leaf
/// (`revalidate_planned_container`).  Metas are included here (the shared
/// verdict fn drops them, mirroring native `raw_segments` sans metas).
#[derive(Debug, Clone)]
pub(crate) struct LeafDescriptor {
    pub(crate) raw: String,
    pub(crate) instance_types: Vec<String>,
    pub(crate) class_types: Vec<String>,
    pub(crate) is_code: bool,
    pub(crate) is_meta: bool,
}

/// The shared verdict core: given an ordered leaf sequence and the already
/// resolved root grammar, drop metas, trim non-code ends, build synthetic
/// tokens and re-match.  Both the committed and the planned re-validation paths
/// funnel through here so their native-parity semantics stay identical.
pub(crate) fn revalidate_leaf_descriptors(
    leaves: &[LeafDescriptor],
    dialect: &Dialect,
    grammar_id: sqlfluffrs_types::GrammarId,
    tables: &'static sqlfluffrs_types::GrammarTables,
) -> RevalidateOutcome {
    // -- drop metas, trim non-code from both ends (base.py:1263-1264) ---------
    let non_meta: Vec<&LeafDescriptor> = leaves.iter().filter(|l| !l.is_meta).collect();
    let start = non_meta.iter().position(|l| l.is_code);
    let Some(start) = start else {
        // No code at all: empty trimmed content -> Valid (see module docs).
        return RevalidateOutcome::Valid;
    };
    let stop = non_meta.iter().rposition(|l| l.is_code).unwrap();
    let trimmed = &non_meta[start..=stop];

    if trimmed.is_empty() {
        return RevalidateOutcome::Valid;
    }

    // -- build synthetic tokens from the trimmed leaves -----------------------
    let mut tokens: Vec<Token> = trimmed
        .iter()
        .map(|l| {
            Token::synthetic_leaf(
                l.raw.clone(),
                l.instance_types.clone(),
                l.class_types.clone(),
                l.is_code,
                l.is_meta,
            )
        })
        .collect();

    // Re-establish bracket pairing on the synthetic slice.  The initial parse
    // computes `matching_bracket_idx` up front (via `compute_bracket_pairs`),
    // and the table-driven `Bracketed` matcher relies on it for O(1) closing
    // bracket lookup — without it a re-match of any bracketed content (e.g. a
    // sub-select) fails with "Couldn't find closing bracket".  Indices are
    // relative to this trimmed slice, exactly the slice we re-match.
    compute_bracket_pairs(&mut tokens);

    // -- re-match the grammar against exactly those tokens --------------------
    let mut parser = Parser::new(&tokens, *dialect, Default::default());
    let rematch = match parser.match_grammar(grammar_id, tables) {
        Ok(mr) => mr,
        Err(_) => return RevalidateOutcome::Invalid,
    };

    let complete = rematch.end() == tokens.len() && !rematch.is_empty();
    if complete && !rematch.contains_unparsable() {
        RevalidateOutcome::Valid
    } else {
        RevalidateOutcome::Invalid
    }
}

/// Compute bidirectional bracket-pair indices (`matching_bracket_idx`) over a
/// token slice — the same pass the initial parse runs (`python.rs` re-exports
/// this for its Python-token entry points).  The table-driven `Bracketed`
/// matcher uses these for O(1) closing-bracket lookup; without them a re-match
/// of bracketed content fails to pair its brackets.
pub(crate) fn compute_bracket_pairs(tokens: &mut [Token]) {
    let mut bracket_stack: Vec<(usize, char)> = Vec::new();
    for idx in 0..tokens.len() {
        // Only code tokens can be brackets — skip non-code (e.g. bracket chars
        // inside a comment must not pair with real SQL brackets).
        if !tokens[idx].is_code() {
            continue;
        }
        let raw = tokens[idx].raw();
        if let Some(open_char) = match raw {
            "(" => Some('('),
            "[" => Some('['),
            "{" => Some('{'),
            _ => None,
        } {
            bracket_stack.push((idx, open_char));
        } else if let Some(expected_open) = match raw {
            ")" => Some('('),
            "]" => Some('['),
            "}" => Some('{'),
            _ => None,
        } {
            if let Some(pos) = bracket_stack.iter().rposition(|(_, c)| *c == expected_open) {
                let (open_idx, _) = bracket_stack.remove(pos);
                tokens[open_idx].matching_bracket_idx = Some(idx);
                tokens[idx].matching_bracket_idx = Some(open_idx);
            }
        }
    }
}

impl Arena {
    /// Re-validate a container node by re-matching its own grammar against its
    /// own current typed leaves — the Rust-native analogue of Python's
    /// `validate_segment_with_reparse` (`base.py:1249`).
    ///
    /// See the module docs for the full algorithm and native-parity intent.
    pub fn revalidate_container(&self, id: NodeId, dialect: &Dialect) -> RevalidateOutcome {
        // -- 1. resolve grammar (base.py: segments without match_grammar are
        //       not re-checked) ------------------------------------------------
        let Some(class) = self.segment_class(id) else {
            return RevalidateOutcome::Skipped;
        };
        let Some(root_grammar) = dialect.get_segment_grammar(&class) else {
            return RevalidateOutcome::Skipped;
        };

        // -- 2. gather leaves as descriptors (metas kept here; the shared
        //       verdict fn drops metas and trims non-code — base.py:1263-1264) --
        let leaves: Vec<LeafDescriptor> = self
            .revalidate_leaves(id)
            .into_iter()
            .map(|leaf| LeafDescriptor {
                raw: self.raw(leaf),
                instance_types: self.instance_types(leaf),
                class_types: self.class_types(leaf),
                is_code: self.is_code(leaf),
                is_meta: self.is_meta(leaf),
            })
            .collect();

        // -- 3-5. shared verdict core (trim, tokenise, re-match, verdict) ------
        revalidate_leaf_descriptors(
            &leaves,
            dialect,
            root_grammar.grammar_id,
            root_grammar.tables,
        )
    }

    /// Descendant LEAF nodes of `id` in document order (nodes with no children:
    /// Raw / Meta / Unparsable / Empty).  This is the `raw_segments` shape used
    /// by native `BaseSegment.raw_segments`; kept as a private helper here so
    /// the re-validation path is self-contained and doesn't depend on the
    /// crate-public accessor's exact leaf definition changing under it.
    fn revalidate_leaves(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect_revalidate_leaves(id, &mut out);
        out
    }

    fn collect_revalidate_leaves(&self, id: NodeId, out: &mut Vec<NodeId>) {
        let kids = self.children(id);
        if kids.is_empty() {
            out.push(id);
        } else {
            for &c in kids {
                self.collect_revalidate_leaves(c, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::types::{MetaType, Node, RawSegmentKwargs};

    /// A leaf with an explicit class-type hierarchy (mirrors what a real
    /// dialect assigns to a lexed/parsed leaf).  `class_hierarchy` becomes the
    /// leaf's `raw_class` class types (e.g. `["word", "raw", "base"]`).
    fn leaf(
        class: &str,
        ty: &str,
        text: &str,
        instance: &[&str],
        class_hierarchy: &[&str],
    ) -> Node {
        Node::new_raw_with_class_types(
            class.to_string(),
            ty.to_string(),
            ty.to_string(),
            text.to_string(),
            None,
            instance.iter().map(|s| s.to_string()).collect(),
            &class_hierarchy
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            RawSegmentKwargs::default(),
        )
    }

    fn function_name(children: Vec<Node>) -> Node {
        Node::Segment {
            segment_class: "FunctionNameSegment".into(),
            segment_type: Some("function_name".into()),
            pos_marker: None,
            class_types: vec!["function_name".to_string()],
            children,
        }
    }

    /// RF06 corruption case: a `FunctionNameSegment` whose sole leaf is a bare
    /// `naked_identifier` (class types `identifier`, NO `word`).  The container's
    /// grammar expects a `FunctionNameIdentifierSegment` — a `TypedParser("word")`
    /// — so the re-match cannot consume the identifier: `Invalid`.
    #[test]
    fn naked_identifier_in_function_name_is_invalid() {
        let dialect = Dialect::Ansi;
        let tree = function_name(vec![leaf(
            "NakedIdentifierSegment",
            "naked_identifier",
            "foo",
            &["naked_identifier"],
            &["identifier", "raw", "base"],
        )]);
        let arena = Arena::from_node(&tree);
        assert_eq!(
            arena.revalidate_container(arena.root(), &dialect),
            RevalidateOutcome::Invalid,
        );
    }

    /// The correct shape: a `FunctionNameSegment` whose leaf is a
    /// `function_name_identifier` typed `word` — exactly what the grammar's
    /// `TypedParser("word")` expects.  The re-match completes cleanly: `Valid`.
    #[test]
    fn function_name_identifier_word_is_valid() {
        let dialect = Dialect::Ansi;
        let tree = function_name(vec![leaf(
            "WordSegment",
            "function_name_identifier",
            "foo",
            &["function_name_identifier"],
            &["word", "raw", "base"],
        )]);
        let arena = Arena::from_node(&tree);
        assert_eq!(
            arena.revalidate_container(arena.root(), &dialect),
            RevalidateOutcome::Valid,
        );
    }

    /// A container whose `segment_class` resolves to no grammar in the dialect
    /// is skipped (native does not re-check segments without a `match_grammar`).
    #[test]
    fn unknown_segment_class_is_skipped() {
        let dialect = Dialect::Ansi;
        let tree = Node::Segment {
            segment_class: "NoSuchSegmentXyz".into(),
            segment_type: Some("no_such".into()),
            pos_marker: None,
            class_types: vec!["no_such".to_string()],
            children: vec![leaf(
                "WordSegment",
                "function_name_identifier",
                "foo",
                &["function_name_identifier"],
                &["word", "raw", "base"],
            )],
        };
        let arena = Arena::from_node(&tree);
        assert_eq!(
            arena.revalidate_container(arena.root(), &dialect),
            RevalidateOutcome::Skipped,
        );
    }

    /// A leaf node (no grammar-bearing container class) is likewise skipped —
    /// `revalidate_container` is only meaningful on containers.  Here we use a
    /// raw leaf whose class does not resolve to a grammar.
    #[test]
    fn raw_leaf_without_grammar_is_skipped() {
        let dialect = Dialect::Ansi;
        let tree = leaf("RawSegment", "raw", "x", &["raw"], &["raw", "base"]);
        let arena = Arena::from_node(&tree);
        // "RawSegment" has no segment grammar → Skipped.
        assert_eq!(
            arena.revalidate_container(arena.root(), &dialect),
            RevalidateOutcome::Skipped,
        );
    }

    /// Metas are dropped and non-code is trimmed from the ends before the
    /// re-match, so a valid function name surrounded by an indent meta and
    /// trailing whitespace still validates cleanly.
    #[test]
    fn metas_and_trailing_noncode_are_trimmed() {
        let dialect = Dialect::Ansi;
        let tree = function_name(vec![
            Node::Meta {
                meta_type: MetaType::Indent { is_implicit: false },
                pos_marker: None,
                block_uuid: None,
            },
            leaf(
                "WordSegment",
                "function_name_identifier",
                "foo",
                &["function_name_identifier"],
                &["word", "raw", "base"],
            ),
            leaf(
                "WhitespaceSegment",
                "whitespace",
                " ",
                &["whitespace"],
                &["whitespace", "raw", "base"],
            ),
        ]);
        let arena = Arena::from_node(&tree);
        assert_eq!(
            arena.revalidate_container(arena.root(), &dialect),
            RevalidateOutcome::Valid,
        );
    }

    /// A cleanly-parsing multi-leaf container: `schema.foo` as a function name
    /// (`SingleIdentifier "." FunctionNameIdentifier`) exercises the
    /// `AnyNumberOf(Sequence(SingleIdentifier, Dot))` prefix and still validates.
    #[test]
    fn qualified_function_name_is_valid() {
        let dialect = Dialect::Ansi;
        let tree = function_name(vec![
            leaf(
                "NakedIdentifierSegment",
                "naked_identifier",
                "schema",
                &["naked_identifier"],
                &["identifier", "raw", "base"],
            ),
            leaf("SymbolSegment", "dot", ".", &["dot"], &["symbol", "raw", "base"]),
            leaf(
                "WordSegment",
                "function_name_identifier",
                "foo",
                &["function_name_identifier"],
                &["word", "raw", "base"],
            ),
        ]);
        let arena = Arena::from_node(&tree);
        assert_eq!(
            arena.revalidate_container(arena.root(), &dialect),
            RevalidateOutcome::Valid,
        );
    }
}
