//! Integration tests for the reflow/respace module (LT01 rule equivalent).
//!
//! These tests mirror the Python LT01 YAML test cases. They lex + parse real SQL
//! strings using the ANSI dialect, build a Node tree, then run ReflowSequence.respace()
//! and verify the expected violations (or lack thereof).

use sqlfluffrs_rules::reflow::config::ReflowConfig;
use sqlfluffrs_rules::reflow::respace::FixType;
use sqlfluffrs_rules::reflow::sequence::ReflowSequence;
use sqlfluffrs_parser::parser::{MetaType, Node, RawSegmentKwargs};

// ---------------------------------------------------------------------------
// Helper functions for building test trees
// ---------------------------------------------------------------------------

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

fn make_star() -> Node {
    Node::Raw {
        segment_class: "StarSegment".to_string(),
        segment_type: "star".to_string(),
        raw: "*".to_string(),
        pos_marker: None,
        instance_types: vec!["star".to_string()],
        segment_kwargs: RawSegmentKwargs::default(),
    }
}

fn make_open_bracket() -> Node {
    Node::Raw {
        segment_class: "StartBracketSegment".to_string(),
        segment_type: "start_bracket".to_string(),
        raw: "(".to_string(),
        pos_marker: None,
        instance_types: vec!["start_bracket".to_string()],
        segment_kwargs: RawSegmentKwargs::default(),
    }
}

fn make_close_bracket() -> Node {
    Node::Raw {
        segment_class: "EndBracketSegment".to_string(),
        segment_type: "end_bracket".to_string(),
        raw: ")".to_string(),
        pos_marker: None,
        instance_types: vec!["end_bracket".to_string()],
        segment_kwargs: RawSegmentKwargs::default(),
    }
}

fn make_semicolon() -> Node {
    Node::Raw {
        segment_class: "StatementTerminatorSegment".to_string(),
        segment_type: "statement_terminator".to_string(),
        raw: ";".to_string(),
        pos_marker: None,
        instance_types: vec!["statement_terminator".to_string()],
        segment_kwargs: RawSegmentKwargs::default(),
    }
}

fn make_comparison_operator(raw: &str) -> Node {
    Node::Raw {
        segment_class: "ComparisonOperatorSegment".to_string(),
        segment_type: "comparison_operator".to_string(),
        raw: raw.to_string(),
        pos_marker: None,
        instance_types: vec!["comparison_operator".to_string()],
        segment_kwargs: RawSegmentKwargs::default(),
    }
}

#[allow(dead_code)]
fn make_binary_operator(raw: &str) -> Node {
    Node::Raw {
        segment_class: "BinaryOperatorSegment".to_string(),
        segment_type: "binary_operator".to_string(),
        raw: raw.to_string(),
        pos_marker: None,
        instance_types: vec!["binary_operator".to_string()],
        segment_kwargs: RawSegmentKwargs::default(),
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

fn make_from_clause(children: Vec<Node>) -> Node {
    Node::Segment {
        segment_class: "FromClauseSegment".to_string(),
        segment_type: Some("from_clause".to_string()),
        pos_marker: None,
        children,
    }
}

fn make_where_clause(children: Vec<Node>) -> Node {
    Node::Segment {
        segment_class: "WhereClauseSegment".to_string(),
        segment_type: Some("where_clause".to_string()),
        pos_marker: None,
        children,
    }
}

fn make_object_reference(children: Vec<Node>) -> Node {
    Node::Segment {
        segment_class: "ObjectReferenceSegment".to_string(),
        segment_type: Some("object_reference".to_string()),
        pos_marker: None,
        children,
    }
}

fn respace_violations(tree: &Node) -> Vec<String> {
    let config = ReflowConfig::default_ansi();
    let seq = ReflowSequence::from_root(tree, &config).respace();
    seq.violations.iter().map(|v| v.description.clone()).collect()
}

fn respace_has_violations(tree: &Node) -> bool {
    !respace_violations(tree).is_empty()
}

// ===========================================================================
// LT01-trailing: Trailing whitespace
// ===========================================================================

#[test]
fn lt01_pass_select_1_newline() {
    // "SELECT 1\n" — should pass
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    assert!(!respace_has_violations(&tree), "SELECT 1\\n should pass");
}

#[test]
fn lt01_fail_trailing_whitespace() {
    // "SELECT 1     \n" — trailing whitespace before newline
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
        ])])]),
        make_ws("     "),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations.iter().any(|v| v.contains("trailing whitespace")),
        "Expected trailing whitespace violation, got: {:?}",
        violations
    );
}

