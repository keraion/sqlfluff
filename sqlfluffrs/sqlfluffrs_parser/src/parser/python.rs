use hashbrown::HashMap;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};

use crate::parser::MetaSegment;

use super::match_result::MatchResult;
use super::types::NodeTupleValue;
use super::{Node, ParseError, Parser};
use sqlfluffrs_dialects::Dialect;
use sqlfluffrs_python::pyo3_helpers::{pylist_of_str_pairs, pylist_of_strs, pytuple_of_strs};
use sqlfluffrs_python::token::PyToken;
use sqlfluffrs_types::Token;
use std::str::FromStr;

// Create a custom Python exception for parse errors with position info
pyo3::create_exception!(
    sqlfluffrs,
    RsParseError,
    PyException,
    "Rust parser error with position information"
);

/// Helper to convert ParseError to Python exception with position attribute
fn parse_error_to_pyerr(e: ParseError) -> PyErr {
    Python::attach(|py| {
        // Create the exception
        let exc = RsParseError::new_err(e.message.clone());

        // If we have position info, set it as an attribute
        if let Some(pos) = e.pos {
            let _ = exc.value(py).setattr("pos", pos);
        }

        exc
    })
}

/// Python-wrapped Node for AST representation
#[pyclass(name = "RsNode", module = "sqlfluffrs", from_py_object)]
#[derive(Clone)]
pub struct PyNode(pub Node);

#[pymethods]
impl PyNode {
    /// Get the node type as a string
    #[getter]
    fn node_type(&self) -> String {
        match &self.0 {
            Node::Raw { .. } => "raw".to_string(),
            Node::Segment { .. } => "segment".to_string(),
            Node::Meta { .. } => "meta".to_string(),
            Node::Unparsable { .. } => "unparsable".to_string(),
            Node::Empty => "empty".to_string(),
        }
    }

    /// Get the segment type (semantic type like "keyword", "select_statement", etc.)
    #[getter]
    fn segment_type(&self) -> Option<String> {
        self.0.segment_type().map(|s| s.to_string())
    }

    /// Get the segment class name (e.g., "KeywordSegment", "SelectStatementSegment")
    #[getter]
    fn segment_class(&self) -> Option<String> {
        match &self.0 {
            Node::Raw { segment_class, .. } | Node::Segment { segment_class, .. } => {
                Some(segment_class.to_string())
            }
            _ => None,
        }
    }

    /// Get raw text of this node (recursively joins children for containers)
    #[getter]
    fn raw(&self) -> String {
        self.0.raw().to_owned()
    }

    /// Check if node is empty
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Check if node is code (not whitespace/meta)
    fn is_code(&self) -> bool {
        self.0.is_code()
    }

    /// Get children nodes (for Segment and Unparsable nodes)
    fn children(&self) -> Option<Vec<PyNode>> {
        match &self.0 {
            Node::Segment { children, .. } | Node::Unparsable { children, .. } => {
                Some(children.iter().map(|n| PyNode(n.clone())).collect())
            }
            _ => None,
        }
    }

    /// Get instance_types (for Raw nodes)
    fn instance_types<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyList>>> {
        match &self.0 {
            Node::Raw { instance_types, .. } => Ok(Some(pylist_of_strs(py, instance_types)?)),
            _ => Ok(None),
        }
    }

