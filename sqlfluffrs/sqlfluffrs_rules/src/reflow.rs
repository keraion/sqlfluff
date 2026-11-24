//! SPIKE: the reflow sequence build (Python's `ReflowSequence.from_root`):
//! depth map, per-block config resolution and block/point construction.
//! Timing experiment only: nothing consumes the result yet.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use sqlfluffrs_parser::{Arena, NodeId};

/// `[sqlfluff:layout:type:<type>]` for one segment type.
#[derive(Default, Clone)]
pub struct TypeConfig {
    spacing_before: Option<String>,
    spacing_after: Option<String>,
    spacing_within: Option<String>,
    line_position: Option<String>,
    keyword_line_position: Option<String>,
    keyword_line_position_exclusions: Option<String>,
}

pub struct ReflowConfig {
    types: HashMap<String, TypeConfig>,
}

impl ReflowConfig {
    pub fn from_layout(layout: HashMap<String, HashMap<String, String>>) -> Self {
        let types = layout
            .into_iter()
            .map(|(seg_type, mut cfg)| {
                let mut take = |k: &str| cfg.remove(k).filter(|v| !v.is_empty());
                let tc = TypeConfig {
                    spacing_before: take("spacing_before"),
                    spacing_after: take("spacing_after"),
                    spacing_within: take("spacing_within"),
                    line_position: take("line_position"),
                    keyword_line_position: take("keyword_line_position"),
                    keyword_line_position_exclusions: take("keyword_line_position_exclusions"),
                };
                (seg_type, tc)
            })
            .collect();
        ReflowConfig { types }
    }
}

/// Python's `BlockConfig`.
#[derive(Clone)]
pub struct BlockConfig {
    spacing_before: String,
    spacing_after: String,
    spacing_within: Option<String>,
    line_position: Option<String>,
    keyword_line_position: Option<String>,
    keyword_line_position_exclusions: Vec<String>,
}

impl Default for BlockConfig {
    fn default() -> Self {
        BlockConfig {
            spacing_before: "single".into(),
            spacing_after: "single".into(),
            spacing_within: None,
            line_position: None,
            keyword_line_position: None,
            keyword_line_position_exclusions: Vec::new(),
        }
    }
}

impl BlockConfig {
    /// `BlockConfig.incorporate(config=...)`.
    fn incorporate(&mut self, c: &TypeConfig) {
        if let Some(v) = &c.spacing_before {
            self.spacing_before.clone_from(v);
        }
        if let Some(v) = &c.spacing_after {
            self.spacing_after.clone_from(v);
        }
        if c.spacing_within.is_some() {
            self.spacing_within.clone_from(&c.spacing_within);
        }
        if c.line_position.is_some() {
            self.line_position.clone_from(&c.line_position);
        }
        if c.keyword_line_position.is_some() {
            self.keyword_line_position.clone_from(&c.keyword_line_position);
        }
        if let Some(v) = &c.keyword_line_position_exclusions {
            self.keyword_line_position_exclusions = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum PosType {
    None,
    Solo,
    Start,
    End,
}

/// Python's `StackPosition`.
#[derive(Clone, Copy)]
pub struct StackPosition {
    pub idx: u32,
    pub len: u32,
    ty: PosType,
}

/// Python's `DepthInfo`. `positions[i]` is the position, within `stack[i]`,
/// of the child on the path to this raw. Siblings share `stack`.
#[derive(Clone)]
pub struct DepthInfo {
    pub stack: Rc<[NodeId]>,
    pub stack_class_types: Rc<[Arc<Vec<String>>]>,
    pub positions: Vec<StackPosition>,
}

pub struct Block {
    pub segment: NodeId,
    pub config: BlockConfig,
    pub depth_info: DepthInfo,
    /// (stack index, value) for each ancestor that configures it.
    pub stack_spacing_configs: Vec<(u32, String)>,
    pub line_position_configs: Vec<(u32, String)>,
    pub keyword_line_position_configs: Vec<(u32, String)>,
    pub keyword_line_position_exclusions_configs: Vec<(u32, Vec<String>)>,
}

pub struct Point {
    pub segments: Vec<NodeId>,
    pub impulse: i32,
    pub trough: i32,
    pub implicit_indents: Vec<i32>,
}

pub enum Element {
    Block(Box<Block>),
    Point(Point),
}

pub struct ReflowSequence {
    pub elements: Vec<Element>,
    /// Depth info for every raw, in raw order (Python's `DepthMap`).
    pub depth_map: Vec<(NodeId, DepthInfo)>,
}

/// (is_block, n_segments, spacing_before, spacing_after, line_position,
/// stack_depth, stack_spacing_configs, line_position_configs, impulse,
/// trough, implicit_indents)
pub type Digest = (
    bool,
    usize,
    String,
    String,
    Option<String>,
    usize,
    Vec<(u32, String)>,
    Vec<(u32, String)>,
    i32,
    i32,
    Vec<i32>,
);

impl ReflowSequence {
    pub fn digest(&self) -> Vec<Digest> {
        self.elements
            .iter()
            .map(|e| match e {
                Element::Block(b) => (
                    true,
                    1,
                    b.config.spacing_before.clone(),
                    b.config.spacing_after.clone(),
                    b.config.line_position.clone(),
                    b.depth_info.stack.len(),
                    b.stack_spacing_configs.clone(),
                    b.line_position_configs.clone(),
                    0,
                    0,
                    Vec::new(),
                ),
                Element::Point(p) => (
                    false,
                    p.segments.len(),
                    String::new(),
                    String::new(),
                    None,
                    0,
                    Vec::new(),
                    Vec::new(),
                    p.impulse,
                    p.trough,
                    p.implicit_indents.clone(),
                ),
            })
            .collect()
    }
}

struct Builder<'a> {
    arena: &'a Arena,
    config: &'a ReflowConfig,
    code: HashMap<NodeId, bool>,
    /// Python's `get_parent_block_config` cache, per ancestor node.
    parent_cfg: HashMap<NodeId, Option<Rc<BlockConfig>>>,
    depth_map: Vec<(NodeId, DepthInfo)>,
}

impl<'a> Builder<'a> {
    /// `is_code`, memoised (the arena recomputes it for containers).
    fn is_code(&mut self, id: NodeId) -> bool {
        if let Some(&c) = self.code.get(&id) {
            return c;
        }
        let kids = self.arena.children(id);
        let c = if kids.is_empty() {
            self.arena.is_code(id)
        } else {
            let mut any = false;
            for &k in kids {
                // Visit all, so every child is memoised.
                any |= self.is_code(k);
            }
            any
        };
        self.code.insert(id, c);
        c
    }

    /// `DepthMap.from_parent`.
    fn walk(
        &mut self,
        node: NodeId,
        stack: &Rc<[NodeId]>,
        class_types: &Rc<[Arc<Vec<String>>]>,
        positions: &mut Vec<StackPosition>,
    ) {
        let arena = self.arena;
        let kids = arena.children(node);
        let mut first_code = None;
        let mut last_code = None;
        let mut n_code = 0;
        for (i, &k) in kids.iter().enumerate() {
            if self.is_code(k) {
                n_code += 1;
                first_code.get_or_insert(i);
                last_code = Some(i);
            }
        }
        for (i, &child) in kids.iter().enumerate() {
            let ty = if n_code == 1 {
                PosType::Solo
            } else if Some(i) == first_code {
                PosType::Start
            } else if Some(i) == last_code {
                PosType::End
            } else {
                PosType::None
            };
            positions.push(StackPosition {
                idx: i as u32,
                len: kids.len() as u32,
                ty,
            });
            if arena.children(child).is_empty() {
                self.depth_map.push((
                    child,
                    DepthInfo {
                        stack: stack.clone(),
                        stack_class_types: class_types.clone(),
                        positions: positions.clone(),
                    },
                ));
            } else {
                let child_stack: Rc<[NodeId]> =
                    stack.iter().copied().chain(std::iter::once(child)).collect();
                let child_ct: Rc<[Arc<Vec<String>>]> = class_types
                    .iter()
                    .cloned()
                    .chain(std::iter::once(arena.class_types(child)))
                    .collect();
                self.walk(child, &child_stack, &child_ct, positions);
            }
            positions.pop();
        }
    }

    /// `ReflowConfig.get_parent_block_config`: `None` when nothing is configured.
    fn parent_block_config(
        &mut self,
        node: NodeId,
        class_types: &[String],
    ) -> Option<Rc<BlockConfig>> {
        if let Some(c) = self.parent_cfg.get(&node) {
            return c.clone();
        }
        let mut cfg: Option<BlockConfig> = None;
        for t in class_types {
            if let Some(tc) = self.config.types.get(t) {
                cfg.get_or_insert_with(BlockConfig::default).incorporate(tc);
            }
        }
        let cfg = cfg.map(Rc::new);
        self.parent_cfg.insert(node, cfg.clone());
        cfg
    }

    /// `ReflowBlock.from_config`.
    fn block(&mut self, seg: NodeId, depth_info: DepthInfo) -> Block {
        let types = &self.config.types;
        // `ReflowConfig.get_block_config`.
        let mut config = BlockConfig::default();
        let (mut parent_start, mut parent_end) = (true, true);
        for i in (0..depth_info.stack.len()).rev() {
            let ty = depth_info.positions[i].ty;
            if !matches!(ty, PosType::Solo | PosType::Start) {
                parent_start = false;
            }
            if !matches!(ty, PosType::Solo | PosType::End) {
                parent_end = false;
            }
            if !(parent_start || parent_end) {
                break;
            }
            for t in depth_info.stack_class_types[i].iter() {
                if let Some(tc) = types.get(t) {
                    if parent_start {
                        if let Some(v) = &tc.spacing_before {
                            config.spacing_before.clone_from(v);
                        }
                    }
                    if parent_end {
                        if let Some(v) = &tc.spacing_after {
                            config.spacing_after.clone_from(v);
                        }
                    }
                }
            }
        }
        for t in self.arena.class_types(seg).iter() {
            if let Some(tc) = types.get(t) {
                config.incorporate(tc);
            }
        }

        let mut block = Block {
            segment: seg,
            config,
            stack_spacing_configs: Vec::new(),
            line_position_configs: Vec::new(),
            keyword_line_position_configs: Vec::new(),
            keyword_line_position_exclusions_configs: Vec::new(),
            depth_info,
        };
        for i in 0..block.depth_info.stack.len() {
            let node = block.depth_info.stack[i];
            let ct = block.depth_info.stack_class_types[i].clone();
            let Some(cfg) = self.parent_block_config(node, &ct) else {
                continue;
            };
            let i = i as u32;
            if let Some(v) = &cfg.spacing_within {
                block.stack_spacing_configs.push((i, v.clone()));
            }
            if let Some(v) = &cfg.line_position {
                block.line_position_configs.push((i, v.clone()));
            }
            if let Some(v) = &cfg.keyword_line_position {
                block.keyword_line_position_configs.push((i, v.clone()));
            }
            if !cfg.keyword_line_position_exclusions.is_empty() {
                block
                    .keyword_line_position_exclusions_configs
                    .push((i, cfg.keyword_line_position_exclusions.clone()));
            }
        }
        block
    }
}

/// `ReflowPoint._generate_indent_stats`.
fn point(arena: &Arena, segments: Vec<NodeId>) -> Point {
    let (mut trough, mut sum, mut implicit) = (0, 0, Vec::new());
    for &seg in &segments {
        if let Some(is_implicit) = arena.is_implicit(seg) {
            sum += if arena.is_type(seg, "dedent") { -1 } else { 1 };
            if is_implicit {
                implicit.push(sum);
            }
        }
        if sum < trough {
            trough = sum;
        }
    }
    Point {
        segments,
        impulse: sum,
        trough,
        implicit_indents: implicit,
    }
}

/// Is this raw point-like: whitespace, newline, indent, or a literal
/// placeholder that consumed only whitespace.
fn is_point_segment(arena: &Arena, seg: NodeId) -> bool {
    if arena.is_type(seg, "whitespace")
        || arena.is_type(seg, "newline")
        || arena.is_type(seg, "indent")
    {
        return true;
    }
    match arena.source_str(seg) {
        Some(src) if arena.block_type(seg).as_deref() == Some("literal") => {
            !src.is_empty() && src.chars().all(char::is_whitespace)
        }
        _ => false,
    }
}

/// `ReflowSequence.from_root`.
pub fn build(arena: &Arena, config: &ReflowConfig) -> ReflowSequence {
    let root = arena.root();
    let mut b = Builder {
        arena,
        config,
        code: HashMap::new(),
        parent_cfg: HashMap::new(),
        depth_map: Vec::new(),
    };
    let stack: Rc<[NodeId]> = Rc::from(vec![root]);
    let class_types: Rc<[Arc<Vec<String>>]> = Rc::from(vec![arena.class_types(root)]);
    b.walk(root, &stack, &class_types, &mut Vec::new());

    // `_elements_from_raw_segments`.
    let depth_map = std::mem::take(&mut b.depth_map);
    let mut elements = Vec::with_capacity(depth_map.len());
    let mut buff: Vec<NodeId> = Vec::new();
    for (seg, depth_info) in &depth_map {
        if is_point_segment(arena, *seg) {
            buff.push(*seg);
            continue;
        }
        if !elements.is_empty() || !buff.is_empty() {
            elements.push(Element::Point(point(arena, std::mem::take(&mut buff))));
        }
        elements.push(Element::Block(Box::new(b.block(*seg, depth_info.clone()))));
    }
    if !buff.is_empty() {
        elements.push(Element::Point(point(arena, buff)));
    }
    ReflowSequence {
        elements,
        depth_map,
    }
}

// ---------------------------------------------------------------------------
// SPIKE: `ReflowSequence.respace()` as LT01 calls it: the whole file, no
// newline stripping requested, no filter, no earlier results to build on.
// Anything outside that (alignment constraints, unknown constraints, points
// that imply an earlier removal) returns `None` and Python handles the file.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum FixKind {
    Delete,
    Replace,
    CreateAfter,
    CreateBefore,
}

impl FixKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FixKind::Delete => "delete",
            FixKind::Replace => "replace",
            FixKind::CreateAfter => "create_after",
            FixKind::CreateBefore => "create_before",
        }
    }
}

