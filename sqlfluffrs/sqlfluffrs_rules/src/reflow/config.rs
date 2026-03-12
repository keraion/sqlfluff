//! Reflow configuration — maps segment types to spacing constraints.
//!
//! Mirrors Python's `sqlfluff.utils.reflow.config.ReflowConfig` and `BlockConfig`.

use std::collections::{HashMap, HashSet};

use super::depthmap::DepthInfo;

/// Per-segment-type spacing configuration.
#[derive(Debug, Clone, Default)]
pub struct BlockConfig {
    pub spacing_before: String,
    pub spacing_after: String,
    pub spacing_within: Option<String>,
    pub line_position: Option<String>,
}

impl BlockConfig {
    /// Incorporate additional config, only overwriting if the new value is
    /// `Some` / non-empty.  Mirrors Python's `BlockConfig.incorporate()`.
    pub fn incorporate(
        &mut self,
        before: Option<&str>,
        after: Option<&str>,
        within: Option<&str>,
        line_position: Option<&str>,
        config: Option<&HashMap<String, String>>,
    ) {
        let config = config.cloned().unwrap_or_default();
        if let Some(v) = before.or(config.get("spacing_before").map(|s| s.as_str())) {
            if !v.is_empty() {
                self.spacing_before = v.to_string();
            }
        }
        if let Some(v) = after.or(config.get("spacing_after").map(|s| s.as_str())) {
            if !v.is_empty() {
                self.spacing_after = v.to_string();
            }
        }
        if let Some(v) = within.or(config.get("spacing_within").map(|s| s.as_str())) {
            if !v.is_empty() {
                self.spacing_within = Some(v.to_string());
            }
        }
        if let Some(v) =
            line_position.or(config.get("line_position").map(|s| s.as_str()))
        {
            if !v.is_empty() {
                self.line_position = Some(v.to_string());
            }
        }
    }
}

/// Type alias for a single type's config dict (key → value).
pub type ConfigElementType = HashMap<String, String>;
/// Type alias for the full config dict (segment_type → { key → value }).
pub type ConfigDictType = HashMap<String, ConfigElementType>;

/// Top-level reflow configuration.
///
/// This holds the layout config (spacing rules per segment type) and
/// global indent settings.
#[derive(Debug, Clone)]
pub struct ReflowConfig {
    config_dict: ConfigDictType,
    pub config_types: HashSet<String>,
    pub tab_space_size: usize,
    pub indent_unit: String,
    pub max_line_length: usize,
    /// Maps segment type → extra inherited class types (e.g.
    /// "table_reference" → ["object_reference", "base"]).
    /// Populated from Python's segment metaclass hierarchy — replaces
    /// the previously hardcoded `extra_class_types_for_segment_type()`.
    pub class_types_map: HashMap<String, Vec<String>>,
}

impl ReflowConfig {
    /// Construct from a config dict and an optional class-types inheritance map.
    pub fn from_dict(config_dict: ConfigDictType) -> Self {
        Self::from_dict_with_class_types(config_dict, HashMap::new())
    }

    /// Construct from a config dict and class-types inheritance map.
    pub fn from_dict_with_class_types(
        mut config_dict: ConfigDictType,
        class_types_map: HashMap<String, Vec<String>>,
    ) -> Self {
        // Pre-process: expand "align" → "align:<type>:..."
        let keys: Vec<String> = config_dict.keys().cloned().collect();
        for seg_type in &keys {
            for key in &["spacing_before", "spacing_after"] {
                let val = config_dict
                    .get(seg_type)
                    .and_then(|m| m.get(*key))
                    .cloned();
                if val.as_deref() == Some("align") {
                    let mut new_key = format!("align:{}", seg_type);
                    if let Some(aw) = config_dict
                        .get(seg_type)
                        .and_then(|m| m.get("align_within"))
                    {
                        new_key = format!("{}:{}", new_key, aw);
                        if let Some(asc) = config_dict
                            .get(seg_type)
                            .and_then(|m| m.get("align_scope"))
                        {
                            new_key = format!("{}:{}", new_key, asc);
                        }
                    }
                    if let Some(acs) = config_dict
                        .get(seg_type)
                        .and_then(|m| m.get("alignment_coordinate_space"))
                    {
                        new_key = format!("{}:{}", new_key, acs);
                    }
                    config_dict
                        .get_mut(seg_type)
                        .unwrap()
                        .insert(key.to_string(), new_key);
                }
            }
        }
        let config_types: HashSet<String> = config_dict.keys().cloned().collect();
        ReflowConfig {
            config_dict,
            config_types,
            tab_space_size: 4,
            indent_unit: "space".to_string(),
            max_line_length: 80,
            class_types_map,
        }
    }