    /// Get class_types — mirrors Python's class_types property.
    fn class_types<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyList>>> {
        match &self.0 {
            Node::Raw { class_types, .. } => Ok(Some(pylist_of_strs(py, class_types)?)),
            Node::Segment { class_types, .. } => {
                if class_types.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(pylist_of_strs(py, class_types)?))
                }
            }
            _ => Ok(None),
        }
    }

    /// Convert to tuple representation (mirrors Python's to_tuple)
    #[pyo3(signature = (code_only=false, show_raw=false, include_meta=false))]
    fn to_tuple(
        &self,
        py: Python,
        code_only: bool,
        show_raw: bool,
        include_meta: bool,
    ) -> PyResult<Py<PyAny>> {
        let tuple_val = self.0.to_tuple(code_only, show_raw, include_meta);
        Self::tuple_value_to_python(py, &tuple_val)
    }

    /// Get record representation (for YAML serialization)
    #[pyo3(signature = (code_only=false, show_raw=false, include_meta=false))]
    fn as_record(
        &self,
        py: Python,
        code_only: bool,
        show_raw: bool,
        include_meta: bool,
    ) -> PyResult<Option<Py<PyAny>>> {
        match self.0.as_record(code_only, show_raw, include_meta) {
            Some(yaml_val) => {
                let py_obj = Self::yaml_to_python(py, &yaml_val)?;
                Ok(Some(py_obj))
            }
            None => Ok(None),
        }
    }

    /// Represent node as string
    fn __repr__(&self) -> String {
        match &self.0 {
            Node::Raw {
                segment_type, raw, ..
            } => {
                format!("RsNode(Raw(type='{}', raw='{}'))", segment_type, raw)
            }
            Node::Segment {
                segment_class,
                children,
                ..
            } => {
                format!(
                    "RsNode(Segment(class='{}', {} children))",
                    segment_class,
                    children.len()
                )
            }
            Node::Meta { meta_type, .. } => {
                format!("RsNode(Meta({:?}))", meta_type)
            }
            Node::Unparsable {
                expected, children, ..
            } => {
                format!(
                    "RsNode(Unparsable(expected='{}', {} children))",
                    expected,
                    children.len()
                )
            }
            Node::Empty => "RsNode(Empty)".to_string(),
        }
    }

    /// String representation
    fn __str__(&self) -> String {
        self.__repr__()
    }
}

impl PyNode {
    fn tuple_value_to_python(py: Python, val: &NodeTupleValue) -> PyResult<Py<PyAny>> {
        match val {
            NodeTupleValue::Raw(key, s) => Ok((key, s).into_pyobject(py)?.into()),
            NodeTupleValue::Tuple(key, children) => {
                let py_children = PyList::empty(py);
                for child in children {
                    py_children.append(Self::tuple_value_to_python(py, child)?)?;
                }
                Ok((key, py_children).into_pyobject(py)?.into())
            }
        }
    }

    fn yaml_to_python(py: Python, val: &serde_yaml_ng::Value) -> PyResult<Py<PyAny>> {
        use serde_yaml_ng::Value;
        match val {
            Value::Null => Ok(py.None()),
            Value::Bool(b) => Ok(b.into_pyobject(py)?.as_any().clone().unbind()),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(i.into_pyobject(py)?.as_any().clone().unbind())
                } else if let Some(f) = n.as_f64() {
                    Ok(f.into_pyobject(py)?.as_any().clone().unbind())
                } else {
                    Ok(py.None())
                }
            }
            Value::String(s) => Ok(s.into_pyobject(py)?.as_any().clone().unbind()),
            Value::Sequence(seq) => {
                let py_list = PyList::empty(py);
                for item in seq {
                    py_list.append(Self::yaml_to_python(py, item)?)?;
                }
                Ok(py_list.into())
            }
            Value::Mapping(map) => {
                let py_dict = PyDict::new(py);
                for (k, v) in map {
                    let key = Self::yaml_to_python(py, k)?;
                    let value = Self::yaml_to_python(py, v)?;
                    py_dict.set_item(key, value)?;
                }
                Ok(py_dict.into())
            }
            Value::Tagged(tagged) => Self::yaml_to_python(py, &tagged.value),
        }
    }
}

impl From<Node> for PyNode {
    fn from(node: Node) -> Self {
        PyNode(node)
    }
}

/// Python-wrapped MatchResult for deferred AST construction
///
/// This allows Python code to receive match results and apply them using
/// Python's existing apply() logic, avoiding double-counting issues in Rust.
///
/// frozen=true makes this immutable (matches Python's @dataclass(frozen=True))
#[pyclass(name = "RsMatchResult", module = "sqlfluffrs", frozen, from_py_object)]
#[derive(Clone)]
pub struct PyMatchResult(pub MatchResult);

#[pymethods]
impl PyMatchResult {
    /// Get the matched slice as a Python tuple (start, stop)
    #[getter]
    fn matched_slice(&self) -> (usize, usize) {
        (self.0.matched_slice.start, self.0.matched_slice.end)
    }

    /// Get the matched class type as a string (or None)
    #[getter]
    fn matched_class(&self) -> Option<String> {
        self.0
            .matched_class
            .as_ref()
            .map(|s| s.class_name.to_string())
    }

    /// Get child matches as a list of PyMatchResult objects
    #[getter]
    fn child_matches(&self) -> Vec<PyMatchResult> {
        self.0
            .child_matches
            .iter()
            .map(|m| PyMatchResult((**m).clone()))
            .collect()
    }