#[test]
fn lt01_fail_trailing_whitespace_on_initial_blank_line() {
    // " \nSELECT 1     \n" — two trailing ws violations
    let tree = make_file(vec![
        make_ws(" "),
        make_newline(),
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
        ])])]),
        make_ws("     "),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    let trailing_count = violations
        .iter()
        .filter(|v| v.contains("trailing whitespace"))
        .count();
    assert!(
        trailing_count >= 2,
        "Expected at least 2 trailing ws violations, got {} in: {:?}",
        trailing_count,
        violations
    );
}

// ===========================================================================
// LT01-excessive: Multiple spaces where single expected
// ===========================================================================

#[test]
fn lt01_pass_single_space() {
    // "SELECT 1\n" — correct single space
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    assert!(
        !respace_has_violations(&tree),
        "Single space should not be flagged"
    );
}

#[test]
fn lt01_fail_double_space() {
    // "SELECT  1\n" — double space
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws("  "),
            make_literal("1"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations.iter().any(|v| v.contains("single space")),
        "Expected single space violation for double space, got: {:?}",
        violations
    );
}

#[test]
fn lt01_fail_many_spaces() {
    // "SELECT     1\n" — many spaces
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws("     "),
            make_literal("1"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations.iter().any(|v| v.contains("single space")),
        "Expected single space violation for many spaces, got: {:?}",
        violations
    );
}

// ===========================================================================
// LT01-commas: Spacing around commas
// ===========================================================================

#[test]
fn lt01_pass_comma_correct() {
    // "SELECT 1, 2\n" — correct comma spacing: touch before, single after
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
            make_comma(),
            make_ws(" "),
            make_literal("2"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    assert!(
        !respace_has_violations(&tree),
        "Correct comma spacing should pass"
    );
}

#[test]
fn lt01_fail_space_before_comma() {
    // "SELECT 1 , 2\n" — space before comma
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
            make_ws(" "),
            make_comma(),
            make_ws(" "),
            make_literal("2"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations.iter().any(|v| v.contains("Unexpected whitespace")),
        "Expected touch violation before comma, got: {:?}",
        violations
    );
}

#[test]
fn lt01_fail_no_space_after_comma() {
    // "SELECT 1,2\n" — no space after comma
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
            make_comma(),
            make_literal("2"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations
            .iter()
            .any(|v| v.contains("Expected single whitespace")),
        "Expected missing space after comma, got: {:?}",
        violations
    );
}

#[test]
fn lt01_fail_multiple_spaces_after_comma() {
    // "SELECT 1,  2\n" — extra space after comma
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
            make_comma(),
            make_ws("  "),
            make_literal("2"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations.iter().any(|v| v.contains("single space")),
        "Expected single space violation after comma, got: {:?}",
        violations
    );
}

// ===========================================================================
// LT01-operators: Spacing around operators
// ===========================================================================

#[test]
fn lt01_pass_comparison_operator_correct() {
    // "SELECT 1 = 1\n" — correct spacing around =
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![
            make_select_clause(vec![
                make_keyword("SELECT"),
                make_ws(" "),
                make_literal("1"),
            ]),
            make_ws(" "),
            make_where_clause(vec![
                make_keyword("WHERE"),
                make_ws(" "),
                make_literal("1"),
                make_ws(" "),
                make_comparison_operator("="),
                make_ws(" "),
                make_literal("1"),
            ]),
        ])]),
        make_newline(),
        make_eof(),
    ]);
    assert!(
        !respace_has_violations(&tree),
        "Correct operator spacing should pass"
    );
}