    /// Build the default ANSI config (mirrors pyproject.toml [tool.sqlfluff.layout.type.*]).
    pub fn default_ansi() -> Self {
        let mut d = ConfigDictType::new();

        // Helper to insert config for a type.
        fn ins(
            d: &mut ConfigDictType,
            seg_type: &str,
            before: Option<&str>,
            after: Option<&str>,
            within: Option<&str>,
            line_position: Option<&str>,
        ) {
            let mut m = ConfigElementType::new();
            if let Some(v) = before {
                m.insert("spacing_before".into(), v.into());
            }
            if let Some(v) = after {
                m.insert("spacing_after".into(), v.into());
            }
            if let Some(v) = within {
                m.insert("spacing_within".into(), v.into());
            }
            if let Some(v) = line_position {
                m.insert("line_position".into(), v.into());
            }
            d.insert(seg_type.into(), m);
        }

        // Punctuation & operators
        ins(&mut d, "comma", Some("touch"), None, None, Some("trailing"));
        ins(
            &mut d,
            "binary_operator",
            None,
            None,
            Some("touch"),
            Some("leading"),
        );
        ins(
            &mut d,
            "comparison_operator",
            None,
            None,
            Some("touch"),
            Some("leading"),
        );
        ins(
            &mut d,
            "assignment_operator",
            None,
            None,
            Some("touch"),
            Some("leading"),
        );
        ins(
            &mut d,
            "statement_terminator",
            Some("touch"),
            None,
            None,
            Some("trailing"),
        );
        ins(&mut d, "end_of_file", Some("touch"), None, None, None);
        ins(
            &mut d,
            "set_operator",
            None,
            None,
            None,
            Some("alone:strict"),
        );
        ins(&mut d, "dot", Some("touch"), Some("touch"), None, None);
        ins(
            &mut d,
            "casting_operator",
            Some("touch"),
            Some("touch:inline"),
            None,
            None,
        );
        ins(&mut d, "colon", Some("touch"), None, None, None);
        ins(
            &mut d,
            "colon_delimiter",
            Some("touch"),
            Some("touch"),
            None,
            None,
        );
        ins(
            &mut d,
            "sign_indicator",
            None,
            Some("touch:inline"),
            None,
            None,
        );
        ins(&mut d, "tilde", None, Some("touch:inline"), None, None);
        ins(
            &mut d,
            "sqlcmd_operator",
            Some("touch"),
            None,
            None,
            None,
        );

        // Brackets
        ins(&mut d, "start_bracket", None, Some("touch"), None, None);
        ins(&mut d, "end_bracket", Some("touch"), None, None, None);
        ins(
            &mut d,
            "start_square_bracket",
            None,
            Some("touch"),
            None,
            None,
        );
        ins(
            &mut d,
            "end_square_bracket",
            Some("touch"),
            None,
            None,
            None,
        );
        ins(
            &mut d,
            "start_angle_bracket",
            None,
            Some("touch"),
            None,
            None,
        );
        ins(
            &mut d,
            "end_angle_bracket",
            Some("touch"),
            None,
            None,
            None,
        );

        // Identifiers & literals
        ins(
            &mut d,
            "object_reference",
            None,
            None,
            Some("touch:inline"),
            None,
        );
        ins(
            &mut d,
            "numeric_literal",
            None,
            None,
            Some("touch:inline"),
            None,
        );
        ins(
            &mut d,
            "bind_variable",
            None,
            None,
            Some("touch"),
            None,
        );

        // Functions & types
        ins(
            &mut d,
            "function_name",
            None,
            None,
            Some("touch:inline"),
            None,
        );
        ins(
            &mut d,
            "function_contents",
            Some("touch:inline"),
            None,
            None,
            None,
        );
        ins(
            &mut d,
            "function_parameter_list",
            Some("touch:inline"),
            None,
            None,
            None,
        );
        ins(
            &mut d,
            "array_type",
            None,
            None,
            Some("touch:inline"),
            None,
        );
        ins(
            &mut d,
            "typed_array_literal",
            None,
            None,
            Some("touch"),
            None,
        );
        ins(
            &mut d,
            "sized_array_type",
            None,
            None,
            Some("touch"),
            None,
        );
        ins(
            &mut d,
            "struct_type",
            None,
            None,
            Some("touch:inline"),
            None,
        );
        ins(
            &mut d,
            "bracketed_arguments",
            Some("touch:inline"),
            None,
            None,
            None,
        );
        ins(
            &mut d,
            "match_condition",
            None,
            None,
            Some("touch:inline"),
            None,
        );
        ins(
            &mut d,
            "typed_struct_literal",
            None,
            None,
            Some("touch"),
            None,
        );
        ins(
            &mut d,
            "semi_structured_expression",
            Some("touch:inline"),
            None,
            Some("touch:inline"),
            None,
        );
        ins(
            &mut d,
            "array_accessor",
            Some("touch:inline"),
            None,
            None,
            None,
        );
        ins(
            &mut d,
            "path_segment",
            None,
            None,
            Some("touch"),
            None,
        );
        ins(
            &mut d,
            "sql_conf_option",
            None,
            None,
            Some("touch"),
            None,
        );
        ins(
            &mut d,
            "column_path_operator",
            None,
            None,
            Some("touch"),
            Some("leading"),
        );

        // Template/comment types: any spacing
        ins(&mut d, "comment", Some("any"), Some("any"), None, None);
        ins(
            &mut d,
            "placeholder",
            Some("any"),
            Some("any"),
            None,
            None,
        );
        ins(
            &mut d,
            "template_loop",
            Some("any"),
            Some("any"),
            None,
            None,
        );

        // Clauses (line position)
        ins(&mut d, "select_clause", None, None, None, Some("alone"));
        ins(&mut d, "from_clause", None, None, None, Some("alone"));
        ins(&mut d, "where_clause", None, None, None, Some("alone"));
        ins(&mut d, "join_clause", None, None, None, Some("alone"));
        ins(&mut d, "groupby_clause", None, None, None, Some("alone"));
        ins(&mut d, "orderby_clause", None, None, None, Some("leading"));
        ins(&mut d, "having_clause", None, None, None, Some("alone"));
        ins(&mut d, "limit_clause", None, None, None, Some("alone"));

        ReflowConfig::from_dict(d)
    }

