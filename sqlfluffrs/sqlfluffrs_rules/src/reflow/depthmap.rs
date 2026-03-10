//! Depth map — maps each raw segment to its ancestry information.
//!
//! Mirrors Python's `sqlfluff.utils.reflow.depthmap.DepthMap` / `DepthInfo`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sqlfluffrs_parser::parser::Node;

use super::config::ReflowConfig;

/// Position of a segment within its parent.
#[derive(Debug, Clone)]
pub struct StackPosition {
    /// Index within the parent's children list.
    pub idx: usize,
    /// Total number of children in the parent.
    pub len: usize,
    /// Position type: "solo", "start", "end", or "" (middle).
    pub pos_type: String,
}

impl StackPosition {
    fn from_path_step(step: &PathStep) -> Self {
        let pos_type = if step.code_idxs.is_empty() {
            "".to_string()
        } else if step.code_idxs.len() == 1 {
            "solo".to_string()
        } else if step.idx == step.code_idxs[0] {
            "start".to_string()
        } else if step.idx == *step.code_idxs.last().unwrap() {
            "end".to_string()
        } else {
            "".to_string()
        };
        StackPosition {
            idx: step.idx,
            len: step.len,
            pos_type,
        }
    }
}

/// A single step in the path from root to a raw segment.
#[derive(Debug, Clone)]
pub struct PathStep {
    /// Hash of the parent segment (we use the pointer-based identity).
    pub segment_hash: u64,
    /// The class types of the parent segment (Arc-shared across all children).
    pub class_types: Arc<HashSet<String>>,
    /// Index of the child within the parent.
    pub idx: usize,
    /// Total number of children in the parent.
    pub len: usize,
    /// Indices of code children in the parent (Arc-shared across all children).
    pub code_idxs: Arc<Vec<usize>>,
}

/// Ancestry information for a single raw segment.
#[derive(Debug, Clone)]
pub struct DepthInfo {
    pub stack_depth: usize,
    pub stack_hashes: Vec<u64>,
    pub stack_hash_set: HashSet<u64>,
    /// Arc-shared class-type sets — cloning is O(1).
    pub stack_class_types: Vec<Arc<HashSet<String>>>,
    pub stack_positions: HashMap<u64, StackPosition>,
}

impl DepthInfo {
    /// Build from a list of path steps.
    pub fn from_path_steps(steps: &[PathStep]) -> Self {
        let stack_hashes: Vec<u64> = steps.iter().map(|s| s.segment_hash).collect();
        let stack_hash_set: HashSet<u64> = stack_hashes.iter().cloned().collect();
        // Arc::clone is O(1) — no HashSet<String> allocation per step.
        let stack_class_types: Vec<Arc<HashSet<String>>> =
            steps.iter().map(|s| Arc::clone(&s.class_types)).collect();
        let mut stack_positions = HashMap::new();
        for (idx, step) in steps.iter().enumerate() {
            stack_positions.insert(
                stack_hashes[idx],
                StackPosition::from_path_step(step),
            );
        }
        DepthInfo {
            stack_depth: steps.len(),
            stack_hashes,
            stack_hash_set,
            stack_class_types,
            stack_positions,
        }
    }

    /// Find the common ancestor hashes between self and another DepthInfo.
    /// Returns the common prefix of stack_hashes.
    pub fn common_with(&self, other: &DepthInfo) -> Vec<u64> {
        let common_set: HashSet<u64> =
            self.stack_hash_set.intersection(&other.stack_hash_set).cloned().collect();
        // Return common prefix
        self.stack_hashes
            .iter()
            .take_while(|h| common_set.contains(h))
            .cloned()
            .collect()
    }
}

/// Maps each raw segment (by index in the flattened list) to its DepthInfo.
#[derive(Debug)]
pub struct DepthMap {
    /// Key: index into the flat raw-segments list.
    pub depth_info: HashMap<usize, DepthInfo>,
}


