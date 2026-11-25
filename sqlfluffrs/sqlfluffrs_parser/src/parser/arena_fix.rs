//! SPIKE: apply reflow fixes to an arena, mirroring Python's
//! `linter.fix.apply_fixes`.
//!
//! The old arena is left alone. The result is a new `Node` tree (for a new
//! arena) plus a plan for Python to build its segment tree from the old one:
//! unchanged subtrees are reused, everything else is listed in pre-order.
//!
//! Anything this doesn't cover returns `None`, and Python applies the fixes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sqlfluffrs_types::{PositionMarker, Slice, TemplatedFile, Token};

use super::arena::{Arena, NodeId};
use super::types::{MetaType, Node, RawSegmentKwargs};

thread_local! {
    /// Why the last `Fixer::run` on this thread returned `None`, for measuring.
    pub(crate) static DECLINED: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
}

fn decline<T>(why: &'static str) -> Option<T> {
    DECLINED.with(|d| d.set(why));
    None
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum EditType {
    Delete,
    Replace,
    CreateBefore,
    CreateAfter,
}

impl EditType {
    pub(crate) fn parse(edit_type: &str) -> Option<Self> {
        Some(match edit_type {
            "delete" => Self::Delete,
            "replace" => Self::Replace,
            "create_before" => Self::CreateBefore,
            "create_after" => Self::CreateAfter,
            _ => return None,
        })
    }
}

/// A `LintFix`: its edit type and its `edit`, as indexes into the new leaves.
pub(crate) struct Fix {
    pub edit_type: EditType,
    pub edits: Vec<usize>,
}

/// A segment from a fix's `edit`: `(class name, type, class types, instance
/// types, raw, child count)`. A container's children follow it, in pre-order.
pub(crate) type NewLeaf = (String, String, Vec<String>, Vec<String>, String, usize);

/// A node of the tree being built.
#[derive(Clone)]
enum W {
    /// An unchanged subtree of the old arena.
    Old(NodeId),
    /// A leaf made by a fix. Like the edits of a `LintFix` it starts with no
    /// position: `pos` is set when its siblings are positioned, as `(source
    /// slice, templated slice)`.
    New {
        leaf: usize,
        pos: Option<(Slice, Slice)>,
    },
    /// An old leaf with a source fix on it: edit `leaf`, in the old one's
    /// place. Either a template placeholder with a new source, or a raw
    /// with the same text.
    Swap { old: NodeId, leaf: usize },
    /// A container made by a fix, with nothing in it positioned before.
    NewCont {
        node: usize,
        children: Vec<W>,
        pos: Option<(Slice, Slice)>,
        /// Python walked back through it, fixes still being left to apply,
        /// which re-positions everything inside.
        revisited: bool,
    },
    /// An old container with new children.
    Cont { from: NodeId, children: Vec<W> },
}

/// The one segment class with no grammar to validate against. Python then
/// reports whatever its last child reported (a variable left over from the
/// loop over them), so a fix straight inside brackets goes unvalidated
/// unless that last child failed.
const NO_GRAMMAR: &str = "BracketedSegment";

/// What a fix is told of its anchor.
struct Anchor {
    /// What goes in the buffer when the anchor itself stays.
    keep: W,
    raw: String,
    is_leaf: bool,
    class: Option<String>,
    inst: Vec<String>,
    class_types: Vec<String>,
    is_placeholder: bool,
    slices: Option<(Slice, Slice)>,
    /// The old node, when the anchor is one.
    old: Option<NodeId>,
}

/// What Python needs to build its tree. `ops` holds five values per node, in
/// pre-order: `kind, a, b, working line, working position`.
///
/// kind 0: reuse old segment `a` and everything under it (nothing follows
///         for its descendants).
/// kind 1: copy old leaf `a` to the new working position.
/// kind 5: old container `a` with the next `b` nodes as its children.
/// kind 7: new leaf `a` (of the fixes' edits) at the next four `slices`.
/// kind 8: new container `a` at the next four `slices`, with the next `b`
///         nodes as its children.
#[derive(Default)]
pub(crate) struct Plan {
    pub ops: Vec<i64>,
    pub slices: Vec<usize>,
    pub raw: String,
    pub validate_ns: u64,
    pub validations: usize,
}

pub(crate) struct Fixer<'a> {
    pub arena: &'a Arena,
    pub fixes: HashMap<NodeId, Vec<Fix>>,
    /// Containers with a fix somewhere beneath them.
    pub marked: HashSet<NodeId>,
    /// Python rebuilds every segment it walks through while fixes are left
    /// to apply, not only those above a fix. That shows only where positions
    /// aren't already what a rebuild gives: in a templated file.
    pub visit_all: bool,
    /// Classes with `can_start_end_non_code`.
    pub loose_ends: &'a HashSet<String>,
    pub new_leaves: &'a [NewLeaf],
    /// Six per new segment: its source and templated slices and working
    /// location if it had a position (a segment moved from the old tree),
    /// else -1s.
    pub new_pos: &'a [i64],
    /// For each new segment, the old node it is a copy of (same uuid), or -1.
    pub new_origin: &'a [i64],
    pub templated_file: Arc<TemplatedFile>,
    /// Re-match a class's grammar over tokens: `Some(valid)`, or `None` if
    /// that can't be answered here.
    pub validate: &'a mut dyn FnMut(&str, Vec<Token>) -> Option<bool>,
    pub plan: Plan,
    /// Whether Python may reuse unchanged subtrees. Off only to measure the
    /// cost of building every segment anew.
    pub reuse: bool,
    /// Above zero while building under a moved segment whose working
    /// location didn't change: Python then leaves what is inside it alone,
    /// old working locations and all.
    pub stale: usize,
    /// Keep going inside unparsable sections, unvalidated.
    pub fix_even_unparsable: bool,
    /// New leaves which took their anchor's position, and that anchor.
    pub takes_pos: HashMap<usize, NodeId>,
}

