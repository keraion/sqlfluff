//! Respace logic — determines and enforces spacing constraints.
//!
//! Mirrors Python's `sqlfluff.utils.reflow.respace`.

use super::elements::ReflowBlock;

/// A lint violation produced by respace analysis.
#[derive(Debug, Clone)]
pub struct LintViolation {
    /// Description of the violation.
    pub description: String,
    /// The line number (1-based) where the violation occurs.
    pub line_no: usize,
    /// The column number (1-based) where the violation occurs.
    pub line_pos: usize,
    /// The fix action: "delete", "replace", or "create".
    pub fix_type: FixType,
    /// For "replace" fixes: the new text.
    pub edit_text: Option<String>,
    /// The index of the anchor segment (in the flattened raw list).
    pub anchor_idx: usize,
}

/// Type of fix to apply.
#[derive(Debug, Clone, PartialEq)]
pub enum FixType {
    /// Delete the segment at anchor_idx.
    Delete,
    /// Replace the segment at anchor_idx with edit_text.
    Replace { new_text: String },
    /// Create a new segment before/after anchor_idx.
    CreateBefore { new_text: String },
    /// Create a new segment after anchor_idx.
    CreateAfter { new_text: String },
}

/// A spacing constraint between two adjacent blocks.
///
/// Using an enum eliminates the `String` heap allocation that the previous
/// string-based representation incurred on every `unpack_constraint` call.
#[derive(Debug, Clone, PartialEq)]
pub enum Constraint {
    /// No space allowed ("touch" / "touch:inline").
    Touch,
    /// Exactly one space required ("single", the default).
    Single,
    /// No constraint — any whitespace is acceptable ("any").
    Any,
    /// Alignment constraint — handled by a separate pass ("align:...").
    /// Stores the full original constraint string.
    Align(String),
}

/// Unpack a constraint string, handling modifiers like `:inline`.
///
/// Returns (base_constraint, strip_newlines). Zero heap allocation for all
/// common cases (Touch, Single, Any); only Align allocates once.
fn unpack_constraint(constraint: &str, mut strip_newlines: bool) -> (Constraint, bool) {
    // Handle deprecated "inline" → "touch:inline"
    let constraint = if constraint == "inline" {
        "touch:inline"
    } else {
        constraint
    };

    // Alignment constraints — store the full string in the Align variant.
    if constraint.starts_with("align") {
        return (Constraint::Align(constraint.to_string()), strip_newlines);
    }

    // Split on ':' to detect modifiers.
    let (base, modifier) = match constraint.split_once(':') {
        Some((b, m)) => (b, Some(m)),
        None => (constraint, None),
    };

    match modifier {
        None => {}
        Some("inline") => strip_newlines = true,
        Some(other) => {
            log::warn!("Unexpected constraint modifier: {:?}", other);
        }
    }

    let c = match base {
        "touch" => Constraint::Touch,
        "any" => Constraint::Any,
        _ => Constraint::Single,
    };
    (c, strip_newlines)
}

/// Determine spacing constraints from adjacent blocks.
///
/// Returns (pre_constraint, post_constraint, strip_newlines).
pub fn determine_constraints(
    prev_block: Option<&ReflowBlock>,
    next_block: Option<&ReflowBlock>,
    strip_newlines: bool,
) -> (Constraint, Constraint, bool) {
    let (mut pre_constraint, sn) = unpack_constraint(
        prev_block
            .map(|b| b.spacing_after.as_str())
            .unwrap_or("single"),
        strip_newlines,
    );
    let (mut post_constraint, sn2) = unpack_constraint(
        next_block
            .map(|b| b.spacing_before.as_str())
            .unwrap_or("single"),
        sn,
    );
    let mut strip_newlines = sn2;

    // Check for within_spacing from common ancestors.
    let mut within_spacing: Option<Constraint> = None;
    if let (Some(prev), Some(next)) = (prev_block, next_block) {
        if let Some(last_common) = prev.depth_info.last_common_hash(&next.depth_info) {
            if let Some(within_constraint) = prev.stack_spacing_configs.get(&last_common) {
                let (ws, sn3) = unpack_constraint(within_constraint, strip_newlines);
                within_spacing = Some(ws);
                strip_newlines = sn3;
            }
        }
    }

    // Apply within_spacing overrides.
    match within_spacing {
        Some(Constraint::Touch) => {
            if pre_constraint != Constraint::Any {
                pre_constraint = Constraint::Touch;
            }
            if post_constraint != Constraint::Any {
                post_constraint = Constraint::Touch;
            }
        }
        Some(Constraint::Any) => {
            pre_constraint = Constraint::Any;
            post_constraint = Constraint::Any;
        }
        Some(Constraint::Single) | None => {}
        Some(Constraint::Align(_)) => {
            // Alignment within-spacing — treat as single (no override).
        }
    }

    // Python parity: prohibit stripping newlines after comment segments.
    // (inline comments end their line with a newline — that newline must be
    // preserved even if `strip_newlines` would otherwise be true.)
    if strip_newlines {
        if let Some(prev) = prev_block {
            if prev.segment_type.contains("comment") {
                strip_newlines = false;
            }
        }
    }

    (pre_constraint, post_constraint, strip_newlines)
}

