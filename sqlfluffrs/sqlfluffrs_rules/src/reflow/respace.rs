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

/// Unpack a constraint string, handling modifiers like `:inline`.
///
/// Returns (base_constraint, strip_newlines).
fn unpack_constraint(constraint: &str, mut strip_newlines: bool) -> (String, bool) {
    // Handle deprecated "inline" → "touch:inline"
    let constraint = if constraint == "inline" {
        "touch:inline"
    } else {
        constraint
    };

    // Alignment constraints pass through unchanged.
    if constraint.starts_with("align") {
        return (constraint.to_string(), strip_newlines);
    }

    // Split on ':'
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

    (base.to_string(), strip_newlines)
}

/// Determine spacing constraints from adjacent blocks.
///
/// Returns (pre_constraint, post_constraint, strip_newlines).
pub fn determine_constraints(
    prev_block: Option<&ReflowBlock>,
    next_block: Option<&ReflowBlock>,
    strip_newlines: bool,
) -> (String, String, bool) {
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
    let mut within_spacing = String::new();
    if let (Some(prev), Some(next)) = (prev_block, next_block) {
        let common = prev.depth_info.common_with(&next.depth_info);
        if let Some(last_common) = common.last() {
            if let Some(within_constraint) = prev.stack_spacing_configs.get(last_common) {
                let (ws, sn3) = unpack_constraint(within_constraint, strip_newlines);
                within_spacing = ws;
                strip_newlines = sn3;
            }
        }
    }

    // Apply within_spacing overrides.
    match within_spacing.as_str() {
        "touch" => {
            if pre_constraint != "any" {
                pre_constraint = "touch".to_string();
            }
            if post_constraint != "any" {
                post_constraint = "touch".to_string();
            }
        }
        "any" => {
            pre_constraint = "any".to_string();
            post_constraint = "any".to_string();
        }
        "single" | "" => {}
        other => {
            log::warn!("Unexpected within constraint: {:?}", other);
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
/// Returns (cleaned segment types, cleaned raws, last_whitespace_point_idx,
/// violations).
pub fn process_spacing(
    segment_types: &[String],
    raws: &[String],
    raw_indices: &[usize],
    strip_newlines: bool,
) -> (
    Vec<String>,
    Vec<String>,
    Vec<usize>,
    Option<usize>, // last whitespace point-internal-index
    Vec<LintViolation>,
) {
    let mut out_types = Vec::new();
    let mut out_raws = Vec::new();
    let mut out_indices = Vec::new();
    let mut violations = Vec::new();
    let mut last_whitespace: Vec<usize> = Vec::new(); // internal idx of ws segments
    let mut removal_set: Vec<usize> = Vec::new();

    for (i, seg_type) in segment_types.iter().enumerate() {
        if seg_type == "whitespace" {
            last_whitespace.push(i);
        } else if seg_type == "newline" || seg_type == "end_of_file" {
            if strip_newlines && seg_type == "newline" {
                removal_set.push(i);
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

            // Remove trailing whitespace before newline.
            if !last_whitespace.is_empty() {
                for &ws_idx in &last_whitespace {
                    removal_set.push(ws_idx);
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
            last_whitespace.clear();
        }
    }

    // Handle duplicate adjacent whitespace (keep only first).
    if last_whitespace.len() >= 2 {
        for &ws_idx in &last_whitespace[1..] {
            removal_set.push(ws_idx);
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
    let removal_set_hs: std::collections::HashSet<usize> =
        removal_set.into_iter().collect();
    let mut final_last_ws: Option<usize> = None;
    for (i, (seg_type, raw)) in segment_types.iter().zip(raws.iter()).enumerate() {
        if removal_set_hs.contains(&i) {
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
    pre_constraint: &str,
    post_constraint: &str,
    next_block: Option<&ReflowBlock>,
    whitespace_raw: &str,
    whitespace_anchor_idx: usize,
) -> Vec<LintViolation> {
    // "any" → no change needed.
    if pre_constraint == "any" || post_constraint == "any" {
        return vec![];
    }

    // "touch" → delete the whitespace.
    if pre_constraint == "touch" || post_constraint == "touch" {
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
    if post_constraint.starts_with("align") || pre_constraint.starts_with("align") {
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
    pre_constraint: &str,
    post_constraint: &str,
    prev_block: Option<&ReflowBlock>,
    next_block: Option<&ReflowBlock>,
) -> Vec<LintViolation> {
    // "touch" or "any" → no space needed.
    if ["touch", "any"].contains(&pre_constraint)
        || ["touch", "any"].contains(&post_constraint)
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
///
/// `class_types` is the full set of class types for the segment, which may
/// include the grammar-level type (e.g., `"binary_operator"`) even when
/// `segment_type` is the raw lexer type (e.g., `"raw"`).
/// Instance types from the Rust lexer that map to "binary operator" in Python.
/// These are the `instance_types` attached to lexer tokens for arithmetic/bitwise ops.
const BINARY_OP_INSTANCE_TYPES: &[&str] = &[
    "plus",       // +
    "minus",      // -
    "divide",     // /
    "modulo",     // %
    "concat",     // ||
    "ampersand",  // &
    "pipe",       // |
    "caret",      // ^
    "tilde",      // ~
    // NOTE: "star" can mean SELECT-star or multiply — only treated as
    // binary operator if context shows it, but here we handle "binary_operator"
    // class_types directly. "star" by itself stays "star".
];

/// Instance types from the Rust lexer that map to "comparison operator" in Python.
const COMPARISON_OP_INSTANCE_TYPES: &[&str] = &[
    "equals",     // =
    "gt",         // >
    "lt",         // <
    "gte",        // >=
    "lte",        // <=
    "ne",         // !=/<>
    "bang",       // !
];

/// Instance types from the Rust lexer that map to "quoted literal" in Python.
const QUOTED_LITERAL_INSTANCE_TYPES: &[&str] = &[
    "single_quote",   // 'text'
    "double_quote",   // "text"
    "back_quote",     // `text`
    "dollar_quote",   // $$text$$
    "escaped_single_quote", // e'text'
    "escaped_double_quote", // e"text"
];

fn pretty_segment_name(
    segment_type: &str,
    raw: &str,
    class_types: &std::collections::HashSet<String>,
) -> String {
    // Check class_types for grammar-level operator types (takes priority).
    // These appear when the Rust full-parser is used (class_types includes
    // "binary_operator", etc.).
    if class_types.contains("binary_operator") {
        return format!("binary operator '{}'", raw);
    }
    if class_types.contains("comparison_operator") {
        return format!("comparison operator '{}'", raw);
    }
    if class_types.contains("assignment_operator") {
        return format!("assignment operator '{}'", raw);
    }
    if class_types.contains("column_path_operator") {
        return format!("column path operator '{}'", raw);
    }
    // Also check for any "*_operator" pattern in class_types.
    for ct in class_types.iter() {
        if ct.ends_with("_operator") && ct != "binary_operator" {
            return format!("{} '{}'", ct.replace('_', " "), raw);
        }
    }

    // When the Rust LEXER (not full parser) produced the token, class_types
    // contains the lexer-level instance_types (e.g., "plus", "minus") rather
    // than the Python-parser level types (e.g., "binary_operator").
    // Map these back to their Python pretty-names.
    if segment_type == "raw" {
        // Check for binary operator instance types.
        if class_types
            .iter()
            .any(|ct| BINARY_OP_INSTANCE_TYPES.contains(&ct.as_str()))
        {
            return format!("binary operator '{}'", raw);
        }
        // Check for comparison operator instance types.
        if class_types
            .iter()
            .any(|ct| COMPARISON_OP_INSTANCE_TYPES.contains(&ct.as_str()))
        {
            return format!("comparison operator '{}'", raw);
        }
        // Check for quoted literal instance types.
        if class_types
            .iter()
            .any(|ct| QUOTED_LITERAL_INSTANCE_TYPES.contains(&ct.as_str()))
        {
            return "quoted literal".to_string();
        }
    }

    // Fall back to segment_type-based logic (mirrors Python's is_type checks).
    if segment_type.contains("keyword") || segment_type == "keyword" {
        format!("{:?} keyword", raw)
    } else if segment_type.contains("symbol") {
        format!("{} {:?}", segment_type.replace('_', " "), raw)
    } else if segment_type == "raw" {
        // For raw tokens, try to use a meaningful class type.
        // Filter out generic types ("raw", "base", "code").
        let meaningful: Vec<_> = class_types
            .iter()
            .filter(|ct| !matches!(ct.as_str(), "raw" | "base" | "code"))
            .collect();
        if let Some(ct) = meaningful.first() {
            ct.replace('_', " ")
        } else {
            segment_type.replace('_', " ")
        }
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
        assert_eq!(c, "single");
        assert!(!sn);
    }

    #[test]
    fn test_unpack_constraint_touch_inline() {
        let (c, sn) = unpack_constraint("touch:inline", false);
        assert_eq!(c, "touch");
        assert!(sn);
    }

    #[test]
    fn test_unpack_constraint_align() {
        let (c, sn) = unpack_constraint("align:keyword:select_clause", false);
        assert_eq!(c, "align:keyword:select_clause");
        assert!(!sn);
    }

    #[test]
    fn test_determine_constraints_no_blocks() {
        let (pre, post, sn) = determine_constraints(None, None, false);
        assert_eq!(pre, "single");
        assert_eq!(post, "single");
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
        let (out_types, out_raws, _, _, violations) =
            process_spacing(&types, &raws, &indices, false);
        // Whitespace before newline should be removed.
        assert_eq!(out_types, vec!["newline"]);
        assert_eq!(out_raws, vec!["\n"]);
        assert_eq!(violations.len(), 1);
        assert_eq!(
            violations[0].description,
            "Unnecessary trailing whitespace."
        );
    }

    #[test]
    fn test_inline_with_space_touch() {
        let violations = handle_inline_with_space("touch", "single", None, " ", 0);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].fix_type, FixType::Delete);
    }

    #[test]
    fn test_inline_with_space_single_ok() {
        let violations =
            handle_inline_with_space("single", "single", None, " ", 0);
        assert!(violations.is_empty());
    }

    #[test]
    fn test_inline_with_space_excess() {
        let violations =
            handle_inline_with_space("single", "single", None, "  ", 0);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].description.contains("Expected only single space"));
    }

    #[test]
    fn test_inline_without_space_touch() {
        let violations =
            handle_inline_without_space("touch", "single", None, None);
        assert!(violations.is_empty());
    }

    #[test]
    fn test_inline_without_space_single() {
        // With no prev/next blocks, no violation can be created (no anchor).
        let violations =
            handle_inline_without_space("single", "single", None, None);
        assert!(violations.is_empty());
    }
}