/// One `LintResult` with its single `LintFix`. `edit` is the replacement
/// whitespace for `Replace`; creations always insert one space.
pub struct RespaceResult {
    pub anchor: NodeId,
    pub description: String,
    pub kind: FixKind,
    pub fix_anchor: NodeId,
    pub edit: String,
}

/// Python's `repr()` of a str.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if !c.is_ascii() && (c.is_whitespace() || c.is_control()) => {
                let n = c as u32;
                if n <= 0xff {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `helpers.pretty_segment_name`.
fn pretty_segment_name(arena: &Arena, seg: NodeId) -> String {
    if arena.is_type(seg, "symbol") {
        format!(
            "{} {}",
            arena.get_type(seg).replace('_', " "),
            py_repr(&arena.raw(seg))
        )
    } else if arena.is_type(seg, "keyword") {
        format!("{} keyword", py_repr(&arena.raw(seg)))
    } else {
        arena.get_type(seg).replace('_', " ")
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

const OPERATOR_CHARS: &str = "+-*/<>=~!@#%^&|`?";

fn would_join_words(arena: &Arena, prev: &Block, next: &Block) -> bool {
    let (p, n) = (arena.raw(prev.segment), arena.raw(next.segment));
    matches!((p.chars().last(), n.chars().next()), (Some(a), Some(b)) if is_word_char(a) && is_word_char(b))
}

fn would_fuse_tokens(arena: &Arena, prev: Option<&Block>, next: Option<&Block>) -> bool {
    let (Some(prev), Some(next)) = (prev, next) else {
        return false;
    };
    let (p, n) = (arena.raw(prev.segment), arena.raw(next.segment));
    let (Some(a), Some(b)) = (p.chars().last(), n.chars().next()) else {
        return false;
    };
    if a == '-' && b == '-' {
        return true;
    }
    (arena.is_type(prev.segment, "sign_indicator") || arena.is_type(prev.segment, "tilde"))
        && OPERATOR_CHARS.contains(b)
}

/// `_unpack_constraint`. `None` for a modifier Python would reject.
fn unpack_constraint<'a>(constraint: &'a str, strip_newlines: &mut bool) -> Option<&'a str> {
    if constraint == "inline" {
        *strip_newlines = true;
        return Some("touch");
    }
    if constraint.starts_with("align") {
        return Some(constraint);
    }
    let (head, modifier) = constraint.split_once(':').unwrap_or((constraint, ""));
    match modifier {
        "" => {}
        "inline" => *strip_newlines = true,
        _ => return None,
    }
    Some(head)
}

fn head(constraint: &str) -> &str {
    constraint.split(':').next().unwrap_or("")
}

/// `determine_constraints`: (pre, post, strip_newlines).
fn determine_constraints<'a>(
    arena: &Arena,
    prev: Option<&'a Block>,
    next: Option<&'a Block>,
    strip_newlines: bool,
) -> Option<(&'a str, &'a str, bool)> {
    let mut strip = strip_newlines;
    let mut pre = unpack_constraint(
        prev.map_or("single", |b| b.config.spacing_after.as_str()),
        &mut strip,
    )?;
    let mut post = unpack_constraint(
        next.map_or("single", |b| b.config.spacing_before.as_str()),
        &mut strip,
    )?;

    let mut within_spacing = "";
    if let (Some(p), Some(n)) = (prev, next) {
        let (a, b) = (&p.depth_info.stack, &n.depth_info.stack);
        let common = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
        let mut within = p
            .stack_spacing_configs
            .iter()
            .find(|(i, _)| *i as usize + 1 == common)
            .map(|(_, c)| c.as_str());
        if head(within.unwrap_or("")) != "any" {
            for (i, c) in p.stack_spacing_configs.iter().rev() {
                if head(c) == "any" && (*i as usize) < common {
                    within = Some(c.as_str());
                    break;
                }
            }
        }
        if let Some(w) = within {
            within_spacing = unpack_constraint(w, &mut strip)?;
        }
        if arena.is_type(p.segment, "comment") || arena.is_type(n.segment, "comment") {
            strip = false;
        }
    }

    match within_spacing {
        "touch" => {
            if pre != "any" {
                pre = "touch";
            }
            if post != "any" {
                post = "touch";
            }
        }
        "any" => {
            pre = "any";
            post = "any";
        }
        "single" | "" => {}
        _ => return None,
    }

    if pre == "touch" || post == "touch" {
        if let (Some(p), Some(n)) = (prev, next) {
            if would_join_words(arena, p, n) {
                if pre == "touch" {
                    pre = "single";
                }
                if post == "touch" {
                    post = "single";
                }
            }
        }
    }

    if strip
        && (matches!(pre, "touch" | "any") || matches!(post, "touch" | "any"))
        && would_fuse_tokens(arena, prev, next)
    {
        strip = false;
    }
    Some((pre, post, strip))
}

fn delete(seg: NodeId, description: &str) -> RespaceResult {
    RespaceResult {
        anchor: seg,
        description: description.to_string(),
        kind: FixKind::Delete,
        fix_anchor: seg,
        edit: String::new(),
    }
}