/// Process the segments in a point: basic pruning of trailing whitespace
/// and duplicate whitespace.
///
/// `is_literals[i]` is `true` when segment `i` has a source position that
/// matches its rendered position (i.e. it was not produced by a Jinja macro
/// expansion or other template substitution).  When a newline is *not*
/// literal the engine clears the trailing-whitespace buffer without emitting
/// a violation — mirroring Python's `pos_marker.is_literal()` guard in
/// `_process_spacing` ("Skipping templated newline").
///
/// Returns (cleaned segment types, cleaned raws, last_whitespace_point_idx,
/// violations).
pub fn process_spacing(
    segment_types: &[String],
    raws: &[String],
    raw_indices: &[usize],
    is_literals: &[bool],
    strip_newlines: bool,
) -> (
    Vec<String>,
    Vec<String>,
    Vec<usize>,
    Option<usize>, // last whitespace point-internal-index
    Vec<LintViolation>,
) {
    let mut out_types = Vec::with_capacity(segment_types.len());
    let mut out_raws = Vec::with_capacity(raws.len());
    let mut out_indices = Vec::with_capacity(raw_indices.len());
    let mut violations = Vec::with_capacity(segment_types.len());
    let mut last_whitespace: Vec<usize> = Vec::new(); // internal idx of ws segments
    let mut remove_flags = vec![false; segment_types.len()];

    for (i, seg_type) in segment_types.iter().enumerate() {
        if seg_type == "whitespace" {
            last_whitespace.push(i);
        } else if seg_type == "newline" || seg_type == "end_of_file" {
            // Mirror Python: if the newline is non-literal (came from a Jinja
            // macro expansion or similar template substitution), clear the
            // trailing-whitespace buffer without generating a violation.
            // The whitespace before it was valid in the source template even
            // though it appears to be trailing in the rendered output.
            let literal = is_literals.get(i).copied().unwrap_or(true);
            if !literal {
                last_whitespace.clear();
                continue;
            }

            if strip_newlines && seg_type == "newline" {
                remove_flags[i] = true;
                violations.push(LintViolation {
                    description: "Unexpected line break.".to_string(),
                    line_no: 0,
                    line_pos: 0,
                    fix_type: FixType::Delete,
                    edit_text: None,
                    anchor_idx: raw_indices.get(i).copied().unwrap_or(0),
                });
                continue;
            }

            // Remove trailing whitespace before literal newline.
            if !last_whitespace.is_empty() {
                for ws_idx in last_whitespace.drain(..) {
                    remove_flags[ws_idx] = true;
                    violations.push(LintViolation {
                        description: "Unnecessary trailing whitespace.".to_string(),
                        line_no: 0,
                        line_pos: 0,
                        fix_type: FixType::Delete,
                        edit_text: None,
                        anchor_idx: raw_indices.get(ws_idx).copied().unwrap_or(0),
                    });
                }
            }
        }
    }

    // Handle duplicate adjacent whitespace (keep only first).
    if last_whitespace.len() >= 2 {
        for &ws_idx in &last_whitespace[1..] {
            remove_flags[ws_idx] = true;
            violations.push(LintViolation {
                description: "Removing duplicate whitespace.".to_string(),
                line_no: 0,
                line_pos: 0,
                fix_type: FixType::Delete,
                edit_text: None,
                anchor_idx: raw_indices.get(ws_idx).copied().unwrap_or(0),
            });
        }
    }

    // Build cleaned output.
    let mut final_last_ws: Option<usize> = None;
    for (i, (seg_type, raw)) in segment_types.iter().zip(raws.iter()).enumerate() {
        if remove_flags[i] {
            continue;
        }
        let out_idx = out_types.len();
        out_types.push(seg_type.clone());
        out_raws.push(raw.clone());
        out_indices.push(raw_indices.get(i).copied().unwrap_or(0));
        if seg_type == "whitespace" {
            final_last_ws = Some(out_idx);
        }
    }

    (out_types, out_raws, out_indices, final_last_ws, violations)
}