/// `PositionMarker.infer_next_position`: move `loc` past `raw`.
fn advance(loc: &mut (usize, usize), raw: &str) {
    match raw.rfind('\n') {
        Some(i) => {
            loc.0 += raw.matches('\n').count();
            loc.1 = raw[i + 1..].chars().count() + 1;
        }
        None => loc.1 += raw.chars().count(),
    }
}

enum LeafRef<'w> {
    Old(NodeId),
    New(&'w W),
}

impl<'a> Fixer<'a> {
    /// The arena node for a new leaf, built as `RsTree.from_segments` would.
    /// Is this new segment an indent or dedent?
    fn is_meta_leaf(&self, leaf: usize) -> bool {
        matches!(self.new_leaves[leaf].1.as_str(), "indent" | "dedent")
    }

    /// Does this new leaf carry source fixes? It then has to replace a leaf
    /// just like it.
    fn has_source_fixes(&self, node: usize) -> bool {
        self.new_pos.get(node * 6) == Some(&-2)
    }

    /// The working location a new segment had before: its own, or that of
    /// the anchor whose place it took.
    fn old_loc(&self, node: usize) -> Option<(usize, usize)> {
        match self.takes_pos.get(&node) {
            Some(anchor) => self.arena.pos_ref(*anchor).map(|p| p.working_loc()),
            None => self.preset(node).map(|p| p.1),
        }
    }

    /// The position a new segment came with: its slices and working location.
    fn preset(&self, node: usize) -> Option<((Slice, Slice), (usize, usize))> {
        let p = self.new_pos.get(node * 6..node * 6 + 6)?;
        if p[0] < 0 {
            return None;
        }
        let slice = |a: i64, b: i64| Slice {
            start: a as usize,
            stop: b as usize,
        };
        Some((
            (slice(p[0], p[1]), slice(p[2], p[3])),
            (p[4] as usize, p[5] as usize),
        ))
    }

    fn new_node(&self, leaf: usize, pos: Option<PositionMarker>) -> Node {
        let (name, class_type, class_types, inst, raw, _) = &self.new_leaves[leaf];
        if self.is_meta_leaf(leaf) {
            let is_implicit = inst.iter().any(|t| t == "implicit");
            return Node::Meta {
                meta_type: if class_type == "dedent" {
                    MetaType::Dedent { is_implicit }
                } else {
                    MetaType::Indent { is_implicit }
                },
                pos_marker: pos,
                block_uuid: None,
            };
        }
        let segment_type = inst.first().cloned().unwrap_or_else(|| class_type.clone());
        Node::new_raw_with_class_types(
            name.clone(),
            segment_type,
            class_type.clone(),
            raw.clone(),
            pos,
            inst.clone(),
            class_types,
            RawSegmentKwargs::default(),
        )
    }

    fn slices(&self, w: &W) -> Option<(Slice, Slice)> {
        match w {
            W::Old(id) | W::Cont { from: id, .. } | W::Swap { old: id, .. } => self
                .arena
                .pos_ref(*id)
                .map(|p| (p.source_slice, p.templated_slice)),
            W::New { pos, .. } | W::NewCont { pos, .. } => *pos,
        }
    }

    /// The tree for the edit starting at `*next` in the new segments.
    fn new_w(&self, next: &mut usize) -> Option<W> {
        let node = *next;
        let count = self.new_leaves.get(node)?.5;
        let pos = self.preset(node).map(|p| p.0);
        *next += 1;
        if count == 0 {
            return Some(W::New { leaf: node, pos });
        }
        let children: Option<Vec<W>> = (0..count).map(|_| self.new_w(next)).collect();
        Some(W::NewCont {
            node,
            children: children?,
            pos,
            revisited: false,
        })
    }

