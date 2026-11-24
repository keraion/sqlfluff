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
use super::types::{Node, RawSegmentKwargs};

thread_local! {
    /// Why the last `Fixer::run` on this thread returned `None`, for measuring.
    pub(crate) static DECLINED: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
}

fn decline<T>(why: &'static str) -> Option<T> {
    DECLINED.with(|d| d.set(why));
    None
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum FixKind {
    Delete,
    ReplaceWhitespace,
    IndentAfterNewline,
    CreateBefore,
    ReplaceWithNewline,
    MoveCommentBefore,
    CreateAfter,
    CreateSpaceAfter,
    CreateSpaceBefore,
}

#[derive(PartialEq)]
enum EditType {
    Delete,
    Replace,
    CreateBefore,
    CreateAfter,
}

impl FixKind {
    pub(crate) fn parse(kind: &str) -> Option<Self> {
        Some(match kind {
            "delete" => Self::Delete,
            "replace_whitespace" => Self::ReplaceWhitespace,
            "indent_after_newline" => Self::IndentAfterNewline,
            "create_before" => Self::CreateBefore,
            "replace_with_newline" => Self::ReplaceWithNewline,
            "move_comment_before" => Self::MoveCommentBefore,
            "create_after" => Self::CreateAfter,
            "create_space_after" => Self::CreateSpaceAfter,
            "create_space_before" => Self::CreateSpaceBefore,
            _ => return None,
        })
    }

    /// The `edit_type` of the `LintFix` Python builds for this kind.
    fn edit_type(self) -> EditType {
        match self {
            Self::Delete => EditType::Delete,
            Self::ReplaceWhitespace | Self::IndentAfterNewline | Self::ReplaceWithNewline => {
                EditType::Replace
            }
            Self::CreateBefore | Self::MoveCommentBefore | Self::CreateSpaceBefore => {
                EditType::CreateBefore
            }
            Self::CreateAfter | Self::CreateSpaceAfter => EditType::CreateAfter,
        }
    }
}

pub(crate) struct Fix {
    pub kind: FixKind,
    pub text: String,
    pub extra: Option<NodeId>,
}

/// `(class name, type, class types)` of a leaf class Python may create.
pub(crate) type LeafClass = (String, String, Vec<String>);

/// What a new leaf is made from.
#[derive(Clone, Copy)]
enum Like {
    Whitespace,
    Newline,
    /// An old leaf with new text (`RawSegment.edit`).
    Edit(NodeId),
    /// A copy of an old leaf, placed somewhere else.
    Copy(NodeId),
}

/// A node of the tree being built.
enum W {
    /// An unchanged subtree of the old arena.
    Old(NodeId),
    /// A leaf made by a fix. Like the edits of a `LintFix` it starts with no
    /// position: `pos` is set when its siblings are positioned, as `(source
    /// slice, templated slice)`.
    New {
        like: Like,
        text: String,
        pos: Option<(Slice, Slice)>,
    },
    /// An old container with new children.
    Cont { from: NodeId, children: Vec<W> },
}

/// What Python needs to build its tree. `ops` holds five values per node, in
/// pre-order: `kind, a, b, working line, working position`.
///
/// kind 0: reuse old segment `a` and everything under it (nothing follows
///         for its descendants).
/// kind 1: copy old leaf `a` to the new working position.
/// kind 2: old leaf `a` edited to the next text, at the next four `slices`.
/// kind 3: new whitespace with the next text, at the next four `slices`.
/// kind 4: new newline at the next four `slices`.
/// kind 5: old container `a` with the next `b` nodes as its children.
/// kind 6: a copy of old leaf `a` at the next four `slices`.
#[derive(Default)]
pub(crate) struct Plan {
    pub ops: Vec<i64>,
    pub texts: Vec<String>,
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
    /// Classes with `can_start_end_non_code`.
    pub loose_ends: &'a HashSet<String>,
    pub whitespace: &'a LeafClass,
    pub newline: &'a LeafClass,
    pub templated_file: Arc<TemplatedFile>,
    /// Re-match a class's grammar over tokens: `Some(valid)`, or `None` if
    /// that can't be answered here.
    pub validate: &'a mut dyn FnMut(&str, Vec<Token>) -> Option<bool>,
    pub plan: Plan,
    /// Whether Python may reuse unchanged subtrees. Off only to measure the
    /// cost of building every segment anew.
    pub reuse: bool,
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
    fn slices(&self, w: &W) -> Option<(Slice, Slice)> {
        match w {
            W::Old(id) | W::Cont { from: id, .. } => self
                .arena
                .pos_ref(*id)
                .map(|p| (p.source_slice, p.templated_slice)),
            W::New { pos, .. } => *pos,
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
            W::New {
                like: Like::Edit(id) | Like::Copy(id),
                ..
            } => self.arena.is_code(*id),
            W::New { .. } => false,
            W::Cont { children, .. } => children.iter().any(|c| self.is_code(c)),
        }
    }

    /// `BaseSegment._is_code_or_meta`.
    fn is_code_or_meta(&self, w: &W) -> bool {
        self.is_code(w) || matches!(w, W::Old(id) if self.arena.is_meta(*id))
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
        parent: NodeId,
        parent_loc: (usize, usize),
        child_loc: &HashMap<NodeId, (usize, usize)>,
        buf: &mut [W],
    ) -> Option<()> {
        if !buf.iter().any(|w| matches!(w, W::New { pos: None, .. })) {
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
        for idx in 0..buf.len() {
            if !matches!(buf[idx], W::New { pos: None, .. }) {
                continue;
            }
            // Points are (source, templated, working location). The working
            // location is only used to compare the two, as Python does.
            let mut start = if idx > 0 {
                let (source, templated) = self.slices(&buf[idx - 1])?;
                (
                    source.stop,
                    templated.stop,
                    self.templated_file
                        .get_line_pos_of_char_pos(templated.stop, false),
                )
            } else {
                let p = self.arena.pos_ref(parent)?;
                (p.source_slice.start, p.templated_slice.start, parent_loc)
            };
            let mut end = None;
            for fwd in idx + 1..buf.len() {
                if flags[fwd] {
                    continue;
                }
                let (p, loc) = match &buf[fwd] {
                    W::Old(id) => (
                        self.arena.pos_ref(self.first_leaf(*id))?,
                        *child_loc.get(id)?,
                    ),
                    // Not positioned yet, or (a replace keeping its anchor's
                    // position) a leaf this never has to look past.
                    W::New { pos: None, .. } => continue,
                    _ => return None,
                };
                end = Some((p.source_slice.start, p.templated_slice.start, loc));
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
            if let W::New { pos, .. } = &mut buf[idx] {
                *pos = Some(new_pos);
            }
        }
        Some(())
    }

    fn leaves<'w>(&self, w: &'w W, out: &mut Vec<LeafRef<'w>>) {
        match w {
            W::Old(id) => out.extend(self.arena.raw_segments(*id).into_iter().map(LeafRef::Old)),
            W::Cont { children, .. } => children.iter().for_each(|c| self.leaves(c, out)),
            new => out.push(LeafRef::New(new)),
        }
    }

    fn token(&self, leaf: &LeafRef) -> Option<Token> {
        let like_old = |id: NodeId, raw: String, pos: PositionMarker| {
            Token::typed_leaf(
                raw,
                pos,
                self.arena.segment_class(id).unwrap_or_default(),
                self.arena.class_type(id),
                self.arena.class_types(id).iter().cloned().collect(),
                self.arena.instance_types_arc(id).as_ref().clone(),
                self.arena.is_code(id),
                self.arena.is_whitespace(id),
                self.arena.is_comment(id),
            )
        };
        let LeafRef::New(W::New { like, text, pos }) = leaf else {
            let LeafRef::Old(id) = leaf else { return None };
            return Some(like_old(
                *id,
                self.arena.raw(*id),
                self.arena.pos_marker(*id)?,
            ));
        };
        let (source, templated) = (*pos)?;
        let pos = PositionMarker::new(source, templated, &self.templated_file, Some(0), Some(0));
        Some(match like {
            Like::Edit(id) | Like::Copy(id) => like_old(*id, text.clone(), pos),
            Like::Whitespace | Like::Newline => {
                let class = if matches!(like, Like::Newline) {
                    self.newline
                } else {
                    self.whitespace
                };
                Token::typed_leaf(
                    text.clone(),
                    pos,
                    class.0.clone(),
                    class.1.clone(),
                    class.2.iter().cloned().collect(),
                    vec![],
                    false,
                    // As a leaf with no instance types reads from the arena.
                    false,
                    false,
                )
            }
        })
    }

    /// `validate_segment_with_reparse` for a rebuilt container.
    fn validate_cont(&mut self, w: &W) -> Option<bool> {
        let W::Cont { from, .. } = w else { return None };
        let name = self.arena.segment_class(*from)?;
        let mut leaves = Vec::new();
        self.leaves(w, &mut leaves);
        let is_meta = |l: &LeafRef| matches!(l, LeafRef::Old(id) if self.arena.is_meta(*id));
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
        let new = |like: Like, text: &str| W::New {
            like,
            text: text.to_string(),
            pos: None,
        };
        for &child in arena.children(id) {
            let Some(mut fixes) = self.fixes.remove(&child) else {
                buf.push(W::Old(child));
                continue;
            };
            applied = true;
            let single = fixes.len() == 1;
            if fixes.len() == 2 && fixes[0].kind.edit_type() == EditType::CreateAfter {
                fixes.reverse();
            }
            for fix in &fixes {
                let text = fix.text.as_str();
                let edit_type = fix.kind.edit_type();
                // Everything but a like-for-like replace needs validating.
                requires_validate |= fix.kind != FixKind::ReplaceWhitespace;
                if edit_type == EditType::CreateAfter && single {
                    buf.push(W::Old(child));
                }
                // An edit with the anchor's own text keeps its position.
                let edited = |text: &str| {
                    if text.is_empty() || text == arena.leaf_raw(child) {
                        W::Old(child)
                    } else {
                        new(Like::Edit(child), text)
                    }
                };
                match fix.kind {
                    FixKind::Delete => {}
                    FixKind::ReplaceWhitespace => buf.push(edited(text)),
                    FixKind::IndentAfterNewline => {
                        buf.push(W::Old(child));
                        buf.push(new(Like::Whitespace, text));
                    }
                    FixKind::CreateBefore | FixKind::CreateAfter => {
                        buf.push(new(Like::Newline, "\n"));
                        if !text.is_empty() {
                            buf.push(new(Like::Whitespace, text));
                        }
                    }
                    FixKind::ReplaceWithNewline => {
                        buf.push(new(Like::Newline, "\n"));
                        if !text.is_empty() {
                            buf.push(edited(text));
                        }
                    }
                    FixKind::CreateSpaceAfter | FixKind::CreateSpaceBefore => {
                        buf.push(new(Like::Whitespace, " "))
                    }
                    FixKind::MoveCommentBefore => {
                        let comment = fix.extra?;
                        buf.push(new(Like::Copy(comment), arena.leaf_raw(comment)));
                        buf.push(new(Like::Newline, "\n"));
                        if !text.is_empty() {
                            buf.push(new(Like::Whitespace, text));
                        }
                    }
                }
                if edit_type == EditType::CreateBefore {
                    buf.push(W::Old(child));
                }
            }
        }
        if applied {
            let mut child_loc = HashMap::new();
            let mut loc = at;
            for &child in arena.children(id) {
                child_loc.insert(child, loc);
                advance(&mut loc, &arena.raw(child));
            }
            self.position_new(id, at, &child_loc, &mut buf)?;
        }

        // Recurse into the children which have fixes beneath them.
        let queue = std::mem::take(&mut buf);
        let mut loc = at;
        for w in queue {
            match w {
                W::Old(child) => {
                    let child_at = loc;
                    advance(&mut loc, &arena.raw(child));
                    if self.marked.contains(&child) {
                        let (new_child, before, after, validated) = self.apply(child, child_at)?;
                        buf.extend(before);
                        buf.push(new_child);
                        buf.extend(after);
                        requires_validate |= !validated;
                    } else {
                        buf.push(W::Old(child));
                    }
                }
                other => {
                    if let W::New { text, .. } = &other {
                        advance(&mut loc, text);
                    }
                    buf.push(other);
                }
            }
        }

        // Non-code left at either end moves up to the parent.
        let (mut before, mut after) = (Vec::new(), Vec::new());
        let class = arena.segment_class(id)?;
        if !self.loose_ends.contains(&class) {
            let start = buf
                .iter()
                .position(|w| self.is_code_or_meta(w))
                .unwrap_or(buf.len());
            before = buf.drain(..start).collect();
            let end = buf
                .iter()
                .rposition(|w| self.is_code_or_meta(w))
                .map_or(0, |i| i + 1);
            after = buf.drain(end..).collect();
        }
        if buf.is_empty() {
            return None;
        }
        let rebuilt = W::Cont {
            from: id,
            children: buf,
        };
        let mut validated = true;
        if requires_validate {
            // Python handles unparsable sections its own way: leave it to it.
            if arena.descendant_type_set(id).contains("unparsable")
                || arena.is_type(id, "unparsable")
            {
                return decline("fix inside an unparsable section");
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
        let at = *loc;
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
            W::New { like, text, pos } => {
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
                advance(loc, text);
                self.plan.raw.push_str(text);
                let made = |class: &LeafClass| {
                    Node::new_raw_with_class_types(
                        class.0.clone(),
                        class.1.clone(),
                        class.1.clone(),
                        text.clone(),
                        Some(pos.clone()),
                        vec![],
                        &class.2,
                        RawSegmentKwargs::default(),
                    )
                };
                match like {
                    Like::Whitespace => {
                        op(&mut self.plan, 3, 0, 0);
                        self.plan.texts.push(text.clone());
                        made(self.whitespace)
                    }
                    Like::Newline => {
                        op(&mut self.plan, 4, 0, 0);
                        made(self.newline)
                    }
                    Like::Edit(from) => {
                        op(&mut self.plan, 2, from.index(), 0);
                        self.plan.texts.push(text.clone());
                        arena.node_like(*from, Some(pos), Some(text.clone()), vec![])
                    }
                    Like::Copy(from) => {
                        op(&mut self.plan, 6, from.index(), 0);
                        arena.node_like(*from, Some(pos), None, vec![])
                    }
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
        if !before.is_empty() || !after.is_empty() || !self.fixes.is_empty() {
            return None;
        }
        let mut loc = self.arena.pos_ref(root)?.working_loc();
        let node = self.build(&rebuilt, &mut loc)?;
        Some((node, self.plan))
    }
}