    /// Get instance_types (semantic type markers like "keyword", "star")
    #[getter]
    fn instance_types<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyList>>> {
        self.0
            .matched_class
            .as_ref()
            .and_then(|s| s.segment_kwargs.instance_types.as_ref())
            .map(|v| pylist_of_strs(py, v))
            .transpose()
    }

    /// Get trim_chars for the segment
    #[getter]
    fn trim_chars<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyTuple>>> {
        self.0
            .matched_class
            .as_ref()
            .and_then(|s| s.segment_kwargs.trim_chars.as_ref())
            .map(|v| pytuple_of_strs(py, v))
            .transpose()
    }

    /// Get casefold mode (for case-insensitive matching)
    #[getter]
    fn casefold(&self) -> Option<String> {
        match self
            .0
            .matched_class
            .as_ref()
            .map(|s| s.segment_kwargs.casefold)
            .unwrap_or_default()
        {
            sqlfluffrs_types::token::CaseFold::None => None,
            sqlfluffrs_types::token::CaseFold::Upper => Some("upper".to_string()),
            sqlfluffrs_types::token::CaseFold::Lower => Some("lower".to_string()),
        }
    }

    /// Get quoted_value for identifier normalization
    #[getter]
    fn quoted_value(&self, py: Python<'_>) -> Option<(String, Py<PyAny>)> {
        self.0
            .matched_class
            .as_ref()
            .and_then(|s| s.segment_kwargs.quoted_value.clone())
            .as_ref()
            .map(|(pattern, group)| {
                let py_group: Py<PyAny> = match group {
                    sqlfluffrs_types::regex::RegexModeGroup::Index(idx) => {
                        idx.into_pyobject(py).unwrap().into()
                    }
                    sqlfluffrs_types::regex::RegexModeGroup::Name(name) => {
                        name.clone().into_pyobject(py).unwrap().into()
                    }
                };
                (pattern.clone(), py_group)
            })
    }

    /// Get escape_replacements for escape sequence handling
    #[getter]
    fn escape_replacements<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyList>>> {
        self.0
            .matched_class
            .as_ref()
            .and_then(|s| s.segment_kwargs.escape_replacements.as_deref())
            .map(|pairs| pylist_of_str_pairs(py, pairs))
            .transpose()
    }

    /// Get insert_segments (meta segments like Indent/Dedent to insert)
    #[getter]
    fn insert_segments(&self) -> Vec<(usize, String, bool)> {
        self.0
            .insert_segments
            .iter()
            .map(|(idx, seg_type)| {
                let (type_name, is_implicit) = match seg_type {
                    MetaSegment::Indent { is_implicit } => ("indent", is_implicit),
                    MetaSegment::Dedent { is_implicit } => ("dedent", is_implicit),
                };
                (*idx, type_name.to_string(), *is_implicit)
            })
            .collect()
    }

    /// Get parse_error (error message and token position) if present
    #[getter]
    fn parse_error(&self) -> Option<(String, usize)> {
        self.0
            .matched_class
            .as_ref()
            .and_then(|s| s.segment_kwargs.parse_error.clone())
    }

    /// Get segment_kwargs dictionary (e.g., parsed properties for segments)
    #[getter]
    fn segment_kwargs(&self, py: Python) -> PyResult<Py<PyAny>> {
        let dict = PyDict::new(py);

        if let Some(matched_class) = self.0.matched_class.as_ref() {
            let sk = &matched_class.segment_kwargs;

            if let Some((ref msg, _pos)) = sk.parse_error.as_ref() {
                dict.set_item("expected", msg.clone())?;
            }
        }

        Ok(dict.into())
    }

    /// Check if this is an empty match
    fn is_empty(&self) -> bool {
        self.0.matched_slice.is_empty()
    }

    /// Get length of matched slice
    fn __len__(&self) -> usize {
        self.0.matched_slice.end - self.0.matched_slice.start
    }

    /// Boolean conversion - truthy if has length
    fn __bool__(&self) -> bool {
        !self.0.matched_slice.is_empty() || !self.0.insert_segments.is_empty()
    }

    /// String representation for debugging
    fn __repr__(&self) -> String {
        format!(
            "RsMatchResult(slice={}..{}, class={:?}, {} children)",
            self.0.matched_slice.start,
            self.0.matched_slice.end,
            self.0.matched_class,
            self.0.child_matches.len()
        )
    }