    /// The text under a node of the tree being built.
    fn new_raw(&self, w: &W, out: &mut String) {
        match w {
            W::Old(id) => out.push_str(&self.arena.raw(*id)),
            W::Swap { old, .. } => out.push_str(self.arena.leaf_raw(*old)),
            W::New { leaf, .. } => out.push_str(&self.new_leaves[*leaf].4),
            W::NewCont { children, .. } | W::Cont { children, .. } => {
                children.iter().for_each(|c| self.new_raw(c, out))
            }
        }
    }

    /// Where the first leaf of a segment a fix brought with a position
    /// starts, and its working location in the old tree.
    fn first_preset_leaf(&self, w: &W) -> Option<(usize, usize, (usize, usize))> {
        match w {
            W::New { leaf, .. } => {
                let ((source, templated), loc) = self.preset(*leaf)?;
                Some((source.start, templated.start, loc))
            }
            W::NewCont { children, .. } => self.first_preset_leaf(children.first()?),
            _ => None,
        }
    }

    /// Is there anything without a position at or under this node?
    fn needs_placing(w: &W) -> bool {
        match w {
            W::New { pos, .. } => pos.is_none(),
            W::NewCont { pos, children, .. } => {
                pos.is_none() || children.iter().any(Self::needs_placing)
            }
            _ => false,
        }
    }

    fn first_leaf(&self, mut id: NodeId) -> NodeId {
        while let Some(&c) = self.arena.children(id).first() {
            id = c;
        }
        id
    }

    fn is_code(&self, w: &W) -> bool {
        match w {
            W::Old(id) => self.arena.is_code(*id),
            W::Swap { old, .. } => self.arena.is_code(*old),
            // ponytail: builds a node per call; cache per leaf if it shows up.
            W::New { leaf, .. } => self.new_node(*leaf, None).is_code(),
            W::Cont { children, .. } | W::NewCont { children, .. } => {
                children.iter().any(|c| self.is_code(c))
            }
        }
    }

    /// `BaseSegment._is_code_or_meta`.
    fn is_code_or_meta(&self, w: &W) -> bool {
        self.is_code(w)
            || matches!(w, W::Old(id) if self.arena.is_meta(*id))
            || matches!(w, W::Swap { old, .. } if self.arena.is_meta(*old))
            || matches!(w, W::New { leaf, .. } if self.is_meta_leaf(*leaf))
    }

    /// The part of `_position_segments` which gives new segments a source
    /// and templated position: between the previous sibling's end and the
    /// next positioned sibling's start.
    ///
    /// Python compares the two ends by working position, as it stands when
    /// this level is reached: `parent_loc` for the parent, `child_loc` for
    /// its original children.
    fn position_new(
        &self,
        parent_start: (usize, usize),
        parent_loc: (usize, usize),
        child_loc: &HashMap<NodeId, (usize, usize)>,
        buf: &mut [W],
    ) -> Option<()> {
        let unplaced = |w: &W| matches!(w, W::New { pos: None, .. } | W::NewCont { pos: None, .. });
        if !buf.iter().any(Self::needs_placing) {
            return Some(());
        }
        // `inside_next_segment_flags`.
        let mut flags = vec![false; buf.len()];
        let mut next_start: Option<usize> = None;
        for idx in (0..buf.len()).rev() {
            let Some((_, templated)) = self.slices(&buf[idx]) else {
                continue;
            };
            if templated.start != templated.stop {
                next_start = Some(templated.start);
            } else if let Some(next) = next_start {
                flags[idx] = next < templated.start;
            }
        }
        // The working location each sibling starts at, as the text runs.
        let mut loc = parent_loc;
        for idx in 0..buf.len() {
            let here = loc;
            let mut raw = String::new();
            self.new_raw(&buf[idx], &mut raw);
            advance(&mut loc, &raw);
            if unplaced(&buf[idx]) {
                // Points are (source, templated, working location). The
                // working location is only used to compare the two, as
                // Python does.
                let mut start = if idx > 0 {
                    let (source, templated) = self.slices(&buf[idx - 1])?;
                    (
                        source.stop,
                        templated.stop,
                        self.templated_file
                            .get_line_pos_of_char_pos(templated.stop, false),
                    )
                } else {
                    (parent_start.0, parent_start.1, parent_loc)
                };
                let mut end = None;
                for fwd in idx + 1..buf.len() {
                    if flags[fwd] {
                        continue;
                    }
                    end = Some(match &buf[fwd] {
                        W::Old(id) | W::Swap { old: id, .. } => {
                            // An expression which rendered nothing may sit
                            // before a sibling at an earlier templated
                            // position: its position is no end point.
                            let arena = self.arena;
                            if arena.is_type(*id, "placeholder")
                                && arena.block_type(*id).as_deref() == Some("templated")
                                && arena.pos_ref(*id).is_some_and(|p| {
                                    p.templated_slice.start == p.templated_slice.stop
                                        && p.source_slice.start != p.source_slice.stop
                                })
                            {
                                continue;
                            }
                            let p = arena.pos_ref(self.first_leaf(*id))?;
                            (
                                p.source_slice.start,
                                p.templated_slice.start,
                                *child_loc.get(id)?,
                            )
                        }
                        // Not positioned yet.
                        w if unplaced(w) => continue,
                        // In its anchor's place.
                        W::New { leaf, pos: Some(p) } if self.takes_pos.contains_key(leaf) => {
                            (p.0.start, p.1.start, *child_loc.get(&self.takes_pos[leaf])?)
                        }
                        // Moved here with its position.
                        w => self.first_preset_leaf(w)?,
                    });
                    break;
                }
                if let Some(e) = end {
                    if idx > 0 && flags[idx - 1] {
                        start = e;
                    }
                }
                let new_pos = match end {
                    Some(e) if start.2 != e.2 => (
                        Slice {
                            start: start.0,
                            stop: e.0,
                        },
                        Slice {
                            start: start.1,
                            stop: e.1,
                        },
                    ),
                    _ => (
                        Slice {
                            start: start.0,
                            stop: start.0,
                        },
                        Slice {
                            start: start.1,
                            stop: start.1,
                        },
                    ),
                };
                if let W::New { pos, .. } | W::NewCont { pos, .. } = &mut buf[idx] {
                    *pos = Some(new_pos);
                }
            }
            // Then whatever is inside it, from where it now starts.
            if let W::NewCont {
                pos: Some(p),
                children,
                ..
            } = &mut buf[idx]
            {
                let start = (p.0.start, p.1.start);
                self.position_new(start, here, &HashMap::new(), children)?;
            }
        }
        Some(())
    }