/// Handle the inline case where whitespace exists between two blocks.
///
/// Returns new violations.
pub fn handle_inline_with_space(
    pre_constraint: &Constraint,
    post_constraint: &Constraint,
    next_block: Option<&ReflowBlock>,
    whitespace_raw: &str,
    whitespace_anchor_idx: usize,
) -> Vec<LintViolation> {
    // "any" → no change needed.
    if matches!(pre_constraint, Constraint::Any) || matches!(post_constraint, Constraint::Any) {
        return vec![];
    }

    // "touch" → delete the whitespace.
    if matches!(pre_constraint, Constraint::Touch) || matches!(post_constraint, Constraint::Touch) {
        let desc = if let Some(nb) = next_block {
            format!(
                "Unexpected whitespace before {}.",
                pretty_segment_name(&nb.segment_type, &nb.raw, &nb.class_types)
            )
        } else {
            "Unexpected whitespace.".to_string()
        };
        return vec![LintViolation {
            description: desc,
            line_no: 0,
            line_pos: 0,
            fix_type: FixType::Delete,
            edit_text: None,
            anchor_idx: whitespace_anchor_idx,
        }];
    }

    // "align:..." → alignment is handled separately (requires cross-line
    // tree traversal). Skip violation for existing whitespace here.
    if matches!(post_constraint, Constraint::Align(_))
        || matches!(pre_constraint, Constraint::Align(_))
    {
        return vec![];
    }

    let desired_space = " ".to_string();

    if whitespace_raw != desired_space {
        let desc = if let Some(nb) = next_block {
            format!(
                "Expected only single space before {}. Found {:?}.",
                pretty_segment_name(&nb.segment_type, &nb.raw, &nb.class_types),
                whitespace_raw
            )
        } else {
            format!(
                "Expected only single space. Found {:?}.",
                whitespace_raw
            )
        };
        return vec![LintViolation {
            description: desc,
            line_no: 0,
            line_pos: 0,
            fix_type: FixType::Replace {
                new_text: desired_space,
            },
            edit_text: Some(" ".to_string()),
            anchor_idx: whitespace_anchor_idx,
        }];
    }

    vec![]
}

/// Handle the inline case where NO whitespace exists between two blocks.
///
/// Returns new violations.
pub fn handle_inline_without_space(
    pre_constraint: &Constraint,
    post_constraint: &Constraint,
    prev_block: Option<&ReflowBlock>,
    next_block: Option<&ReflowBlock>,
) -> Vec<LintViolation> {
    // "touch" or "any" → no space needed.
    if matches!(pre_constraint, Constraint::Touch | Constraint::Any)
        || matches!(post_constraint, Constraint::Touch | Constraint::Any)
    {
        return vec![];
    }

    // "align" → insert single space (refined later).
    // "single" → insert single space.
    let desc = if let (Some(pb), Some(nb)) = (prev_block, next_block) {
        format!(
            "Expected single whitespace between {} and {}.",
            pretty_segment_name(&pb.segment_type, &pb.raw, &pb.class_types),
            pretty_segment_name(&nb.segment_type, &nb.raw, &nb.class_types)
        )
    } else {
        "Expected single whitespace.".to_string()
    };

    let _anchor_idx = if let Some(nb) = next_block {
        nb.raw_idx
    } else if let Some(pb) = prev_block {
        pb.raw_idx
    } else {
        0
    };

    // Decide whether to create_before (next) or create_after (prev).
    if let Some(pb) = prev_block {
        vec![LintViolation {
            description: desc,
            line_no: 0,
            line_pos: 0,
            fix_type: FixType::CreateAfter {
                new_text: " ".to_string(),
            },
            edit_text: Some(" ".to_string()),
            anchor_idx: pb.raw_idx,
        }]
    } else if let Some(nb) = next_block {
        vec![LintViolation {
            description: desc,
            line_no: 0,
            line_pos: 0,
            fix_type: FixType::CreateBefore {
                new_text: " ".to_string(),
            },
            edit_text: Some(" ".to_string()),
            anchor_idx: nb.raw_idx,
        }]
    } else {
        vec![]
    }
}

