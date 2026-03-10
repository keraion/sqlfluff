//! ReflowSequence — the main entry point for reflow/respace analysis.
//!
//! Mirrors Python's `sqlfluff.utils.reflow.sequence.ReflowSequence`.

use super::config::ReflowConfig;
use super::depthmap::{DepthMap, RawRef};
use super::elements::{ReflowBlock, ReflowElement, ReflowPoint};
use super::respace::{
    determine_constraints, handle_inline_with_space, handle_inline_without_space,
    process_spacing, LintViolation,
};
use sqlfluffrs_parser::parser::Node;

/// The reflow sequence: alternating blocks and points built from a parsed
/// Node tree, with methods to analyze and fix spacing.
#[derive(Debug)]
pub struct ReflowSequence {
    /// Alternating Block / Point elements.
    pub elements: Vec<ReflowElement>,
    /// Accumulated lint violations.
    pub violations: Vec<LintViolation>,
}

impl ReflowSequence {
    /// Build a ReflowSequence from a root Node.
    ///
    /// This is the main entry point, equivalent to Python's
    /// `ReflowSequence.from_root()`.
    pub fn from_root(root: &Node, config: &ReflowConfig) -> Self {
        let (depth_map, raws) = DepthMap::from_node(root, config);
        Self::from_raws(&raws, &depth_map, config)
    }

    /// Build from an already-flattened list of raw segment refs.
    fn from_raws(raws: &[RawRef], depth_map: &DepthMap, config: &ReflowConfig) -> Self {
        let elements = Self::elements_from_raws(raws, depth_map, config);
        ReflowSequence {
            elements,
            violations: Vec::new(),
        }
    }

    /// Split raw segments into alternating Block / Point elements.
    fn elements_from_raws(
        raws: &[RawRef],
        depth_map: &DepthMap,
        config: &ReflowConfig,
    ) -> Vec<ReflowElement> {
        let mut elem_buff: Vec<ReflowElement> = Vec::new();
        let mut seg_indices: Vec<usize> = Vec::new();
        let mut seg_types: Vec<String> = Vec::new();
        let mut seg_raws: Vec<String> = Vec::new();

        for raw_ref in raws {
            // Determine if this is a "point" segment (whitespace/newline/indent)
            // or a "block" segment (code, end_of_file).
            // NOTE: Template placeholders that consumed whitespace (block_type
            // == "literal" with all-whitespace source_str) are treated as
            // point-like, mirroring Python's `get_consumed_whitespace()` check.
            let is_point_like = raw_ref.is_type("whitespace")
                || raw_ref.is_type("newline")
                || raw_ref.is_type("indent")
                || raw_ref.is_type("dedent")
                || raw_ref.consumed_whitespace.is_some();

            if is_point_like {
                seg_indices.push(raw_ref.index);
                seg_types.push(raw_ref.segment_type.to_string());
                seg_raws.push(raw_ref.raw.to_string());
                continue;
            }

            // This is a block-like segment. First flush any pending point.
            if !elem_buff.is_empty() || !seg_indices.is_empty() {
                elem_buff.push(ReflowElement::Point(ReflowPoint::new(
                    seg_indices.clone(),
                    seg_types.clone(),
                    seg_raws.clone(),
                )));
                seg_indices.clear();
                seg_types.clear();
                seg_raws.clear();
            }

            // Create the block.
            let di = depth_map
                .get_depth_info(raw_ref.index)
                .cloned()
                .unwrap_or_else(|| {
                    super::depthmap::DepthInfo::from_path_steps(&[])
                });
            elem_buff.push(ReflowElement::Block(ReflowBlock::from_config(
                raw_ref.index,
                raw_ref.segment_type,
                raw_ref.raw,
                raw_ref.instance_types,
                config,
                di,
            )));
        }

        // Flush any trailing point.
        if !seg_indices.is_empty() {
            elem_buff.push(ReflowElement::Point(ReflowPoint::new(
                seg_indices,
                seg_types,
                seg_raws,
            )));
        }

        elem_buff
    }