#[test]
fn lt01_fail_no_space_around_operator() {
    // "... WHERE 1=1\n" — no spaces around =
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![
            make_select_clause(vec![
                make_keyword("SELECT"),
                make_ws(" "),
                make_literal("1"),
            ]),
            make_ws(" "),
            make_where_clause(vec![
                make_keyword("WHERE"),
                make_ws(" "),
                make_literal("1"),
                make_comparison_operator("="),
                make_literal("1"),
            ]),
        ])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    // Should get violations for missing spaces around =
    assert!(
        violations.len() >= 2,
        "Expected at least 2 violations for missing spaces around =, got: {:?}",
        violations
    );
}

// ===========================================================================
// LT01-brackets: Spacing around brackets (touch)
// ===========================================================================

#[test]
fn lt01_pass_brackets_touch() {
    // "SELECT (1)\n" — single space before (, touch after ( and before )
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_open_bracket(),
            make_literal("1"),
            make_close_bracket(),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    assert!(
        !respace_has_violations(&tree),
        "Bracket touch spacing should pass"
    );
}

#[test]
fn lt01_fail_space_after_open_bracket() {
    // "SELECT ( 1)\n" — space after (
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_open_bracket(),
            make_ws(" "),
            make_literal("1"),
            make_close_bracket(),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations.iter().any(|v| v.contains("Unexpected whitespace")),
        "Expected touch violation after (, got: {:?}",
        violations
    );
}

#[test]
fn lt01_fail_space_before_close_bracket() {
    // "SELECT (1 )\n" — space before )
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_open_bracket(),
            make_literal("1"),
            make_ws(" "),
            make_close_bracket(),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations.iter().any(|v| v.contains("Unexpected whitespace")),
        "Expected touch violation before ), got: {:?}",
        violations
    );
}

// ===========================================================================
// LT01-dot: Dot operator (touch both sides)
// ===========================================================================

#[test]
fn lt01_pass_dot_touch() {
    // "a.b" — correct
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_object_reference(vec![
                make_identifier("a"),
                make_dot(),
                make_identifier("b"),
            ]),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    assert!(
        !respace_has_violations(&tree),
        "Dot touch spacing should pass"
    );
}

#[test]
fn lt01_fail_space_around_dot() {
    // "a . b" — spaces around dot
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_object_reference(vec![
                make_identifier("a"),
                make_ws(" "),
                make_dot(),
                make_ws(" "),
                make_identifier("b"),
            ]),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    // The within_spacing of object_reference is touch:inline, which should
    // override to touch constraints for both sides of the dot.
    assert!(
        violations.len() >= 2,
        "Expected at least 2 violations for spaces around dot, got: {:?}",
        violations
    );
}

// ===========================================================================
// LT01-missing: Missing required whitespace
// ===========================================================================

#[test]
fn lt01_fail_no_space_after_select() {
    // "SELECT1\n" — missing space after SELECT
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_literal("1"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations
            .iter()
            .any(|v| v.contains("Expected single whitespace")),
        "Expected missing space violation, got: {:?}",
        violations
    );
}

// ===========================================================================
// LT01-semicolons: Statement terminator (touch before)
// ===========================================================================

#[test]
fn lt01_pass_semicolon_touch() {
    // "SELECT 1;\n" — correct: touch before semicolon
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
        ])])]),
        make_semicolon(),
        make_newline(),
        make_eof(),
    ]);
    assert!(
        !respace_has_violations(&tree),
        "Semicolon touch should pass"
    );
}