    /// Build the mutable arena tree (`RsTree`) from this MatchResult and tokens.
    ///
    /// Applies the match result against the provided tokens to construct the
    /// immutable [`Node`] tree, then ingests it into the id-addressable arena
    /// that backs the Rust-side segment façade (`RsTree`/`RsHandle`) used by
    /// linting and (in later milestones) fixing — so per-node navigation never
    /// round-trips through Python's segment tree. Optionally prepend `leading`
    /// and append `trailing` non-code tokens to the root.
    ///
    /// This is the single PyO3 entry-point for tree construction.
    #[pyo3(signature = (tokens, leading=vec![], trailing=vec![]))]
    fn apply_as_tree(
        &self,
        tokens: Vec<PyToken>,
        leading: Vec<PyToken>,
        trailing: Vec<PyToken>,
    ) -> super::arena_py::PyTree {
        let rust_leading: Vec<Token> = leading.into_iter().map(|t| t.into()).collect();
        let rust_tokens: Vec<Token> = tokens.into_iter().map(|t| t.into()).collect();
        let rust_trailing: Vec<Token> = trailing.into_iter().map(|t| t.into()).collect();
        let node = self
            .0
            .clone()
            .apply_as_root(&rust_tokens, &rust_leading, &rust_trailing);
        super::arena_py::PyTree::new(super::arena::Arena::from_node(&node))
    }
}

/// Python-wrapped Parser
#[pyclass(name = "RsParser", module = "sqlfluffrs")]
pub struct PyParser {
    dialect: Dialect,
    indent_config: hashbrown::HashMap<&'static str, bool>,
    max_parser_iterations: usize,
    parser_warn_threshold: usize,
    max_parse_depth: usize,
    max_parse_nodes: usize,
}

impl PyParser {
    /// Match the named segment's grammar against `tokens`; see
    /// `rematch_segment`.
    fn run_rematch(
        &self,
        name: &str,
        mut rust_tokens: Vec<Token>,
    ) -> PyResult<Option<(usize, usize)>> {
        fn count_unparsable(node: &Node) -> usize {
            match node {
                Node::Unparsable { children, .. } => {
                    1 + children.iter().map(count_unparsable).sum::<usize>()
                }
                Node::Segment { children, .. } => children.iter().map(count_unparsable).sum(),
                _ => 0,
            }
        }
        self.dialect
            .get_bracket_pairs()
            .compute_matching_indices(&mut rust_tokens);
        let mut parser = Parser::new_with_max_parse_depth(
            &rust_tokens,
            self.dialect,
            self.indent_config.clone(),
            self.max_parse_depth,
        )
        .with_parser_limits(self.max_parser_iterations, self.parser_warn_threshold);
        let Some(m) = parser.rematch_segment(name).map_err(parse_error_to_pyerr)? else {
            return Ok(None);
        };
        if std::env::var_os("SQLFLUFF_RS_REMATCH_DEBUG").is_some() {
            fn dump(m: &MatchResult, depth: usize, tokens: &[Token]) {
                let class = m
                    .matched_class
                    .as_ref()
                    .map(|c| format!("{} ({:?})", c.class_name, c.segment_type))
                    .unwrap_or_else(|| "-".to_string());
                let raws: Vec<&str> = tokens[m.matched_slice.clone()]
                    .iter()
                    .map(|t| t.raw())
                    .collect();
                eprintln!(
                    "{}{:?} {} {:?}",
                    "  ".repeat(depth),
                    m.matched_slice,
                    class,
                    raws
                );
                for c in &m.child_matches {
                    dump(c, depth + 1, tokens);
                }
            }
            dump(&m, 0, &rust_tokens);
        }
        let matched = m.matched_slice.end.saturating_sub(m.matched_slice.start);
        // The match tree marks unparsable sections by class; the applied
        // nodes are checked too in case they're only made on apply.
        let unparsable = usize::from(m.contains_unparsable())
            + m.apply(&rust_tokens)
                .iter()
                .map(count_unparsable)
                .sum::<usize>();
        Ok(Some((matched, unparsable)))
    }
}