/// `ReflowPoint.respace_point` for one point. `None` = leave it to Python.
fn respace_point(
    arena: &Arena,
    point: &Point,
    prev: Option<&Block>,
    next: Option<&Block>,
    results: &mut Vec<RespaceResult>,
) -> Option<()> {
    let (pre, post, strip) = determine_constraints(arena, prev, next, false)?;

    // `process_spacing`.
    let mut removed: Vec<NodeId> = Vec::new();
    let mut last_ws: Vec<NodeId> = Vec::new();
    for &seg in &point.segments {
        if arena.is_type(seg, "whitespace") {
            last_ws.push(seg);
        } else if arena.is_type(seg, "newline") || arena.is_type(seg, "end_of_file") {
            if arena.pos_marker(seg).is_some_and(|pm| !pm.is_literal()) {
                last_ws.clear();
                continue;
            }
            if strip && arena.is_type(seg, "newline") {
                removed.push(seg);
                results.push(delete(seg, "Unexpected line break."));
                continue;
            }
            for &ws in &last_ws {
                removed.push(ws);
                results.push(delete(ws, "Unnecessary trailing whitespace."));
            }
            last_ws.clear();
        }
    }
    for &ws in last_ws.iter().skip(1) {
        removed.push(ws);
        results.push(delete(ws, "Removing duplicate whitespace."));
    }
    let mut last_whitespace = last_ws.first().copied();
    let next_is_eof = next.is_some_and(|b| arena.is_type(b.segment, "end_of_file"));

    if next_is_eof {
        if let Some(ws) = last_whitespace.take() {
            results.push(delete(
                ws,
                "Unnecessary trailing whitespace at end of file.",
            ));
            removed.push(ws);
        }
    }

    let has_newline = point
        .segments
        .iter()
        .any(|s| !removed.contains(s) && arena.is_type(*s, "newline"));
    if (has_newline && !strip) || next_is_eof {
        // Indentation's job, bar one case: whitespace after a newline where
        // the positions imply something was removed between them. That needs
        // the earlier result it belongs to, so hand the file back.
        if let Some(ws) = last_whitespace {
            let ws_idx = point.segments.iter().position(|s| *s == ws)?;
            if ws_idx > 0 {
                let mut prev_seg = point.segments[ws_idx - 1];
                for i in (0..ws_idx).rev() {
                    prev_seg = point.segments[i];
                    if !arena.is_type(prev_seg, "indent") {
                        break;
                    }
                }
                if arena.is_type(prev_seg, "newline") {
                    let end = arena
                        .pos_marker(prev_seg)?
                        .working_loc_after(&arena.raw(prev_seg));
                    let start = arena.pos_marker(ws)?.working_loc();
                    if end < start {
                        return None;
                    }
                }
            }
        }
        return Some(());
    }

    if let Some(ws) = last_whitespace {
        // `handle_respace__inline_with_space`.
        if pre == "any" || post == "any" {
            return Some(());
        }
        if pre == "touch" || post == "touch" {
            if would_fuse_tokens(arena, prev, next) {
                return Some(());
            }
            let description = match next {
                Some(n) => format!(
                    "Unexpected whitespace before {}.",
                    pretty_segment_name(arena, n.segment)
                ),
                None => "Unexpected whitespace".to_string(),
            };
            results.push(delete(ws, &description));
            return Some(());
        }
        if post.starts_with("align") || pre != "single" || post != "single" {
            return None;
        }
        let raw = arena.raw(ws);
        if raw != " " {
            let description = match next {
                Some(n) => format!(
                    "Expected only single space before {}. Found {}.",
                    pretty_segment_name(arena, n.segment),
                    py_repr(&raw)
                ),
                None => format!("Expected only single space. Found {}.", py_repr(&raw)),
            };
            results.push(RespaceResult {
                anchor: ws,
                description,
                kind: FixKind::Replace,
                fix_anchor: ws,
                edit: " ".to_string(),
            });
        }
        return Some(());
    }

    // `handle_respace__inline_without_space`.
    if matches!(pre, "touch" | "any") || matches!(post, "touch" | "any") {
        return Some(());
    }
    if post.starts_with("align") || pre != "single" || post != "single" {
        return None;
    }
    // A neighbour with no position is itself an insertion; Python folds the
    // new whitespace into that insertion's fix.
    if prev.is_some_and(|b| arena.pos_marker(b.segment).is_none())
        || next.is_some_and(|b| arena.pos_marker(b.segment).is_none())
    {
        return None;
    }
    let description = match (prev, next) {
        (Some(p), Some(n)) => format!(
            "Expected single whitespace between {} and {}.",
            pretty_segment_name(arena, p.segment),
            pretty_segment_name(arena, n.segment)
        ),
        _ => "Expected single whitespace.".to_string(),
    };
    match (prev, next) {
        (Some(p), n) => results.push(RespaceResult {
            anchor: n.map_or(p.segment, |n| n.segment),
            description,
            kind: FixKind::CreateAfter,
            fix_anchor: p.segment,
            edit: " ".to_string(),
        }),
        (None, Some(n)) => results.push(RespaceResult {
            anchor: n.segment,
            description,
            kind: FixKind::CreateBefore,
            fix_anchor: n.segment,
            edit: " ".to_string(),
        }),
        (None, None) => {}
    }
    Some(())
}

/// `ReflowSequence.respace()`. `None` = a case this doesn't cover.
pub fn respace(arena: &Arena, seq: &ReflowSequence) -> Option<Vec<RespaceResult>> {
    let mut results = Vec::new();
    let block = |i: usize| match seq.elements.get(i) {
        Some(Element::Block(b)) => Some(&**b),
        _ => None,
    };
    for (idx, elem) in seq.elements.iter().enumerate() {
        if let Element::Point(point) = elem {
            let prev = if idx > 0 { block(idx - 1) } else { None };
            respace_point(arena, point, prev, block(idx + 1), &mut results)?;
        }
    }
    Some(results)
}

// ---------------------------------------------------------------------------
// SPIKE: `ReflowSequence.reindent()` as LT02 calls it, for files with no
// templating. Returns `None` (Python handles the file) for: any placeholder
// or template loop, `implicit_indents = require`, leading-comma alignment,
// an indent unit other than tab/space, and any step which would have to
// anchor a fix on a segment an earlier step in the same pass created.
// ---------------------------------------------------------------------------

pub struct ReindentOptions {
    pub indent_unit: String,
    pub tab_space_size: usize,
    pub implicit_indents: String,
    pub ignore_comment_lines: bool,
    pub skip_indentation_in: Vec<String>,
}

#[derive(Clone, Copy)]
pub enum IndentFixKind {
    /// Delete the target.
    Delete,
    /// Replace the target whitespace with whitespace `text`.
    ReplaceWhitespace,
    /// Replace the target newline with itself plus whitespace `text`.
    IndentAfterNewline,
    /// Create a newline (plus whitespace `text`, if any) before the target.
    CreateBefore,
    /// Replace the target whitespace with a newline (plus the whitespace
    /// edited to `text`, if any).
    ReplaceWithNewline,
    /// Create the `extra` comment, a newline and (if any) whitespace `text`
    /// before the target.
    MoveCommentBefore,
    /// Create a newline (plus whitespace `text`, if any) after the target.
    CreateAfter,
    /// Create a single space after the target.
    CreateSpaceAfter,
    /// Create a single space before the target.
    CreateSpaceBefore,
}

impl IndentFixKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IndentFixKind::Delete => "delete",
            IndentFixKind::ReplaceWhitespace => "replace_whitespace",
            IndentFixKind::IndentAfterNewline => "indent_after_newline",
            IndentFixKind::CreateBefore => "create_before",
            IndentFixKind::ReplaceWithNewline => "replace_with_newline",
            IndentFixKind::MoveCommentBefore => "move_comment_before",
            IndentFixKind::CreateAfter => "create_after",
            IndentFixKind::CreateSpaceAfter => "create_space_after",
            IndentFixKind::CreateSpaceBefore => "create_space_before",
        }
    }
}

pub struct IndentFix {
    pub kind: IndentFixKind,
    pub target: NodeId,
    pub text: String,
    /// The comment being moved, for `MoveCommentBefore`.
    pub extra: Option<NodeId>,
}

pub struct IndentResult {
    pub anchor: NodeId,
    pub description: String,
    pub source: &'static str,
    pub fixes: Vec<IndentFix>,
}

/// A segment of a point, which may have been edited by an earlier step.
#[derive(Clone)]
enum PSeg {
    Orig(NodeId),
    NewNewline,
    NewWhitespace(String),
    /// An original whitespace edited to a new raw.
    Edited(NodeId, String),
}

/// Python's `_IndentPoint`.
#[derive(Clone)]
struct IndentPoint {
    idx: usize,
    impulse: i32,
    trough: i32,
    initial_balance: i32,
    last_line_break_idx: Option<usize>,
    is_line_break: bool,
    untaken_indents: Vec<i32>,
}

impl IndentPoint {
    fn closing_balance(&self) -> i32 {
        self.initial_balance + self.impulse
    }
}

/// Python's `_IndentLine`.
struct IndentLine {
    initial_balance: i32,
    points: Vec<IndentPoint>,
}

/// Python truthiness of an `Optional[int]`.
fn truthy(idx: Option<usize>) -> bool {
    matches!(idx, Some(n) if n != 0)
}

impl IndentLine {
    fn from_points(points: Vec<IndentPoint>) -> Self {
        let last = points.last().unwrap();
        let initial_balance = if truthy(last.last_line_break_idx) {
            points[0].closing_balance()
        } else {
            0
        };
        IndentLine {
            initial_balance,
            points,
        }
    }

    /// The element range `iter_elements` covers.
    fn range(&self) -> std::ops::Range<usize> {
        let last = self.points.last().unwrap();
        if last.last_line_break_idx.is_none() {
            0..last.idx
        } else {
            self.points[0].idx..last.idx
        }
    }

    fn opening_balance(&self) -> i32 {
        if self.points.last().unwrap().last_line_break_idx.is_none() {
            0
        } else {
            self.points[0].closing_balance()
        }
    }

    fn closing_balance(&self) -> i32 {
        self.points.last().unwrap().closing_balance()
    }

    fn desired_indent_units(&self, forced: &[i32]) -> i32 {
        let p = &self.points[0];
        let relevant = if p.trough != 0 {
            let limit = self.initial_balance - (p.impulse - p.trough);
            p.untaken_indents.iter().filter(|i| **i <= limit).count()
        } else {
            p.untaken_indents.len()
        };
        self.initial_balance - relevant as i32 + forced.len() as i32
    }
}

type Stats = (i32, i32, Vec<i32>);