    /// Get the effective `BlockConfig` for a set of class types, optionally
    /// incorporating parent-boundary config from `DepthInfo`.
    ///
    /// Accepts a `&[String]` slice (Vec or array) to avoid requiring a
    /// `HashSet` allocation at the call site.
    pub fn get_block_config(
        &self,
        block_class_types: &[String],
        depth_info: Option<&DepthInfo>,
    ) -> BlockConfig {
        let mut block_config = BlockConfig {
            spacing_before: "single".to_string(),
            spacing_after: "single".to_string(),
            spacing_within: None,
            line_position: None,
        };

        // Apply parent boundary config from depth info.
        if let Some(di) = depth_info {
            let mut parent_start = true;
            let mut parent_end = true;

            for (idx, &hash) in di.stack_hashes.iter().rev().enumerate() {
                if let Some(pos) = di.stack_positions.get(&hash) {
                    if !matches!(pos.pos_type.as_str(), "solo" | "start") {
                        parent_start = false;
                    }
                    if !matches!(pos.pos_type.as_str(), "solo" | "end") {
                        parent_end = false;
                    }
                }
                if !parent_start && !parent_end {
                    break;
                }
                let parent_idx = di.stack_class_types.len() - 1 - idx;
                if parent_idx < di.stack_class_types.len() {
                    let parent_classes = &di.stack_class_types[parent_idx];
                    // Iterate the (typically small) parent class-type set and
                    // check membership in the config HashSet — one Vec alloc
                    // shared across the parent_start and parent_end branches.
                    let configured_parent_types: Vec<&String> = parent_classes
                        .iter()
                        .filter(|t| self.config_types.contains(t.as_str()))
                        .collect();
                    if parent_start {
                        for seg_type in &configured_parent_types {
                            let before = self
                                .config_dict
                                .get(seg_type.as_str())
                                .and_then(|m| m.get("spacing_before"))
                                .map(|s| s.as_str());
                            block_config.incorporate(before, None, None, None, None);
                        }
                    }
                    if parent_end {
                        for seg_type in &configured_parent_types {
                            let after = self
                                .config_dict
                                .get(seg_type.as_str())
                                .and_then(|m| m.get("spacing_after"))
                                .map(|s| s.as_str());
                            block_config.incorporate(None, after, None, None, None);
                        }
                    }
                }
            }
        }

        // Apply direct config for matched types — iterate the (small) block
        // class-type slice and look up each hit in the config HashMap.
        // No intermediate Vec allocation needed.
        for seg_type in block_class_types
            .iter()
            .filter(|t| self.config_types.contains(t.as_str()))
        {
            let config = self.config_dict.get(seg_type.as_str());
            block_config.incorporate(None, None, None, None, config);
        }

        block_config
    }