    /// Iterate points with their adjacent blocks (prev, next).
    fn iter_points_with_constraints(
        &self,
    ) -> Vec<(usize, Option<&ReflowBlock>, Option<&ReflowBlock>)> {
        let mut result = Vec::new();
        for (idx, elem) in self.elements.iter().enumerate() {
            if let ReflowElement::Point(_) = elem {
                let prev = if idx > 0 {
                    match &self.elements[idx - 1] {
                        ReflowElement::Block(b) => Some(b),
                        _ => None,
                    }
                } else {
                    None
                };
                let next = if idx + 1 < self.elements.len() {
                    match &self.elements[idx + 1] {
                        ReflowElement::Block(b) => Some(b),
                        _ => None,
                    }
                } else {
                    None
                };
                result.push((idx, prev, next));
            }
        }
        result
    }

    /// Run the respace algorithm on all points.
    ///
    /// Returns a new ReflowSequence with accumulated violations.
    pub fn respace(mut self) -> Self {
        let mut all_violations: Vec<LintViolation> = Vec::new();

        let constraints = self.iter_points_with_constraints();

        for (idx, prev_block, next_block) in &constraints {
            let point = match &self.elements[*idx] {
                ReflowElement::Point(p) => p,
                _ => continue,
            };

            // Determine constraints.
            let (pre_constraint, post_constraint, strip_newlines) =
                determine_constraints(*prev_block, *next_block, false);

            // Process spacing (trailing ws removal, duplicate ws removal).
            let (seg_types, seg_raws, raw_indices, last_ws_idx, mut violations) =
                process_spacing(
                    &point.segment_types,
                    &point.raws,
                    &point.raw_indices,
                    strip_newlines,
                );

            // Check for trailing whitespace at end of file.
            if let Some(nb) = next_block {
                if nb.class_types.contains("end_of_file") {
                    if let Some(ws_idx) = last_ws_idx {
                        violations.push(LintViolation {
                            description:
                                "Unnecessary trailing whitespace at end of file."
                                    .to_string(),
                            line_no: 0,
                            line_pos: 0,
                            fix_type: super::respace::FixType::Delete,
                            edit_text: None,
                            anchor_idx: raw_indices
                                .get(ws_idx)
                                .copied()
                                .unwrap_or(0),
                        });
                    }
                }
            }

            // Check if there's a newline in the buffer.
            let has_newline = seg_types.iter().any(|t| t == "newline");
            let is_eof = next_block
                .map(|nb| nb.class_types.contains("end_of_file"))
                .unwrap_or(false);

            if (has_newline && !strip_newlines) || is_eof {
                // Newline case: handled by indent rules (LT02), not respace.
                all_violations.extend(violations);
                continue;
            }

            // Inline case: no newline.
            if let Some(ws_idx) = last_ws_idx {
                // There IS whitespace.
                let ws_raw = &seg_raws[ws_idx];
                let ws_anchor = raw_indices.get(ws_idx).copied().unwrap_or(0);
                let inline_violations = handle_inline_with_space(
                    &pre_constraint,
                    &post_constraint,
                    *next_block,
                    ws_raw,
                    ws_anchor,
                );
                violations.extend(inline_violations);
            } else {
                // No whitespace.
                let inline_violations = handle_inline_without_space(
                    &pre_constraint,
                    &post_constraint,
                    *prev_block,
                    *next_block,
                );
                violations.extend(inline_violations);
            }

            all_violations.extend(violations);
        }

        self.violations = all_violations;
        self
    }

    /// Get the accumulated violations.
    pub fn get_violations(&self) -> &[LintViolation] {
        &self.violations
    }