/// `_indent_description`.
fn indent_description(indent: &str) -> Option<String> {
    Some(if indent.is_empty() {
        "no indent".to_string()
    } else if indent.contains(' ') && indent.contains('\t') {
        "mixed indent".to_string()
    } else if indent.starts_with(' ') {
        format!("indent of {} spaces", indent.chars().count())
    } else if indent.starts_with('\t') {
        format!("indent of {} tabs", indent.chars().count())
    } else {
        return None;
    })
}

struct Reindenter<'a> {
    arena: &'a Arena,
    elements: &'a [Element],
    /// Points replaced by an earlier step, by element index.
    edited: HashMap<usize, Vec<PSeg>>,
    results: Vec<IndentResult>,
    single_indent: String,
}

impl<'a> Reindenter<'a> {
    fn block(&self, idx: usize) -> Option<&'a Block> {
        match self.elements.get(idx) {
            Some(Element::Block(b)) => Some(b),
            _ => None,
        }
    }

    fn point(&self, idx: usize) -> Option<&'a Point> {
        match self.elements.get(idx) {
            Some(Element::Point(p)) => Some(p),
            _ => None,
        }
    }

    fn block_is(&self, idx: usize, seg_type: &str) -> bool {
        self.block(idx)
            .is_some_and(|b| self.arena.is_type(b.segment, seg_type))
    }

    /// The current segments of the point at `idx`.
    fn segs(&self, idx: usize) -> Option<Vec<PSeg>> {
        if let Some(s) = self.edited.get(&idx) {
            return Some(s.clone());
        }
        Some(self.point(idx)?.segments.iter().map(|s| PSeg::Orig(*s)).collect())
    }

    fn is_newline(&self, s: &PSeg) -> bool {
        match s {
            PSeg::Orig(id) => self.arena.is_type(*id, "newline"),
            PSeg::NewNewline => true,
            _ => false,
        }
    }

    fn is_whitespace(&self, s: &PSeg) -> bool {
        match s {
            PSeg::Orig(id) => self.arena.is_type(*id, "whitespace"),
            PSeg::NewNewline => false,
            _ => true,
        }
    }

    fn raw(&self, s: &PSeg) -> String {
        match s {
            PSeg::Orig(id) => self.arena.raw(*id),
            PSeg::NewNewline => "\n".to_string(),
            PSeg::NewWhitespace(r) | PSeg::Edited(_, r) => r.clone(),
        }
    }

    /// `ReflowPoint._get_indent_segment`: index into `segs`.
    fn indent_segment(&self, segs: &[PSeg]) -> Option<usize> {
        let mut indent = None;
        for (i, s) in segs.iter().enumerate().rev() {
            if self.is_newline(s) {
                return indent;
            } else if self.is_whitespace(s) {
                indent = Some(i);
            }
        }
        None
    }

    /// `_deduce_line_current_indent`.
    fn current_indent(&self, last_line_break_idx: Option<usize>) -> Option<String> {
        let first_is_point = self.point(0).is_some();
        let first_segs = if first_is_point { self.segs(0)? } else { Vec::new() };
        if first_is_point && first_segs.is_empty() {
            return Some(String::new());
        }
        if truthy(last_line_break_idx) {
            let segs = self.segs(last_line_break_idx?)?;
            return Some(match self.indent_segment(&segs) {
                Some(i) => self.raw(&segs[i]),
                None => String::new(),
            });
        }
        if first_is_point {
            let PSeg::Orig(first) = &first_segs[0] else {
                return None;
            };
            if self.arena.pos_marker(*first)?.working_loc() == (1, 1) {
                // The last whitespace, else whatever the scan ended on.
                let found = first_segs.iter().rev().find(|s| self.is_whitespace(s));
                return Some(found.map_or_else(String::new, |s| self.raw(s)));
            }
        }
        Some(String::new())
    }

    fn push(&mut self, anchor: NodeId, description: String, source: &'static str, fixes: Vec<IndentFix>) {
        self.results.push(IndentResult {
            anchor,
            description,
            source,
            fixes,
        });
    }

    /// `ReflowPoint.indent_to(desired, before=..., source=...)`.
    fn indent_to(&mut self, idx: usize, desired: &str, before: NodeId, source: &'static str) -> Option<()> {
        self.indent_to_anchor(idx, desired, before, false, source)
    }

    /// `indent_to` with either `before=` (the segment after the point) or,
    /// when `after` is set, `after=` (the segment before it).
    fn indent_to_anchor(
        &mut self,
        idx: usize,
        desired: &str,
        before: NodeId,
        after: bool,
        source: &'static str,
    ) -> Option<()> {
        let mut segs = self.segs(idx)?;
        let indent = self.indent_segment(&segs);
        let has_newline = segs.iter().any(|s| self.is_newline(s));
        let expected = || Some(format!("Expected {}.", indent_description(desired)?));

        if has_newline {
            if let Some(i) = indent {
                if self.raw(&segs[i]) == desired {
                    return Some(());
                }
                let PSeg::Orig(ws) = segs[i] else {
                    return None;
                };
                if desired.is_empty() {
                    self.push(
                        ws,
                        "Line should not be indented.".to_string(),
                        source,
                        vec![IndentFix {
                            kind: IndentFixKind::Delete,
                            target: ws,
                            text: String::new(),
                            extra: None,
                        }],
                    );
                    segs.remove(i);
                } else {
                    self.push(
                        ws,
                        expected()?,
                        source,
                        vec![IndentFix {
                            kind: IndentFixKind::ReplaceWhitespace,
                            target: ws,
                            text: desired.to_string(),
                            extra: None,
                        }],
                    );
                    segs[i] = PSeg::Edited(ws, desired.to_string());
                }
            } else {
                if desired.is_empty() {
                    return Some(());
                }
                let i = segs.iter().rposition(|s| self.is_newline(s))?;
                let PSeg::Orig(newline) = segs[i] else {
                    return None;
                };
                self.push(
                    before,
                    expected()?,
                    source,
                    vec![IndentFix {
                        kind: IndentFixKind::IndentAfterNewline,
                        target: newline,
                        text: desired.to_string(),
                        extra: None,
                    }],
                );
                segs.insert(i + 1, PSeg::NewWhitespace(desired.to_string()));
            }
        } else {
            let description = format!(
                "Expected line break and {} before {}.",
                indent_description(desired)?,
                py_repr(&self.arena.raw(before))
            );
            // The scan runs backwards without stopping: the first whitespace.
            match segs.iter().position(|s| self.is_whitespace(s)) {
                None => {
                    self.push(
                        before,
                        description,
                        source,
                        vec![IndentFix {
                            kind: if after {
                                IndentFixKind::CreateAfter
                            } else {
                                IndentFixKind::CreateBefore
                            },
                            target: before,
                            text: desired.to_string(),
                            extra: None,
                        }],
                    );
                    segs = vec![PSeg::NewNewline];
                    if !desired.is_empty() {
                        segs.push(PSeg::NewWhitespace(desired.to_string()));
                    }
                }
                Some(i) => {
                    let PSeg::Orig(ws) = segs[i] else {
                        return None;
                    };
                    self.push(
                        ws,
                        description,
                        source,
                        vec![IndentFix {
                            kind: IndentFixKind::ReplaceWithNewline,
                            target: ws,
                            text: desired.to_string(),
                            extra: None,
                        }],
                    );
                    let mut new = vec![PSeg::NewNewline];
                    if !desired.is_empty() {
                        new.push(PSeg::Edited(ws, desired.to_string()));
                    }
                    segs.splice(i..=i, new);
                }
            }
        }
        self.edited.insert(idx, segs);
        Some(())
    }

    /// `single_indent * n`.
    fn indent(&self, units: i32) -> String {
        self.single_indent.repeat(units.max(0) as usize)
    }

    /// `_lint_line_starting_indent`.
    fn lint_starting_indent(&mut self, line: &IndentLine, forced: &[i32]) -> Option<()> {
        let first = &line.points[0];
        let idx = first.idx;
        let before = self.block(idx + 1)?.segment;
        let current = self.current_indent(line.points.last()?.last_line_break_idx)?;
        let desired = self.indent(line.desired_indent_units(forced));
        if current == desired {
            return Some(());
        }
        if idx > 0 && idx < self.elements.len() - 1 {
            if self.block_is(idx + 1, "comment") {
                let last_indent = self.current_indent(first.last_line_break_idx)?;
                if current.chars().count() == last_indent.chars().count() {
                    return Some(());
                }
            }
            if self.block_is(idx - 1, "block_comment")
                && self.block_is(idx + 1, "block_comment")
                && current.chars().count() > desired.chars().count()
            {
                return Some(());
            }
        }
        if idx == 0 && !first.is_line_break {
            let segs = self.segs(0)?;
            let mut fixes = Vec::new();
            for s in &segs {
                let PSeg::Orig(id) = s else {
                    return None;
                };
                fixes.push(IndentFix {
                    kind: IndentFixKind::Delete,
                    target: *id,
                    text: String::new(),
                    extra: None,
                });
            }
            let anchor = fixes.first()?.target;
            self.push(
                anchor,
                "First line should not be indented.".to_string(),
                "reflow.indent.existing",
                fixes,
            );
            self.edited.insert(0, Vec::new());
            return Some(());
        }
        self.indent_to(idx, &desired, before, "reflow.indent.existing")
    }

    /// `_lint_line_untaken_positive_indents`: the indents it forced.
    fn lint_untaken_positive(&mut self, line: &IndentLine, imbalanced: &[usize]) -> Option<Vec<i32>> {
        let mut forced: Vec<i32> = Vec::new();
        for ip in &line.points {
            if imbalanced.contains(&ip.idx) {
                let mut untaken: Vec<i32> = ip.untaken_indents.clone();
                untaken.sort_unstable();
                untaken.dedup();
                let remaining = untaken.iter().filter(|u| !forced.contains(u)).count() as i32;
                let desired = self.indent(ip.closing_balance() - remaining);
                let before = self.block(ip.idx + 1)?.segment;
                self.indent_to(ip.idx, &desired, before, "reflow.indent.imbalance")?;
                forced.push(ip.closing_balance());
            }
        }
        if !forced.is_empty() {
            return Some(forced);
        }

        let starting_balance = line.opening_balance();
        let last = line.points.last()?;
        if last.initial_balance + last.trough <= starting_balance {
            return Some(forced);
        }
        let mut closing_trough = last.initial_balance + if last.trough != 0 { last.trough } else { last.impulse };
        let mut bal = 0;
        for i in last.idx + 1..self.elements.len() {
            match &self.elements[i] {
                Element::Block(b) => {
                    if !self.arena.is_type(b.segment, "comment") {
                        break;
                    }
                }
                Element::Point(p) => {
                    // Later points are untouched, so their stats still hold.
                    if p.impulse > 0 {
                        break;
                    }
                    closing_trough = bal + p.trough;
                    bal += p.impulse;
                }
            }
        }
        if !last.untaken_indents.contains(&closing_trough) {
            return Some(forced);
        }
        let ip = line
            .points
            .iter()
            .find(|ip| ip.closing_balance() == closing_trough)?;
        let desired = self.indent(ip.closing_balance() - ip.untaken_indents.len() as i32);
        let before = self.block(ip.idx + 1)?.segment;
        self.indent_to(ip.idx, &desired, before, "reflow.indent.positive")?;
        Some(vec![closing_trough])
    }

    /// `_lint_line_untaken_negative_indents`.
    fn lint_untaken_negative(&mut self, line: &IndentLine, forced: &[i32]) -> Option<()> {
        if line.closing_balance() >= line.opening_balance() {
            return Some(());
        }
        for ip in &line.points[..line.points.len() - 1] {
            if ip.is_line_break || ip.impulse >= 0 {
                continue;
            }
            if ip.initial_balance + ip.trough >= line.opening_balance() {
                continue;
            }
            // range(initial, initial + trough, -1)
            let all_untaken = ((ip.initial_balance + ip.trough + 1)..=ip.initial_balance)
                .all(|c| ip.untaken_indents.contains(&c) && !forced.contains(&c));
            if all_untaken {
                continue;
            }
            if self.block_is(ip.idx + 1, "statement_terminator") || self.block_is(ip.idx + 1, "comma") {
                continue;
            }
            let desired = self.indent(
                ip.closing_balance() - ip.untaken_indents.len() as i32 + forced.len() as i32,
            );
            let before = self.block(ip.idx + 1)?.segment;
            self.indent_to(ip.idx, &desired, before, "reflow.indent.negative")?;
        }
        Some(())
    }
}