impl DepthMap {
    /// Build a DepthMap from a root Node by walking the tree.
    ///
    /// Returns the depth map and the flattened list of raw segments (as
    /// references into the tree).
    pub fn from_node<'a>(root: &'a Node, config: &ReflowConfig) -> (Self, Vec<RawRef<'a>>) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);

        let mut depth_info = HashMap::new();
        let mut raws = Vec::new();

        fn walk<'a>(
            node: &'a Node,
            path: &mut Vec<PathStep>,
            depth_info: &mut HashMap<usize, DepthInfo>,
            raws: &mut Vec<RawRef<'a>>,
            counter: &AtomicU64,
            class_types_map: &HashMap<String, Vec<String>>,
        ) {
            match node {
                Node::Raw {
                    segment_type,
                    raw,
                    instance_types,
                    pos_marker,
                    ..
                } => {
                    let idx = raws.len();
                    raws.push(RawRef {
                        index: idx,
                        segment_type: segment_type.as_str(),
                        raw: raw.as_str(),
                        instance_types,
                        pos_marker: pos_marker.as_ref(),
                        is_code: node.is_code(),
                        consumed_whitespace: None,
                    });
                    depth_info.insert(idx, DepthInfo::from_path_steps(path));
                }
                Node::Segment {
                    segment_class,
                    segment_type,
                    children,
                    ..
                } => {
                    let seg_hash = counter.fetch_add(1, Ordering::Relaxed);

                    // Build class types once and wrap in Arc so all N children
                    // share the same allocation (O(1) clone per child).
                    let class_types = Arc::new({
                        let mut ht = HashSet::new();
                        if let Some(st) = segment_type {
                            // Inject extra inherited class types from Python's
                            // segment metaclass hierarchy.
                            if let Some(extras) = class_types_map.get(st.as_str()) {
                                for extra in extras {
                                    ht.insert(extra.clone());
                                }
                            }
                            ht.insert(st.clone());
                        }
                        ht.insert(to_snake_case(segment_class));
                        ht
                    });

                    // Same for code_idxs — built once, Arc-cloned per child.
                    let code_idxs = Arc::new(
                        children
                            .iter()
                            .enumerate()
                            .filter(|(_, c)| c.is_code())
                            .map(|(i, _)| i)
                            .collect::<Vec<_>>(),
                    );

                    for (child_idx, child) in children.iter().enumerate() {
                        path.push(PathStep {
                            segment_hash: seg_hash,
                            class_types: Arc::clone(&class_types),
                            idx: child_idx,
                            len: children.len(),
                            code_idxs: Arc::clone(&code_idxs),
                        });
                        walk(child, path, depth_info, raws, counter, class_types_map);
                        path.pop();
                    }
                }
                Node::Meta { meta_type, pos_marker, .. } => {
                    let idx = raws.len();
                    let (seg_type, is_code, consumed_ws) = match meta_type {
                        sqlfluffrs_parser::parser::MetaType::Indent { .. } => ("indent", false, None),
                        sqlfluffrs_parser::parser::MetaType::Dedent { .. } => ("dedent", false, None),
                        sqlfluffrs_parser::parser::MetaType::Template {
                            source_str,
                            block_type,
                        } => {
                            // Mirror Python's get_consumed_whitespace():
                            // block_type == "literal" and source_str is non-empty
                            // whitespace → the placeholder consumed whitespace.
                            let cw = if block_type == "literal"
                                && !source_str.is_empty()
                                && source_str.chars().all(|c| c.is_whitespace())
                            {
                                Some(source_str.as_str())
                            } else {
                                None
                            };
                            ("placeholder", false, cw)
                        }
                        sqlfluffrs_parser::parser::MetaType::TemplateLoop => {
                            ("template_loop", false, None)
                        }
                        sqlfluffrs_parser::parser::MetaType::EndOfFile => ("end_of_file", false, None),
                    };
                    raws.push(RawRef {
                        index: idx,
                        segment_type: seg_type,
                        raw: "",
                        instance_types: &[],
                        pos_marker: pos_marker.as_ref(),
                        is_code,
                        consumed_whitespace: consumed_ws,
                    });
                    depth_info.insert(idx, DepthInfo::from_path_steps(path));
                }
                Node::Unparsable { children, .. } => {
                    for child in children.iter() {
                        walk(child, path, depth_info, raws, counter, class_types_map);
                    }
                }
                Node::Empty => {}
            }
        }

        let mut path = Vec::new();
        walk(root, &mut path, &mut depth_info, &mut raws, &COUNTER, &config.class_types_map);
        (DepthMap { depth_info }, raws)
    }

    /// Get depth info for a raw segment at a given index.
    pub fn get_depth_info(&self, index: usize) -> Option<&DepthInfo> {
        self.depth_info.get(&index)
    }
}