    /// Get the current raw representation of the sequence.
    pub fn get_raw(&self) -> String {
        self.elements.iter().map(|e| e.raw()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlfluffrs_parser::parser::{MetaType, Node, RawSegmentKwargs};

    fn make_raw(seg_type: &str, raw: &str, instance_types: Vec<String>) -> Node {
        Node::Raw {
            segment_class: format!("{}Segment", seg_type),
            segment_type: seg_type.to_string(),
            raw: raw.to_string(),
            pos_marker: None,
            instance_types,
            segment_kwargs: RawSegmentKwargs::default(),
        }
    }

    fn make_keyword(raw: &str) -> Node {
        Node::Raw {
            segment_class: "KeywordSegment".to_string(),
            segment_type: "keyword".to_string(),
            raw: raw.to_string(),
            pos_marker: None,
            instance_types: vec!["keyword".to_string()],
            segment_kwargs: RawSegmentKwargs::default(),
        }
    }

    fn make_ws(raw: &str) -> Node {
        make_raw("whitespace", raw, vec!["whitespace".to_string()])
    }

    fn make_newline() -> Node {
        make_raw("newline", "\n", vec!["newline".to_string()])
    }

    fn make_eof() -> Node {
        Node::Meta {
            meta_type: MetaType::EndOfFile,
            pos_marker: None,
        }
    }

    fn make_file(children: Vec<Node>) -> Node {
        Node::Segment {
            segment_class: "FileSegment".to_string(),
            segment_type: Some("file".to_string()),
            pos_marker: None,
            children,
        }
    }

    fn make_statement(children: Vec<Node>) -> Node {
        Node::Segment {
            segment_class: "StatementSegment".to_string(),
            segment_type: Some("statement".to_string()),
            pos_marker: None,
            children,
        }
    }

    fn make_select_stmt(children: Vec<Node>) -> Node {
        Node::Segment {
            segment_class: "SelectStatementSegment".to_string(),
            segment_type: Some("select_statement".to_string()),
            pos_marker: None,
            children,
        }
    }

    fn make_select_clause(children: Vec<Node>) -> Node {
        Node::Segment {
            segment_class: "SelectClauseSegment".to_string(),
            segment_type: Some("select_clause".to_string()),
            pos_marker: None,
            children,
        }
    }

    fn make_comma() -> Node {
        Node::Raw {
            segment_class: "CommaSegment".to_string(),
            segment_type: "comma".to_string(),
            raw: ",".to_string(),
            pos_marker: None,
            instance_types: vec!["comma".to_string()],
            segment_kwargs: RawSegmentKwargs::default(),
        }
    }

    fn make_literal(raw: &str) -> Node {
        Node::Raw {
            segment_class: "NumericLiteralSegment".to_string(),
            segment_type: "numeric_literal".to_string(),
            raw: raw.to_string(),
            pos_marker: None,
            instance_types: vec![
                "numeric_literal".to_string(),
                "literal".to_string(),
            ],
            segment_kwargs: RawSegmentKwargs::default(),
        }
    }

    fn make_dot() -> Node {
        Node::Raw {
            segment_class: "DotSegment".to_string(),
            segment_type: "dot".to_string(),
            raw: ".".to_string(),
            pos_marker: None,
            instance_types: vec!["dot".to_string()],
            segment_kwargs: RawSegmentKwargs::default(),
        }
    }

    fn make_identifier(raw: &str) -> Node {
        Node::Raw {
            segment_class: "IdentifierSegment".to_string(),
            segment_type: "naked_identifier".to_string(),
            raw: raw.to_string(),
            pos_marker: None,
            instance_types: vec!["naked_identifier".to_string(), "identifier".to_string()],
            segment_kwargs: RawSegmentKwargs::default(),
        }
    }

    // ---- Tests ----

    #[test]
    fn test_select_1_no_violations() {
        // "SELECT 1\n" — should pass with no violations.
        let tree = make_file(vec![
            make_statement(vec![make_select_stmt(vec![make_select_clause(
                vec![
                    make_keyword("SELECT"),
                    make_ws(" "),
                    make_literal("1"),
                ],
            )])]),
            make_newline(),
            make_eof(),
        ]);

        let config = ReflowConfig::default_ansi();
        let seq = ReflowSequence::from_root(&tree, &config).respace();
        assert!(
            seq.violations.is_empty(),
            "Expected no violations for 'SELECT 1\\n', got: {:?}",
            seq.violations
        );
    }

    #[test]
    fn test_trailing_whitespace() {
        // "SELECT 1     \n" — should detect trailing whitespace.
        let tree = make_file(vec![
            make_statement(vec![make_select_stmt(vec![make_select_clause(
                vec![
                    make_keyword("SELECT"),
                    make_ws(" "),
                    make_literal("1"),
                ],
            )])]),
            make_ws("     "),
            make_newline(),
            make_eof(),
        ]);

        let config = ReflowConfig::default_ansi();
        let seq = ReflowSequence::from_root(&tree, &config).respace();
        assert!(
            !seq.violations.is_empty(),
            "Expected violations for trailing whitespace"
        );
        assert!(
            seq.violations
                .iter()
                .any(|v| v.description.contains("trailing whitespace")),
            "Expected trailing whitespace violation, got: {:?}",
            seq.violations
        );
    }

    #[test]
    fn test_excessive_whitespace() {
        // "SELECT  1\n" — double space should be flagged.
        let tree = make_file(vec![
            make_statement(vec![make_select_stmt(vec![make_select_clause(
                vec![
                    make_keyword("SELECT"),
                    make_ws("  "),
                    make_literal("1"),
                ],
            )])]),
            make_newline(),
            make_eof(),
        ]);

        let config = ReflowConfig::default_ansi();
        let seq = ReflowSequence::from_root(&tree, &config).respace();
        assert!(
            !seq.violations.is_empty(),
            "Expected violations for double space"
        );
        assert!(
            seq.violations
                .iter()
                .any(|v| v.description.contains("Expected only single space")),
            "Expected single space violation, got: {:?}",
            seq.violations
        );
    }

    #[test]
    fn test_missing_whitespace() {
        // "SELECT1\n" — missing space between SELECT and 1.
        let tree = make_file(vec![
            make_statement(vec![make_select_stmt(vec![make_select_clause(
                vec![make_keyword("SELECT"), make_literal("1")],
            )])]),
            make_newline(),
            make_eof(),
        ]);

        let config = ReflowConfig::default_ansi();
        let seq = ReflowSequence::from_root(&tree, &config).respace();
        assert!(
            !seq.violations.is_empty(),
            "Expected violations for missing space"
        );
        assert!(
            seq.violations
                .iter()
                .any(|v| v.description.contains("Expected single whitespace")),
            "Expected missing whitespace violation, got: {:?}",
            seq.violations
        );
    }

    #[test]
    fn test_comma_touch_before() {
        // "SELECT 1 ,2\n" — space before comma should be removed.
        let tree = make_file(vec![
            make_statement(vec![make_select_stmt(vec![make_select_clause(
                vec![
                    make_keyword("SELECT"),
                    make_ws(" "),
                    make_literal("1"),
                    make_ws(" "),
                    make_comma(),
                    make_literal("2"),
                ],
            )])]),
            make_newline(),
            make_eof(),
        ]);

        let config = ReflowConfig::default_ansi();
        let seq = ReflowSequence::from_root(&tree, &config).respace();
        // Should have: touch violation before comma AND missing space after comma
        let has_touch = seq
            .violations
            .iter()
            .any(|v| v.description.contains("Unexpected whitespace"));
        let has_missing = seq
            .violations
            .iter()
            .any(|v| v.description.contains("Expected single whitespace"));
        assert!(
            has_touch || has_missing,
            "Expected comma-related violations, got: {:?}",
            seq.violations
        );
    }

    #[test]
    fn test_dot_touch_both_sides() {
        // "SELECT a . b\n" — spaces around dot should be removed.
        let obj_ref = Node::Segment {
            segment_class: "ObjectReferenceSegment".to_string(),
            segment_type: Some("object_reference".to_string()),
            pos_marker: None,
            children: vec![
                make_identifier("a"),
                make_ws(" "),
                make_dot(),
                make_ws(" "),
                make_identifier("b"),
            ],
        };

        let tree = make_file(vec![
            make_statement(vec![make_select_stmt(vec![make_select_clause(
                vec![make_keyword("SELECT"), make_ws(" "), obj_ref],
            )])]),
            make_newline(),
            make_eof(),
        ]);

        let config = ReflowConfig::default_ansi();
        let seq = ReflowSequence::from_root(&tree, &config).respace();
        // Should flag the spaces around the dot.
        assert!(
            seq.violations.len() >= 2,
            "Expected at least 2 violations for spaces around dot, got: {:?}",
            seq.violations
        );
    }

    #[test]
    fn test_no_violations_for_correct_comma_spacing() {
        // "SELECT 1, 2\n" — correct: touch before comma, single after.
        let tree = make_file(vec![
            make_statement(vec![make_select_stmt(vec![make_select_clause(
                vec![
                    make_keyword("SELECT"),
                    make_ws(" "),
                    make_literal("1"),
                    make_comma(),
                    make_ws(" "),
                    make_literal("2"),
                ],
            )])]),
            make_newline(),
            make_eof(),
        ]);

        let config = ReflowConfig::default_ansi();
        let seq = ReflowSequence::from_root(&tree, &config).respace();
        assert!(
            seq.violations.is_empty(),
            "Expected no violations for 'SELECT 1, 2\\n', got: {:?}",
            seq.violations
        );
    }
}
