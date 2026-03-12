//! Python bindings for the reflow module.

use pyo3::prelude::*;
use std::collections::HashMap;

use super::config::ReflowConfig;
use super::respace::{FixType, LintViolation};
use super::sequence::ReflowSequence;

/// Python-wrapped LintViolation.
#[pyclass(name = "RsLintViolation", module = "sqlfluffrs", frozen)]
#[derive(Clone)]
pub struct PyLintViolation(pub LintViolation);

#[pymethods]
impl PyLintViolation {
    /// Violation description.
    #[getter]
    fn description(&self) -> String {
        self.0.description.clone()
    }

    /// Line number (1-based), 0 if not available.
    #[getter]
    fn line_no(&self) -> usize {
        self.0.line_no
    }

    /// Column number (1-based), 0 if not available.
    #[getter]
    fn line_pos(&self) -> usize {
        self.0.line_pos
    }

    /// Fix type: "delete", "replace", "create_before", or "create_after".
    #[getter]
    fn fix_type(&self) -> String {
        match &self.0.fix_type {
            FixType::Delete => "delete".to_string(),
            FixType::Replace { .. } => "replace".to_string(),
            FixType::CreateBefore { .. } => "create_before".to_string(),
            FixType::CreateAfter { .. } => "create_after".to_string(),
        }
    }

    /// The new text for replace/create fixes (None for delete).
    #[getter]
    fn edit_text(&self) -> Option<String> {
        match &self.0.fix_type {
            FixType::Delete => None,
            FixType::Replace { new_text } => Some(new_text.clone()),
            FixType::CreateBefore { new_text } => Some(new_text.clone()),
            FixType::CreateAfter { new_text } => Some(new_text.clone()),
        }
    }

    /// The raw-segment index of the anchor.
    #[getter]
    fn anchor_idx(&self) -> usize {
        self.0.anchor_idx
    }

