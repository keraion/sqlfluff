//! Reflow elements — ReflowBlock and ReflowPoint.
//!
//! Mirrors Python's `sqlfluff.utils.reflow.elements`.

use std::collections::HashMap;

use super::config::ReflowConfig;
use super::depthmap::DepthInfo;

/// A code element in the reflow sequence (a single raw segment that is code).
#[derive(Debug, Clone)]
pub struct ReflowBlock {
    /// Index into the flat raw-segments list.
    pub raw_idx: usize,
    /// The segment type (e.g. "keyword", "numeric_literal").
    pub segment_type: String,
    /// The actual text.
    pub raw: String,
    /// Class types of this segment (instance_types + segment_type).
    /// Stored as a Vec (not HashSet) — typical size is 2-5 elements so
    /// linear scan beats HashSet's bucket-array overhead.
    pub class_types: Vec<String>,
    /// Spacing constraint before this block.
    pub spacing_before: String,
    /// Spacing constraint after this block.
    pub spacing_after: String,
    /// Line position constraint (if any).
    pub line_position: Option<String>,
    /// Depth info for this segment.
    pub depth_info: DepthInfo,
    /// Spacing configs from ancestor segments (hash → within_spacing).
    pub stack_spacing_configs: HashMap<u64, String>,
}

impl ReflowBlock {
    /// Construct a ReflowBlock from a raw segment reference, with config.
    pub fn from_config(
        raw_idx: usize,
        segment_type: &str,
        raw: &str,
        instance_types: &[String],
        class_types_from_node: &[String],
        config: &ReflowConfig,
        depth_info: DepthInfo,
    ) -> Self {
        // Build class types — prefer the Node's pre-computed class_types
        // which already include the raw-class hierarchy from codegen.
        // Fall back to instance_types + segment_type + extra_class_types
        // for nodes that don't have class_types yet (e.g. test helpers).
        let class_types = if class_types_from_node.is_empty() {
            let mut ct = Vec::with_capacity(instance_types.len() + 1);
            ct.extend_from_slice(instance_types);
            if !ct.iter().any(|t| t == segment_type) {
                ct.push(segment_type.to_string());
            }
            for extra in config.extra_class_types(segment_type) {
                if !ct.iter().any(|t| t == extra) {
                    ct.push(extra.clone());
                }
            }
            ct
        } else {
            class_types_from_node.to_vec()
        };

        // Get block config for this segment (accepts &[String]).
        let block_config = config.get_block_config(&class_types, Some(&depth_info));

        // Build stack_spacing_configs: for each ancestor, check if its class
        // types have a spacing_within config.  Use the fast-path
        // `get_spacing_within` rather than the full `get_block_config` to
        // avoid O(depth) BlockConfig constructions per block.
        let mut stack_spacing_configs = HashMap::new();
        for (hash, parent_classes) in depth_info
            .stack_hashes
            .iter()
            .zip(depth_info.stack_class_types.iter())
        {
            if let Some(within) = config.get_spacing_within(parent_classes) {
                stack_spacing_configs.insert(*hash, within);
            }
        }

        ReflowBlock {
            raw_idx,
            segment_type: segment_type.to_string(),
            raw: raw.to_string(),
            class_types,
            spacing_before: block_config.spacing_before,
            spacing_after: block_config.spacing_after,
            line_position: block_config.line_position,
            depth_info,
            stack_spacing_configs,
        }
    }
}

/// A whitespace/newline/indent point between code blocks.
#[derive(Debug, Clone)]
pub struct ReflowPoint {
    /// The indices of the raw segments that make up this point.
    pub raw_indices: Vec<usize>,
    /// The segment types of each raw segment in the point.
    pub segment_types: Vec<String>,
    /// The raw text of each segment in the point.
    pub raws: Vec<String>,
    /// Whether each segment is "literal" (source position == rendered position).
    /// Non-literal segments come from Jinja macro expansions or other template
    /// constructs where the rendered content does not correspond directly to
    /// source text.  The respace engine skips trailing-whitespace checks when
    /// it encounters a non-literal newline, mirroring Python's
    /// `pos_marker.is_literal()` guard in `_process_spacing`.
    pub is_literals: Vec<bool>,
}

impl ReflowPoint {
    pub fn new(
        raw_indices: Vec<usize>,
        segment_types: Vec<String>,
        raws: Vec<String>,
        is_literals: Vec<bool>,
    ) -> Self {
        ReflowPoint {
            raw_indices,
            segment_types,
            raws,
            is_literals,
        }
    }

    pub fn empty() -> Self {
        ReflowPoint {
            raw_indices: Vec::new(),
            segment_types: Vec::new(),
            raws: Vec::new(),
            is_literals: Vec::new(),
        }
    }

    /// Get the concatenated raw text of this point.
    pub fn raw(&self) -> String {
        self.raws.join("")
    }

    /// Check if any segment in this point is a newline.
    pub fn has_newline(&self) -> bool {
        self.segment_types.iter().any(|t| t == "newline")
    }

    /// Find the last whitespace segment in this point (if any).
    /// Returns its index within the point's segments.
    pub fn last_whitespace_idx(&self) -> Option<usize> {
        self.segment_types
            .iter()
            .enumerate()
            .rev()
            .find(|(_, t)| t.as_str() == "whitespace")
            .map(|(i, _)| i)
    }
}

/// A reflow element — either a block or a point.
#[derive(Debug, Clone)]
pub enum ReflowElement {
    Block(ReflowBlock),
    Point(ReflowPoint),
}

impl ReflowElement {
    pub fn raw(&self) -> String {
        match self {
            ReflowElement::Block(b) => b.raw.clone(),
            ReflowElement::Point(p) => p.raw(),
        }
    }
}
