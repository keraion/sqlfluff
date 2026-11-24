//! PyO3 bindings for the Rust-native rules.
//!
//! Each rule's binding sits next to its detection logic; the root crate just
//! calls [`register`]. Bindings are read-only over the arena and return compact
//! results the Python side anchors fixes from.

use std::collections::HashSet;

use pyo3::prelude::*;
use sqlfluffrs_parser::PyTree;

/// Experimental: detect CP01 (keyword capitalisation) violations natively.
///
/// Runs CP01's detection loop entirely in Rust over the arena read API and
/// returns `(leaf_index, fixed_raw)` for each keyword/operator node needing a
/// fix — `leaf_index` is the position in the depth-first leaf order, 1:1 with
/// Python's `raw_segments`. One FFI crossing for the whole result; the caller
/// anchors via `raw_segments[leaf_index]`. Read-only (no arena mutation).
#[pyfunction]
#[pyo3(signature = (tree, policy, ignore_words=vec![], ignore_templated=false))]
fn cp01_violations(
    tree: &PyTree,
    policy: &str,
    ignore_words: Vec<String>,
    ignore_templated: bool,
) -> Vec<(usize, String)> {
    let ignore: HashSet<String> = ignore_words.into_iter().collect();
    tree.with_arena(|arena| crate::cp01::cp01_violations(arena, policy, &ignore, ignore_templated))
}

/// Experimental: detect CP03 (function-name capitalisation) violations
/// natively. Same contract as [`cp01_violations`]; `policy` additionally
/// accepts `pascal`/`camel`/`snake` (CP03 reads `extended_capitalisation_policy`).
#[pyfunction]
#[pyo3(signature = (tree, policy, ignore_words=vec![], ignore_templated=false))]
fn cp03_violations(
    tree: &PyTree,
    policy: &str,
    ignore_words: Vec<String>,
    ignore_templated: bool,
) -> Vec<(usize, String)> {
    let ignore: HashSet<String> = ignore_words.into_iter().collect();
    tree.with_arena(|arena| crate::cp03::cp03_violations(arena, policy, &ignore, ignore_templated))
}

/// Experimental: detect CP04 (boolean/null literal capitalisation) violations
/// natively. Same contract as [`cp01_violations`].
#[pyfunction]
#[pyo3(signature = (tree, policy, ignore_words=vec![], ignore_templated=false))]
fn cp04_violations(
    tree: &PyTree,
    policy: &str,
    ignore_words: Vec<String>,
    ignore_templated: bool,
) -> Vec<(usize, String)> {
    let ignore: HashSet<String> = ignore_words.into_iter().collect();
    tree.with_arena(|arena| crate::cp04::cp04_violations(arena, policy, &ignore, ignore_templated))
}

type Layout = std::collections::HashMap<String, std::collections::HashMap<String, String>>;

thread_local! {
    /// SPIKE: the sequence for the most recently linted tree, so the four
    /// reflow rules build it once per file between them. Keyed by the root
    /// node's uuid (unique per arena) and the layout config it was built with.
    static LAST_SEQUENCE: std::cell::RefCell<
        Option<(u128, Layout, std::rc::Rc<(crate::reflow::ReflowConfig, crate::reflow::ReflowSequence)>)>,
    > = const { std::cell::RefCell::new(None) };
}

/// Run `f` with the tree's reflow sequence, building it only if the last
/// call was for a different tree or layout.
fn with_sequence<R>(
    tree: &PyTree,
    layout: Layout,
    f: impl FnOnce(
        &sqlfluffrs_parser::Arena,
        &crate::reflow::ReflowConfig,
        &crate::reflow::ReflowSequence,
    ) -> R,
) -> R {
    tree.with_arena(|arena| {
        let key = arena.uuid(arena.root());
        let cached = LAST_SEQUENCE.with(|c| {
            c.borrow()
                .as_ref()
                .filter(|(k, l, _)| *k == key && *l == layout)
                .map(|(_, _, s)| s.clone())
        });
        let entry = cached.unwrap_or_else(|| {
            let config = crate::reflow::ReflowConfig::from_layout(layout.clone());
            let seq = crate::reflow::build(arena, &config);
            let entry = std::rc::Rc::new((config, seq));
            LAST_SEQUENCE.with(|c| *c.borrow_mut() = Some((key, layout, entry.clone())));
            entry
        });
        f(arena, &entry.0, &entry.1)
    })
}

/// SPIKE: build the reflow sequence (depth map, block config, elements)
/// natively. Returns the element count, or with `digest` a per-element
/// summary for checking against Python's `ReflowSequence.from_root`.
#[pyfunction]
#[pyo3(signature = (tree, layout, digest=false))]
#[allow(clippy::type_complexity)]
fn reflow_build(
    tree: &PyTree,
    layout: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    digest: bool,
) -> (usize, Vec<crate::reflow::Digest>) {
    let config = crate::reflow::ReflowConfig::from_layout(layout);
    tree.with_arena(|arena| {
        let seq = crate::reflow::build(arena, &config);
        let d = if digest { seq.digest() } else { Vec::new() };
        (seq.elements.len(), d)
    })
}