/// `_prune_untaken_indents` + `_update_crawl_balances`.
fn update_crawl_balances(untaken: &mut Vec<i32>, balance: &mut i32, stats: &Stats, has_newline: bool) {
    let (impulse, trough, implicit) = stats;
    let limit = if trough < impulse {
        *balance + impulse + trough
    } else {
        *balance + impulse
    };
    untaken.retain(|x| *x <= limit);
    if impulse > trough && !has_newline {
        for i in *trough..*impulse {
            if !implicit.contains(&(i + 1)) {
                untaken.push(*balance + i + 1);
            }
        }
    }
    *balance += impulse;
}

/// `_crawl_indent_points`.
fn crawl_indent_points(arena: &Arena, elements: &[Element], implicit_indents: &str) -> Option<Vec<IndentPoint>> {
    let is_block_type = |i: usize, t: &str| match elements.get(i) {
        Some(Element::Block(b)) => arena.is_type(b.segment, t),
        _ => false,
    };
    let point_has_newline = |p: &Point| p.segments.iter().any(|s| arena.is_type(*s, "newline"));

    let mut out = Vec::new();
    let mut last_line_break_idx: Option<usize> = None;
    let mut balance = 0;
    let mut untaken: Vec<i32> = Vec::new();
    let mut cached_stats: Option<Stats> = None;
    let mut cached_point: Option<IndentPoint> = None;

    for (idx, elem) in elements.iter().enumerate() {
        let Element::Point(point) = elem else {
            continue;
        };
        // A point is always followed by a block (the end of file at least).
        let Some(Element::Block(next)) = elements.get(idx + 1) else {
            return None;
        };
        // `IndentStats.from_combination`.
        let mut stats: Stats = match &cached_stats {
            None => (point.impulse, point.trough, point.implicit_indents.clone()),
            Some((fi, ft, _)) => (
                fi + point.impulse,
                (*ft).min(fi + point.trough),
                point.implicit_indents.clone(),
            ),
        };
        if !stats.2.is_empty() {
            let mut unclosed = false;
            if implicit_indents != "forbid" && arena.is_type(next.segment, "start_bracket") {
                let depth = next.depth_info.stack.len();
                unclosed = true;
                for elem_j in &elements[idx + 1..] {
                    match elem_j {
                        Element::Point(p) => {
                            if point_has_newline(p) {
                                break;
                            }
                        }
                        Element::Block(b) => {
                            if arena.is_type(b.segment, "end_bracket") && b.depth_info.stack.len() == depth {
                                unclosed = false;
                                break;
                            }
                        }
                    }
                }
            }
            if unclosed || implicit_indents == "forbid" {
                stats.2.clear();
            }
        }

        if cached_stats.is_some() {
            let cp = cached_point.as_ref()?;
            if cp.is_line_break {
                out.push(IndentPoint {
                    idx: cp.idx,
                    impulse: stats.0,
                    trough: stats.1,
                    initial_balance: balance,
                    last_line_break_idx: cp.last_line_break_idx,
                    is_line_break: true,
                    untaken_indents: untaken.clone(),
                });
                update_crawl_balances(&mut untaken, &mut balance, &stats, true);
                stats = (0, 0, stats.2);
            } else {
                out.push(IndentPoint {
                    idx: cp.idx,
                    impulse: 0,
                    trough: 0,
                    initial_balance: balance,
                    last_line_break_idx: cp.last_line_break_idx,
                    is_line_break: false,
                    untaken_indents: untaken.clone(),
                });
            }
        }
        cached_stats = None;
        cached_point = None;

        let has_newline = point_has_newline(point);
        let indent_point = IndentPoint {
            idx,
            impulse: stats.0,
            trough: stats.1,
            initial_balance: balance,
            last_line_break_idx,
            is_line_break: has_newline,
            untaken_indents: untaken.clone(),
        };
        if has_newline {
            last_line_break_idx = Some(idx);
        }

        if is_block_type(idx + 1, "comment") {
            cached_stats = Some(stats);
            cached_point = Some(indent_point);
            continue;
        } else if has_newline
            || stats.0 != 0
            || stats.1 != 0
            || idx == 0
            || arena.is_type(next.segment, "end_of_file")
        {
            out.push(indent_point);
        }
        update_crawl_balances(&mut untaken, &mut balance, &stats, has_newline);
    }
    Some(out)
}

/// `_map_line_buffers`: the lines, and the imbalanced indent locations.
fn map_line_buffers(arena: &Arena, elements: &[Element], implicit_indents: &str) -> Option<(Vec<IndentLine>, Vec<usize>)> {
    let is_block_type = |i: usize, t: &str| match elements.get(i) {
        Some(Element::Block(b)) => arena.is_type(b.segment, t),
        _ => false,
    };
    let mut lines = Vec::new();
    let mut buffer: Vec<IndentPoint> = Vec::new();
    let mut previous: HashMap<usize, IndentPoint> = HashMap::new();
    let mut untaken_locs: HashMap<i32, usize> = HashMap::new();
    let mut imbalanced: Vec<usize> = Vec::new();

    for ip in crawl_indent_points(arena, elements, implicit_indents)? {
        buffer.push(ip.clone());
        previous.insert(ip.idx, ip.clone());
        let Some(Element::Point(point)) = elements.get(ip.idx) else {
            return None;
        };
        if !ip.is_line_break {
            if ip.impulse > ip.trough && !(implicit_indents != "forbid" && !point.implicit_indents.is_empty()) {
                untaken_locs.insert(ip.initial_balance + ip.impulse, ip.idx);
            }
            continue;
        }
        lines.push(IndentLine::from_points(buffer.clone()));

        if ip.trough != 0 && !is_block_type(ip.idx + 1, "end_of_file") {
            // reversed(range(initial, initial + trough, -1))
            for i in (ip.initial_balance + ip.trough + 1)..=ip.initial_balance {
                let Some(&loc) = untaken_locs.get(&i) else {
                    break;
                };
                if is_block_type(loc + 1, "start_bracket") {
                    continue;
                }
                if buffer.iter().any(|p| p.idx == loc) {
                    continue;
                }
                // The end of the line the untaken indent sits on.
                if loc >= ip.idx {
                    return None;
                }
                let mut pt: Option<&IndentPoint> = None;
                for j in loc..ip.idx {
                    pt = previous.get(&j);
                    if pt.is_some_and(|p| p.is_line_break) {
                        break;
                    }
                }
                let pt = pt?;
                if (pt.idx + 1..ip.idx).step_by(2).all(|k| is_block_type(k, "comment")) {
                    continue;
                }
                imbalanced.push(loc);
            }
        }
        untaken_locs.retain(|k, _| *k <= ip.initial_balance + ip.trough);
        buffer = vec![ip];
    }
    if buffer.len() > 1 {
        lines.push(IndentLine::from_points(buffer));
    }
    Some((lines, imbalanced))
}

/// Is the rendered file the source file, character for character? Only then
/// is every position literal and every segment's source text its raw.
fn is_untemplated(arena: &Arena, seq: &ReflowSequence) -> bool {
    let Some(pm) = seq.depth_map.iter().find_map(|(seg, _)| arena.pos_marker(*seg)) else {
        return false;
    };
    pm.templated_file.source_str == pm.templated_file.templated_str
        && !seq.depth_map.iter().any(|(seg, _)| {
            arena.is_type(*seg, "placeholder") || arena.is_type(*seg, "template_loop")
        })
}