    fn leaves<'w>(&self, w: &'w W, out: &mut Vec<LeafRef<'w>>) {
        match w {
            W::Old(id) => out.extend(self.arena.raw_segments(*id).into_iter().map(LeafRef::Old)),
            // A placeholder either way: a meta, which validation leaves out.
            W::Swap { old, .. } => out.push(LeafRef::Old(*old)),
            W::Cont { children, .. } | W::NewCont { children, .. } => {
                children.iter().for_each(|c| self.leaves(c, out))
            }
            new => out.push(LeafRef::New(new)),
        }
    }

    fn token(&self, leaf: &LeafRef) -> Option<Token> {
        let like = |arena: &Arena, id: NodeId, raw: String, pos: PositionMarker| {
            Token::typed_leaf(
                raw,
                pos,
                arena.segment_class(id).unwrap_or_default(),
                arena.class_type(id),
                arena.class_types(id).iter().cloned().collect(),
                arena.instance_types_arc(id).as_ref().clone(),
                arena.is_code(id),
                arena.is_whitespace(id),
                arena.is_comment(id),
            )
        };
        match leaf {
            LeafRef::Old(id) => Some(like(
                self.arena,
                *id,
                self.arena.raw(*id),
                self.arena.pos_marker(*id)?,
            )),
            LeafRef::New(W::New { leaf, pos }) => {
                let (source, templated) = (*pos)?;
                let pos =
                    PositionMarker::new(source, templated, &self.templated_file, Some(0), Some(0));
                // A one-node arena, so a new leaf reads exactly as an old one.
                let one = Arena::from_node(&self.new_node(*leaf, Some(pos.clone())));
                Some(like(
                    &one,
                    one.root(),
                    self.new_leaves[*leaf].4.clone(),
                    pos,
                ))
            }
            LeafRef::New(_) => None,
        }
    }

    /// `validate_segment_with_reparse` for a rebuilt container.
    fn validate_cont(&mut self, w: &W) -> Option<bool> {
        let name = match w {
            W::Cont { from, .. } => self.arena.segment_class(*from)?,
            W::NewCont { node, .. } => self.new_leaves[*node].0.clone(),
            _ => return None,
        };
        let mut leaves = Vec::new();
        self.leaves(w, &mut leaves);
        let is_meta = |l: &LeafRef| match l {
            LeafRef::Old(id) => self.arena.is_meta(*id),
            LeafRef::New(W::New { leaf, .. }) => self.is_meta_leaf(*leaf),
            LeafRef::New(_) => false,
        };
        let is_code = |l: &LeafRef| match l {
            LeafRef::Old(id) => self.arena.is_code(*id),
            LeafRef::New(w) => self.is_code(w),
        };
        let content: Vec<&LeafRef> = leaves.iter().filter(|l| !is_meta(l)).collect();
        let Some(first) = content.iter().position(|l| is_code(l)) else {
            // Nothing but non-code left: fine only if the class allows it.
            return self.loose_ends.contains(&name).then_some(true);
        };
        let last = content.iter().rposition(|l| is_code(l))?;
        let tokens: Option<Vec<Token>> = content[first..=last]
            .iter()
            .map(|l| self.token(l))
            .collect();
        let tokens = tokens?;
        let started = std::time::Instant::now();
        let out = (self.validate)(&name, tokens);
        self.plan.validate_ns += started.elapsed().as_nanos() as u64;
        self.plan.validations += 1;
        out
    }

    /// What a fix needs to know of its anchor, which is either an old node
    /// or (inside a segment a fix brought) a new one.
    fn anchor_old(&self, child: NodeId) -> Anchor {
        let arena = self.arena;
        Anchor {
            keep: W::Old(child),
            raw: arena.raw(child),
            is_leaf: arena.children(child).is_empty(),
            class: arena.segment_class(child),
            inst: arena.instance_types_arc(child).as_ref().clone(),
            class_types: arena.class_types(child).as_ref().clone(),
            is_placeholder: arena.is_type(child, "placeholder"),
            slices: arena
                .pos_ref(child)
                .map(|p| (p.source_slice, p.templated_slice)),
            old: Some(child),
        }
    }

    fn anchor_new(&self, w: &W) -> Option<Anchor> {
        let (node, is_leaf) = match w {
            W::New { leaf, .. } => (*leaf, true),
            W::NewCont { node, .. } => (*node, false),
            _ => return None,
        };
        let new = &self.new_leaves[node];
        let mut raw = String::new();
        self.new_raw(w, &mut raw);
        let class_types = if is_leaf {
            let one = Arena::from_node(&self.new_node(node, None));
            one.class_types(one.root()).as_ref().clone()
        } else {
            new.2.clone()
        };
        Some(Anchor {
            keep: w.clone(),
            raw,
            is_leaf,
            class: Some(new.0.clone()),
            inst: new.3.clone(),
            class_types,
            is_placeholder: false,
            slices: self.slices(w),
            old: None,
        })
    }

    /// The loop of `apply_fixes` over the fixes for one anchor: what goes
    /// into the parent's buffer in its place.
    fn apply_at(
        &mut self,
        anchor: &Anchor,
        mut fixes: Vec<Fix>,
        buf: &mut Vec<W>,
        requires_validate: &mut bool,
    ) -> Option<()> {
        let single = fixes.len() == 1;
        if fixes.len() == 2 && fixes[0].edit_type == EditType::CreateAfter {
            fixes.reverse();
        }
        for fix in &fixes {
            if fix.edit_type == EditType::Delete {
                *requires_validate = true;
                continue;
            }
            if fix.edit_type == EditType::CreateAfter && single {
                buf.push(anchor.keep.clone());
            }
            let mut consumed_pos = false;
            for &leaf in &fix.edits {
                let new = &self.new_leaves[leaf];
                if new.1 == "placeholder" {
                    // A placeholder's source edited: the same segment with a
                    // source fix.
                    let (true, true, true, Some(old)) = (
                        fix.edit_type == EditType::Replace,
                        fix.edits.len() == 1,
                        anchor.is_placeholder,
                        anchor.old,
                    ) else {
                        return decline("placeholder edit which isn't a replace of one");
                    };
                    buf.push(W::Swap { old, leaf });
                    continue;
                }
                if new.5 != 0 {
                    let Some(mut w) = self.new_w(&mut { leaf }) else {
                        return decline("malformed edit");
                    };
                    let mut raw = String::new();
                    self.new_raw(&w, &mut raw);
                    if fix.edit_type == EditType::Replace && !consumed_pos && raw == anchor.raw {
                        // In the anchor's place. What is inside has to have
                        // come with positions: Python only places it if the
                        // working location changes.
                        consumed_pos = true;
                        let (Some(p), Some(old), W::NewCont { pos, children, .. }) =
                            (anchor.slices, anchor.old, &mut w)
                        else {
                            return decline("replacement keeps the anchor's position");
                        };
                        if children.iter().any(Self::needs_placing) {
                            return decline("replacement keeps the anchor's position");
                        }
                        *pos = Some(p);
                        self.takes_pos.insert(leaf, old);
                    }
                    buf.push(w);
                    continue;
                }
                // The first edit of a replace with the anchor's raw takes its
                // position.
                if fix.edit_type == EditType::Replace && !consumed_pos && new.4 == anchor.raw {
                    consumed_pos = true;
                    // Only an anchor replaced by its own like is the same
                    // node as before.
                    if anchor.is_leaf
                        && anchor.class.as_deref() == Some(new.0.as_str())
                        && anchor.inst == new.3
                    {
                        if !self.has_source_fixes(leaf) {
                            buf.push(anchor.keep.clone());
                            continue;
                        }
                        let Some(old) = anchor.old else {
                            return decline("source fix inside a moved copy");
                        };
                        buf.push(W::Swap { old, leaf });
                        continue;
                    }
                    if self.has_source_fixes(leaf) {
                        return decline("source fix on another kind of segment");
                    }
                    // Another kind of leaf, in the anchor's place.
                    let (Some(p), Some(old)) = (anchor.slices, anchor.old) else {
                        return decline("replacement keeps the anchor's position");
                    };
                    self.takes_pos.insert(leaf, old);
                    buf.push(W::New { leaf, pos: Some(p) });
                    continue;
                }
                if self.has_source_fixes(leaf) {
                    return decline("source fix on another kind of segment");
                }
                buf.push(W::New { leaf, pos: None });
            }
            // A like-for-like edit needs no validating; anything else does.
            let same_types = || {
                let old = &anchor.class_types;
                let first = &self.new_leaves[fix.edits[0]];
                if first.1 == "placeholder" {
                    return true;
                }
                if first.5 != 0 {
                    return old.len() == first.2.len() && old.iter().all(|t| first.2.contains(t));
                }
                let one = Arena::from_node(&self.new_node(fix.edits[0], None));
                let new = one.class_types(one.root());
                old.len() == new.len() && old.iter().all(|t| new.contains(t))
            };
            if !(fix.edit_type == EditType::Replace && fix.edits.len() == 1 && same_types()) {
                *requires_validate = true;
            }
            if fix.edit_type == EditType::CreateBefore {
                buf.push(anchor.keep.clone());
            }
        }
        Some(())
    }

    /// Move non-code at either end of a segment's children out, unless its
    /// class may start or end with it.
    fn trim_ends(&self, class: &str, buf: &mut Vec<W>) -> (Vec<W>, Vec<W>) {
        if self.loose_ends.contains(class) {
            return (Vec::new(), Vec::new());
        }
        let start = buf
            .iter()
            .position(|w| self.is_code_or_meta(w))
            .unwrap_or(buf.len());
        let before = buf.drain(..start).collect();
        let end = buf
            .iter()
            .rposition(|w| self.is_code_or_meta(w))
            .map_or(0, |i| i + 1);
        let after = buf.drain(end..).collect();
        (before, after)
    }

    /// `apply_fixes` for a container a fix brought, which Python walks back
    /// through while fixes are left to apply. A copy keeps its uuid, so a fix
    /// anchored on what it is a copy of lands on whichever is met first.
    fn apply_new(&mut self, cont: W, at: (usize, usize)) -> Option<(W, Vec<W>, Vec<W>, bool)> {
        let W::NewCont {
            node,
            children,
            pos,
            ..
        } = cont
        else {
            return Some((cont, Vec::new(), Vec::new(), true));
        };
        let mut buf: Vec<W> = Vec::with_capacity(children.len() + 2);
        let mut requires_validate = false;
        let mut applied = false;
        for child in children {
            let origin = match &child {
                W::New { leaf: n, .. } | W::NewCont { node: n, .. } => self
                    .new_origin
                    .get(*n)
                    .filter(|o| **o >= 0)
                    .map(|o| NodeId::from_index(*o as usize)),
                _ => None,
            };
            let Some(fixes) = origin.and_then(|o| self.fixes.remove(&o)) else {
                buf.push(child);
                continue;
            };
            applied = true;
            let anchor = self.anchor_new(&child)?;
            self.apply_at(&anchor, fixes, &mut buf, &mut requires_validate)?;
        }
        if applied {
            let Some(p) = pos else {
                return decline("could not position a new segment");
            };
            if self
                .position_new((p.0.start, p.1.start), at, &HashMap::new(), &mut buf)
                .is_none()
            {
                return decline("could not position a new segment");
            }
        }
        let queue = std::mem::take(&mut buf);
        let mut loc = at;
        let mut last_validated = true;
        for w in queue {
            let here = loc;
            let mut raw = String::new();
            self.new_raw(&w, &mut raw);
            advance(&mut loc, &raw);
            last_validated = true;
            if self.fixes.is_empty() || !matches!(w, W::NewCont { .. }) {
                buf.push(w);
                continue;
            }
            let (child, before, after, validated) = self.apply_new(w, here)?;
            buf.extend(before);
            buf.push(child);
            buf.extend(after);
            requires_validate |= !validated;
            last_validated = validated;
        }
        let class = self.new_leaves[node].0.clone();
        let (before, after) = self.trim_ends(&class, &mut buf);
        if buf.is_empty() {
            return decline("segment left empty");
        }
        let rebuilt = W::NewCont {
            node,
            children: buf,
            pos,
            revisited: true,
        };
        let mut validated = true;
        if requires_validate {
            if self.new_leaves[node].1 == "unparsable" {
                return decline("fix inside an unparsable moved copy");
            }
            if class == NO_GRAMMAR {
                return Some((rebuilt, before, after, last_validated));
            }
            validated = match self.validate_cont(&rebuilt) {
                Some(valid) => valid,
                None => return decline("segment could not be re-matched in Rust"),
            };
        }
        Some((rebuilt, before, after, validated))
    }

    /// `apply_fixes` for the container `id`. Returns the rebuilt node, any
    /// non-code pushed out before and after it, and whether it's validated.
    ///
    /// `at` is the working position `id` starts at by now: an ancestor's
    /// fixes may already have moved it.
    fn apply(&mut self, id: NodeId, at: (usize, usize)) -> Option<(W, Vec<W>, Vec<W>, bool)> {
        let arena = self.arena;
        let mut buf: Vec<W> = Vec::with_capacity(arena.children(id).len() + 2);
        let mut requires_validate = false;
        let mut applied = false;
        for &child in arena.children(id) {
            let Some(fixes) = self.fixes.remove(&child) else {
                buf.push(W::Old(child));
                continue;
            };
            applied = true;
            let anchor = self.anchor_old(child);
            self.apply_at(&anchor, fixes, &mut buf, &mut requires_validate)?;
        }
        if applied {
            let mut child_loc = HashMap::new();
            let mut loc = at;
            for &child in arena.children(id) {
                child_loc.insert(child, loc);
                advance(&mut loc, &arena.raw(child));
            }
            let Some(parent) = arena.pos_ref(id) else {
                return decline("could not position a new segment");
            };
            let parent_start = (parent.source_slice.start, parent.templated_slice.start);
            if self
                .position_new(parent_start, at, &child_loc, &mut buf)
                .is_none()
            {
                return decline("could not position a new segment");
            }
        }

        // Recurse into the children which have fixes beneath them.
        let queue = std::mem::take(&mut buf);
        let mut loc = at;
        let mut last_validated = true;
        for w in queue {
            last_validated = true;
            match w {
                W::Old(child) => {
                    let child_at = loc;
                    advance(&mut loc, &arena.raw(child));
                    if self.marked.contains(&child)
                        || (self.visit_all
                            && !self.fixes.is_empty()
                            && !arena.children(child).is_empty())
                    {
                        let (new_child, before, after, validated) = self.apply(child, child_at)?;
                        buf.extend(before);
                        buf.push(new_child);
                        buf.extend(after);
                        requires_validate |= !validated;
                        last_validated = validated;
                    } else {
                        buf.push(W::Old(child));
                    }
                }
                other => {
                    let here = loc;
                    let mut raw = String::new();
                    self.new_raw(&other, &mut raw);
                    advance(&mut loc, &raw);
                    if self.fixes.is_empty() || !matches!(other, W::NewCont { .. }) {
                        buf.push(other);
                        continue;
                    }
                    // Python walks back through what it just put in.
                    let (child, before, after, validated) = self.apply_new(other, here)?;
                    buf.extend(before);
                    buf.push(child);
                    buf.extend(after);
                    requires_validate |= !validated;
                    last_validated = validated;
                }
            }
        }

        // Non-code left at either end moves up to the parent.
        let class = arena
            .segment_class(id)
            .unwrap_or_else(|| "UnparsableSegment".to_string());
        let (before, after) = self.trim_ends(&class, &mut buf);
        if buf.is_empty() {
            return decline("segment left empty");
        }
        let rebuilt = W::Cont {
            from: id,
            children: buf,
        };
        let mut validated = true;
        if requires_validate {
            if arena.descendant_type_set(id).contains("unparsable")
                || arena.is_type(id, "unparsable")
            {
                if !self.fix_even_unparsable {
                    // It was unparsable already, so this can't be validated:
                    // nothing within it is fixed.
                    return Some((W::Old(id), Vec::new(), Vec::new(), true));
                }
                // Fixed regardless, and not for validating.
                return Some((rebuilt, before, after, true));
            }
            if class == NO_GRAMMAR {
                return Some((rebuilt, before, after, last_validated));
            }
            validated = match self.validate_cont(&rebuilt) {
                Some(valid) => valid,
                None => return decline("segment could not be re-matched in Rust"),
            };
        }
        Some((rebuilt, before, after, validated))
    }

    /// Lay the rebuilt tree out in pre-order: working positions follow the
    /// text, as a full `_position_segments` pass leaves them.
    fn build(&mut self, w: &W, loc: &mut (usize, usize)) -> Option<Node> {
        let arena = self.arena;
        let at = match w {
            W::New { leaf: n, .. } | W::NewCont { node: n, .. } if self.stale > 0 => {
                self.old_loc(*n).unwrap_or(*loc)
            }
            _ => *loc,
        };
        let op = |plan: &mut Plan, kind: i64, a: usize, b: usize| {
            plan.ops
                .extend([kind, a as i64, b as i64, at.0 as i64, at.1 as i64]);
        };
        Some(match w {
            W::Old(id) => {
                let pos = arena.pos_ref(*id)?;
                if self.reuse && pos.working_loc() == at {
                    op(&mut self.plan, 0, id.index(), 0);
                    let raw = arena.raw(*id);
                    advance(loc, &raw);
                    self.plan.raw.push_str(&raw);
                    arena.node_deep(*id)
                } else if arena.children(*id).is_empty() {
                    op(&mut self.plan, 1, id.index(), 0);
                    let raw = arena.leaf_raw(*id);
                    advance(loc, raw);
                    self.plan.raw.push_str(raw);
                    arena.node_like(
                        *id,
                        Some(pos.with_working_position(at.0, at.1)),
                        None,
                        vec![],
                    )
                } else {
                    let kids = arena.children(*id);
                    op(&mut self.plan, 5, id.index(), kids.len());
                    let children: Option<Vec<Node>> =
                        kids.iter().map(|&c| self.build(&W::Old(c), loc)).collect();
                    arena.node_like(
                        *id,
                        Some(pos.with_working_position(at.0, at.1)),
                        None,
                        children?,
                    )
                }
            }
            W::New { leaf, pos } => {
                let (source, templated) = (*pos)?;
                let pos = PositionMarker::new(
                    source,
                    templated,
                    &self.templated_file,
                    Some(at.0),
                    Some(at.1),
                );
                self.plan.slices.extend([
                    source.start,
                    source.stop,
                    templated.start,
                    templated.stop,
                ]);
                let text = &self.new_leaves[*leaf].4;
                advance(loc, text);
                self.plan.raw.push_str(text);
                op(&mut self.plan, 7, *leaf, 0);
                self.new_node(*leaf, Some(pos))
            }
            W::Swap { old, leaf } => {
                let old_pos = arena.pos_ref(*old)?;
                let (source, templated) = (old_pos.source_slice, old_pos.templated_slice);
                op(&mut self.plan, 7, *leaf, 0);
                self.plan.slices.extend([
                    source.start,
                    source.stop,
                    templated.start,
                    templated.stop,
                ]);
                let raw = arena.leaf_raw(*old);
                advance(loc, raw);
                self.plan.raw.push_str(raw);
                let pos = Some(old_pos.with_working_position(at.0, at.1));
                match arena.block_type(*old) {
                    Some(block_type) => Node::Meta {
                        meta_type: MetaType::Template {
                            source_str: self.new_leaves[*leaf].4.clone(),
                            block_type,
                        },
                        pos_marker: pos,
                        block_uuid: arena.block_uuid(*old),
                    },
                    None => arena.node_like(*old, pos, None, vec![]),
                }
            }
            W::NewCont {
                node,
                children,
                pos,
                revisited,
            } => {
                let (source, templated) = (*pos)?;
                op(&mut self.plan, 8, *node, children.len());
                self.plan.slices.extend([
                    source.start,
                    source.stop,
                    templated.start,
                    templated.stop,
                ]);
                let pos = PositionMarker::new(
                    source,
                    templated,
                    &self.templated_file,
                    Some(at.0),
                    Some(at.1),
                );
                let (name, class_type, class_types, ..) = &self.new_leaves[*node];
                let keeps = !*revisited && (self.stale > 0 || self.old_loc(*node) == Some(at));
                self.stale += usize::from(keeps);
                let kids: Option<Vec<Node>> = children.iter().map(|c| self.build(c, loc)).collect();
                self.stale -= usize::from(keeps);
                Node::Segment {
                    segment_class: std::borrow::Cow::Owned(name.clone()),
                    segment_type: Some(std::borrow::Cow::Owned(class_type.clone())),
                    pos_marker: Some(pos),
                    class_types: class_types.clone(),
                    children: kids?,
                }
            }
            W::Cont { from, children } => {
                op(&mut self.plan, 5, from.index(), children.len());
                let pos = arena.pos_ref(*from)?.with_working_position(at.0, at.1);
                let kids: Option<Vec<Node>> = children.iter().map(|c| self.build(c, loc)).collect();
                arena.node_like(*from, Some(pos), None, kids?)
            }
        })
    }

    /// Apply every fix. `None` if Python should do it instead.
    pub(crate) fn run(mut self) -> Option<(Node, Plan)> {
        DECLINED.with(|d| d.set("other"));
        let root = self.arena.root();
        let at = self.arena.pos_ref(root)?.working_loc();
        let (rebuilt, before, after, validated) = self.apply(root, at)?;
        if !validated {
            return decline("validation failed");
        }
        if !before.is_empty() || !after.is_empty() {
            return decline("non-code pushed out of the root");
        }
        // A fix whose anchor was never reached (the root, or inside something
        // another fix removed) is dropped, as in Python.
        let mut loc = self.arena.pos_ref(root)?.working_loc();
        let node = self.build(&rebuilt, &mut loc)?;
        Some((node, self.plan))
    }
}