    fn __repr__(&self) -> String {
        format!(
            "RsLintViolation(desc={:?}, fix={:?}, anchor={})",
            self.0.description,
            self.fix_type(),
            self.0.anchor_idx
        )
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

/// Python-wrapped ReflowSequence for respace analysis.
///
/// Usage from Python:
/// ```python
/// from sqlfluffrs import RsReflowSequence, RsNode
///
/// node = parser.parse(tokens).apply_as_root(tokens)
/// cfg = rs_make_reflow_config(layout_type_dict)
/// seq = RsReflowSequence.from_node_with_config_obj(node, cfg)
/// violations = seq.respace()
/// ```
#[pyclass(name = "RsReflowSequence", module = "sqlfluffrs")]
pub struct PyReflowSequence {
    inner: Option<ReflowSequence>,
}

#[pymethods]
impl PyReflowSequence {
    /// Construct from a RsNode with a custom layout config dict.
    ///
    /// The config_dict is a dict[str, dict[str, str]] mapping segment types
    /// to their layout config (spacing_before, spacing_after, etc.).
    #[staticmethod]
    fn from_node_with_config(
        node: &sqlfluffrs_parser::parser::PyNode,
        config_dict: HashMap<String, HashMap<String, String>>,
    ) -> Self {
        let config = ReflowConfig::from_dict(config_dict);
        let seq = ReflowSequence::from_root(&node.0, &config);
        PyReflowSequence { inner: Some(seq) }
    }

    /// Construct from a RsNode with a pre-built config object.
    #[staticmethod]
    fn from_node_with_config_obj(
        node: &sqlfluffrs_parser::parser::PyNode,
        config: &PyReflowConfig,
    ) -> Self {
        let seq = ReflowSequence::from_root(&node.0, &config.0);
        PyReflowSequence { inner: Some(seq) }
    }

    /// Run the respace algorithm. Returns a list of RsLintViolation objects.
    fn respace(&mut self) -> Vec<PyLintViolation> {
        let seq = self
            .inner
            .take()
            .expect("ReflowSequence already consumed");
        let result = seq.respace();
        let violations: Vec<PyLintViolation> = result
            .violations
            .iter()
            .map(|v| PyLintViolation(v.clone()))
            .collect();
        self.inner = Some(result);
        violations
    }

    /// Get current violations.
    fn get_violations(&self) -> Vec<PyLintViolation> {
        self.inner
            .as_ref()
            .map(|s| {
                s.violations
                    .iter()
                    .map(|v| PyLintViolation(v.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get the raw text representation of the sequence.
    fn get_raw(&self) -> String {
        self.inner
            .as_ref()
            .map(|s| s.get_raw())
            .unwrap_or_default()
    }

    fn __repr__(&self) -> String {
        if let Some(s) = &self.inner {
            format!(
                "RsReflowSequence({} elements, {} violations)",
                s.elements.len(),
                s.violations.len()
            )
        } else {
            "RsReflowSequence(consumed)".to_string()
        }
    }
}

/// Standalone function: run respace with a custom config dict.
///
/// Builds a fresh ``ReflowConfig`` on every call.  Prefer
/// ``rs_make_reflow_config`` + ``rs_respace_with_config_obj`` when the
/// same config is reused across many files (avoids repeated dict→struct
/// conversion).
#[pyfunction]
pub fn rs_respace_node_with_config(
    node: &sqlfluffrs_parser::parser::PyNode,
    config_dict: HashMap<String, HashMap<String, String>>,
) -> Vec<PyLintViolation> {
    let config = ReflowConfig::from_dict(config_dict);
    let seq = ReflowSequence::from_root(&node.0, &config).respace();
    seq.violations
        .iter()
        .map(|v| PyLintViolation(v.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Cached-config API — preferred for production use
// ---------------------------------------------------------------------------

use std::sync::Arc;

/// A pre-built, immutable reflow configuration that can be cached in Python
/// and reused across many files / rule evaluations.
///
/// Build it once with :func:`rs_make_reflow_config` and pass it to
/// :func:`rs_respace_with_config_obj`.  This avoids the
/// ``dict`` → ``ReflowConfig`` conversion on every call.
///
/// Internally it holds an ``Arc<ReflowConfig>`` so Python-side copies are
/// cheap (reference-counted pointer bump, not a deep clone).
#[pyclass(name = "RsReflowConfig", module = "sqlfluffrs", frozen)]
#[derive(Clone)]
pub struct PyReflowConfig(pub Arc<ReflowConfig>);

#[pymethods]
impl PyReflowConfig {
    fn __repr__(&self) -> String {
        format!("RsReflowConfig({} configured types)", self.0.config_types.len())
    }
}

/// Build a ``RsReflowConfig`` from a ``dict[str, dict[str, str]]``.
///
/// Typically called once per ``FluffConfig`` and cached on the Python side::
///
///     _config_cache: dict[int, RsReflowConfig] = {}
///
///     def _get_rs_config(fluff_config):
///         key = id(fluff_config)
///         if key not in _config_cache:
///             _config_cache[key] = rs_make_reflow_config(
///                 fluff_config.get_section(["layout", "type"])
///             )
///         return _config_cache[key]
#[pyfunction]
#[pyo3(signature = (config_dict, class_types_map=HashMap::new()))]
pub fn rs_make_reflow_config(
    config_dict: HashMap<String, HashMap<String, String>>,
    class_types_map: HashMap<String, Vec<String>>,
) -> PyReflowConfig {
    PyReflowConfig(Arc::new(ReflowConfig::from_dict_with_class_types(
        config_dict,
        class_types_map,
    )))
}

/// Run respace on a Node using a pre-built ``RsReflowConfig``.
///
/// This is the production-quality entry point: the config has already been
/// converted from Python and cached, so this call only pays for the actual
/// Rust reflow computation.
#[pyfunction]
pub fn rs_respace_with_config_obj(
    node: &sqlfluffrs_parser::parser::PyNode,
    config: &PyReflowConfig,
) -> Vec<PyLintViolation> {
    let seq = ReflowSequence::from_root(&node.0, &config.0).respace();
    seq.violations
        .iter()
        .map(|v| PyLintViolation(v.clone()))
        .collect()
}