/// `ReflowSequence.reindent()`. `None` = a case this doesn't cover.
pub fn reindent(
    arena: &Arena,
    config: &ReflowConfig,
    seq: &ReflowSequence,
    options: &ReindentOptions,
) -> Option<Vec<IndentResult>> {
    let single_indent = match options.indent_unit.as_str() {
        "tab" => "\t".to_string(),
        "space" => " ".repeat(options.tab_space_size),
        _ => return None,
    };
    if options.implicit_indents == "require" {
        return None;
    }
    for t in ["comma", "binary_operator"] {
        let aligned = config
            .types
            .get(t)
            .and_then(|c| c.line_position.as_deref())
            .is_some_and(|lp| lp == "leading:align-following");
        if aligned {
            return None;
        }
    }
    if !is_untemplated(arena, seq) {
        return None;
    }

    let elements = &seq.elements[..];
    let (mut lines, imbalanced) = map_line_buffers(arena, elements, &options.implicit_indents)?;

    // `_revise_comment_lines`.
    let all_comments = |line: &IndentLine| {
        let mut any = false;
        for e in &elements[line.range()] {
            if let Element::Block(b) = e {
                if !arena.is_type(b.segment, "comment") {
                    return false;
                }
                any = true;
            }
        }
        any
    };
    if options.ignore_comment_lines {
        lines.retain(|l| !all_comments(l));
    } else {
        let mut comment_lines: Vec<usize> = Vec::new();
        for idx in 0..lines.len() {
            if all_comments(&lines[idx]) {
                comment_lines.push(idx);
            } else {
                let balance = lines[idx].initial_balance;
                for c in comment_lines.drain(..) {
                    lines[c].initial_balance = balance;
                }
            }
        }
        for c in comment_lines {
            lines[c].initial_balance = 0;
        }
    }

    // Lines inside `skip_indentation_in`. Siblings share a stack, so the
    // answer is remembered per stack.
    if !options.skip_indentation_in.is_empty() {
        let mut skip_cache: HashMap<*const NodeId, bool> = HashMap::new();
        lines.retain(|line| {
            !elements[line.range()].iter().any(|e| match e {
                Element::Block(b) => *skip_cache
                    .entry(b.depth_info.stack.as_ptr())
                    .or_insert_with(|| {
                        b.depth_info
                            .stack_class_types
                            .iter()
                            .any(|types| types.iter().any(|t| options.skip_indentation_in.contains(t)))
                    }),
                Element::Point(_) => false,
            })
        });
    }

    let mut r = Reindenter {
        arena,
        elements,
        edited: HashMap::new(),
        results: Vec::new(),
        single_indent,
    };
    let mut forced: Vec<i32> = Vec::new();
    for line in &lines {
        // `_lint_line_buffer_indents`.
        r.lint_starting_indent(line, &forced)?;
        let new_indents = r.lint_untaken_positive(line, &imbalanced)?;
        if !new_indents.is_empty() {
            forced.extend(new_indents);
            continue;
        }
        r.lint_untaken_negative(line, &forced)?;
        let closing = line.closing_balance();
        forced.retain(|i| *i <= closing);
    }
    Some(r.results)
}

// ---------------------------------------------------------------------------
// SPIKE: `ReflowSequence.break_long_lines()` as LT05 calls it, for files
// with no templating: which lines are too long, and the fixes for them.
// ---------------------------------------------------------------------------

pub struct LongLineOptions {
    pub indent_unit: String,
    pub tab_space_size: usize,
    pub implicit_indents: String,
    pub trailing_comments: String,
    pub list_wrapping: String,
    pub max_line_length: i64,
}

/// `_source_char_len`, where a segment's source text is its raw.
fn source_char_len(arena: &Arena, elements: &[Element]) -> Option<usize> {
    let mut char_len = 0usize;
    let mut last_slice: Option<(usize, usize)> = None;
    for e in elements {
        let segs: &[NodeId] = match e {
            Element::Block(b) => std::slice::from_ref(&b.segment),
            Element::Point(p) => &p.segments,
        };
        for &seg in segs {
            if arena.is_type(seg, "indent") {
                continue;
            }
            let pm = arena.pos_marker(seg)?;
            let slice = (pm.source_slice.start, pm.source_slice.stop);
            let raw = arena.raw(seg);
            if let Some(pos) = raw.find('\n') {
                return Some(char_len + raw[..pos].chars().count());
            }
            let slice_len = slice.1.saturating_sub(slice.0);
            if Some(slice) != last_slice {
                if !raw.is_empty() && slice_len == 0 {
                    char_len += raw.chars().count();
                } else if slice_len == 0 {
                    continue;
                } else {
                    char_len += raw.chars().count();
                    last_slice = Some(slice);
                }
            }
        }
    }
    Some(char_len)
}

/// `identify_rebreak_spans` + `_rebreak_priorities` over one line: where a
/// break could go (index within `line`), and how readily.
fn rebreak_priorities(arena: &Arena, line: &[Element]) -> Option<HashMap<usize, i64>> {
    let mut priorities = HashMap::new();
    if line.len() < 4 {
        return Some(priorities);
    }
    let mut add = |target: NodeId, start: usize, end: usize, config: &str| -> Option<()> {
        let raw = arena.raw(target).to_uppercase();
        let priority = if raw == "," {
            1
        } else if arena.is_type(target, "assignment_operator") {
            2
        } else if raw == "OR" {
            3
        } else if raw == "AND" {
            4
        } else if arena.is_type(target, "comparison_operator") {
            5
        } else if matches!(raw.as_str(), "*" | "/" | "%") {
            7
        } else {
            6
        };
        match head(config) {
            "leading" => {
                priorities.insert(start - 1, priority);
            }
            "trailing" => {
                priorities.insert(end + 1, priority);
            }
            "alone" => {
                priorities.insert(start - 1, priority);
                priorities.insert(end + 1, priority);
            }
            _ => return None,
        }
        Some(())
    };
    for idx in 2..line.len() - 2 {
        let Element::Block(elem) = &line[idx] else {
            continue;
        };
        if let Some(lp) = elem.config.line_position.as_deref().filter(|lp| !lp.is_empty()) {
            add(elem.segment, idx, idx, lp)?;
        }
        for (key, config) in &elem.line_position_configs {
            let key = *key as usize;
            if elem.depth_info.positions[key].idx != 0 {
                continue;
            }
            let ancestor = elem.depth_info.stack[key];
            for (end_idx, end_elem) in line.iter().enumerate().skip(idx) {
                let Element::Block(b) = end_elem else {
                    continue;
                };
                let final_idx = if b.depth_info.stack.get(key) != Some(&ancestor) {
                    Some(end_idx - 2)
                } else if matches!(b.depth_info.positions[key].ty, PosType::End | PosType::Solo) {
                    Some(end_idx)
                } else {
                    None
                };
                if let Some(final_idx) = final_idx {
                    add(ancestor, idx, final_idx, config)?;
                    break;
                }
            }
        }
    }
    Some(priorities)
}

/// `_is_point_before_final_statement_terminator`.
fn before_final_terminator(arena: &Arena, line: &[Element], idx: usize) -> bool {
    let mut found = false;
    for e in &line[idx + 1..] {
        let segs: &[NodeId] = match e {
            Element::Block(b) => std::slice::from_ref(&b.segment),
            Element::Point(p) => &p.segments,
        };
        for &seg in segs {
            if arena.is_type(seg, "statement_terminator") {
                found = true;
            } else if arena.is_type(seg, "newline") {
                return found;
            } else if arena.is_code(seg) {
                return false;
            }
        }
    }
    found
}

/// `_match_indents`. Keys are balances in hundredths, so `1.56` is `156`.
fn match_indents(
    arena: &Arena,
    line: &[Element],
    priorities: &HashMap<usize, i64>,
    line_start: usize,
    newline_idx: usize,
    implicit_indents: &str,
) -> std::collections::BTreeMap<i64, Vec<usize>> {
    let mut balance: i64 = 0;
    let mut matched: std::collections::BTreeMap<i64, Vec<usize>> = Default::default();
    let mut implicit: Vec<usize> = Vec::new();
    for (idx, e) in line.iter().enumerate() {
        let Element::Point(p) = e else {
            continue;
        };
        let e_idx = line_start + idx;
        if !p.implicit_indents.is_empty() {
            implicit.push(e_idx);
        }
        // `_increment_balance`.
        let mut new: Vec<i64> = Vec::new();
        let (impulse, trough) = (p.impulse as i64, p.trough as i64);
        if trough < 0 {
            for b in (trough + 1..=0).rev() {
                new.push((balance + b) * 100);
            }
            balance += impulse;
        } else if impulse > 0 {
            for b in 0..impulse {
                new.push((balance + b + 1) * 100);
            }
            balance += impulse;
        }
        if before_final_terminator(arena, line, idx) {
            continue;
        }
        for key in new {
            matched.entry(key).or_default().push(e_idx);
        }
        if let Some(priority) = priorities.get(&idx) {
            matched.entry(balance * 100 + 50 + priority).or_default().push(e_idx);
        }
    }
    matched.retain(|_, v| v.as_slice() != [newline_idx]);
    if implicit_indents != "forbid" {
        matched.retain(|_, v| v.iter().any(|i| *i != newline_idx && !implicit.contains(i)));
    }
    matched
}

impl<'a> Reindenter<'a> {
    /// `indent_to`, returning the fixes instead of recording a result.
    fn indent_to_fixes(&mut self, idx: usize, desired: &str, before: NodeId) -> Option<Vec<IndentFix>> {
        let mark = self.results.len();
        self.indent_to(idx, desired, before, "")?;
        Some(self.results.drain(mark..).flat_map(|r| r.fixes).collect())
    }

    fn first_segment(&self, idx: usize) -> Option<NodeId> {
        match self.elements.get(idx)? {
            Element::Block(b) => Some(b.segment),
            Element::Point(_) => match self.segs(idx)?.first()? {
                PSeg::Orig(id) => Some(*id),
                _ => None,
            },
        }
    }