/// Construct a human-readable name for a segment.
///
/// Mirrors Python's `pretty_segment_name(segment)` from `helpers.py`.
fn pretty_segment_name(
    segment_type: &str,
    raw: &str,
    class_types: &[String],
) -> String {
    fn py_string_repr(raw: &str) -> String {
        let escaped = raw.replace('\\', "\\\\").replace('\'', "\\'");
        format!("'{}'", escaped)
    }

    // Mirror Python's pretty_segment_name() behavior from helpers.py:
    // - symbol: "<type with spaces> '<raw>'"
    // - keyword: "'<raw>' keyword"
    // - else: "<type with spaces>"
    let is_symbol = segment_type.contains("symbol")
        || class_types.iter().any(|ct| ct == "symbol");
    let is_keyword = segment_type.contains("keyword")
        || class_types.iter().any(|ct| ct == "keyword");

    if is_symbol {
        format!("{} {}", segment_type.replace('_', " "), py_string_repr(raw))
    } else if is_keyword {
        format!("{} keyword", py_string_repr(raw))
    } else {
        segment_type.replace('_', " ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unpack_constraint_simple() {
        let (c, sn) = unpack_constraint("single", false);
        assert_eq!(c, Constraint::Single);
        assert!(!sn);
    }

    #[test]
    fn test_unpack_constraint_touch_inline() {
        let (c, sn) = unpack_constraint("touch:inline", false);
        assert_eq!(c, Constraint::Touch);
        assert!(sn);
    }

    #[test]
    fn test_unpack_constraint_align() {
        let (c, sn) = unpack_constraint("align:keyword:select_clause", false);
        assert_eq!(
            c,
            Constraint::Align("align:keyword:select_clause".to_string())
        );
        assert!(!sn);
    }

    #[test]
    fn test_determine_constraints_no_blocks() {
        let (pre, post, sn) = determine_constraints(None, None, false);
        assert_eq!(pre, Constraint::Single);
        assert_eq!(post, Constraint::Single);
        assert!(!sn);
    }

    #[test]
    fn test_process_spacing_trailing_ws() {
        let types = vec![
            "whitespace".to_string(),
            "newline".to_string(),
        ];
        let raws = vec![" ".to_string(), "\n".to_string()];
        let indices = vec![0, 1];
        let is_literals = vec![true, true];
        let (out_types, out_raws, _, _, violations) =
            process_spacing(&types, &raws, &indices, &is_literals, false);
        // Whitespace before newline should be removed.
        assert_eq!(out_types, vec!["newline"]);
        assert_eq!(out_raws, vec!["\n"]);
        assert_eq!(violations.len(), 1);
        assert_eq!(
            violations[0].description,
            "Unnecessary trailing whitespace."
        );
    }

    /// Non-literal newlines (from Jinja macro expansion) must NOT trigger
    /// trailing-whitespace violations — the whitespace was valid in source.
    #[test]
    fn test_process_spacing_non_literal_newline_skipped() {
        let types = vec![
            "whitespace".to_string(),
            "newline".to_string(),
        ];
        let raws = vec!["        ".to_string(), "\n".to_string()];
        let indices = vec![0, 1];
        // The newline is non-literal (template macro expansion).
        let is_literals = vec![true, false];
        let (out_types, out_raws, _, _, violations) =
            process_spacing(&types, &raws, &indices, &is_literals, false);
        // No violation — the newline is from a template, not a source newline.
        assert!(violations.is_empty(), "Expected no violations, got: {violations:?}");
        assert_eq!(out_types, vec!["whitespace", "newline"]);
        assert_eq!(out_raws, vec!["        ", "\n"]);
    }

    #[test]
    fn test_inline_with_space_touch() {
        let violations =
            handle_inline_with_space(&Constraint::Touch, &Constraint::Single, None, " ", 0);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].fix_type, FixType::Delete);
    }

    #[test]
    fn test_inline_with_space_single_ok() {
        let violations =
            handle_inline_with_space(&Constraint::Single, &Constraint::Single, None, " ", 0);
        assert!(violations.is_empty());
    }

    #[test]
    fn test_inline_with_space_excess() {
        let violations =
            handle_inline_with_space(&Constraint::Single, &Constraint::Single, None, "  ", 0);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].description.contains("Expected only single space"));
    }

    #[test]
    fn test_inline_without_space_touch() {
        let violations =
            handle_inline_without_space(&Constraint::Touch, &Constraint::Single, None, None);
        assert!(violations.is_empty());
    }

    #[test]
    fn test_inline_without_space_single() {
        // With no prev/next blocks, no violation can be created (no anchor).
        let violations =
            handle_inline_without_space(&Constraint::Single, &Constraint::Single, None, None);
        assert!(violations.is_empty());
    }
}