    /// Return the `spacing_within` value for a set of class types, or `None`
    /// if no `spacing_within` is configured for any of the matching types.
    ///
    /// This is a fast-path alternative to calling `get_block_config` just
    /// to check `spacing_within` — used in the hot `stack_spacing_configs`
    /// loop in `ReflowBlock::from_config`.
    pub fn get_spacing_within(&self, class_types: &HashSet<String>) -> Option<String> {
        for seg_type in self.config_types.intersection(class_types) {
            if let Some(within) = self
                .config_dict
                .get(seg_type.as_str())
                .and_then(|m| m.get("spacing_within"))
            {
                if !within.is_empty() {
                    return Some(within.clone());
                }
            }
        }
        None
    }

    /// Return extra inherited class types for *segment_type*.
    ///
    /// Looks up the `class_types_map` populated from Python's segment
    /// metaclass hierarchy.  Returns an empty slice when no extras are
    /// registered (most segment types).
    pub fn extra_class_types(&self, segment_type: &str) -> &[String] {
        self.class_types_map
            .get(segment_type)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_has_comma() {
        let cfg = ReflowConfig::default_ansi();
        assert!(cfg.config_types.contains("comma"));
        let bc = cfg.get_block_config(&["comma".to_string()], None);
        assert_eq!(bc.spacing_before, "touch");
        assert_eq!(bc.spacing_after, "single");
    }

    #[test]
    fn test_default_config_dot() {
        let cfg = ReflowConfig::default_ansi();
        let bc = cfg.get_block_config(&["dot".to_string()], None);
        assert_eq!(bc.spacing_before, "touch");
        assert_eq!(bc.spacing_after, "touch");
    }

    #[test]
    fn test_default_config_end_of_file() {
        let cfg = ReflowConfig::default_ansi();
        let bc = cfg.get_block_config(&["end_of_file".to_string()], None);
        assert_eq!(bc.spacing_before, "touch");
    }

    #[test]
    fn test_unconfigured_type_defaults_single() {
        let cfg = ReflowConfig::default_ansi();
        let bc = cfg.get_block_config(&["some_random_type".to_string()], None);
        assert_eq!(bc.spacing_before, "single");
        assert_eq!(bc.spacing_after, "single");
    }
}