    /// `_fix_long_line_with_comment`.
    fn fix_with_comment(
        &mut self,
        line: std::ops::Range<usize>,
        current_indent: &str,
        last_indent_idx: Option<usize>,
        options: &LongLineOptions,
    ) -> Option<Vec<IndentFix>> {
        let comment = self.block(line.end - 1)?.segment;
        let raw = self.arena.raw(comment);
        if raw.contains("noqa") {
            return Some(Vec::new());
        }
        if (raw.chars().count() + current_indent.chars().count()) as i64 > options.max_line_length {
            return Some(Vec::new());
        }
        let point_idx = line.end - 2;
        self.point(point_idx)?;
        match options.trailing_comments.as_str() {
            "after" => self.indent_to_fixes(point_idx, current_indent, comment),
            "before" => {
                let mut fixes = vec![IndentFix {
                    kind: IndentFixKind::Delete,
                    target: comment,
                    text: String::new(),
                    extra: None,
                }];
                for s in self.segs(point_idx)? {
                    if self.is_whitespace(&s) {
                        let PSeg::Orig(ws) = s else {
                            return None;
                        };
                        fixes.push(IndentFix {
                            kind: IndentFixKind::Delete,
                            target: ws,
                            text: String::new(),
                            extra: None,
                        });
                    }
                }
                let (anchor, text) = match last_indent_idx {
                    None => (self.first_segment(line.start)?, String::new()),
                    Some(idx) => (self.first_segment(idx + 1)?, current_indent.to_string()),
                };
                fixes.push(IndentFix {
                    kind: IndentFixKind::MoveCommentBefore,
                    target: anchor,
                    text,
                    extra: Some(comment),
                });
                Some(fixes)
            }
            _ => None,
        }
    }

    /// `_fill_breaks`.
    fn fill_breaks(
        &self,
        breaks: &[usize],
        line: std::ops::Range<usize>,
        first_indent: &str,
        next_indent: &str,
        limit: i64,
    ) -> Option<Vec<usize>> {
        let mut chosen = Vec::new();
        let mut start = line.start;
        let mut indent_len = first_indent.chars().count();
        let mut idx = 0;
        while idx < breaks.len() {
            let group_len = if idx + 1 < breaks.len() && breaks[idx + 1] == breaks[idx] + 2 {
                2
            } else {
                1
            };
            let group = &breaks[idx..idx + group_len];
            idx += group_len;
            let next = if idx < breaks.len() { breaks[idx] } else { line.end };
            let len = source_char_len(self.arena, self.elements.get(start..next)?)?;
            if (indent_len + len) as i64 > limit {
                chosen.extend_from_slice(group);
                start = group[group_len - 1] + 1;
                indent_len = next_indent.chars().count();
            }
        }
        Some(chosen)
    }

    /// `_fix_long_line_with_integer_targets`.
    fn fix_integer_targets(&mut self, breaks: &[usize], limit: i64, inner: &str, outer: &str) -> Option<Vec<IndentFix>> {
        let mut purge_before = 0;
        for &e_idx in breaks {
            let next = self.block(e_idx + 1)?.segment;
            if self.arena.pos_marker(next)?.working_line_pos as i64 > limit {
                break;
            }
            if self.point(e_idx)?.trough < 0 {
                continue;
            }
            purge_before = e_idx;
        }
        let mut fixes = Vec::new();
        for &e_idx in breaks.iter().filter(|e| **e >= purge_before) {
            let point = self.point(e_idx)?;
            let (impulse, trough) = (point.impulse, point.trough);
            let new_indent = if impulse < 0 {
                if self.block_is(e_idx + 1, "statement_terminator") || self.block_is(e_idx + 1, "comma") {
                    break;
                }
                outer
            } else {
                inner
            };
            let before = self.block(e_idx + 1)?.segment;
            fixes.extend(self.indent_to_fixes(e_idx, new_indent, before)?);
            if trough < 0 {
                break;
            }
        }
        Some(fixes)
    }

    /// `lint_line_length`.
    fn lint_line_length(&mut self, options: &LongLineOptions) -> Option<()> {
        let arena = self.arena;
        let elements = self.elements;
        let limit = options.max_line_length;
        let mut line_start: Option<usize> = None;
        let mut last_indent_idx: Option<usize> = None;
        for (i, elem) in elements.iter().enumerate() {
            let closes_line = match elem {
                Element::Point(p) => {
                    let Some(Element::Block(next)) = elements.get(i + 1) else {
                        return None;
                    };
                    arena.is_type(next.segment, "end_of_file")
                        || p.segments.iter().any(|s| arena.is_type(*s, "newline"))
                }
                Element::Block(_) => false,
            };
            if !closes_line {
                line_start.get_or_insert(i);
                continue;
            }
            let Some(start) = line_start.take() else {
                continue;
            };
            let current_indent = match last_indent_idx {
                Some(idx) => self.current_indent(Some(idx))?,
                None => String::new(),
            };
            let char_len = source_char_len(arena, &elements[start..i])?;
            // Python asserts the line starts with a segment.
            let first_seg = self.first_segment(start)?;
            let line_len = (current_indent.chars().count() + char_len) as i64;
            if line_len > limit {
                // The line, its closing point, and the block after.
                let priorities = rebreak_priorities(arena, &elements[start..i + 2])?;
                let matched = match_indents(
                    arena,
                    &elements[start..=i],
                    &priorities,
                    start,
                    i,
                    &options.implicit_indents,
                );
                let fixes = if i - start > 1 && self.block_is(i - 1, "inline_comment") {
                    self.fix_with_comment(start..i, &current_indent, last_indent_idx, options)?
                } else if let Some((&target, breaks)) = matched.iter().next() {
                    let mut desired = current_indent.clone();
                    if target >= 100 {
                        desired.push_str(&self.single_indent);
                    }
                    let mut breaks = breaks.clone();
                    if let Some(pos) = breaks.iter().position(|b| *b == i) {
                        breaks.remove(pos);
                    }
                    if breaks.is_empty() {
                        Vec::new()
                    } else if target % 100 == 0 {
                        self.fix_integer_targets(&breaks, limit, &desired, &current_indent)?
                    } else {
                        if options.list_wrapping == "fill"
                            && breaks.iter().all(|e| self.block_is(e - 1, "comma") || self.block_is(e + 1, "comma"))
                        {
                            breaks = self.fill_breaks(&breaks, start..i, &current_indent, &desired, limit)?;
                        }
                        let mut fixes = Vec::new();
                        for e_idx in breaks {
                            let before = self.block(e_idx + 1)?.segment;
                            fixes.extend(self.indent_to_fixes(e_idx, &desired, before)?);
                        }
                        fixes
                    }
                } else {
                    Vec::new()
                };
                self.push(
                    first_seg,
                    format!("Line is too long ({line_len} > {limit})."),
                    "reflow.long_line",
                    fixes,
                );
            }
            last_indent_idx = Some(i);
        }
        Some(())
    }
}

/// `ReflowSequence.break_long_lines()`. `None` = a case this doesn't cover.
pub fn break_long_lines(arena: &Arena, seq: &ReflowSequence, options: &LongLineOptions) -> Option<Vec<IndentResult>> {
    if options.max_line_length <= 0 {
        return Some(Vec::new());
    }
    let single_indent = match options.indent_unit.as_str() {
        "tab" => "\t".to_string(),
        "space" => " ".repeat(options.tab_space_size),
        _ => return None,
    };
    if !is_untemplated(arena, seq) {
        return None;
    }
    let mut r = Reindenter {
        arena,
        elements: &seq.elements[..],
        edited: HashMap::new(),
        results: Vec::new(),
        single_indent,
    };
    r.lint_line_length(options)?;
    Some(r.results)
}

// ---------------------------------------------------------------------------
// SPIKE: `ReflowSequence.rebreak("keywords")` as LT14 calls it, for files
// with no templating: which keywords need moving, and the fixes for them.
// ---------------------------------------------------------------------------

/// `_RebreakIndices.from_elements(...).newline_pt_idx`. `Ok(None)` is
/// Python's `UnboundLocalError` (the location is skipped); `Err` is a case
/// where Python would index out of range.
fn newline_point_idx(arena: &Arena, elements: &[Element], start_idx: usize, forward: bool) -> Result<Option<usize>, ()> {
    let len = elements.len() as i64;
    let dir: i64 = if forward { 1 } else { -1 };
    let mut i = start_idx as i64 + dir;
    let mut found = None;
    // range(adj, limit, 2 * dir), where limit is `len` forwards and 0 back.
    while (forward && i < len) || (!forward && i > 0) {
        found = Some(i as usize);
        let has_newline = match &elements[i as usize] {
            Element::Point(p) => p.segments.iter().any(|s| arena.is_type(*s, "newline")),
            Element::Block(b) => arena.is_type(b.segment, "newline"),
        };
        let neighbour = i + dir;
        if neighbour < 0 || neighbour >= len {
            return Err(());
        }
        let neighbour_is_code = match &elements[neighbour as usize] {
            Element::Point(p) => p.segments.iter().any(|s| arena.is_code(*s)),
            Element::Block(b) => arena.is_code(b.segment),
        };
        if has_newline || neighbour_is_code {
            break;
        }
        i += 2 * dir;
    }
    Ok(found)
}

/// Python's `_RebreakLocation`, for a keyword span.
struct KeywordLocation {
    target: NodeId,
    /// The points either side of the span.
    prev_adj: usize,
    next_adj: usize,
    /// The nearest points either side which hold a newline or border code.
    prev_newline: usize,
    next_newline: usize,
    line_position: String,
    attached: bool,
}