#[pymethods]
impl PyParser {
    #[new]
    #[pyo3(signature = (dialect=None, indent_config=None, max_parser_iterations=None, parser_warn_threshold=None, max_parse_depth=0, max_parse_nodes=0))]
    pub fn new(
        dialect: Option<&str>,
        indent_config: Option<HashMap<String, bool>>,
        max_parser_iterations: Option<usize>,
        parser_warn_threshold: Option<usize>,
        max_parse_depth: usize,
        max_parse_nodes: usize,
    ) -> PyResult<Self> {
        let dialect = dialect
            .and_then(|d| Dialect::from_str(d).ok())
            .unwrap_or(Dialect::Ansi);

        // Convert Python HashMap<String, bool> to Rust HashMap<&'static str, bool>
        // by leaking the strings to get 'static references
        let indent_config = if let Some(config) = indent_config {
            config
                .into_iter()
                .map(|(k, v)| {
                    let static_key: &'static str = Box::leak(k.into_boxed_str());
                    (static_key, v)
                })
                .collect()
        } else {
            hashbrown::HashMap::new()
        };

        Ok(PyParser {
            dialect,
            indent_config,
            max_parser_iterations: max_parser_iterations.unwrap_or(3_000_000),
            parser_warn_threshold: parser_warn_threshold.unwrap_or(2_000_000),
            max_parse_depth,
            max_parse_nodes,
        })
    }

    /// Get dialect name
    #[getter]
    fn dialect(&self) -> String {
        format!("{:?}", self.dialect).to_lowercase()
    }

    /// SPIKE: re-match a segment class's grammar against leaves taken from a
    /// (possibly fixed) Python tree, as `validate_segment_with_reparse` does.
    /// Returns `(matched_length, unparsable_count)`, or `None` if Rust has no
    /// grammar for that class.
    pub fn rematch_segment(
        &self,
        name: &str,
        leaves: Vec<Bound<'_, PyAny>>,
    ) -> PyResult<Option<(usize, usize)>> {
        let mut rust_tokens: Vec<Token> = Vec::with_capacity(leaves.len());
        for leaf in &leaves {
            let pos_marker: sqlfluffrs_python::marker::PySqlFluffPositionMarker =
                leaf.getattr("pos_marker")?.extract()?;
            rust_tokens.push(Token::typed_leaf(
                leaf.getattr("raw")?.extract()?,
                pos_marker.into(),
                leaf.get_type().name()?.extract()?,
                leaf.getattr("type")?.extract()?,
                leaf.getattr("class_types")?.extract()?,
                leaf.getattr("instance_types")?.extract()?,
                leaf.getattr("is_code")?.extract()?,
                leaf.getattr("is_whitespace")?.extract()?,
                leaf.getattr("is_comment")?.extract()?,
            ));
        }
        self.run_rematch(name, rust_tokens)
    }

    /// SPIKE: as `rematch_segment`, with the leaves read from an arena.
    /// `leaf_indices` index the arena's leaves in order (as Python's
    /// `raw_segments`); the caller has already dropped metas and trimmed
    /// non-code from the ends.
    pub fn rematch_arena_leaves(
        &self,
        tree: &super::arena_py::PyTree,
        name: &str,
        leaf_indices: Vec<usize>,
    ) -> PyResult<Option<(usize, usize)>> {
        let tokens: Option<Vec<Token>> = tree.with_arena(|arena| {
            let leaves = arena.raw_segments(arena.root());
            leaf_indices
                .iter()
                .map(|i| {
                    let id = *leaves.get(*i)?;
                    Some(Token::typed_leaf(
                        arena.raw(id),
                        arena.pos_marker(id)?,
                        arena.segment_class(id).unwrap_or_default(),
                        arena.class_type(id),
                        arena.class_types(id).iter().cloned().collect(),
                        arena.instance_types_arc(id).as_ref().clone(),
                        arena.is_code(id),
                        arena.is_whitespace(id),
                        arena.is_comment(id),
                    ))
                })
                .collect()
        });
        match tokens {
            Some(tokens) => self.run_rematch(name, tokens),
            None => Ok(None),
        }
    }