/// SPIKE: LT01 natively. Builds the reflow sequence and respaces it, returning
/// `(anchor_leaf, description, fix_kind, fix_anchor_leaf, edit)` per result,
/// with leaves indexed as Python's `raw_segments`. `None` means the file uses
/// something this doesn't cover and Python should lint it.
#[pyfunction]
#[allow(clippy::type_complexity)]
fn reflow_respace(
    tree: &PyTree,
    layout: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
) -> Option<Vec<(usize, String, &'static str, usize, String)>> {
    with_sequence(tree, layout, |arena, _config, seq| {
        let results = crate::reflow::respace(arena, seq)?;
        let leaf: std::collections::HashMap<_, _> = seq
            .depth_map
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (*id, i))
            .collect();
        Some(
            results
                .into_iter()
                .map(|r| {
                    (
                        leaf[&r.anchor],
                        r.description,
                        r.kind.as_str(),
                        leaf[&r.fix_anchor],
                        r.edit,
                    )
                })
                .collect(),
        )
    })
}

/// SPIKE: LT02 natively. Returns `(anchor_leaf, description, source,
/// [(fix_kind, target_leaf, text)])` per result, or `None` when Python should
/// lint the file (templated files, and config this doesn't cover).
#[pyfunction]
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn reflow_reindent(
    tree: &PyTree,
    layout: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    indent_unit: String,
    tab_space_size: usize,
    implicit_indents: String,
    ignore_comment_lines: bool,
    skip_indentation_in: Vec<String>,
) -> Option<PyIndentResults> {
    let options = crate::reflow::ReindentOptions {
        indent_unit,
        tab_space_size,
        implicit_indents,
        ignore_comment_lines,
        skip_indentation_in,
    };
    with_sequence(tree, layout, |arena, config, seq| {
        let results = crate::reflow::reindent(arena, config, seq, &options)?;
        Some(indent_results_to_py(seq, results))
    })
}

type PyIndentResults = Vec<(
    usize,
    String,
    &'static str,
    Vec<(&'static str, usize, String, Option<usize>)>,
)>;

fn indent_results_to_py(
    seq: &crate::reflow::ReflowSequence,
    results: Vec<crate::reflow::IndentResult>,
) -> PyIndentResults {
    let leaf: std::collections::HashMap<_, _> = seq
        .depth_map
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (*id, i))
        .collect();
    results
        .into_iter()
        .map(|r| {
            (
                leaf[&r.anchor],
                r.description,
                r.source,
                r.fixes
                    .into_iter()
                    .map(|f| {
                        (
                            f.kind.as_str(),
                            leaf[&f.target],
                            f.text,
                            f.extra.map(|e| leaf[&e]),
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}

/// SPIKE: LT05 natively, for untemplated files. Same result shape as
/// `reflow_reindent`. `None` means Python should lint the file.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn reflow_long_lines(
    tree: &PyTree,
    layout: Layout,
    indent_unit: String,
    tab_space_size: usize,
    implicit_indents: String,
    trailing_comments: String,
    list_wrapping: String,
    max_line_length: i64,
) -> Option<PyIndentResults> {
    let options = crate::reflow::LongLineOptions {
        indent_unit,
        tab_space_size,
        implicit_indents,
        trailing_comments,
        list_wrapping,
        max_line_length,
    };
    with_sequence(tree, layout, |arena, _config, seq| {
        let results = crate::reflow::break_long_lines(arena, seq, &options)?;
        Some(indent_results_to_py(seq, results))
    })
}

/// SPIKE: LT14 natively, for untemplated files. Same result shape as
/// `reflow_reindent`. `None` means Python should lint the file.
#[pyfunction]
fn reflow_rebreak_keywords(tree: &PyTree, layout: Layout) -> Option<PyIndentResults> {
    with_sequence(tree, layout, |arena, _config, seq| {
        let results = crate::reflow::rebreak_keywords(arena, seq)?;
        Some(indent_results_to_py(seq, results))
    })
}

/// Register this crate's rule bindings on the extension module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(cp01_violations, m)?)?;
    m.add_function(wrap_pyfunction!(cp03_violations, m)?)?;
    m.add_function(wrap_pyfunction!(cp04_violations, m)?)?;
    m.add_function(wrap_pyfunction!(reflow_build, m)?)?;
    m.add_function(wrap_pyfunction!(reflow_respace, m)?)?;
    m.add_function(wrap_pyfunction!(reflow_reindent, m)?)?;
    m.add_function(wrap_pyfunction!(reflow_long_lines, m)?)?;
    m.add_function(wrap_pyfunction!(reflow_rebreak_keywords, m)?)?;
    Ok(())
}