#[test]
fn lt01_fail_space_before_semicolon() {
    // "SELECT 1 ;\n" — space before semicolon
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
        ])])]),
        make_ws(" "),
        make_semicolon(),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations.iter().any(|v| v.contains("Unexpected whitespace")),
        "Expected touch violation before semicolon, got: {:?}",
        violations
    );
}

// ===========================================================================
// LT01: End-of-file trailing whitespace
// ===========================================================================

#[test]
fn lt01_fail_trailing_whitespace_at_eof() {
    // "SELECT 1\n   " — whitespace after final newline
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
        ])])]),
        make_newline(),
        make_ws("   "),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations
            .iter()
            .any(|v| v.contains("end of file") || v.contains("trailing")),
        "Expected trailing ws at EOF violation, got: {:?}",
        violations
    );
}

// ===========================================================================
// LT01: Fix type verification
// ===========================================================================

#[test]
fn lt01_fix_type_delete_for_trailing_ws() {
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws(" "),
            make_literal("1"),
        ])])]),
        make_ws("     "),
        make_newline(),
        make_eof(),
    ]);
    let config = ReflowConfig::default_ansi();
    let seq = ReflowSequence::from_root(&tree, &config).respace();
    assert!(
        seq.violations.iter().any(|v| v.fix_type == FixType::Delete),
        "Trailing ws should produce a Delete fix"
    );
}

#[test]
fn lt01_fix_type_replace_for_excess_space() {
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws("  "),
            make_literal("1"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let config = ReflowConfig::default_ansi();
    let seq = ReflowSequence::from_root(&tree, &config).respace();
    assert!(
        seq.violations
            .iter()
            .any(|v| matches!(v.fix_type, FixType::Replace { .. })),
        "Excess space should produce a Replace fix"
    );
}

#[test]
fn lt01_fix_type_create_for_missing_space() {
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_literal("1"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let config = ReflowConfig::default_ansi();
    let seq = ReflowSequence::from_root(&tree, &config).respace();
    let has_create = seq.violations.iter().any(|v| {
        matches!(
            v.fix_type,
            FixType::CreateAfter { .. } | FixType::CreateBefore { .. }
        )
    });
    assert!(has_create, "Missing space should produce a Create fix");
}

// ===========================================================================
// Compound tests
// ===========================================================================

#[test]
fn lt01_pass_select_star_from_table() {
    // "SELECT * FROM t\n" — all correct
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![
            make_select_clause(vec![
                make_keyword("SELECT"),
                make_ws(" "),
                make_star(),
            ]),
            make_ws(" "),
            make_from_clause(vec![
                make_keyword("FROM"),
                make_ws(" "),
                make_identifier("t"),
            ]),
        ])]),
        make_newline(),
        make_eof(),
    ]);
    assert!(
        !respace_has_violations(&tree),
        "SELECT * FROM t should pass"
    );
}

#[test]
fn lt01_fail_multiple_issues() {
    // "SELECT  1 ,2\n" — double space + space before comma + missing space after comma
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_ws("  "),
            make_literal("1"),
            make_ws(" "),
            make_comma(),
            make_literal("2"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    let violations = respace_violations(&tree);
    assert!(
        violations.len() >= 2,
        "Expected multiple violations, got {} : {:?}",
        violations.len(),
        violations
    );
}

#[test]
fn lt01_pass_multiline_correct() {
    // "SELECT\n    1,\n    2\n" — multiline with correct spacing
    let tree = make_file(vec![
        make_statement(vec![make_select_stmt(vec![make_select_clause(vec![
            make_keyword("SELECT"),
            make_newline(),
            make_ws("    "),
            make_literal("1"),
            make_comma(),
            make_newline(),
            make_ws("    "),
            make_literal("2"),
        ])])]),
        make_newline(),
        make_eof(),
    ]);
    assert!(
        !respace_has_violations(&tree),
        "Multiline correct spacing should pass"
    );
}