    /// SPIKE: why the last `apply_leaf_fixes` on this thread returned `None`.
    #[staticmethod]
    pub fn last_apply_decline() -> &'static str {
        super::arena_fix::DECLINED.with(|d| d.get())
    }

    /// SPIKE: apply fixes to the arena, validating each rebuilt segment as
    /// Python's `apply_fixes` does. `fixes` are `(edit type, anchor, edits)`:
    /// the anchor is its pre-order position in the tree and the edits index
    /// `new_leaves`. Returns the new arena and the plan for building the
    /// Python tree (see `arena_fix::Plan`), or `None` if Python should apply
    /// the fixes itself.
    #[allow(clippy::type_complexity)]
    pub fn apply_leaf_fixes(
        &self,
        tree: &super::arena_py::PyTree,
        fixes: Vec<(String, usize, Vec<usize>)>,
        new_leaves: Vec<(String, String, Vec<String>, Vec<String>, String, usize)>,
        new_positions: Vec<i64>,
        fix_even_unparsable: bool,
        loose_ends: std::collections::HashSet<String>,
    ) -> Option<(
        super::arena_py::PyTree,
        Vec<i64>,
        Vec<usize>,
        String,
        u64,
        usize,
        u64,
    )> {
        use super::arena_fix::{EditType, Fix, Fixer, Plan};
        let started = std::time::Instant::now();
        if new_positions.len() != new_leaves.len() * 6 {
            return None;
        }
        let (node, plan) = tree.with_arena(|arena| {
            let root = arena.root();
            let mut by_anchor: std::collections::HashMap<_, Vec<Fix>> =
                std::collections::HashMap::new();
            let mut marked = std::collections::HashSet::new();
            for (edit_type, anchor, edits) in fixes {
                if edits.iter().any(|&e| e >= new_leaves.len()) {
                    return None;
                }
                // An anchor which isn't in the tree is never reached.
                let target = super::arena::NodeId::from_index(anchor.min(u32::MAX as usize));
                by_anchor.entry(target).or_default().push(Fix {
                    edit_type: EditType::parse(&edit_type)?,
                    edits,
                });
                let mut up = (anchor < arena.len())
                    .then(|| arena.parent(target))
                    .flatten();
                while let Some(p) = up {
                    if !marked.insert(p) {
                        break;
                    }
                    up = arena.parent(p);
                }
            }
            let templated_file = arena.pos_marker(root)?.templated_file;
            let mut validate = |name: &str, tokens: Vec<Token>| {
                let n = tokens.len();
                let (matched, unparsable) = self.run_rematch(name, tokens).ok()??;
                Some(matched == n && unparsable == 0)
            };
            Fixer {
                arena,
                fixes: by_anchor,
                marked,
                loose_ends: &loose_ends,
                new_leaves: &new_leaves,
                new_pos: &new_positions,
                visit_all: templated_file.source_str != templated_file.templated_str,
                templated_file,
                validate: &mut validate,
                plan: Plan::default(),
                reuse: std::env::var_os("SQLFLUFF_RUST_APPLY_NO_REUSE").is_none(),
                stale: 0,
                fix_even_unparsable,
                takes_pos: std::collections::HashMap::new(),
            }
            .run()
        })?;
        let new_tree = super::arena_py::PyTree::new(super::arena::Arena::from_node(&node));
        let total = started.elapsed().as_nanos() as u64;
        Some((
            new_tree,
            plan.ops,
            plan.slices,
            plan.raw,
            plan.validate_ns,
            plan.validations,
            total,
        ))
    }

    /// Parse SQL from tokens and return MatchResult (deferred AST construction)
    ///
    /// This returns a MatchResult that Python can apply using its own apply() logic,
    /// avoiding double-counting issues and allowing Python to maintain control over
    /// the AST construction process.
    pub fn parse_match_result_from_tokens(&self, tokens: Vec<PyToken>) -> PyResult<PyMatchResult> {
        // Convert PyToken to internal Token
        let mut rust_tokens: Vec<Token> = tokens.into_iter().map(|t| t.into()).collect();

        // Compute bracket pairs for the tokens.
        // When tokens come from Python (lexed by Python lexer), matching_bracket_idx is None.
        // We need to compute it for the Bracketed grammar to work correctly.
        self.dialect
            .get_bracket_pairs()
            .compute_matching_indices(&mut rust_tokens);

        // Create parser
        let mut parser = Parser::new_with_max_parse_depth(
            &rust_tokens,
            self.dialect,
            self.indent_config.clone(),
            self.max_parse_depth,
        )
        .with_parser_limits(self.max_parser_iterations, self.parser_warn_threshold)
        .with_node_limit(self.max_parse_nodes);

        // Parse and get the MatchResult directly
        let match_result = parser.call_rule_as_root().map_err(parse_error_to_pyerr)?;

        Ok(PyMatchResult(match_result))
    }

    /// Parse SQL from tokens and return MatchResult along with parser statistics.
    ///
    /// This is used for comparing Python and Rust parser behavior to verify
    /// that both parsers are doing equivalent work (pruning, caching, etc.)
    ///
    /// Returns a tuple: (PyMatchResult, stats_dict)
    /// where stats_dict contains:
    /// - cache_hits: number of cache hits
    /// - cache_misses: number of cache misses
    /// - cache_entries: number of cache entries
    /// - pruning_calls: number of prune_options calls
    /// - pruning_total: total options considered
    /// - pruning_kept: options kept after pruning
    /// - pruning_hinted: options that had hints
    /// - pruning_complex: options that returned None (complex)
    pub fn parse_match_result_with_stats(
        &self,
        tokens: Vec<PyToken>,
    ) -> PyResult<(PyMatchResult, HashMap<String, usize>)> {
        // Convert PyToken to internal Token
        let mut rust_tokens: Vec<Token> = tokens.into_iter().map(|t| t.into()).collect();

        // Compute bracket pairs for the tokens.
        self.dialect
            .get_bracket_pairs()
            .compute_matching_indices(&mut rust_tokens);

        // Create parser
        let mut parser = Parser::new_with_max_parse_depth(
            &rust_tokens,
            self.dialect,
            self.indent_config.clone(),
            self.max_parse_depth,
        )
        .with_parser_limits(self.max_parser_iterations, self.parser_warn_threshold)
        .with_node_limit(self.max_parse_nodes);

        // Parse and get the MatchResult directly
        let match_result = parser
            .call_rule_as_root()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.message))?;

        // Collect statistics
        let (cache_hits, cache_misses, _) = parser.table_cache.stats();
        let cache_entries = parser.table_cache.len();

        let mut stats = HashMap::new();
        stats.insert("cache_hits".to_string(), cache_hits);
        stats.insert("cache_misses".to_string(), cache_misses);
        stats.insert("cache_entries".to_string(), cache_entries);
        // Diagnostic counters are owned by the parser; pull them as a unit.
        stats.extend(parser.diagnostics());

        Ok((PyMatchResult(match_result), stats))
    }

    /// Parse SQL from tokens and return grammar call counts for debugging.
    ///
    /// Returns a tuple: (PyMatchResult, grammar_counts_dict)
    /// where grammar_counts_dict maps grammar names to call counts.
    ///
    /// This is used to identify which grammars are called most frequently,
    /// helping debug performance differences between Python and Rust parsers.
    pub fn parse_match_result_with_grammar_counts(
        &self,
        tokens: Vec<PyToken>,
    ) -> PyResult<(PyMatchResult, HashMap<String, usize>)> {
        // Convert PyToken to internal Token
        let mut rust_tokens: Vec<Token> = tokens.into_iter().map(|t| t.into()).collect();

        // Compute bracket pairs for the tokens.
        self.dialect
            .get_bracket_pairs()
            .compute_matching_indices(&mut rust_tokens);

        // Create parser with grammar counting enabled
        let mut parser = Parser::new_with_max_parse_depth(
            &rust_tokens,
            self.dialect,
            self.indent_config.clone(),
            self.max_parse_depth,
        )
        .with_parser_limits(self.max_parser_iterations, self.parser_warn_threshold)
        .with_node_limit(self.max_parse_nodes);

        // Track grammar calls using cache misses as a proxy
        // Each unique (grammar_id, pos) pair in the cache represents one grammar call
        let match_result = parser
            .call_rule_as_root()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.message))?;

        // Count calls per grammar by iterating cache entries
        let mut grammar_counts = HashMap::new();

        for (key, _result) in parser.table_cache.iter() {
            let grammar_id = sqlfluffrs_types::GrammarId(key.grammar_id);

            // Skip invalid grammar IDs (can happen during failed parse attempts)
            if grammar_id.get() as usize >= parser.grammar_ctx.len() {
                continue;
            }

            // Use grammar_id_name which handles all variants (Ref, StringParser, etc.)
            let grammar_name = parser.grammar_ctx.grammar_id_name(grammar_id);
            *grammar_counts.entry(grammar_name).or_insert(0) += 1;
        }

        Ok((PyMatchResult(match_result), grammar_counts))
    }
}