/// `identify_keyword_rebreak_spans` + `_RebreakLocation.from_span`.
fn keyword_locations(arena: &Arena, elements: &[Element]) -> Option<Vec<KeywordLocation>> {
    let n = elements.len();
    let mut out = Vec::new();
    if n < 4 {
        return Some(out);
    }
    for idx in 2..n - 2 {
        let Element::Block(elem) = &elements[idx] else {
            continue;
        };
        for (key, config) in &elem.keyword_line_position_configs {
            let key = *key as usize;
            if config.to_lowercase() == "none" {
                continue;
            }
            if elem.depth_info.positions[key].idx > 1 {
                continue;
            }
            if elem.depth_info.stack.len() > key + 1 {
                continue;
            }
            if !arena.is_type(elem.segment, "keyword") {
                continue;
            }
            let ancestor = elem.depth_info.stack[key];
            let mut final_idx = None;
            for (end_idx, end_elem) in elements.iter().enumerate().skip(idx) {
                match end_elem {
                    Element::Point(p) => {
                        if p.segments.iter().any(|s| arena.is_type(*s, "indent")) {
                            final_idx = Some(end_idx - 1);
                            break;
                        }
                    }
                    Element::Block(b) => {
                        // Python looks the ancestor up in this block's stack,
                        // and raises if the block has left it.
                        if b.depth_info.stack.get(key) != Some(&ancestor) {
                            return None;
                        }
                        if matches!(b.depth_info.positions[key].ty, PosType::End | PosType::Solo) {
                            final_idx = Some(end_idx);
                            break;
                        }
                    }
                }
            }
            let Some(final_idx) = final_idx else {
                continue;
            };
            let excluded = elem
                .keyword_line_position_exclusions_configs
                .iter()
                .find(|(k, _)| *k as usize == key)
                .is_some_and(|(_, exclusions)| {
                    elem.depth_info
                        .stack_class_types
                        .iter()
                        .any(|types| types.iter().any(|t| exclusions.contains(t)))
                });
            if excluded {
                continue;
            }
            let (Some(prev_newline), Some(next_newline)) = (
                newline_point_idx(arena, elements, idx, false).ok()?,
                newline_point_idx(arena, elements, final_idx, true).ok()?,
            ) else {
                continue;
            };
            out.push(KeywordLocation {
                target: elem.segment,
                prev_adj: idx - 1,
                next_adj: final_idx + 1,
                prev_newline,
                next_newline,
                line_position: head(config).to_string(),
                attached: config.contains("attached"),
            });
        }
    }
    Some(out)
}

impl<'a> Reindenter<'a> {
    /// `ReflowElement.num_newlines`, on the point as it currently stands.
    fn num_newlines(&self, idx: usize) -> Option<usize> {
        match self.elements.get(idx)? {
            Element::Block(b) => Some(usize::from(self.arena.is_type(b.segment, "newline"))),
            Element::Point(_) => Some(self.segs(idx)?.iter().filter(|s| self.is_newline(s)).count()),
        }
    }

    /// `ReflowPoint.get_indent() or ""`.
    fn get_indent(&self, idx: usize) -> Option<String> {
        let segs = self.segs(idx)?;
        if !segs.iter().any(|s| self.is_newline(s)) {
            return Some(String::new());
        }
        Some(match self.indent_segment(&segs) {
            Some(i) => self.raw(&segs[i]),
            None => String::new(),
        })
    }

    fn indent_to_fixes_after(&mut self, idx: usize, desired: &str, after: NodeId) -> Option<Vec<IndentFix>> {
        let mark = self.results.len();
        self.indent_to_anchor(idx, desired, after, true, "")?;
        Some(self.results.drain(mark..).flat_map(|r| r.fixes).collect())
    }

    /// `ReflowPoint.respace_point(..., strip_newlines=True)` on the point as
    /// it currently stands, returning its fixes.
    fn respace_stripping(&mut self, idx: usize) -> Option<Vec<IndentFix>> {
        let arena = self.arena;
        let prev = if idx > 0 { self.block(idx - 1) } else { None };
        let next = self.block(idx + 1);
        let (pre, post, strip) = determine_constraints(arena, prev, next, true)?;
        let mut segs = self.segs(idx)?;
        let orig = |s: &PSeg| match s {
            PSeg::Orig(id) => Some(*id),
            _ => None,
        };
        let delete = |id: NodeId| IndentFix {
            kind: IndentFixKind::Delete,
            target: id,
            text: String::new(),
            extra: None,
        };
        let mut fixes = Vec::new();

        // `process_spacing`.
        let mut removed: Vec<usize> = Vec::new();
        let mut last_ws: Vec<usize> = Vec::new();
        for (i, s) in segs.iter().enumerate() {
            if self.is_whitespace(s) {
                last_ws.push(i);
            } else if self.is_newline(s) {
                if strip {
                    removed.push(i);
                    fixes.push(delete(orig(s)?));
                    continue;
                }
                for &ws in &last_ws {
                    removed.push(ws);
                    fixes.push(delete(orig(&segs[ws])?));
                }
                last_ws.clear();
            }
        }
        for &ws in last_ws.iter().skip(1) {
            removed.push(ws);
            fixes.push(delete(orig(&segs[ws])?));
        }
        let mut last_whitespace = last_ws.first().copied();
        let next_is_eof = next.is_some_and(|b| arena.is_type(b.segment, "end_of_file"));
        if next_is_eof {
            if let Some(ws) = last_whitespace.take() {
                fixes.push(delete(orig(&segs[ws])?));
                removed.push(ws);
            }
        }

        let has_newline = segs
            .iter()
            .enumerate()
            .any(|(i, s)| !removed.contains(&i) && self.is_newline(s));
        if !((has_newline && !strip) || next_is_eof) {
            if let Some(ws) = last_whitespace {
                // `handle_respace__inline_with_space`.
                if pre == "any" || post == "any" {
                } else if pre == "touch" || post == "touch" {
                    if !would_fuse_tokens(arena, prev, next) {
                        fixes.push(delete(orig(&segs[ws])?));
                        removed.push(ws);
                    }
                } else if post.starts_with("align") || pre != "single" || post != "single" {
                    return None;
                } else if self.raw(&segs[ws]) != " " {
                    let id = orig(&segs[ws])?;
                    fixes.push(IndentFix {
                        kind: IndentFixKind::ReplaceWhitespace,
                        target: id,
                        text: " ".to_string(),
                        extra: None,
                    });
                    segs[ws] = PSeg::Edited(id, " ".to_string());
                }
            } else if matches!(pre, "touch" | "any") || matches!(post, "touch" | "any") {
                // `handle_respace__inline_without_space`: nothing to add.
            } else if post.starts_with("align") || pre != "single" || post != "single" {
                return None;
            } else {
                match (prev, next) {
                    (Some(p), _) => fixes.push(IndentFix {
                        kind: IndentFixKind::CreateSpaceAfter,
                        target: p.segment,
                        text: String::new(),
                        extra: None,
                    }),
                    (None, Some(n)) => fixes.push(IndentFix {
                        kind: IndentFixKind::CreateSpaceBefore,
                        target: n.segment,
                        text: String::new(),
                        extra: None,
                    }),
                    (None, None) => return None,
                }
                segs.push(PSeg::NewWhitespace(" ".to_string()));
            }
        } else if let Some(ws) = last_whitespace {
            // Whitespace after a newline where the positions imply a removal
            // between them: Python folds it into an earlier result.
            if ws > 0 {
                let mut prev_i = ws - 1;
                for i in (0..ws).rev() {
                    prev_i = i;
                    let is_indent = matches!(&segs[i], PSeg::Orig(id) if arena.is_type(*id, "indent"));
                    if !is_indent {
                        break;
                    }
                }
                if self.is_newline(&segs[prev_i]) {
                    let (nl, w) = (orig(&segs[prev_i])?, orig(&segs[ws])?);
                    let end = arena.pos_marker(nl)?.working_loc_after(&arena.raw(nl));
                    if end < arena.pos_marker(w)?.working_loc() {
                        return None;
                    }
                }
            }
        }
        let new_segs: Vec<PSeg> = segs
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !removed.contains(i))
            .map(|(_, s)| s)
            .collect();
        self.edited.insert(idx, new_segs);
        Some(fixes)
    }
}

/// `ReflowSequence.rebreak("keywords")`. `None` = a case this doesn't cover.
pub fn rebreak_keywords(arena: &Arena, seq: &ReflowSequence) -> Option<Vec<IndentResult>> {
    if !is_untemplated(arena, seq) {
        return None;
    }
    let elements = &seq.elements[..];
    let mut r = Reindenter {
        arena,
        elements,
        edited: HashMap::new(),
        results: Vec::new(),
        single_indent: String::new(),
    };
    for loc in keyword_locations(arena, elements)? {
        let (n_prev, n_next) = (r.num_newlines(loc.prev_newline)?, r.num_newlines(loc.next_newline)?);
        // `has_inappropriate_newlines(strict=True)`.
        if n_prev > 0 && n_next > 0 && !loc.attached {
            continue;
        }
        let name = pretty_segment_name(arena, loc.target);
        let (fixes, description) = match loc.line_position.as_str() {
            "leading" => {
                if n_prev > 0 && (!loc.attached || n_next == 0) {
                    continue;
                }
                let indent = r.get_indent(loc.next_adj)?;
                let before = r.first_segment(loc.prev_adj + 1)?;
                let mut fixes = r.indent_to_fixes(loc.prev_adj, &indent, before)?;
                fixes.extend(r.respace_stripping(loc.next_adj)?);
                (fixes, format!("The {name} should always start a new line."))
            }
            "trailing" => {
                if n_next > 0 && (!loc.attached || n_prev == 0) {
                    continue;
                }
                let indent = r.get_indent(loc.prev_adj)?;
                let after = r.block(loc.next_adj - 1)?.segment;
                let mut fixes = r.indent_to_fixes_after(loc.next_adj, &indent, after)?;
                fixes.extend(r.respace_stripping(loc.prev_adj)?);
                (fixes, format!("The {name} should always be at the end of a line."))
            }
            "alone" => {
                // Python keeps only the fixes from the last of these to run.
                let mut fixes = None;
                if n_next == 0 {
                    let indent = r.get_indent(loc.prev_adj)?;
                    let after = r.block(loc.next_adj - 1)?.segment;
                    fixes = Some(r.indent_to_fixes_after(loc.next_adj, &indent, after)?);
                }
                if r.num_newlines(loc.prev_adj)? == 0 {
                    let indent = r.get_indent(loc.next_adj)?;
                    let before = r.first_segment(loc.prev_adj + 1)?;
                    fixes = Some(r.indent_to_fixes(loc.prev_adj, &indent, before)?);
                }
                (
                    fixes?,
                    format!("The {name} should always have a line break both before and after."),
                )
            }
            _ => return None,
        };
        r.push(loc.target, description, "", fixes);
    }
    Some(r.results)
}