/// A lightweight reference to a raw segment in the flattened list.
#[derive(Debug, Clone)]
pub struct RawRef<'a> {
    pub index: usize,
    pub segment_type: &'a str,
    pub raw: &'a str,
    pub instance_types: &'a [String],
    pub pos_marker: Option<&'a sqlfluffrs_types::PositionMarker>,
    pub is_code: bool,
    /// For template placeholders with `block_type == "literal"`: the
    /// source string that was consumed.  When this is non-empty and
    /// all-whitespace, the placeholder should be treated as point-like
    /// (mirroring Python's `get_consumed_whitespace`).
    pub consumed_whitespace: Option<&'a str>,
}

impl<'a> RawRef<'a> {
    /// Check if this raw segment is of a given type.
    pub fn is_type(&self, target: &str) -> bool {
        self.segment_type == target
            || self.instance_types.iter().any(|t| t == target)
    }
}

/// Convert a CamelCase segment class name to snake_case segment type.
fn to_snake_case(s: &str) -> String {
    // Remove "Segment" suffix if present.
    let s = s.strip_suffix("Segment").unwrap_or(s);
    let mut result = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_lowercase().next().unwrap());
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlfluffrs_parser::parser::{MetaType, Node, RawSegmentKwargs};

    #[test]
    fn test_to_snake_case() {
        assert_eq!(to_snake_case("SelectStatementSegment"), "select_statement");
        assert_eq!(to_snake_case("KeywordSegment"), "keyword");
        assert_eq!(to_snake_case("FromClauseSegment"), "from_clause");
    }

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

    #[test]
    fn test_depth_map_simple() {
        // SELECT 1
        let tree = Node::Segment {
            segment_class: "FileSegment".to_string(),
            segment_type: Some("file".to_string()),
            pos_marker: None,
            children: vec![
                Node::Segment {
                    segment_class: "StatementSegment".to_string(),
                    segment_type: Some("statement".to_string()),
                    pos_marker: None,
                    children: vec![
                        Node::Segment {
                            segment_class: "SelectStatementSegment".to_string(),
                            segment_type: Some("select_statement".to_string()),
                            pos_marker: None,
                            children: vec![
                                Node::Segment {
                                    segment_class: "SelectClauseSegment".to_string(),
                                    segment_type: Some("select_clause".to_string()),
                                    pos_marker: None,
                                    children: vec![
                                        make_raw(
                                            "keyword",
                                            "SELECT",
                                            vec!["keyword".to_string()],
                                        ),
                                        make_raw(
                                            "whitespace",
                                            " ",
                                            vec!["whitespace".to_string()],
                                        ),
                                        make_raw(
                                            "numeric_literal",
                                            "1",
                                            vec![
                                                "numeric_literal".to_string(),
                                                "literal".to_string(),
                                            ],
                                        ),
                                    ],
                                },
                            ],
                        },
                    ],
                },
                Node::Meta {
                    meta_type: MetaType::EndOfFile,
                    pos_marker: None,
                },
            ],
        };

        let config = ReflowConfig::from_dict(HashMap::new());
        let (dm, raws) = DepthMap::from_node(&tree, &config);
        // Should have 4 raw segments: SELECT, " ", 1, end_of_file
        assert_eq!(raws.len(), 4);
        assert_eq!(raws[0].raw, "SELECT");
        assert_eq!(raws[1].raw, " ");
        assert_eq!(raws[2].raw, "1");
        assert_eq!(raws[3].segment_type, "end_of_file");

        // SELECT should have depth 4: file > statement > select_statement > select_clause
        let di = dm.get_depth_info(0).unwrap();
        assert_eq!(di.stack_depth, 4);

        // Whitespace should have same depth
        let di_ws = dm.get_depth_info(1).unwrap();
        assert_eq!(di_ws.stack_depth, 4);

        // Common hashes between SELECT and " " should be 4 (same parent)
        let common = di.common_with(di_ws);
        assert_eq!(common.len(), 4);
    }
}
