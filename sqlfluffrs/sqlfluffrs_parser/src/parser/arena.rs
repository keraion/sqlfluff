//! Mutable, id-addressable arena tree for Rust-side linting & fixing.
//!
//! Today the Rust parser produces an immutable [`Node`] tree (see
//! [`super::types`]) which Python materialises into a pure-Python
//! `BaseSegment` tree where all linting and fixing happens.  The arena here is
//! the foundation for moving that work onto the Rust side: it is a flattened,
//! parent-linked tree addressed by stable [`NodeId`]s, so navigation
//! (`recursive_crawl`, `path_to`, ancestor walks) and — in later milestones —
//! edits become cheap, and Python only ever holds lightweight handles
//! (`(tree, node_id)`) rather than cloned subtrees.
//!
//! ## Milestone 1 scope (read-only navigation)
//!
//! This module currently implements ingest-from-[`Node`] plus the read-only
//! navigation/accessor surface that the Python rule API depends on.  uuids are
//! minted at ingest time (sufficient for read-only linting); threading a stable
//! uuid through `apply_as_root` is deferred to the fixing milestone where
//! cross-reingest identity matters for `LintFix` anchoring.

// Most of the arena's crate-public surface is consumed via the python-gated
// `arena_py` bindings; a no-python build (plain `cargo test`) would otherwise
// drown in cascade dead-code warnings for code the extension build exercises.
#![cfg_attr(not(feature = "python"), allow(dead_code))]

use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::Arc;

use hashbrown::{HashMap, HashSet};
use sqlfluffrs_dialects::Dialect;
use sqlfluffrs_types::token::CaseFold;
use sqlfluffrs_types::{PositionMarker, Slice};

use super::revalidate::{revalidate_leaf_descriptors, LeafDescriptor, ParseLimits, RevalidateOutcome};
use super::types::{MetaType, Node, RawSegmentKwargs};

/// A source-level edit attached to a segment, mirroring Python's ``SourceFix``
/// (`sqlfluff.core.parser.segments.base.SourceFix`): the replacement text and
/// the source/templated regions it rewrites.  Carried by fix *edit* segments
/// (e.g. JJ01 reformatting a jinja tag) and consumed by the patch generator.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceFixSpec {
    pub(crate) edit: String,
    pub(crate) source_slice: Slice,
    pub(crate) templated_slice: Slice,
}

/// The four native `LintFix` edit types (`fix.py:42-47`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditKind {
    Delete,
    Replace,
    CreateBefore,
    CreateAfter,
}

/// One fix to apply: native `LintFix` reduced to what the arena needs —
/// the anchor's uuid (fix matching is by `anchor.uuid`, `fix.py:160`), the
/// edit type, and the replacement/insertion segments as [`NodeSpec`]s.
#[derive(Debug, Clone)]
pub(crate) struct EditOp {
    pub(crate) anchor_uuid: u128,
    pub(crate) kind: EditKind,
    pub(crate) edits: Vec<NodeSpec>,
}

/// Payload for a to-be-created node, mirroring [`ArenaKind`] minus caches.
#[derive(Debug, Clone)]
pub(crate) enum SpecKind {
    Raw {
        segment_class: String,
        segment_type: String,
        class_type: String,
        raw: String,
        instance_types: Vec<String>,
        class_types: Vec<String>,
        kwargs: RawSegmentKwargs,
    },
    Segment {
        segment_class: String,
        segment_type: Option<String>,
        class_types: Vec<String>,
    },
    Meta {
        meta_type: MetaType,
        block_uuid: Option<u128>,
    },
}

/// A position marker carried on a fix-edit spec, as plain scalars:
/// `(source_start, source_stop, templated_start, templated_stop,
/// working_line_no, working_line_pos)`.  Rehydrated against the tree's
/// `TemplatedFile` on ingest.
pub(crate) type MarkerSpec = (usize, usize, usize, usize, usize, usize);

/// A recursive description of a Python fix *edit* segment to ingest into the
/// arena.  Built by the façade (`_segment_to_spec`) from `LintFix.edit`
/// segments.  `marker` mirrors the segment's `pos_marker`: native strips it
/// from the TOP-LEVEL edit segments in the `LintFix` constructor
/// (`fix.py:53-75`) but *descendants* keep theirs (e.g. ST05 nests clones of
/// real tree segments inside a new CTE) — and rules read those positions on
/// the mutated tree (`_is_child` CTE ordering), so they must survive ingest.
#[derive(Debug, Clone)]
pub(crate) struct NodeSpec {
    /// The Python segment's uuid (PYTHON_TAG space — disjoint from arena-minted
    /// RUST_TAG uuids). Preserved on ingest so identities match a native tree;
    /// a duplicate (a rule reusing one edit segment object across fixes —
    /// `BaseSegment.copy()` keeps uuids) is re-minted instead.
    pub(crate) uuid: u128,
    pub(crate) kind: SpecKind,
    pub(crate) marker: Option<MarkerSpec>,
    pub(crate) source_fixes: Vec<SourceFixSpec>,
    pub(crate) children: Vec<NodeSpec>,
}

impl NodeSpec {
    /// Joined raw of the spec subtree — mirrors `BaseSegment.raw` for the
    /// yet-to-be-created segment (used for the `consumed_pos` match).
    pub(crate) fn spec_raw(&self) -> String {
        match &self.kind {
            SpecKind::Raw { raw, .. } => raw.clone(),
            SpecKind::Meta { .. } => String::new(),
            SpecKind::Segment { .. } => {
                let mut s = String::new();
                for c in &self.children {
                    s.push_str(&c.spec_raw());
                }
                s
            }
        }
    }

    fn is_meta(&self) -> bool {
        matches!(self.kind, SpecKind::Meta { .. })
    }

    /// Mirrors [`Arena::is_code`] over the not-yet-created payload (used by
    /// the non-code bubble-up check on planned children).
    fn is_code(&self) -> bool {
        match &self.kind {
            SpecKind::Meta { .. } => false,
            SpecKind::Raw {
                segment_type,
                instance_types,
                class_types,
                ..
            } => {
                let non_code_by_instance = instance_types.iter().any(|t| {
                    matches!(
                        t.as_str(),
                        "whitespace"
                            | "newline"
                            | "comment"
                            | "inline_comment"
                            | "block_comment"
                            | "trailing_newline"
                    )
                });
                let non_code_by_type = segment_type.contains("comment")
                    || matches!(segment_type.as_str(), "whitespace" | "newline");
                let non_code_by_class = class_types.iter().any(|t| t == "comment");
                !(non_code_by_instance || non_code_by_type || non_code_by_class)
            }
            SpecKind::Segment { .. } => self.children.iter().any(|c| c.is_code()),
        }
    }

    /// The spec's declared class types (what Python sent — the edit segment's
    /// full `class_types` set), for the native validate-skip comparison
    /// (`f.edit[0].class_types == seg.class_types`, fix.py:223-227).
    fn class_type_set(&self) -> HashSet<String> {
        match &self.kind {
            SpecKind::Raw { class_types, .. } | SpecKind::Segment { class_types, .. } => {
                class_types.iter().cloned().collect()
            }
            SpecKind::Meta { .. } => HashSet::new(),
        }
    }

    /// Emit this spec subtree's ordered LEAF descriptors (specs with no
    /// children: Raw / Meta) for planned re-validation — the spec analogue of
    /// [`Arena::planned_leaves`]'s existing-leaf branch.  A Segment spec
    /// recurses into its children; a Raw/Meta is itself a leaf.
    fn spec_leaves(&self, out: &mut Vec<LeafDescriptor>) {
        match &self.kind {
            SpecKind::Raw {
                raw,
                segment_type,
                instance_types,
                class_types,
                ..
            } => {
                // Some fix-edit leaves (notably indent/dedent metas copied via
                // `RsSegment.copy`) arrive as `SpecKind::Raw` with meta types
                // rather than `SpecKind::Meta`.  Re-validation must still treat
                // them as metas (native drops `is_meta` leaves before the
                // re-match), so classify by type like native's `is_meta`: a
                // `meta` class-type or an indent/dedent segment-type.
                let is_meta = class_types.iter().any(|t| t == "meta")
                    || matches!(segment_type.as_str(), "indent" | "dedent");
                out.push(LeafDescriptor {
                    raw: raw.clone(),
                    instance_types: instance_types.clone(),
                    class_types: class_types.clone(),
                    is_code: !is_meta && self.is_code(),
                    is_meta,
                });
            }
            SpecKind::Meta { .. } => out.push(LeafDescriptor {
                raw: String::new(),
                instance_types: Vec::new(),
                class_types: Vec::new(),
                is_code: false,
                is_meta: true,
            }),
            SpecKind::Segment { .. } => {
                for c in &self.children {
                    c.spec_leaves(out);
                }
            }
        }
    }

    /// Subtree source fixes in document order (spec analogue of
    /// [`Arena::node_source_fixes`]).
    fn spec_source_fixes(&self, out: &mut Vec<SourceFixSpec>) {
        out.extend(self.source_fixes.iter().cloned());
        for c in &self.children {
            c.spec_source_fixes(out);
        }
    }
}

/// Stable, arena-local identity for a node.  A plain index for milestone 1
/// (read-only); generational keys will be introduced alongside deletion in the
/// fixing milestone so that stale ids fail loudly instead of aliasing.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct NodeId(u32);

impl NodeId {
    #[inline]
    fn idx(self) -> usize {
        self.0 as usize
    }
}

/// The payload of an arena node, mirroring the variants of [`Node`] but without
/// owned children (children live in [`ArenaNode::children`] as ids).
#[derive(Debug, Clone, PartialEq)]
enum ArenaKind {
    Raw {
        segment_class: Cow<'static, str>,
        segment_type: Cow<'static, str>,
        /// Class-level type (native ``BaseSegment.type``); differs from
        /// ``segment_type`` (native ``get_type()``) when a parser assigns an
        /// instance override (e.g. class ``symbol`` vs. instance ``star``).
        class_type: Cow<'static, str>,
        raw: String,
        instance_types: Vec<String>,
        class_types: Vec<String>,
        kwargs: RawSegmentKwargs,
    },
    Segment {
        segment_class: Cow<'static, str>,
        segment_type: Option<Cow<'static, str>>,
        class_types: Vec<String>,
    },
    Meta {
        meta_type: MetaType,
        /// Template-block identity (uuid as `u128`), carried from the lexer
        /// token so reflow reindent can group indents by originating block.
        block_uuid: Option<u128>,
    },
    Unparsable {
        expected: String,
    },
    Empty,
}

/// A single node in the arena: payload, structural links, position, and lazily
/// computed caches.
struct ArenaNode {
    uuid: u128,
    parent: Option<NodeId>,
    parent_idx: usize,
    children: Vec<NodeId>,
    pos_marker: Option<PositionMarker>,
    kind: ArenaKind,
    /// Cached joined raw for containers (mirrors `BaseSegment.raw`).  Reset on
    /// any structural mutation of this node or its descendants.
    cached_raw: RefCell<Option<String>>,
    /// Cached `descendant_type_set` (mirrors `BaseSegment.descendant_type_set`),
    /// used by the crawler to prune subtrees.
    descendant_types: RefCell<Option<Arc<HashSet<String>>>>,
}

/// A flattened, parent-linked, id-addressable parse tree.
pub struct Arena {
    nodes: Vec<ArenaNode>,
    root: NodeId,
    by_uuid: HashMap<u128, NodeId>,
    /// Uuids of tombstoned (replaced/deleted) nodes.  An ingested spec may
    /// carry one of these — `RsSegment.copy()` preserves uuids like native
    /// `BaseSegment.copy()`, so a fix edit cloned from the very subtree it
    /// replaces re-presents the old uuids.  They must NOT be re-registered:
    /// the Python façade interns `RsSegment` wrappers by uuid, and reusing a
    /// retired uuid would resurrect the dead node's wrapper (whose handle
    /// still points at the tombstoned subtree), making re-crawls see stale
    /// content.  `alloc_with_uuid` mints a fresh uuid instead.
    retired_uuids: HashSet<u128>,
    /// Mutation epoch: bumped once per committed edit batch.  Python-side
    /// wrapper caches key their validity off this (and tests assert on it).
    epoch: u64,
    /// Source fixes per *leaf* node, kept as a sparse side-table rather than a
    /// field on every `ArenaKind::Raw` — fixes are rare (a handful per file at
    /// most) while nodes number in the tens of thousands.  Containers aggregate
    /// over their subtree on read (mirroring `BaseSegment.source_fixes`).
    source_fixes: HashMap<NodeId, Vec<SourceFixSpec>>,
    /// A staged (planned but uncommitted) edit batch.  Staging computes the
    /// full splice plan *without mutating* so the Python fix loop can run
    /// native's loop-detection gates on the predicted state and only then
    /// commit — mirroring how native `apply_fixes` builds a new tree
    /// functionally and adopts it after the checks (`linter.py:623-656`).
    staged: Option<StagedBatch>,
}

/// One entry in a planned child list: either an existing arena node (kept,
/// possibly relocated by non-code bubble-up) or a to-be-created [`NodeSpec`]
/// (with the anchor's position marker when inherited via `consumed_pos`).
#[derive(Debug, Clone)]
enum PlannedChild {
    Existing(NodeId),
    New(Box<NodeSpec>, Option<PositionMarker>),
}

/// A fully-planned edit batch: for every container whose child list changes,
/// the complete new list; plus the subtree roots removed by delete/replace.
pub(crate) struct StagedBatch {
    children_of: HashMap<NodeId, Vec<PlannedChild>>,
    tombstones: Vec<NodeId>,
    /// The lowest containers whose child list changed in a way that native
    /// marks `requires_validate` (delete / create / type-CHANGING replace) —
    /// i.e. each such op's anchor's PARENT (captured pre-mutation, so ids are
    /// still valid).  `validate_staged` runs the native bottom-up rescue walk
    /// from each of these.  Deduped.  Empty for purely type-preserving edits
    /// (e.g. CP01 keyword-case), which native never re-validates.
    validate_containers: Vec<NodeId>,
}

/// What staging predicts, so Python can gate the commit (native's
/// `(raw, source_fixes)` loop checks) without mutating anything.
pub(crate) struct StageSummary {
    pub(crate) staged_raw: String,
    pub(crate) staged_source_fixes: Vec<SourceFixSpec>,
    pub(crate) applied: usize,
    pub(crate) unapplied_anchors: Vec<u128>,
    pub(crate) reverted_containers: usize,
    pub(crate) changed: bool,
}

/// Result of a subtree plan (mirrors native `apply_fixes`' recursive return:
/// the planned node plus non-code bubbled up to the parent).
struct SubPlan {
    changed: bool,
    pre: Vec<PlannedChild>,
    post: Vec<PlannedChild>,
    children_of: HashMap<NodeId, Vec<PlannedChild>>,
    tombstones: Vec<NodeId>,
    applied: usize,
    reverted: usize,
}

impl SubPlan {
    fn unchanged() -> Self {
        SubPlan {
            changed: false,
            pre: Vec::new(),
            post: Vec::new(),
            children_of: HashMap::new(),
            tombstones: Vec::new(),
            applied: 0,
            reverted: 0,
        }
    }

    fn merge_child(&mut self, other: SubPlan) -> (Vec<PlannedChild>, Vec<PlannedChild>) {
        self.changed |= other.changed;
        self.children_of.extend(other.children_of);
        self.tombstones.extend(other.tombstones);
        self.applied += other.applied;
        self.reverted += other.reverted;
        (other.pre, other.post)
    }
}

/// A single step on a path between two nodes — mirrors Python's `PathStep`
/// dataclass (`segment`, `idx`, `len`, `code_idxs`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PathStep {
    pub(crate) node: NodeId,
    pub(crate) idx: usize,
    pub(crate) len: usize,
    pub(crate) code_idxs: Vec<usize>,
}

impl Arena {
    // -- construction --------------------------------------------------------

    /// Build an arena from an existing [`Node`] tree.
    pub(crate) fn from_node(node: &Node) -> Self {
        let mut arena = Arena {
            nodes: Vec::new(),
            root: NodeId(0),
            by_uuid: HashMap::new(),
            retired_uuids: HashSet::new(),
            epoch: 0,
            source_fixes: HashMap::new(),
            staged: None,
        };
        let root = arena.ingest(node, None, 0);
        arena.root = root;
        arena
    }

    fn next_uuid(&self) -> u128 {
        // Monotonic tagged counter (same source as tokens, #7997). Node identity
        // only needs per-arena uniqueness + stability — not randomness — so this
        // avoids a CSPRNG draw per node (the arena mints one per segment). The
        // RUST_TAG in `next_id` keeps these disjoint from Python-minted ids.
        sqlfluffrs_types::identity::next_id()
    }

    fn alloc(
        &mut self,
        kind: ArenaKind,
        pos_marker: Option<PositionMarker>,
        parent: Option<NodeId>,
        parent_idx: usize,
    ) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        let uuid = self.next_uuid();
        self.nodes.push(ArenaNode {
            uuid,
            parent,
            parent_idx,
            children: Vec::new(),
            pos_marker,
            kind,
            cached_raw: RefCell::new(None),
            descendant_types: RefCell::new(None),
        });
        self.by_uuid.insert(uuid, id);
        id
    }

    /// Like [`alloc`], but with a caller-supplied uuid (a Python edit segment's
    /// identity, preserved across the FFI).  On collision — native
    /// `BaseSegment.copy()` keeps uuids, so a rule reusing one edit segment
    /// object across fixes produces duplicates — a fresh Rust uuid is minted
    /// instead, keeping `by_uuid` a bijection over attached nodes.
    fn alloc_with_uuid(
        &mut self,
        kind: ArenaKind,
        pos_marker: Option<PositionMarker>,
        parent: Option<NodeId>,
        parent_idx: usize,
        uuid: u128,
    ) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        let uuid = if self.by_uuid.contains_key(&uuid) || self.retired_uuids.contains(&uuid) {
            self.next_uuid()
        } else {
            uuid
        };
        self.nodes.push(ArenaNode {
            uuid,
            parent,
            parent_idx,
            children: Vec::new(),
            pos_marker,
            kind,
            cached_raw: RefCell::new(None),
            descendant_types: RefCell::new(None),
        });
        self.by_uuid.insert(uuid, id);
        id
    }

    /// Recursively allocate a [`NodeSpec`] subtree (a Python fix edit segment)
    /// into the arena.  Nodes are created *detached-by-construction*: the
    /// splice engine links the returned root into a parent's `children`.
    /// `pos_marker` stays `None` throughout — the position pass synthesises
    /// markers after splicing, mirroring native `_position_segments`.
    pub(crate) fn ingest_spec(
        &mut self,
        spec: &NodeSpec,
        parent: Option<NodeId>,
        parent_idx: usize,
    ) -> NodeId {
        let kind = match &spec.kind {
            SpecKind::Raw {
                segment_class,
                segment_type,
                class_type,
                raw,
                instance_types,
                class_types,
                kwargs,
            } => ArenaKind::Raw {
                segment_class: Cow::Owned(segment_class.clone()),
                segment_type: Cow::Owned(segment_type.clone()),
                class_type: Cow::Owned(class_type.clone()),
                raw: raw.clone(),
                instance_types: instance_types.clone(),
                class_types: class_types.clone(),
                kwargs: kwargs.clone(),
            },
            SpecKind::Segment {
                segment_class,
                segment_type,
                class_types,
            } => ArenaKind::Segment {
                segment_class: Cow::Owned(segment_class.clone()),
                segment_type: segment_type.clone().map(Cow::Owned),
                class_types: class_types.clone(),
            },
            SpecKind::Meta {
                meta_type,
                block_uuid,
            } => ArenaKind::Meta {
                meta_type: meta_type.clone(),
                block_uuid: *block_uuid,
            },
        };
        let id = self.alloc_with_uuid(kind, None, parent, parent_idx, spec.uuid);
        // Rehydrate the spec's own position marker (a descendant clone of a
        // real tree segment keeps its marker through `LintFix`, and native
        // apply_fixes splices it in as-is — `_position_segments` only
        // synthesises for marker-LESS segments). Without this, rules reading
        // positions off the mutated tree (e.g. ST05's `_is_child` CTE
        // ordering) see synthesised insert-point markers instead.
        if let Some((s0, s1, t0, t1, wln, wlp)) = spec.marker {
            if let Some(root_pm) = self.nodes[self.root.idx()].pos_marker.as_ref() {
                let tf = Arc::clone(&root_pm.templated_file);
                self.nodes[id.idx()].pos_marker = Some(PositionMarker::new(
                    Slice { start: s0, stop: s1 },
                    Slice { start: t0, stop: t1 },
                    &tf,
                    Some(wln),
                    Some(wlp),
                ));
            }
        }
        if !spec.source_fixes.is_empty() {
            self.source_fixes.insert(id, spec.source_fixes.clone());
        }
        let child_ids: Vec<NodeId> = spec
            .children
            .iter()
            .enumerate()
            .map(|(i, c)| self.ingest_spec(c, Some(id), i))
            .collect();
        self.nodes[id.idx()].children = child_ids;
        id
    }

    fn ingest(&mut self, node: &Node, parent: Option<NodeId>, parent_idx: usize) -> NodeId {
        match node {
            Node::Raw {
                segment_class,
                segment_type,
                class_type,
                raw,
                pos_marker,
                instance_types,
                class_types,
                segment_kwargs,
            } => self.alloc(
                ArenaKind::Raw {
                    segment_class: segment_class.clone(),
                    segment_type: segment_type.clone(),
                    class_type: class_type.clone(),
                    raw: raw.clone(),
                    instance_types: instance_types.clone(),
                    class_types: class_types.clone(),
                    kwargs: segment_kwargs.clone(),
                },
                pos_marker.clone(),
                parent,
                parent_idx,
            ),
            Node::Segment {
                segment_class,
                segment_type,
                pos_marker,
                class_types,
                children,
            } => {
                let id = self.alloc(
                    ArenaKind::Segment {
                        segment_class: segment_class.clone(),
                        segment_type: segment_type.clone(),
                        class_types: class_types.clone(),
                    },
                    pos_marker.clone(),
                    parent,
                    parent_idx,
                );
                let child_ids: Vec<NodeId> = children
                    .iter()
                    .enumerate()
                    .map(|(i, c)| self.ingest(c, Some(id), i))
                    .collect();
                self.nodes[id.idx()].children = child_ids;
                id
            }
            Node::Unparsable {
                expected,
                pos_marker,
                children,
            } => {
                let id = self.alloc(
                    ArenaKind::Unparsable {
                        expected: expected.clone(),
                    },
                    pos_marker.clone(),
                    parent,
                    parent_idx,
                );
                let child_ids: Vec<NodeId> = children
                    .iter()
                    .enumerate()
                    .map(|(i, c)| self.ingest(c, Some(id), i))
                    .collect();
                self.nodes[id.idx()].children = child_ids;
                id
            }
            Node::Meta {
                meta_type,
                pos_marker,
                block_uuid,
            } => self.alloc(
                ArenaKind::Meta {
                    meta_type: meta_type.clone(),
                    block_uuid: *block_uuid,
                },
                pos_marker.clone(),
                parent,
                parent_idx,
            ),
            Node::Empty => self.alloc(ArenaKind::Empty, None, parent, parent_idx),
        }
    }

    // -- basic access --------------------------------------------------------

    #[inline]
    pub fn root(&self) -> NodeId {
        self.root
    }

    #[inline]
    fn node(&self, id: NodeId) -> &ArenaNode {
        &self.nodes[id.idx()]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    // Companion to `len()` for the accessor surface; not yet wired up on the
    // Python side (the façade will use it), so allow it to be unused for now.
    #[inline]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node_by_uuid(&self, uuid: u128) -> Option<NodeId> {
        self.by_uuid.get(&uuid).copied()
    }

    /// Mutation epoch — bumped once per committed edit batch.
    #[inline]
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Bump the mutation epoch.  Called at commit time by the splice engine;
    /// exposed at crate level so groundwork tests can drive it before the
    /// engine lands.
    #[inline]
    pub(crate) fn bump_epoch(&mut self) {
        self.epoch += 1;
    }

    /// A node is *detached* once tombstoned by an edit: unlinked from its
    /// parent but with payload intact so outstanding handles keep reading
    /// (mirrors native, where a rule holding a deleted segment still reads it).
    /// The root is never detached.
    #[inline]
    pub(crate) fn is_detached(&self, id: NodeId) -> bool {
        id != self.root && self.nodes[id.idx()].parent.is_none()
    }

    /// Attach source fixes to a (leaf) node.  Used when ingesting fix edit
    /// segments; replaces any existing entry.
    pub(crate) fn set_source_fixes(&mut self, id: NodeId, fixes: Vec<SourceFixSpec>) {
        if fixes.is_empty() {
            self.source_fixes.remove(&id);
        } else {
            self.source_fixes.insert(id, fixes);
        }
    }

    /// Subtree source fixes in document order — mirrors
    /// `BaseSegment.source_fixes` (chained over children).
    pub(crate) fn node_source_fixes(&self, id: NodeId) -> Vec<SourceFixSpec> {
        let mut out = Vec::new();
        self.collect_source_fixes(id, &mut out);
        out
    }

    fn collect_source_fixes(&self, id: NodeId, out: &mut Vec<SourceFixSpec>) {
        if let Some(fixes) = self.source_fixes.get(&id) {
            out.extend(fixes.iter().cloned());
        }
        for &c in &self.nodes[id.idx()].children {
            self.collect_source_fixes(c, out);
        }
    }

    /// The stored ``source_str`` of a Template placeholder meta (`None` for
    /// any other node kind).  Native `TemplateSegment.source_str` is a stored
    /// attribute — deriving it from the pos marker (as the façade previously
    /// did) breaks once a placeholder's source is *edited* by a fix.
    pub(crate) fn meta_source_str(&self, id: NodeId) -> Option<String> {
        match &self.node(id).kind {
            ArenaKind::Meta {
                meta_type: MetaType::Template { source_str, .. },
                ..
            } => Some(source_str.clone()),
            _ => None,
        }
    }

    // -- edit staging / commit (the splice engine) ---------------------------
    //
    // Replicates native `apply_fixes` (src/sqlfluff/core/linter/fix.py:107-348)
    // over the arena, split into a *plan* phase (`stage_edit_batch` — computes
    // the full splice without mutating, so Python can run native's
    // loop-detection gates on the predicted state) and a *commit* phase
    // (`commit_staged` — installs the plan).  Grammar re-validation is a later
    // milestone; the local "already unparsable → silently revert" guard
    // (fix.py:317-330) IS replicated.

    /// Native `can_start_end_non_code`: only file and unparsable containers
    /// may start/end with non-code (whitespace) children.
    fn can_start_end_non_code(&self, id: NodeId) -> bool {
        self.is_type(id, "file") || self.is_type(id, "unparsable")
    }

    fn is_code_or_meta(&self, id: NodeId) -> bool {
        self.is_code(id) || self.is_meta(id)
    }

    fn planned_is_code_or_meta(&self, pc: &PlannedChild) -> bool {
        match pc {
            PlannedChild::Existing(id) => self.is_code_or_meta(*id),
            PlannedChild::New(spec, _) => spec.is_code() || spec.is_meta(),
        }
    }

    /// Plan an edit batch without mutating.  Returns the predicted state; the
    /// plan is stored on the arena for `commit_staged` / `discard_staged`.
    pub(crate) fn stage_edit_batch(
        &mut self,
        ops: Vec<EditOp>,
        fix_even_unparsable: bool,
    ) -> StageSummary {
        // Group ops by anchor uuid (at most two per anchor: the
        // create_before+create_after pair — Python's compute_anchor_edit_info
        // enforces validity before we're called).
        let mut ops_map: HashMap<u128, Vec<EditOp>> = HashMap::new();
        for op in ops {
            ops_map.entry(op.anchor_uuid).or_default().push(op);
        }

        // Validation targets (native `requires_validate`): the anchor's PARENT
        // for every op that native re-validates — delete, create_before,
        // create_after, or a type-CHANGING single/multi replace.  A single
        // replace whose sole edit has the SAME class_types as the anchor is
        // type-preserving and skipped (fix.py:223-227) — this is what keeps
        // CP01 keyword-case fixes from ever re-validating.  Captured here, on
        // the pre-mutation tree, so parents/uuids are still valid.  These are
        // only the DIRECT parents; `validate_staged` walks ancestors upward.
        let mut validate_containers: Vec<NodeId> = Vec::new();
        let mut seen_vc: HashSet<NodeId> = HashSet::new();
        for ops in ops_map.values() {
            for op in ops {
                let Some(anchor) = self.node_by_uuid(op.anchor_uuid) else {
                    continue;
                };
                let requires_validate = match op.kind {
                    EditKind::Delete | EditKind::CreateBefore | EditKind::CreateAfter => true,
                    EditKind::Replace => {
                        !(op.edits.len() == 1
                            && op.edits[0].class_type_set()
                                == self
                                    .class_types(anchor)
                                    .into_iter()
                                    .collect::<HashSet<String>>())
                    }
                };
                if requires_validate {
                    if let Some(parent) = self.parent(anchor) {
                        if seen_vc.insert(parent) {
                            validate_containers.push(parent);
                        }
                    }
                }
            }
        }

        // Dirty set: every ancestor chain from each anchor's PARENT to the
        // root (fixes are applied from the parent; native visits everything
        // but only paths towards remaining anchors can match).
        let mut dirty: HashSet<NodeId> = HashSet::new();
        for uuid in ops_map.keys() {
            if let Some(anchor) = self.node_by_uuid(*uuid) {
                let mut cur = self.parent(anchor);
                while let Some(p) = cur {
                    if !dirty.insert(p) {
                        break; // chain above already marked
                    }
                    cur = self.parent(p);
                }
            }
        }

        let plan = self.plan_node(self.root, &mut ops_map, &dirty, fix_even_unparsable);
        // Anything still unconsumed never matched a visited child: unknown
        // uuids, root-anchored ops (native never matches the root — fixes
        // apply from the parent), or anchors inside deleted/replaced subtrees.
        let unapplied: Vec<u128> = ops_map.keys().copied().collect();

        let mut children_of = plan.children_of;
        // Non-code bubbled out of the root has nowhere to go; native's root is
        // a `file` (can_start_end_non_code), so pre/post are always empty
        // there.  Guard anyway: fold them into the root's planned list.
        if !plan.pre.is_empty() || !plan.post.is_empty() {
            let entry = children_of.entry(self.root).or_insert_with(|| {
                self.children(self.root)
                    .iter()
                    .map(|&c| PlannedChild::Existing(c))
                    .collect()
            });
            let mut merged = plan.pre.clone();
            merged.extend(entry.iter().cloned());
            merged.extend(plan.post.clone());
            *entry = merged;
        }

        let staged_raw = self.planned_raw(self.root, &children_of);
        let mut staged_source_fixes = Vec::new();
        self.planned_source_fixes(self.root, &children_of, &mut staged_source_fixes);
        let changed = plan.changed && !children_of.is_empty();

        self.staged = Some(StagedBatch {
            children_of,
            tombstones: plan.tombstones,
            validate_containers,
        });

        StageSummary {
            staged_raw,
            staged_source_fixes,
            applied: plan.applied,
            unapplied_anchors: unapplied,
            reverted_containers: plan.reverted,
            changed,
        }
    }

    /// Recursive plan step for one container — mirrors the body of native
    /// `apply_fixes` (child loop → splice → recurse → non-code trim →
    /// unparsable guard).
    fn plan_node(
        &self,
        id: NodeId,
        ops: &mut HashMap<u128, Vec<EditOp>>,
        dirty: &HashSet<NodeId>,
        fix_even_unparsable: bool,
    ) -> SubPlan {
        // Mirrors native's early-out (`if not fixes or segment.is_raw()`):
        // nothing can change below a leaf, and off the dirty paths no
        // remaining anchor can match.
        if self.children(id).is_empty() || (!dirty.contains(&id) && ops.is_empty()) {
            return SubPlan::unchanged();
        }
        if !dirty.contains(&id) {
            return SubPlan::unchanged();
        }

        let mut out = SubPlan::unchanged();
        let mut buffer: Vec<PlannedChild> = Vec::new();
        let mut requires_validate = false;
        let mut fixes_applied = false;

        // -- splice (native fix.py:157-234) -----------------------------------
        for &child in self.children(id) {
            let anchor_ops = match ops.remove(&self.uuid(child)) {
                None => {
                    buffer.push(PlannedChild::Existing(child));
                    continue;
                }
                Some(a) => a,
            };
            fixes_applied = true;
            let mut anchor_ops = anchor_ops;
            // The pair case: ensure create_before comes first.
            if anchor_ops.len() == 2 && anchor_ops[0].kind == EditKind::CreateAfter {
                anchor_ops.reverse();
            }
            let n_fixes = anchor_ops.len();
            for op in anchor_ops {
                out.applied += 1;
                match op.kind {
                    EditKind::Delete => {
                        requires_validate = true;
                        out.tombstones.push(child);
                    }
                    EditKind::Replace | EditKind::CreateBefore | EditKind::CreateAfter => {
                        if op.kind == EditKind::CreateAfter && n_fixes == 1 {
                            // Unpaired create_after: anchor first.
                            buffer.push(PlannedChild::Existing(child));
                        }
                        let child_raw = self.raw(child);
                        let mut consumed_pos = false;
                        let n_edits = op.edits.len();
                        // Validate-skip: single same-class-types replace
                        // (fix.py:223-227).
                        if !(op.kind == EditKind::Replace
                            && n_edits == 1
                            && op.edits[0].class_type_set()
                                == self
                                    .class_types(child)
                                    .into_iter()
                                    .collect::<HashSet<String>>())
                        {
                            requires_validate = true;
                        }
                        for spec in op.edits {
                            // consumed_pos: on replace, the FIRST edit whose
                            // raw equals the anchor's raw inherits the
                            // anchor's full pos_marker (fix.py:209-217).
                            let marker = if op.kind == EditKind::Replace
                                && !consumed_pos
                                && spec.spec_raw() == child_raw
                            {
                                consumed_pos = true;
                                self.pos_marker(child)
                            } else {
                                None
                            };
                            buffer.push(PlannedChild::New(Box::new(spec), marker));
                        }
                        if op.kind == EditKind::Replace {
                            out.tombstones.push(child);
                        }
                        if op.kind == EditKind::CreateBefore {
                            buffer.push(PlannedChild::Existing(child));
                        }
                    }
                }
            }
        }

        // -- recurse (native fix.py:249-270) ----------------------------------
        let mut recursed: Vec<PlannedChild> = Vec::new();
        for pc in buffer {
            match pc {
                PlannedChild::Existing(c) => {
                    let sub = self.plan_node(c, ops, dirty, fix_even_unparsable);
                    let (pre, post) = out.merge_child(sub);
                    recursed.extend(pre);
                    recursed.push(PlannedChild::Existing(c));
                    recursed.extend(post);
                }
                PlannedChild::New(mut spec, marker) => {
                    // Native recurses into freshly spliced edit segments too:
                    // `BaseSegment.copy()` preserves uuids, so remaining
                    // anchors can match INSIDE a replacement copy.
                    let (pre, post, rv) = Self::plan_spec(&mut spec, ops, &mut out);
                    requires_validate |= rv;
                    recursed.extend(
                        pre.into_iter()
                            .map(|s| PlannedChild::New(Box::new(s), None)),
                    );
                    recursed.push(PlannedChild::New(spec, marker));
                    recursed.extend(
                        post.into_iter()
                            .map(|s| PlannedChild::New(Box::new(s), None)),
                    );
                }
            }
        }
        let mut seg_buffer = recursed;

        // -- non-code bubble-up (native fix.py:280-293) ------------------------
        let mut pre: Vec<PlannedChild> = Vec::new();
        let mut post: Vec<PlannedChild> = Vec::new();
        if !self.can_start_end_non_code(id) {
            let start = seg_buffer
                .iter()
                .position(|pc| self.planned_is_code_or_meta(pc))
                .unwrap_or(seg_buffer.len());
            pre = seg_buffer.drain(..start).collect();
            let stop = seg_buffer
                .iter()
                .rposition(|pc| self.planned_is_code_or_meta(pc))
                .map(|i| i + 1)
                .unwrap_or(0);
            post = seg_buffer.split_off(stop);
        }

        let changed_here = fixes_applied
            || out.changed
            || !pre.is_empty()
            || !post.is_empty()
            || seg_buffer.len() != self.children(id).len()
            || seg_buffer
                .iter()
                .zip(self.children(id))
                .any(|(pc, &c)| !matches!(pc, PlannedChild::Existing(e) if *e == c));

        if !changed_here {
            // Nothing changed at or below this container — keep the plan
            // sparse so raw prediction stays cheap.
            return SubPlan {
                changed: out.changed,
                pre: Vec::new(),
                post: Vec::new(),
                children_of: out.children_of,
                tombstones: out.tombstones,
                applied: out.applied,
                reverted: out.reverted,
            };
        }

        // -- "already unparsable" silent revert (native fix.py:317-330) --------
        if requires_validate && !fix_even_unparsable {
            let scoped_unparsable = self.is_type(id, "unparsable")
                || self.descendant_type_set(id).contains("unparsable");
            if scoped_unparsable {
                // Discard every planned change at/below this container and
                // return the ORIGINAL subtree (native returns `segment`).
                // Ops consumed within stay consumed — native popped them too.
                return SubPlan {
                    changed: false,
                    pre: Vec::new(),
                    post: Vec::new(),
                    children_of: HashMap::new(),
                    tombstones: Vec::new(),
                    applied: out.applied,
                    reverted: out.reverted + 1,
                };
            }
        }

        out.changed = true;
        out.children_of.insert(id, seg_buffer);
        out.pre = pre;
        out.post = post;
        out
    }

    /// Spec-level analogue of the child loop + recursion: applies remaining
    /// ops whose anchors match uuids INSIDE a replacement copy (native reaches
    /// them by recursing into the spliced edit segments), and trims non-code
    /// ends of new containers.  Returns (pre, post, requires_validate).
    fn plan_spec(
        spec: &mut NodeSpec,
        ops: &mut HashMap<u128, Vec<EditOp>>,
        out: &mut SubPlan,
    ) -> (Vec<NodeSpec>, Vec<NodeSpec>, bool) {
        if spec.children.is_empty() || ops.is_empty() {
            return (Vec::new(), Vec::new(), false);
        }
        let mut requires_validate = false;
        let mut buffer: Vec<NodeSpec> = Vec::new();
        for child in std::mem::take(&mut spec.children) {
            let anchor_ops = match ops.remove(&child.uuid) {
                None => {
                    buffer.push(child);
                    continue;
                }
                Some(a) => a,
            };
            let mut anchor_ops = anchor_ops;
            if anchor_ops.len() == 2 && anchor_ops[0].kind == EditKind::CreateAfter {
                anchor_ops.reverse();
            }
            let n_fixes = anchor_ops.len();
            for op in anchor_ops {
                out.applied += 1;
                match op.kind {
                    EditKind::Delete => {
                        requires_validate = true;
                    }
                    EditKind::Replace | EditKind::CreateBefore | EditKind::CreateAfter => {
                        if op.kind == EditKind::CreateAfter && n_fixes == 1 {
                            buffer.push(child.clone());
                        }
                        if !(op.kind == EditKind::Replace
                            && op.edits.len() == 1
                            && op.edits[0].class_type_set() == child.class_type_set())
                        {
                            requires_validate = true;
                        }
                        buffer.extend(op.edits);
                        if op.kind == EditKind::CreateBefore {
                            buffer.push(child.clone());
                        }
                    }
                }
            }
        }
        // Recurse into spec children.
        let mut recursed: Vec<NodeSpec> = Vec::new();
        for mut child in buffer {
            let (pre, post, rv) = Self::plan_spec(&mut child, ops, out);
            requires_validate |= rv;
            recursed.extend(pre);
            recursed.push(child);
            recursed.extend(post);
        }
        // Non-code trim: a new container may not start/end with non-code
        // unless it's a file/unparsable (never the case for edit specs).
        let start = recursed
            .iter()
            .position(|s| s.is_code() || s.is_meta())
            .unwrap_or(recursed.len());
        let pre: Vec<NodeSpec> = recursed.drain(..start).collect();
        let stop = recursed
            .iter()
            .rposition(|s| s.is_code() || s.is_meta())
            .map(|i| i + 1)
            .unwrap_or(0);
        let post = recursed.split_off(stop);
        spec.children = recursed;
        (pre, post, requires_validate)
    }

    /// Predicted joined raw with the plan overlaid.
    fn planned_raw(&self, id: NodeId, children_of: &HashMap<NodeId, Vec<PlannedChild>>) -> String {
        match children_of.get(&id) {
            None => self.raw(id),
            Some(planned) => {
                let mut s = String::new();
                for pc in planned {
                    match pc {
                        PlannedChild::Existing(c) => s.push_str(&self.planned_raw(*c, children_of)),
                        PlannedChild::New(spec, _) => s.push_str(&spec.spec_raw()),
                    }
                }
                s
            }
        }
    }

    /// Predicted document-order source fixes with the plan overlaid.
    fn planned_source_fixes(
        &self,
        id: NodeId,
        children_of: &HashMap<NodeId, Vec<PlannedChild>>,
        out: &mut Vec<SourceFixSpec>,
    ) {
        if let Some(fixes) = self.source_fixes.get(&id) {
            out.extend(fixes.iter().cloned());
        }
        match children_of.get(&id) {
            None => {
                for &c in self.children(id) {
                    self.planned_source_fixes(c, children_of, out);
                }
            }
            Some(planned) => {
                for pc in planned {
                    match pc {
                        PlannedChild::Existing(c) => {
                            self.planned_source_fixes(*c, children_of, out)
                        }
                        PlannedChild::New(spec, _) => spec.spec_source_fixes(out),
                    }
                }
            }
        }
    }

    /// The ordered LEAF descriptors of a container under the staged plan —
    /// the planned analogue of the leaf gathering in [`Self::revalidate_container`].
    /// For a container with no planned override this reads the committed
    /// subtree leaves; for a planned position it emits: `Existing(c)` → if `c`
    /// is a leaf, itself, else recurse over its planned/committed children;
    /// `New(spec, _)` → the spec subtree's leaves.  Metas are included; the
    /// shared verdict fn drops them.
    fn planned_leaves(
        &self,
        id: NodeId,
        children_of: &HashMap<NodeId, Vec<PlannedChild>>,
        out: &mut Vec<LeafDescriptor>,
    ) {
        match children_of.get(&id) {
            None => {
                // No planned override at this container: source from committed
                // leaves (recurse over real children until we hit leaves).
                if self.children(id).is_empty() {
                    out.push(LeafDescriptor {
                        raw: self.raw(id),
                        instance_types: self.instance_types(id),
                        class_types: self.class_types(id),
                        is_code: self.is_code(id),
                        is_meta: self.is_meta(id),
                    });
                } else {
                    for &c in self.children(id) {
                        self.planned_leaves(c, children_of, out);
                    }
                }
            }
            Some(planned) => {
                for pc in planned {
                    match pc {
                        PlannedChild::Existing(c) => self.planned_leaves(*c, children_of, out),
                        PlannedChild::New(spec, _) => spec.spec_leaves(out),
                    }
                }
            }
        }
    }

    /// Re-validate a container under the STAGED plan — the planned counterpart
    /// of [`Self::revalidate_container`].  Sources leaves from the plan
    /// (`planned_leaves`) instead of the committed tree, then funnels through
    /// the same shared verdict core.
    fn revalidate_planned_container(
        &self,
        id: NodeId,
        children_of: &HashMap<NodeId, Vec<PlannedChild>>,
        dialect: &Dialect,
        limits: ParseLimits,
    ) -> RevalidateOutcome {
        let Some(class) = self.segment_class(id) else {
            return RevalidateOutcome::Skipped;
        };
        let Some(root_grammar) = dialect.get_segment_grammar(&class) else {
            return RevalidateOutcome::Skipped;
        };
        let mut leaves = Vec::new();
        self.planned_leaves(id, children_of, &mut leaves);
        revalidate_leaf_descriptors(
            &leaves,
            dialect,
            root_grammar.grammar_id,
            root_grammar.tables,
            limits,
        )
    }

    /// Run native's bottom-up rescue walk over the staged plan and decide
    /// whether the whole batch is grammar-valid (native `apply_fixes`
    /// validation, fix.py:253-270 / 316-340).
    ///
    /// For each recorded validate-container `Cp` (the anchor's direct parent),
    /// walk ancestors upward: the FIRST clean re-match RESCUES the fix; a
    /// `Skipped` (no grammar) keeps propagating; an `Invalid` propagates to the
    /// parent.  The chain only REJECTS if it runs off the top of the tree still
    /// having seen an `Invalid`.  Returns true iff every chain is OK.
    ///
    /// Returns true when nothing is staged or there are no validate-containers
    /// (type-preserving batches — e.g. CP01 — never validate, matching native).
    ///
    /// `limits` carries the file's configured parse ceilings so re-validation
    /// re-matches under the same limits as the initial parse.
    pub(crate) fn validate_staged(&self, dialect: &Dialect, limits: ParseLimits) -> bool {
        let Some(staged) = self.staged.as_ref() else {
            return true;
        };
        for &cp in &staged.validate_containers {
            let mut cur = Some(cp);
            let mut hit_invalid = false;
            while let Some(c) = cur {
                match self.revalidate_planned_container(c, &staged.children_of, dialect, limits) {
                    // Clean re-match rescues (or the container never failed).
                    RevalidateOutcome::Valid => break,
                    RevalidateOutcome::Invalid => {
                        hit_invalid = true;
                        cur = self.parent(c);
                    }
                    // No grammar here: keep propagating upward.
                    RevalidateOutcome::Skipped => {
                        cur = self.parent(c);
                    }
                }
            }
            // The chain rejects iff we ran off the top having seen an Invalid.
            let chain_ok = !(cur.is_none() && hit_invalid);
            if !chain_ok {
                return false;
            }
        }
        true
    }

    /// Install the staged plan.  Returns the new epoch, or `None` if nothing
    /// was staged.
    pub(crate) fn commit_staged(&mut self) -> Option<u64> {
        let staged = self.staged.take()?;

        // 1. Tombstone removed subtrees FIRST: purge every uuid in each
        //    subtree from `by_uuid` and unlink the root.  Purging before spec
        //    ingest lets a replacement copy REUSE the anchor's uuid (native
        //    `copy()` preserves uuids), keeping identity continuity for
        //    subsequent crawls without tripping the collision policy.
        for &t in &staged.tombstones {
            self.purge_uuids(t);
            self.nodes[t.idx()].parent = None;
        }

        // 2. Install planned child lists.  Containers are independent — a
        //    node moved by bubble-up is simply present in its new parent's
        //    list and absent from the old one; whichever installs last sets
        //    the final parent link, and both lists agree by construction.
        let containers: Vec<NodeId> = staged.children_of.keys().copied().collect();
        for (&container, planned) in &staged.children_of {
            let mut final_children: Vec<NodeId> = Vec::with_capacity(planned.len());
            for (idx, pc) in planned.iter().enumerate() {
                let cid = match pc {
                    PlannedChild::Existing(c) => *c,
                    PlannedChild::New(spec, marker) => {
                        let nid = self.ingest_spec(spec, Some(container), idx);
                        if let Some(m) = marker {
                            self.nodes[nid.idx()].pos_marker = Some(m.clone());
                        }
                        nid
                    }
                };
                final_children.push(cid);
            }
            for (idx, &cid) in final_children.iter().enumerate() {
                self.nodes[cid.idx()].parent = Some(container);
                self.nodes[cid.idx()].parent_idx = idx;
            }
            self.nodes[container.idx()].children = final_children;
        }

        // 3. Invalidate subtree-derived caches on every changed container and
        //    every ancestor up to the root (both are functions of the whole
        //    subtree below a node).
        for &container in &containers {
            let mut cur = Some(container);
            while let Some(c) = cur {
                *self.nodes[c.idx()].cached_raw.borrow_mut() = None;
                *self.nodes[c.idx()].descendant_types.borrow_mut() = None;
                cur = self.nodes[c.idx()].parent;
            }
        }

        // 4. Position pass: synthesise markers for new nodes and re-infer
        //    working positions.  The dirty closure (changed containers + all
        //    their ancestors) tells the pass which unchanged-marker subtrees
        //    still need descending into (native repositions per-level during
        //    its reform; we run once from the root instead).
        let mut dirty: HashSet<NodeId> = HashSet::new();
        for &container in &containers {
            let mut cur = Some(container);
            while let Some(c) = cur {
                if !dirty.insert(c) {
                    break;
                }
                cur = self.nodes[c.idx()].parent;
            }
        }
        if let Some(root_pos) = self.pos_marker(self.root) {
            let root = self.root;
            self.position_children(root, &root_pos, &dirty);
        }

        self.epoch += 1;
        Some(self.epoch)
    }

    /// Drop the staged plan without mutating.
    pub(crate) fn discard_staged(&mut self) {
        self.staged = None;
    }

    /// Whether a batch is currently staged.
    pub(crate) fn has_staged(&self) -> bool {
        self.staged.is_some()
    }

    /// Remove a subtree's uuids from `by_uuid` (the nodes stay allocated and
    /// internally linked so outstanding handles keep reading).
    fn purge_uuids(&mut self, id: NodeId) {
        let uuid = self.nodes[id.idx()].uuid;
        // Only remove if the map still points at THIS node (a replacement may
        // have re-claimed the uuid — never clobber the live mapping).
        if self.by_uuid.get(&uuid) == Some(&id) {
            self.by_uuid.remove(&uuid);
            // Never reissue it: the façade's wrapper interning is uuid-keyed,
            // so a reused uuid would resurrect the dead node's wrapper.
            self.retired_uuids.insert(uuid);
        }
        let children = self.nodes[id.idx()].children.clone();
        for c in children {
            self.purge_uuids(c);
        }
    }

    // -- position pass (port of BaseSegment._position_segments) --------------

    /// Marker equality for the recurse-on-change check.  Mirrors the native
    /// dataclass `__eq__` (slices + working position; the templated file
    /// compares by identity in practice — one `Arc` per tree).
    fn marker_eq(a: &PositionMarker, b: &PositionMarker) -> bool {
        a.source_slice == b.source_slice
            && a.templated_slice == b.templated_slice
            && a.working_line_no == b.working_line_no
            && a.working_line_pos == b.working_line_pos
            && Arc::ptr_eq(&a.templated_file, &b.templated_file)
    }

    /// The issue-#6261 skip: a zero-length *templated* placeholder (a jinja
    /// expression that rendered to "") may sit before a sibling with an
    /// earlier templated position; using it as a forward end-point would
    /// produce an over-wide marker (base.py:493-511).
    fn is_zero_templated_placeholder(&self, id: NodeId, pm: &PositionMarker) -> bool {
        self.is_type(id, "placeholder")
            && self.raw(id).is_empty()
            && pm.templated_slice.is_empty()
            && !pm.source_slice.is_empty()
            && self.block_type(id).as_deref() == Some("templated")
    }

    /// Refresh positions below `parent`: assign point (or widened) markers to
    /// position-less nodes and re-infer every node's working line/pos.  Port
    /// of `_position_segments` (base.py:448-562), run once top-down at commit
    /// time instead of native's per-level reform — `dirty` carries every
    /// container whose child list changed plus their ancestors, so spliced
    /// subtrees whose own marker didn't change are still descended into.
    fn position_children(
        &mut self,
        parent: NodeId,
        parent_pos: &PositionMarker,
        dirty: &HashSet<NodeId>,
    ) {
        let children = self.nodes[parent.idx()].children.clone();
        if children.is_empty() {
            // Native asserts on an empty sequence; an emptied container is
            // tolerated here (deliberate safe superset).
            return;
        }
        let mut line_no = parent_pos.working_line_no;
        let mut line_pos = parent_pos.working_line_pos;
        for (idx, &child) in children.iter().enumerate() {
            let old = self.pos_marker(child);
            let mut newp = old.clone();
            if old.is_none() {
                // Start point: the previous sibling's (already updated) end,
                // else the parent's start (base.py:479-489).
                let start_point = if idx > 0 {
                    self.pos_marker(children[idx - 1])
                        .map(|m| m.end_point_marker())
                } else {
                    Some(parent_pos.start_point_marker())
                };
                // End point: forward scan over the (not yet updated) rest,
                // skipping #6261 placeholders and descending to the first raw
                // (base.py:491-516).
                let mut end_point = None;
                for &fwd in &children[idx + 1..] {
                    if let Some(fm) = self.pos_marker(fwd) {
                        if self.is_zero_templated_placeholder(fwd, &fm) {
                            continue;
                        }
                        let leaves = self.raw_segments(fwd);
                        let target = leaves.first().copied().unwrap_or(fwd);
                        let lm = self.pos_marker(target).unwrap_or(fm);
                        end_point = Some(lm.start_point_marker());
                        break;
                    }
                }
                newp = match (start_point, end_point) {
                    (Some(s), Some(e)) if !Self::marker_eq(&s, &e) => {
                        // Widened marker across the insertion gap.
                        Some(PositionMarker::from_points(&s, &e))
                    }
                    (Some(s), _) => Some(s),
                    (None, Some(e)) => Some(e),
                    // Native raises "Unable to position new segment"; leave
                    // unpositioned (only reachable on marker-less test trees).
                    (None, None) => None,
                };
            }
            let Some(np) = newp else {
                continue;
            };
            // Regardless of repositioning, refresh the working location and
            // advance the cursor over this node's raw (base.py:536-541).
            let np = np.with_working_position(line_no, line_pos);
            let raw = self.raw(child);
            let (nl, npos) = np.infer_next_position(&raw, line_no, line_pos);
            line_no = nl;
            line_pos = npos;
            let changed = match &old {
                None => true,
                Some(o) => !Self::marker_eq(o, &np),
            };
            self.nodes[child.idx()].pos_marker = Some(np.clone());
            if !self.nodes[child.idx()].children.is_empty() && (changed || dirty.contains(&child)) {
                self.position_children(child, &np, dirty);
            }
        }
    }

    #[inline]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id.idx()].children
    }

    #[inline]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id.idx()].parent
    }

    #[inline]
    pub fn uuid(&self, id: NodeId) -> u128 {
        self.nodes[id.idx()].uuid
    }

    #[inline]
    pub fn pos_marker(&self, id: NodeId) -> Option<PositionMarker> {
        self.nodes[id.idx()].pos_marker.clone()
    }

    // -- payload accessors (mirror Node / BaseSegment) -----------------------

    /// Joined raw text (cached for containers).
    pub fn raw(&self, id: NodeId) -> String {
        let n = self.node(id);
        match &n.kind {
            ArenaKind::Raw { raw, .. } => raw.clone(),
            ArenaKind::Meta { .. } | ArenaKind::Empty => String::new(),
            ArenaKind::Segment { .. } | ArenaKind::Unparsable { .. } => {
                if let Some(cached) = n.cached_raw.borrow().as_ref() {
                    return cached.clone();
                }
                let mut s = String::new();
                for &c in &n.children {
                    s.push_str(&self.raw(c));
                }
                *n.cached_raw.borrow_mut() = Some(s.clone());
                s
            }
        }
    }

    pub fn raw_upper(&self, id: NodeId) -> String {
        self.raw(id).to_uppercase()
    }

    /// Semantic type string (mirrors [`Node::get_type`]).
    pub fn get_type(&self, id: NodeId) -> String {
        match &self.node(id).kind {
            ArenaKind::Raw { segment_type, .. } => segment_type.to_string(),
            ArenaKind::Segment { segment_type, .. } => {
                segment_type.as_deref().unwrap_or_default().to_string()
            }
            ArenaKind::Meta { meta_type, .. } => meta_type_str(meta_type).to_string(),
            ArenaKind::Unparsable { .. } => "unparsable".to_string(),
            ArenaKind::Empty => "empty".to_string(),
        }
    }

    /// Class-level type string (mirrors native `BaseSegment.type` — the concrete
    /// class's `type` attribute).  For a Raw this is the stored `class_type`
    /// (which differs from `get_type()` when a parser assigned an instance
    /// override, e.g. class `symbol` vs. instance `star`).  For containers the
    /// class type equals `segment_type` (no instance override), and metas carry
    /// no override so it equals `get_type()`.
    pub(crate) fn class_type(&self, id: NodeId) -> String {
        match &self.node(id).kind {
            ArenaKind::Raw { class_type, .. } => class_type.to_string(),
            ArenaKind::Segment { segment_type, .. } => {
                segment_type.as_deref().unwrap_or_default().to_string()
            }
            ArenaKind::Meta { meta_type, .. } => meta_type_str(meta_type).to_string(),
            ArenaKind::Unparsable { .. } => "unparsable".to_string(),
            ArenaKind::Empty => "empty".to_string(),
        }
    }

    /// The structural class types every node of a given kind carries, mirroring
    /// the Python `RawSegment`/`MetaSegment`/`BaseSegment` class hierarchy
    /// (which always contributes `raw`/`base`/`meta` to `_class_types`).  These
    /// are deterministic by node kind, unlike the dialect-specific raw-class
    /// ancestors (e.g. `word` for `KeywordSegment`) which come from the parser's
    /// codegen data.
    fn structural_types(&self, id: NodeId) -> &'static [&'static str] {
        match &self.node(id).kind {
            ArenaKind::Raw { .. } => &["raw", "base"],
            ArenaKind::Segment { .. } => &["base"],
            // Python's `Dedent` subclasses `Indent`, so a dedent node carries
            // `indent` in its `_class_types` in addition to `dedent`.  Metas are
            // parser-inserted (not token-matched), so this core hierarchy isn't
            // in the grammar tables and must be encoded here.
            ArenaKind::Meta {
                meta_type: MetaType::Dedent { .. },
                ..
            } => &["dedent", "indent", "meta", "raw", "base"],
            ArenaKind::Meta { .. } => &["meta", "raw", "base"],
            ArenaKind::Unparsable { .. } => &["unparsable", "base"],
            ArenaKind::Empty => &[],
        }
    }

    /// Mirrors [`Node::is_type`], plus the structural hierarchy types.
    pub fn is_type(&self, id: NodeId, target: &str) -> bool {
        if self.structural_types(id).contains(&target) {
            return true;
        }
        match &self.node(id).kind {
            ArenaKind::Raw {
                segment_type,
                class_types,
                ..
            } => segment_type.as_ref() == target || class_types.iter().any(|t| t == target),
            ArenaKind::Segment {
                segment_type,
                class_types,
                ..
            } => segment_type.as_deref() == Some(target) || class_types.iter().any(|t| t == target),
            _ => self.get_type(id) == target,
        }
    }

    pub fn is_any_type(&self, id: NodeId, targets: &[String]) -> bool {
        targets.iter().any(|t| self.is_type(id, t))
    }

    /// The set of types this node "is" — `class_types`, the semantic type, and
    /// the structural hierarchy types.
    fn node_type_set(&self, id: NodeId) -> Vec<String> {
        let mut out: Vec<String> = match &self.node(id).kind {
            ArenaKind::Raw { class_types, .. } => class_types.clone(),
            ArenaKind::Segment { class_types, .. } => class_types.clone(),
            _ => Vec::new(),
        };
        let push_unique = |out: &mut Vec<String>, t: String| {
            if !out.iter().any(|x| x == &t) {
                out.push(t);
            }
        };
        push_unique(&mut out, self.get_type(id));
        for t in self.structural_types(id) {
            push_unique(&mut out, t.to_string());
        }
        out
    }

    pub fn class_types(&self, id: NodeId) -> Vec<String> {
        self.node_type_set(id)
    }

    pub fn instance_types(&self, id: NodeId) -> Vec<String> {
        match &self.node(id).kind {
            ArenaKind::Raw { instance_types, .. } => instance_types.clone(),
            _ => Vec::new(),
        }
    }

    pub fn segment_class(&self, id: NodeId) -> Option<String> {
        match &self.node(id).kind {
            ArenaKind::Raw { segment_class, .. } | ArenaKind::Segment { segment_class, .. } => {
                Some(segment_class.to_string())
            }
            _ => None,
        }
    }

    /// `is_implicit` flag for Indent/Dedent meta nodes (`None` for non-metas).
    pub fn is_implicit(&self, id: NodeId) -> Option<bool> {
        match &self.node(id).kind {
            ArenaKind::Meta {
                meta_type: MetaType::Indent { is_implicit } | MetaType::Dedent { is_implicit },
                ..
            } => Some(*is_implicit),
            _ => None,
        }
    }

    /// Characters to trim from both ends of a raw token (if set on the token).
    pub fn trim_chars(&self, id: NodeId) -> Option<Vec<String>> {
        match &self.node(id).kind {
            ArenaKind::Raw { kwargs, .. } => kwargs.trim_chars.clone(),
            _ => None,
        }
    }

    /// The `(pattern, group)` quote-extraction spec for a quoted raw token.
    pub fn quoted_value(&self, id: NodeId) -> Option<(String, String)> {
        match &self.node(id).kind {
            ArenaKind::Raw { kwargs, .. } => kwargs.quoted_value.clone(),
            _ => None,
        }
    }

    /// The escape `(pattern, replacement)` pairs for a raw token.
    pub fn escape_replacements(&self, id: NodeId) -> Option<Vec<(String, String)>> {
        match &self.node(id).kind {
            ArenaKind::Raw { kwargs, .. } => kwargs.escape_replacements.clone(),
            _ => None,
        }
    }

    /// Prefix sequences stripped before `trim_chars` by `raw_trimmed`.
    pub(crate) fn trim_start(&self, id: NodeId) -> Option<Vec<String>> {
        match &self.node(id).kind {
            ArenaKind::Raw { kwargs, .. } => kwargs.trim_start.clone(),
            _ => None,
        }
    }

    /// The dialect casefold mode for a raw token, as a lowercase string
    /// (`"upper"`/`"lower"`), or `None` when no fold applies. Mirrors the
    /// per-segment `RawSegment.casefold` callable.
    pub(crate) fn casefold(&self, id: NodeId) -> Option<String> {
        match &self.node(id).kind {
            ArenaKind::Raw { kwargs, .. } => match kwargs.casefold {
                CaseFold::Upper => Some("upper".to_string()),
                CaseFold::Lower => Some("lower".to_string()),
                CaseFold::None => None,
            },
            _ => None,
        }
    }

    /// The `block_type` of a placeholder (Template) meta segment
    /// (e.g. `"block_start"`, `"comment"`); `None` for any other node.
    pub(crate) fn block_type(&self, id: NodeId) -> Option<String> {
        match &self.node(id).kind {
            ArenaKind::Meta {
                meta_type: MetaType::Template { block_type, .. },
                ..
            } => Some(block_type.clone()),
            _ => None,
        }
    }

    /// The template-block identity (uuid as `u128`) of a meta segment, mirroring
    /// `TemplateSegment.block_uuid`; `None` for structural metas and non-metas.
    /// Used by reflow reindent to group indents by originating template block.
    pub(crate) fn block_uuid(&self, id: NodeId) -> Option<u128> {
        match &self.node(id).kind {
            ArenaKind::Meta { block_uuid, .. } => *block_uuid,
            _ => None,
        }
    }

    pub fn is_raw(&self, id: NodeId) -> bool {
        // Mirrors `BaseSegment.is_raw` (`len(self.segments) == 0`): any leaf,
        // including meta segments (which are RawSegment subclasses in Python).
        self.children(id).is_empty()
    }

    pub fn is_meta(&self, id: NodeId) -> bool {
        matches!(self.node(id).kind, ArenaKind::Meta { .. })
    }

    /// Mirrors [`Node::is_code`].
    pub fn is_code(&self, id: NodeId) -> bool {
        match &self.node(id).kind {
            ArenaKind::Meta { .. } | ArenaKind::Empty => false,
            ArenaKind::Raw {
                segment_type,
                instance_types,
                class_types,
                ..
            } => {
                let non_code_by_instance = instance_types.iter().any(|t| {
                    matches!(
                        t.as_str(),
                        "whitespace"
                            | "newline"
                            | "comment"
                            | "inline_comment"
                            | "block_comment"
                            | "trailing_newline"
                    )
                });
                let non_code_by_type = segment_type.contains("comment")
                    || matches!(segment_type.as_ref(), "whitespace" | "newline");
                let non_code_by_class = class_types.iter().any(|t| t == "comment");
                !(non_code_by_instance || non_code_by_type || non_code_by_class)
            }
            ArenaKind::Segment { .. } => self.children(id).iter().any(|&c| self.is_code(c)),
            ArenaKind::Unparsable { .. } => true,
        }
    }

    pub fn is_whitespace(&self, id: NodeId) -> bool {
        match &self.node(id).kind {
            ArenaKind::Raw {
                segment_type,
                instance_types,
                ..
            } => {
                // Check the semantic type as well as instance_types: lexed
                // tokens always carry instance_types, but nodes ingested from
                // fix edit segments (e.g. a NewlineSegment created by a rule)
                // may not — the type check mirrors `is_code`'s.
                matches!(segment_type.as_ref(), "whitespace" | "newline")
                    || instance_types
                        .iter()
                        .any(|t| matches!(t.as_str(), "whitespace" | "newline"))
            }
            ArenaKind::Segment { .. } | ArenaKind::Unparsable { .. } => {
                let kids = self.children(id);
                !kids.is_empty() && kids.iter().all(|&c| self.is_whitespace(c))
            }
            _ => false,
        }
    }

    pub fn is_comment(&self, id: NodeId) -> bool {
        match &self.node(id).kind {
            ArenaKind::Raw {
                segment_type,
                class_types,
                ..
            } => segment_type.contains("comment") || class_types.iter().any(|t| t == "comment"),
            ArenaKind::Segment { .. } | ArenaKind::Unparsable { .. } => {
                let kids = self.children(id);
                !kids.is_empty() && kids.iter().all(|&c| self.is_comment(c))
            }
            _ => false,
        }
    }

    /// Whether any descendant raw carries templated source (best-effort mirror
    /// of `BaseSegment.is_templated`: a non-literal, non-point source slice).
    pub fn is_templated(&self, id: NodeId) -> bool {
        match self.node(id).pos_marker.as_ref() {
            Some(pm) => !pm.is_literal() && !pm.is_point(),
            None => false,
        }
    }

    // -- navigation ----------------------------------------------------------

    /// First child matching any of `seg_type` (mirrors `get_child`).
    pub fn get_child(&self, id: NodeId, seg_type: &[String]) -> Option<NodeId> {
        self.children(id)
            .iter()
            .copied()
            .find(|&c| self.is_any_type(c, seg_type))
    }

    /// All children matching any of `seg_type` (mirrors `get_children`).
    pub fn get_children(&self, id: NodeId, seg_type: &[String]) -> Vec<NodeId> {
        self.children(id)
            .iter()
            .copied()
            .filter(|&c| self.is_any_type(c, seg_type))
            .collect()
    }

    /// Depth-first leaf nodes (mirrors `raw_segments` / `get_raw_segments`).
    pub fn raw_segments(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect_raw_segments(id, &mut out);
        out
    }

    fn collect_raw_segments(&self, id: NodeId, out: &mut Vec<NodeId>) {
        let kids = self.children(id);
        if kids.is_empty() {
            // Leaf: Raw, Meta, Empty, or empty container all count as a "raw".
            out.push(id);
        } else {
            for &c in kids {
                self.collect_raw_segments(c, out);
            }
        }
    }

    /// Mirrors `BaseSegment.recursive_crawl`.  `no_recursive_seg_type` stops
    /// recursion (but the stopping node is still yielded if it matches).
    pub fn recursive_crawl(
        &self,
        id: NodeId,
        seg_type: &[String],
        recurse_into: bool,
        no_recursive_seg_type: &[String],
        allow_self: bool,
    ) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.recursive_crawl_into(
            id,
            seg_type,
            recurse_into,
            no_recursive_seg_type,
            allow_self,
            &mut out,
        );
        out
    }

    fn recursive_crawl_into(
        &self,
        id: NodeId,
        seg_type: &[String],
        recurse_into: bool,
        no_recursive_seg_type: &[String],
        allow_self: bool,
        out: &mut Vec<NodeId>,
    ) {
        let matches = allow_self && self.is_any_type(id, seg_type);
        if matches {
            out.push(id);
        }
        // Early-exit if no target type is reachable below here (mirrors Python's
        // `descendant_type_set.intersection(seg_type)` check on *self*).
        let dts = self.descendant_type_set(id);
        if !seg_type.iter().any(|t| dts.contains(t)) {
            return;
        }
        if recurse_into || !matches {
            for &c in self.children(id) {
                // `no_recursive_seg_type` is applied to each CHILD before
                // recursing into it — never to `self` — so that crawling *from*
                // a node of a no_recursive type (e.g. a `with_compound_statement`
                // when excluding nested ones) still inspects its children.
                if no_recursive_seg_type.is_empty() || !self.is_any_type(c, no_recursive_seg_type) {
                    self.recursive_crawl_into(
                        c,
                        seg_type,
                        recurse_into,
                        no_recursive_seg_type,
                        true,
                        out,
                    );
                }
            }
        }
    }

    /// All descendants in document order (mirrors `recursive_crawl_all`).
    pub fn recursive_crawl_all(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.crawl_all_into(id, &mut out);
        out
    }

    fn crawl_all_into(&self, id: NodeId, out: &mut Vec<NodeId>) {
        out.push(id);
        for &c in self.children(id) {
            self.crawl_all_into(c, out);
        }
    }

    /// Mirrors `BaseSegment.descendant_type_set` — the union over direct
    /// children of `(child.class_types ∪ child.descendant_type_set)`.
    pub fn descendant_type_set(&self, id: NodeId) -> Arc<HashSet<String>> {
        if let Some(cached) = self.node(id).descendant_types.borrow().as_ref() {
            return cached.clone();
        }
        let mut set: HashSet<String> = HashSet::new();
        for &c in self.children(id) {
            for t in self.node_type_set(c) {
                set.insert(t);
            }
            for t in self.descendant_type_set(c).iter() {
                set.insert(t.clone());
            }
        }
        let rc = Arc::new(set);
        *self.node(id).descendant_types.borrow_mut() = Some(rc.clone());
        rc
    }

    /// Parent and the index of `id` within it (mirrors `get_parent`).
    pub fn get_parent(&self, id: NodeId) -> Option<(NodeId, usize)> {
        self.parent(id).map(|p| (p, self.node(id).parent_idx))
    }

    /// Path from `self` (an ancestor) down to `other`.  Returns the steps from
    /// `self` toward `other` (mirroring the common ancestor->descendant use of
    /// `BaseSegment.path_to`).  Empty if `self` is not an ancestor of `other`.
    pub(crate) fn path_to(&self, from: NodeId, to: NodeId) -> Vec<PathStep> {
        // Walk up from `to` collecting ancestors until we reach `from`.
        let mut chain: Vec<NodeId> = Vec::new();
        let mut cur = to;
        while cur != from {
            chain.push(cur);
            match self.parent(cur) {
                Some(p) => cur = p,
                None => return Vec::new(), // `from` is not an ancestor of `to`
            }
        }
        // chain is [to, ..., child_of_from]; reverse to top-down order.
        chain.reverse();
        let mut steps = Vec::with_capacity(chain.len());
        let mut parent = from;
        for node in chain {
            let idx = self.node(node).parent_idx;
            let siblings = self.children(parent);
            let code_idxs: Vec<usize> = siblings
                .iter()
                .enumerate()
                .filter(|(_, &s)| self.is_code(s))
                .map(|(i, _)| i)
                .collect();
            steps.push(PathStep {
                node: parent,
                idx,
                len: siblings.len(),
                code_idxs,
            });
            parent = node;
        }
        steps
    }

    /// Bulk equivalent of `path_to(root, leaf)` for every leaf under `root`, in a
    /// single DFS (O(n) total) instead of O(leaves * depth) separate walks. Each
    /// returned entry is `(leaf, path_from_root_to_leaf)`, where the path matches
    /// `path_to(root, leaf)` exactly. This is the reflow hot path
    /// (`raw_segments_with_ancestors` / `DepthMap`).
    pub(crate) fn raw_segments_with_ancestors(&self, root: NodeId) -> Vec<(NodeId, Vec<PathStep>)> {
        let mut out = Vec::new();
        let mut stack: Vec<PathStep> = Vec::new();
        self.rwa_dfs(root, &mut stack, &mut out);
        out
    }

    fn rwa_dfs(
        &self,
        node: NodeId,
        stack: &mut Vec<PathStep>,
        out: &mut Vec<(NodeId, Vec<PathStep>)>,
    ) {
        let kids: Vec<NodeId> = self.children(node).to_vec();
        if kids.is_empty() {
            out.push((node, stack.clone()));
            return;
        }
        // Same PathStep shape as path_to: the ancestor `node`, the descended
        // child index, the child count, and the code-child indices of `node`.
        let code_idxs: Vec<usize> = kids
            .iter()
            .enumerate()
            .filter(|(_, &s)| self.is_code(s))
            .map(|(i, _)| i)
            .collect();
        let len = kids.len();
        for (i, &child) in kids.iter().enumerate() {
            stack.push(PathStep {
                node,
                idx: i,
                len,
                code_idxs: code_idxs.clone(),
            });
            self.rwa_dfs(child, stack, out);
            stack.pop();
        }
    }

    /// Fully arena-side depth-info for reflow's `DepthMap`. One DFS emits, per
    /// leaf, its top-down stack of `(ancestor_uuid, idx, len, stack_pos)` — where
    /// `stack_pos` is the interpreted `StackPosition.type` string ("", "solo",
    /// "start", "end"), computed here exactly mirroring Python's
    /// `_stack_pos_interpreter`. Ancestor `class_types` are deduped into the
    /// second returned Vec (an ancestor appears in many leaves' paths, so its
    /// class_types are marshalled once, keyed by uuid). This lets the Python
    /// façade build `DepthInfo` objects directly with no per-step PathStep /
    /// PyHandle marshalling.
    #[allow(clippy::type_complexity)]
    pub(crate) fn reflow_depth_info(
        &self,
        root: NodeId,
    ) -> (
        Vec<(u128, Vec<(u128, usize, usize, String)>)>,
        Vec<(u128, Vec<String>)>,
    ) {
        let mut per_leaf: Vec<(u128, Vec<(u128, usize, usize, String)>)> = Vec::new();
        let mut anc_cts: Vec<(u128, Vec<String>)> = Vec::new();
        let mut seen: HashSet<u128> = HashSet::new();
        let mut stack: Vec<(u128, usize, usize, String)> = Vec::new();
        self.rdi_dfs(root, &mut stack, &mut per_leaf, &mut anc_cts, &mut seen);
        (per_leaf, anc_cts)
    }

    #[allow(clippy::type_complexity)]
    fn rdi_dfs(
        &self,
        node: NodeId,
        stack: &mut Vec<(u128, usize, usize, String)>,
        per_leaf: &mut Vec<(u128, Vec<(u128, usize, usize, String)>)>,
        anc_cts: &mut Vec<(u128, Vec<String>)>,
        seen: &mut HashSet<u128>,
    ) {
        let kids: Vec<NodeId> = self.children(node).to_vec();
        if kids.is_empty() {
            per_leaf.push((self.uuid(node), stack.clone()));
            return;
        }
        // Code-child indices of this ancestor (`node`), used for stack_pos.
        let code_idxs: Vec<usize> = kids
            .iter()
            .enumerate()
            .filter(|(_, &s)| self.is_code(s))
            .map(|(i, _)| i)
            .collect();
        let len = kids.len();
        // Dedupe: marshal this ancestor's class_types once (it recurs across many
        // leaf paths).
        let node_uuid = self.uuid(node);
        if seen.insert(node_uuid) {
            anc_cts.push((node_uuid, self.class_types(node)));
        }
        for (i, &child) in kids.iter().enumerate() {
            // Mirror _stack_pos_interpreter (depthmap.py): "" if no code children,
            // "solo" if exactly one, "start"/"end" for the first/last code index,
            // else "".
            let stack_pos: String = if code_idxs.is_empty() {
                String::new()
            } else if code_idxs.len() == 1 {
                "solo".to_string()
            } else if i == code_idxs[0] {
                "start".to_string()
            } else if i == *code_idxs.last().unwrap() {
                "end".to_string()
            } else {
                String::new()
            };
            stack.push((node_uuid, i, len, stack_pos));
            self.rdi_dfs(child, stack, per_leaf, anc_cts, seen);
            stack.pop();
        }
    }
}

fn meta_type_str(meta_type: &MetaType) -> &'static str {
    match meta_type {
        MetaType::Indent { .. } => "indent",
        MetaType::Dedent { .. } => "dedent",
        MetaType::Template { .. } => "placeholder",
        MetaType::TemplateLoop => "template_loop",
        MetaType::EndOfFile => "end_of_file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::types::RawSegmentKwargs;

    fn raw(class: &str, ty: &str, text: &str, instance: &[&str]) -> Node {
        Node::new_raw(
            class.to_string(),
            ty.to_string(),
            text.to_string(),
            None,
            instance.iter().map(|s| s.to_string()).collect(),
            RawSegmentKwargs::default(),
        )
    }

    fn select_tree() -> Node {
        // statement( select_statement( keyword "SELECT", ws " ", column_reference( naked_identifier "a" ) ) )
        let kw = raw("KeywordSegment", "keyword", "SELECT", &["keyword"]);
        let ws = raw("WhitespaceSegment", "whitespace", " ", &["whitespace"]);
        let ident = raw(
            "IdentifierSegment",
            "naked_identifier",
            "a",
            &["identifier"],
        );
        let col = Node::Segment {
            segment_class: "ColumnReferenceSegment".into(),
            segment_type: Some("column_reference".into()),
            pos_marker: None,
            class_types: vec!["column_reference".to_string()],
            children: vec![ident],
        };
        let select = Node::Segment {
            segment_class: "SelectStatementSegment".into(),
            segment_type: Some("select_statement".into()),
            pos_marker: None,
            class_types: vec!["select_statement".to_string(), "statement".to_string()],
            children: vec![kw, ws, col],
        };
        Node::Segment {
            segment_class: "StatementSegment".into(),
            segment_type: Some("statement".into()),
            pos_marker: None,
            class_types: vec!["statement".to_string()],
            children: vec![select],
        }
    }

    #[test]
    fn raw_matches_node() {
        let node = select_tree();
        let arena = Arena::from_node(&node);
        assert_eq!(arena.raw(arena.root()), node.raw());
        assert_eq!(arena.raw(arena.root()), "SELECT a");
    }

    #[test]
    fn is_type_uses_class_types() {
        let arena = Arena::from_node(&select_tree());
        let root = arena.root();
        // statement node
        assert!(arena.is_type(root, "statement"));
        // select_statement child is also a "statement" via class_types
        let select = arena.children(root)[0];
        assert!(arena.is_type(select, "select_statement"));
        assert!(arena.is_type(select, "statement"));
        assert!(!arena.is_type(select, "keyword"));
    }

    #[test]
    fn recursive_crawl_finds_descendants() {
        let arena = Arena::from_node(&select_tree());
        let kws = arena.recursive_crawl(arena.root(), &["keyword".to_string()], true, &[], true);
        assert_eq!(kws.len(), 1);
        assert_eq!(arena.raw(kws[0]), "SELECT");

        let idents = arena.recursive_crawl(
            arena.root(),
            &["naked_identifier".to_string()],
            true,
            &[],
            true,
        );
        assert_eq!(idents.len(), 1);
        assert_eq!(arena.raw(idents[0]), "a");
    }

    #[test]
    fn raw_segments_in_order() {
        let arena = Arena::from_node(&select_tree());
        let raws = arena.raw_segments(arena.root());
        let texts: Vec<String> = raws.iter().map(|&r| arena.raw(r)).collect();
        assert_eq!(texts, vec!["SELECT", " ", "a"]);
    }

    #[test]
    fn parent_links_and_path() {
        let arena = Arena::from_node(&select_tree());
        let root = arena.root();
        let raws = arena.raw_segments(root);
        let ident = *raws.last().unwrap();
        // path from root down to the identifier
        let steps = arena.path_to(root, ident);
        assert!(!steps.is_empty());
        // first step is at the root, last step's parent is the column_reference
        assert_eq!(steps[0].node, root);
        // get_parent of identifier should be the column_reference
        let (p, idx) = arena.get_parent(ident).unwrap();
        assert_eq!(arena.get_type(p), "column_reference");
        assert_eq!(idx, 0);
    }

    #[test]
    fn descendant_type_set_includes_nested() {
        let arena = Arena::from_node(&select_tree());
        let dts = arena.descendant_type_set(arena.root());
        assert!(dts.contains("keyword"));
        assert!(dts.contains("naked_identifier"));
        assert!(dts.contains("select_statement"));
    }

    #[test]
    fn is_code_and_whitespace() {
        let arena = Arena::from_node(&select_tree());
        let raws = arena.raw_segments(arena.root());
        // SELECT is code, " " is whitespace
        assert!(arena.is_code(raws[0]));
        assert!(!arena.is_code(raws[1]));
        assert!(arena.is_whitespace(raws[1]));
    }

    #[test]
    fn get_child_includes_meta() {
        // Mirror `BaseSegment.get_child`/`get_children`, which consider *all*
        // children including metas: a query for `indent` must return the meta.
        let tree = Node::Segment {
            segment_class: "SelectStatementSegment".into(),
            segment_type: Some("select_statement".into()),
            pos_marker: None,
            class_types: vec!["select_statement".to_string()],
            children: vec![
                raw("KeywordSegment", "keyword", "SELECT", &["keyword"]),
                Node::Meta {
                    meta_type: MetaType::Indent { is_implicit: false },
                    pos_marker: None,
                    block_uuid: None,
                },
                raw("WhitespaceSegment", "whitespace", " ", &["whitespace"]),
            ],
        };
        let arena = Arena::from_node(&tree);
        let root = arena.root();
        let indent = vec!["indent".to_string()];
        let child = arena.get_child(root, &indent).expect("indent meta found");
        assert!(arena.is_meta(child));
        assert_eq!(arena.get_type(child), "indent");
        assert_eq!(arena.get_children(root, &indent).len(), 1);
        // A non-meta query still works alongside metas.
        assert!(arena.get_child(root, &["keyword".to_string()]).is_some());
    }

    #[test]
    fn epoch_starts_zero_and_bumps() {
        let mut arena = Arena::from_node(&select_tree());
        assert_eq!(arena.epoch(), 0);
        arena.bump_epoch();
        assert_eq!(arena.epoch(), 1);
    }

    #[test]
    fn nothing_detached_after_ingest() {
        let arena = Arena::from_node(&select_tree());
        for i in 0..arena.len() {
            assert!(!arena.is_detached(NodeId(i as u32)));
        }
    }

    #[test]
    fn source_fixes_aggregate_in_document_order() {
        let mut arena = Arena::from_node(&select_tree());
        let root = arena.root();
        // Attach fixes to two leaves (keyword + identifier) and assert the
        // subtree aggregate at every ancestor level, in document order.
        let leaves = arena.raw_segments(root);
        let kw = leaves[0]; // "SELECT"
        let ident = *leaves.last().unwrap(); // "a"
        let f1 = SourceFixSpec {
            edit: "select".to_string(),
            source_slice: Slice { start: 0, stop: 6 },
            templated_slice: Slice { start: 0, stop: 6 },
        };
        let f2 = SourceFixSpec {
            edit: "b".to_string(),
            source_slice: Slice { start: 7, stop: 8 },
            templated_slice: Slice { start: 7, stop: 8 },
        };
        arena.set_source_fixes(ident, vec![f2.clone()]);
        arena.set_source_fixes(kw, vec![f1.clone()]);
        // Leaf-level: exactly its own.
        assert_eq!(arena.node_source_fixes(kw), vec![f1.clone()]);
        // Root-level: document order (keyword before identifier), regardless
        // of insertion order.
        assert_eq!(arena.node_source_fixes(root), vec![f1, f2]);
        // Clearing removes the side-table entry.
        arena.set_source_fixes(kw, Vec::new());
        assert_eq!(arena.node_source_fixes(root).len(), 1);
    }

    fn raw_spec(uuid: u128, ty: &str, text: &str) -> NodeSpec {
        NodeSpec {
            uuid,
            marker: None,
            kind: SpecKind::Raw {
                segment_class: "RawSegment".to_string(),
                segment_type: ty.to_string(),
                class_type: ty.to_string(),
                raw: text.to_string(),
                instance_types: Vec::new(),
                class_types: vec![ty.to_string()],
                kwargs: RawSegmentKwargs::default(),
            },
            source_fixes: Vec::new(),
            children: Vec::new(),
        }
    }

    #[test]
    fn spec_raw_joins_subtree() {
        let spec = NodeSpec {
            uuid: 1,
            marker: None,
            kind: SpecKind::Segment {
                segment_class: "CastExpressionSegment".to_string(),
                segment_type: Some("cast_expression".to_string()),
                class_types: vec!["cast_expression".to_string()],
            },
            source_fixes: Vec::new(),
            children: vec![raw_spec(2, "keyword", "cast"), raw_spec(3, "symbol", "(")],
        };
        assert_eq!(spec.spec_raw(), "cast(");
        // Metas contribute nothing.
        let meta = NodeSpec {
            uuid: 4,
            marker: None,
            kind: SpecKind::Meta {
                meta_type: MetaType::Indent { is_implicit: false },
                block_uuid: None,
            },
            source_fixes: Vec::new(),
            children: Vec::new(),
        };
        assert_eq!(meta.spec_raw(), "");
    }

    #[test]
    fn ingest_spec_preserves_uuid_and_structure() {
        let mut arena = Arena::from_node(&select_tree());
        let before_len = arena.len();
        let spec = NodeSpec {
            uuid: (1u128 << 120) | 42, // PYTHON_TAG-style uuid
            marker: None,
            kind: SpecKind::Segment {
                segment_class: "ColumnReferenceSegment".to_string(),
                segment_type: Some("column_reference".to_string()),
                class_types: vec!["column_reference".to_string()],
            },
            source_fixes: Vec::new(),
            children: vec![raw_spec((1u128 << 120) | 43, "naked_identifier", "b")],
        };
        let id = arena.ingest_spec(&spec, None, 0);
        assert_eq!(arena.len(), before_len + 2);
        // uuid preserved + resolvable.
        assert_eq!(arena.uuid(id), (1u128 << 120) | 42);
        assert_eq!(arena.node_by_uuid((1u128 << 120) | 42), Some(id));
        // Structure + payload.
        assert_eq!(arena.raw(id), "b");
        assert_eq!(arena.get_type(id), "column_reference");
        let kids = arena.children(id).to_vec();
        assert_eq!(kids.len(), 1);
        assert_eq!(arena.parent(kids[0]), Some(id));
        assert_eq!(arena.get_type(kids[0]), "naked_identifier");
        // No pos_marker until the position pass runs.
        assert!(arena.pos_marker(id).is_none());
        // Detached by construction (no parent link).
        assert!(arena.is_detached(id));
    }

    #[test]
    fn ingest_spec_mints_on_uuid_collision() {
        let mut arena = Arena::from_node(&select_tree());
        let existing_uuid = arena.uuid(arena.root());
        let spec = raw_spec(existing_uuid, "whitespace", " ");
        let id = arena.ingest_spec(&spec, None, 0);
        // Fresh uuid minted; original mapping untouched.
        assert_ne!(arena.uuid(id), existing_uuid);
        assert_eq!(arena.node_by_uuid(existing_uuid), Some(arena.root()));
        assert_eq!(arena.node_by_uuid(arena.uuid(id)), Some(id));
    }

    #[test]
    fn ingest_spec_carries_source_fixes() {
        let mut arena = Arena::from_node(&select_tree());
        let mut spec = raw_spec(999, "placeholder", "");
        spec.source_fixes = vec![SourceFixSpec {
            edit: "{%+ if true -%}".to_string(),
            source_slice: Slice {
                start: 14,
                stop: 27,
            },
            templated_slice: Slice {
                start: 14,
                stop: 14,
            },
        }];
        let id = arena.ingest_spec(&spec, None, 0);
        let fixes = arena.node_source_fixes(id);
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].edit, "{%+ if true -%}");
    }

    // -- splice engine tests --------------------------------------------------

    fn file_tree() -> Node {
        // file( statement( select( kw "SELECT", ws " ", ident "a" ) ), newline "\n" )
        let kw = raw("KeywordSegment", "keyword", "SELECT", &["keyword"]);
        let ws = raw("WhitespaceSegment", "whitespace", " ", &["whitespace"]);
        let ident = raw(
            "IdentifierSegment",
            "naked_identifier",
            "a",
            &["identifier"],
        );
        let select = Node::Segment {
            segment_class: "SelectStatementSegment".into(),
            segment_type: Some("select_statement".into()),
            pos_marker: None,
            class_types: vec!["select_statement".to_string(), "statement".to_string()],
            children: vec![kw, ws, ident],
        };
        let stmt = Node::Segment {
            segment_class: "StatementSegment".into(),
            segment_type: Some("statement".into()),
            pos_marker: None,
            class_types: vec!["statement".to_string()],
            children: vec![select],
        };
        let nl = raw("NewlineSegment", "newline", "\n", &["newline"]);
        Node::Segment {
            segment_class: "FileSegment".into(),
            segment_type: Some("file".into()),
            pos_marker: None,
            class_types: vec!["file".to_string()],
            children: vec![stmt, nl],
        }
    }

    /// Find the first leaf under root whose raw matches.
    fn leaf_by_raw(arena: &Arena, text: &str) -> NodeId {
        *arena
            .raw_segments(arena.root())
            .iter()
            .find(|&&l| arena.raw(l) == text)
            .expect("leaf found")
    }

    fn op(anchor: u128, kind: EditKind, edits: Vec<NodeSpec>) -> EditOp {
        EditOp {
            anchor_uuid: anchor,
            kind,
            edits,
        }
    }

    #[test]
    fn stage_delete_and_commit() {
        let mut arena = Arena::from_node(&file_tree());
        let ws = leaf_by_raw(&arena, " ");
        let ws_uuid = arena.uuid(ws);
        let summary = arena.stage_edit_batch(vec![op(ws_uuid, EditKind::Delete, vec![])], false);
        assert!(summary.changed);
        assert_eq!(summary.applied, 1);
        assert_eq!(summary.staged_raw, "SELECTa\n");
        assert!(summary.unapplied_anchors.is_empty());
        // Nothing mutated yet.
        assert_eq!(arena.raw(arena.root()), "SELECT a\n");
        assert_eq!(arena.epoch(), 0);
        // Commit.
        let epoch = arena.commit_staged().expect("committed");
        assert_eq!(epoch, 1);
        assert_eq!(arena.raw(arena.root()), "SELECTa\n");
        // Tombstone: detached, uuid purged, payload intact.
        assert!(arena.is_detached(ws));
        assert_eq!(arena.node_by_uuid(ws_uuid), None);
        assert_eq!(arena.raw(ws), " ");
        // parent_idx renumbered for the sibling after the deleted node.
        let select = arena.parent(leaf_by_raw(&arena, "SELECT")).unwrap();
        let kids = arena.children(select).to_vec();
        assert_eq!(kids.len(), 2);
        for (i, &k) in kids.iter().enumerate() {
            assert_eq!(arena.get_parent(k).unwrap().1, i);
            assert_eq!(arena.parent(k), Some(select));
        }
    }

    #[test]
    fn stage_discard_is_identity() {
        let mut arena = Arena::from_node(&file_tree());
        let before_raw = arena.raw(arena.root());
        let before_len = arena.len();
        let ws_uuid = arena.uuid(leaf_by_raw(&arena, " "));
        let _ = arena.stage_edit_batch(vec![op(ws_uuid, EditKind::Delete, vec![])], false);
        assert!(arena.has_staged());
        arena.discard_staged();
        assert!(!arena.has_staged());
        assert_eq!(arena.raw(arena.root()), before_raw);
        assert_eq!(arena.len(), before_len);
        assert_eq!(arena.epoch(), 0);
        assert_eq!(arena.node_by_uuid(ws_uuid), Some(leaf_by_raw(&arena, " ")));
    }

    #[test]
    fn replace_retires_uuid_and_consumed_pos_matches_native() {
        let mut arena = Arena::from_node(&file_tree());
        let kw = leaf_by_raw(&arena, "SELECT");
        let kw_uuid = arena.uuid(kw);
        // Replace "SELECT" with "select" — a copy-style edit reusing the
        // anchor''s uuid (native copy() preserves uuids).
        let mut spec = raw_spec(kw_uuid, "keyword", "select");
        // give the spec the same class_types as the arena node so the
        // validate-skip path is exercised (single same-type replace).
        if let SpecKind::Raw { class_types, .. } = &mut spec.kind {
            *class_types = arena.class_types(kw);
        }
        let summary =
            arena.stage_edit_batch(vec![op(kw_uuid, EditKind::Replace, vec![spec])], false);
        assert_eq!(summary.staged_raw, "select a\n");
        arena.commit_staged().unwrap();
        assert_eq!(arena.raw(arena.root()), "select a\n");
        // The anchor''s uuid is RETIRED (not reissued to the replacement):
        // the Python façade interns wrappers by uuid, so reissuing it would
        // resurrect the dead node''s wrapper and re-crawls would see the old
        // subtree. The replacement gets a fresh uuid instead.
        assert_eq!(arena.node_by_uuid(kw_uuid), None);
        let new_node = leaf_by_raw(&arena, "select");
        assert_ne!(new_node, kw);
        assert_ne!(arena.uuid(new_node), kw_uuid);
        // consumed_pos only fires on raw equality — raws differ here, so the
        // new node has no marker yet (position pass is a later phase).
        assert!(arena.pos_marker(new_node).is_none());
    }

    #[test]
    fn consumed_pos_inherits_marker_on_raw_match() {
        // Build a tree WITH markers via a real parse-like marker.
        use sqlfluffrs_types::TemplatedFile;
        let tf = std::sync::Arc::new(TemplatedFile::new(
            "SELECT a\n".to_string(),
            "<test>".to_string(),
            None,
            None,
            None,
        ));
        let mk = |start: usize, stop: usize| {
            PositionMarker::new(
                Slice { start, stop },
                Slice { start, stop },
                &tf,
                None,
                None,
            )
        };
        let mut node = file_tree();
        // attach a marker to the keyword leaf
        fn set_marker(n: &mut Node, raw_text: &str, pm: PositionMarker) -> bool {
            match n {
                Node::Raw {
                    raw, pos_marker, ..
                } if raw == raw_text => {
                    *pos_marker = Some(pm);
                    true
                }
                Node::Segment { children, .. } | Node::Unparsable { children, .. } => {
                    for c in children {
                        if set_marker(c, raw_text, pm.clone()) {
                            return true;
                        }
                    }
                    false
                }
                _ => false,
            }
        }
        assert!(set_marker(&mut node, "SELECT", mk(0, 6)));
        let mut arena = Arena::from_node(&node);
        let kw = leaf_by_raw(&arena, "SELECT");
        let kw_uuid = arena.uuid(kw);
        // Replace with same-raw edit (e.g. a source_fixes-only edit).
        let spec = raw_spec((1u128 << 120) | 7, "keyword", "SELECT");
        arena.stage_edit_batch(vec![op(kw_uuid, EditKind::Replace, vec![spec])], false);
        arena.commit_staged().unwrap();
        let new_node = arena.node_by_uuid((1u128 << 120) | 7).expect("new node");
        // consumed_pos: full marker inherited.
        let pm = arena.pos_marker(new_node).expect("marker inherited");
        assert_eq!(pm.source_slice, Slice { start: 0, stop: 6 });
    }

    #[test]
    fn create_before_and_after_ordering() {
        let mut arena = Arena::from_node(&file_tree());
        let ident = leaf_by_raw(&arena, "a");
        let u = arena.uuid(ident);
        // Paired create_before + create_after (submitted after-first to check
        // the reorder).
        let before = raw_spec(9001, "whitespace", "<");
        let after = raw_spec(9002, "whitespace", ">");
        let summary = arena.stage_edit_batch(
            vec![
                op(u, EditKind::CreateAfter, vec![after]),
                op(u, EditKind::CreateBefore, vec![before]),
            ],
            false,
        );
        // "<" and ">" are non-code (whitespace instance type is only set via
        // instance_types in raw_spec? they are type whitespace) — they would
        // bubble out of select_statement... use the raw prediction to check
        // ordering regardless of trim behaviour.
        assert!(
            summary.staged_raw.contains("<a>") || summary.staged_raw.contains("< a>") || {
                // if trimmed and bubbled, the order must still be < before a, > after a
                let s = &summary.staged_raw;
                let (ib, ia, ig) = (
                    s.find('<').unwrap(),
                    s.find('a').unwrap(),
                    s.find('>').unwrap(),
                );
                ib < ia && ia < ig
            }
        );
    }

    #[test]
    fn anchor_inside_deleted_subtree_is_unapplied() {
        // AL07''s canonical nested case: delete a container AND replace a
        // child inside it in the same batch → the inner op is dropped.
        let mut arena = Arena::from_node(&file_tree());
        let select = arena.parent(leaf_by_raw(&arena, "SELECT")).unwrap();
        let ident = leaf_by_raw(&arena, "a");
        let select_uuid = arena.uuid(select);
        let ident_uuid = arena.uuid(ident);
        let inner_edit = raw_spec(9010, "naked_identifier", "b");
        let summary = arena.stage_edit_batch(
            vec![
                op(select_uuid, EditKind::Delete, vec![]),
                op(ident_uuid, EditKind::Replace, vec![inner_edit]),
            ],
            false,
        );
        assert_eq!(summary.applied, 1);
        assert_eq!(summary.unapplied_anchors, vec![ident_uuid]);
        assert_eq!(summary.staged_raw, "\n");
    }

    #[test]
    fn root_anchor_never_applies() {
        let mut arena = Arena::from_node(&file_tree());
        let root_uuid = arena.uuid(arena.root());
        let summary = arena.stage_edit_batch(vec![op(root_uuid, EditKind::Delete, vec![])], false);
        assert_eq!(summary.applied, 0);
        assert_eq!(summary.unapplied_anchors, vec![root_uuid]);
        assert!(!summary.changed);
    }

    #[test]
    fn nonconde_bubble_up_to_file_root() {
        // Replace the identifier with (ident, trailing whitespace): the
        // whitespace may not end select_statement/statement, so it bubbles up
        // to the file root (which can hold it).
        let mut arena = Arena::from_node(&file_tree());
        let ident = leaf_by_raw(&arena, "a");
        let u = arena.uuid(ident);
        let e1 = raw_spec(9020, "naked_identifier", "b");
        let e2 = raw_spec(9021, "whitespace", "  ");
        let summary = arena.stage_edit_batch(vec![op(u, EditKind::Replace, vec![e1, e2])], false);
        // The whitespace ends up between the statement and the newline at the
        // FILE level: "SELECT b" + "  " + "\n".
        assert_eq!(summary.staged_raw, "SELECT b  \n");
        arena.commit_staged().unwrap();
        assert_eq!(arena.raw(arena.root()), "SELECT b  \n");
        // The whitespace node''s parent is the file root, not the select.
        let file_kids = arena.children(arena.root()).to_vec();
        assert_eq!(file_kids.len(), 3);
        assert_eq!(arena.raw(file_kids[1]), "  ");
        for (i, &k) in file_kids.iter().enumerate() {
            assert_eq!(arena.parent(k), Some(arena.root()));
            assert_eq!(arena.get_parent(k).unwrap().1, i);
        }
    }

    #[test]
    fn unparsable_scope_reverts_silently() {
        // statement contains an unparsable container; a fix inside it is
        // consumed but the subtree plan is discarded (native fix.py:317-330).
        let kw = raw("KeywordSegment", "keyword", "SELEC", &["keyword"]);
        let unp = Node::Unparsable {
            expected: "select".to_string(),
            pos_marker: None,
            children: vec![kw],
        };
        let stmt = Node::Segment {
            segment_class: "StatementSegment".into(),
            segment_type: Some("statement".into()),
            pos_marker: None,
            class_types: vec!["statement".to_string()],
            children: vec![unp],
        };
        let tree = Node::Segment {
            segment_class: "FileSegment".into(),
            segment_type: Some("file".into()),
            pos_marker: None,
            class_types: vec!["file".to_string()],
            children: vec![stmt],
        };
        let mut arena = Arena::from_node(&tree);
        let kw_id = leaf_by_raw(&arena, "SELEC");
        let u = arena.uuid(kw_id);
        let edit = raw_spec(9030, "keyword", "SELECT");
        let summary = arena.stage_edit_batch(vec![op(u, EditKind::Replace, vec![edit])], false);
        assert_eq!(summary.reverted_containers, 1);
        assert_eq!(summary.staged_raw, "SELEC");
        assert!(!summary.changed);
        // fix_even_unparsable=true applies it.
        let mut arena2 = Arena::from_node(&tree);
        let kw2 = leaf_by_raw(&arena2, "SELEC");
        let u2 = arena2.uuid(kw2);
        let edit2 = raw_spec(9031, "keyword", "SELECT");
        let summary2 = arena2.stage_edit_batch(vec![op(u2, EditKind::Replace, vec![edit2])], false);
        // note: the unparsable IS the direct parent — requires_validate at the
        // unparsable container... with feu=false it reverted; retry with true:
        let _ = summary2;
        arena2.discard_staged();
        let edit3 = raw_spec(9032, "keyword", "SELECT");
        let summary3 = arena2.stage_edit_batch(vec![op(u2, EditKind::Replace, vec![edit3])], true);
        assert_eq!(summary3.staged_raw, "SELECT");
        assert!(summary3.changed);
    }

    #[test]
    fn op_inside_replacement_copy_applies() {
        // Native recurses into spliced edit segments; uuids preserved by
        // copy() let remaining anchors match INSIDE the replacement.
        let mut arena = Arena::from_node(&file_tree());
        let select = arena.parent(leaf_by_raw(&arena, "SELECT")).unwrap();
        let select_uuid = arena.uuid(select);
        let kw = leaf_by_raw(&arena, "SELECT");
        let kw_uuid = arena.uuid(kw);
        // Replacement copy of the select (same uuids for children, native
        // copy-style), plus a second op anchored on the keyword INSIDE it.
        let copy_spec = NodeSpec {
            uuid: select_uuid,
            marker: None,
            kind: SpecKind::Segment {
                segment_class: "SelectStatementSegment".to_string(),
                segment_type: Some("select_statement".to_string()),
                class_types: vec!["select_statement".to_string()],
            },
            source_fixes: Vec::new(),
            children: vec![
                raw_spec(kw_uuid, "keyword", "SELECT"),
                raw_spec(9040, "whitespace", " "),
                raw_spec(9041, "naked_identifier", "z"),
            ],
        };
        let kw_edit = raw_spec(9042, "keyword", "select");
        let summary = arena.stage_edit_batch(
            vec![
                op(select_uuid, EditKind::Replace, vec![copy_spec]),
                op(kw_uuid, EditKind::Replace, vec![kw_edit]),
            ],
            false,
        );
        assert_eq!(summary.applied, 2);
        assert!(summary.unapplied_anchors.is_empty());
        assert_eq!(summary.staged_raw, "select z\n");
        arena.commit_staged().unwrap();
        assert_eq!(arena.raw(arena.root()), "select z\n");
    }

    #[test]
    fn commit_invalidates_ancestor_caches() {
        let mut arena = Arena::from_node(&file_tree());
        let root = arena.root();
        // Prime the caches.
        let _ = arena.raw(root);
        let _ = arena.descendant_type_set(root);
        let ws_uuid = arena.uuid(leaf_by_raw(&arena, " "));
        arena.stage_edit_batch(vec![op(ws_uuid, EditKind::Delete, vec![])], false);
        arena.commit_staged().unwrap();
        // Raw must reflect the mutation (stale cache would return the old).
        assert_eq!(arena.raw(root), "SELECTa\n");
    }

    #[test]
    fn empty_batch_is_noop() {
        let mut arena = Arena::from_node(&file_tree());
        let summary = arena.stage_edit_batch(vec![], false);
        assert!(!summary.changed);
        assert_eq!(summary.applied, 0);
        assert_eq!(summary.staged_raw, "SELECT a\n");
        // commit of an unchanged plan still bumps epoch (Python discards
        // no-op stages instead).
        arena.discard_staged();
        assert_eq!(arena.epoch(), 0);
    }

    // -- position pass golden tests -------------------------------------------

    /// file( statement( select( SELECT, " ", a ) ), "\n" ) over "SELECT a\n"
    /// with real position markers throughout.
    fn marked_file_tree() -> (Node, std::sync::Arc<sqlfluffrs_types::TemplatedFile>) {
        use sqlfluffrs_types::TemplatedFile;
        let tf = std::sync::Arc::new(TemplatedFile::new(
            "SELECT a\n".to_string(),
            "<test>".to_string(),
            None,
            None,
            None,
        ));
        let mk = |start: usize, stop: usize| {
            PositionMarker::new(
                Slice { start, stop },
                Slice { start, stop },
                &tf,
                None,
                None,
            )
        };
        let mut kw = raw("KeywordSegment", "keyword", "SELECT", &["keyword"]);
        let mut ws = raw("WhitespaceSegment", "whitespace", " ", &["whitespace"]);
        let mut ident = raw(
            "IdentifierSegment",
            "naked_identifier",
            "a",
            &["identifier"],
        );
        let mut nl = raw("NewlineSegment", "newline", "\n", &["newline"]);
        for (n, (a, b)) in [
            (&mut kw, (0usize, 6usize)),
            (&mut ws, (6, 7)),
            (&mut ident, (7, 8)),
            (&mut nl, (8, 9)),
        ] {
            if let Node::Raw { pos_marker, .. } = n {
                *pos_marker = Some(mk(a, b));
            }
        }
        let select = Node::Segment {
            segment_class: "SelectStatementSegment".into(),
            segment_type: Some("select_statement".into()),
            pos_marker: Some(mk(0, 8)),
            class_types: vec!["select_statement".to_string(), "statement".to_string()],
            children: vec![kw, ws, ident],
        };
        let stmt = Node::Segment {
            segment_class: "StatementSegment".into(),
            segment_type: Some("statement".into()),
            pos_marker: Some(mk(0, 8)),
            class_types: vec!["statement".to_string()],
            children: vec![select],
        };
        let file = Node::Segment {
            segment_class: "FileSegment".into(),
            segment_type: Some("file".into()),
            pos_marker: Some(mk(0, 9)),
            class_types: vec!["file".to_string()],
            children: vec![stmt, nl],
        };
        (file, tf)
    }

    fn working_of(arena: &Arena, text: &str) -> (usize, usize) {
        arena
            .pos_marker(leaf_by_raw(arena, text))
            .expect("marker")
            .working_loc()
    }

    #[test]
    fn position_pass_updates_working_after_delete() {
        let (node, _tf) = marked_file_tree();
        let mut arena = Arena::from_node(&node);
        // Pre-state: a at (1,8), newline at (1,9).
        assert_eq!(working_of(&arena, "a"), (1, 8));
        let ws_uuid = arena.uuid(leaf_by_raw(&arena, " "));
        arena.stage_edit_batch(vec![op(ws_uuid, EditKind::Delete, vec![])], false);
        arena.commit_staged().unwrap();
        // Working positions cascade left; source/templated slices stay stale
        // (native relies on the raw-vs-templated comparison to detect edits).
        assert_eq!(working_of(&arena, "a"), (1, 7));
        assert_eq!(working_of(&arena, "\n"), (1, 8));
        let a = leaf_by_raw(&arena, "a");
        assert_eq!(
            arena.pos_marker(a).unwrap().source_slice,
            Slice { start: 7, stop: 8 }
        );
    }

    #[test]
    fn position_pass_point_marker_for_insertion() {
        let (node, _tf) = marked_file_tree();
        let mut arena = Arena::from_node(&node);
        let ident = leaf_by_raw(&arena, "a");
        let u = arena.uuid(ident);
        // Insert code (not whitespace, so it stays in the select) before "a".
        let new = raw_spec(9100, "keyword", "DISTINCT");
        arena.stage_edit_batch(vec![op(u, EditKind::CreateBefore, vec![new])], false);
        arena.commit_staged().unwrap();
        let nn = arena.node_by_uuid(9100).expect("inserted");
        let pm = arena.pos_marker(nn).expect("positioned");
        // Point marker between prev sibling (ws, ends 7) and fwd ("a" starts
        // at 7): start == end → the plain start point, zero-length.
        assert_eq!(pm.source_slice, Slice { start: 7, stop: 7 });
        assert!(pm.templated_slice.is_empty());
        // Working: inserted at (1,8); "a" pushed to (1,16) after "DISTINCT".
        assert_eq!(pm.working_loc(), (1, 8));
        assert_eq!(working_of(&arena, "a"), (1, 16));
    }

    #[test]
    fn position_pass_widened_marker_over_gap() {
        let (node, _tf) = marked_file_tree();
        let mut arena = Arena::from_node(&node);
        // Replace the whitespace (source [6,7)) with an unpositioned,
        // DIFFERENT-raw whitespace: no consumed_pos, so the new node gets a
        // synthesised marker widened across the gap [6,7).
        let ws = leaf_by_raw(&arena, " ");
        let u = arena.uuid(ws);
        let new = raw_spec(9110, "whitespace", "  ");
        arena.stage_edit_batch(vec![op(u, EditKind::Replace, vec![new])], false);
        arena.commit_staged().unwrap();
        let nn = arena.node_by_uuid(9110).expect("replaced");
        let pm = arena.pos_marker(nn).expect("positioned");
        // start = prev (SELECT) end 6; end = fwd ("a") start 7 → widened.
        assert_eq!(pm.source_slice, Slice { start: 6, stop: 7 });
        // Working of following segments re-inferred across the 2-space raw.
        assert_eq!(working_of(&arena, "a"), (1, 9));
    }

    #[test]
    fn position_pass_recurses_into_new_subtree() {
        let (node, _tf) = marked_file_tree();
        let mut arena = Arena::from_node(&node);
        let ident = leaf_by_raw(&arena, "a");
        let u = arena.uuid(ident);
        // Replace "a" with a container( b, ws, c ) — every node unpositioned.
        let sub = NodeSpec {
            uuid: 9120,
            marker: None,
            kind: SpecKind::Segment {
                segment_class: "ColumnReferenceSegment".to_string(),
                segment_type: Some("column_reference".to_string()),
                class_types: vec!["column_reference".to_string()],
            },
            source_fixes: Vec::new(),
            children: vec![
                raw_spec(9121, "naked_identifier", "b"),
                raw_spec(9122, "dot", "."),
                raw_spec(9123, "naked_identifier", "c"),
            ],
        };
        arena.stage_edit_batch(vec![op(u, EditKind::Replace, vec![sub])], false);
        arena.commit_staged().unwrap();
        // Container positioned (widened over the gap [7,8)), children too.
        let cont = arena.node_by_uuid(9120).unwrap();
        assert!(arena.pos_marker(cont).is_some());
        for uu in [9121u128, 9122, 9123] {
            let n = arena.node_by_uuid(uu).unwrap();
            assert!(arena.pos_marker(n).is_some(), "child {uu} positioned");
        }
        // Working cursor walks the new raws: b at (1,8), "." at (1,9), c at (1,10).
        assert_eq!(working_of(&arena, "b"), (1, 8));
        assert_eq!(working_of(&arena, "c"), (1, 10));
        assert_eq!(working_of(&arena, "\n"), (1, 11));
    }

    #[test]
    fn meta_source_str_only_for_template_metas() {
        let tree = Node::Segment {
            segment_class: "FileSegment".into(),
            segment_type: Some("file".into()),
            pos_marker: None,
            class_types: vec!["file".to_string()],
            children: vec![
                raw("KeywordSegment", "keyword", "SELECT", &["keyword"]),
                Node::Meta {
                    meta_type: MetaType::Template {
                        source_str: "{{ ref('a') }}".to_string(),
                        block_type: "templated".to_string(),
                    },
                    pos_marker: None,
                    block_uuid: None,
                },
            ],
        };
        let arena = Arena::from_node(&tree);
        let root = arena.root();
        let kids = arena.children(root).to_vec();
        assert_eq!(arena.meta_source_str(kids[0]), None);
        assert_eq!(
            arena.meta_source_str(kids[1]).as_deref(),
            Some("{{ ref('a') }}")
        );
    }

    // -- validate_staged (Phase 2 grammar re-validation) ----------------------

    /// A leaf Node with an explicit class-type hierarchy (the shape a real
    /// dialect assigns), so re-validation sees the right typed leaves.
    fn typed_leaf(
        class: &str,
        ty: &str,
        text: &str,
        instance: &[&str],
        class_hierarchy: &[&str],
    ) -> Node {
        Node::new_raw_with_class_types(
            class.to_string(),
            ty.to_string(),
            ty.to_string(),
            text.to_string(),
            None,
            instance.iter().map(|s| s.to_string()).collect(),
            &class_hierarchy
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            RawSegmentKwargs::default(),
        )
    }

    /// A raw edit spec carrying an explicit `class_types` set (the edit
    /// segment's declared types — what native compares for validate-skip and
    /// what re-validation feeds the grammar).
    fn typed_raw_spec(
        uuid: u128,
        ty: &str,
        text: &str,
        instance: &[&str],
        class_types: &[&str],
    ) -> NodeSpec {
        NodeSpec {
            uuid,
            marker: None,
            kind: SpecKind::Raw {
                segment_class: "RawSegment".to_string(),
                segment_type: ty.to_string(),
                class_type: ty.to_string(),
                raw: text.to_string(),
                instance_types: instance.iter().map(|s| s.to_string()).collect(),
                class_types: class_types.iter().map(|s| s.to_string()).collect(),
                kwargs: RawSegmentKwargs::default(),
            },
            source_fixes: Vec::new(),
            children: Vec::new(),
        }
    }

    /// A `FunctionNameSegment` container whose grammar (ANSI) expects a
    /// `TypedParser("word")` for its identifier — the RF06 shape.
    fn function_name_tree(leaf: Node) -> Node {
        Node::Segment {
            segment_class: "FunctionNameSegment".into(),
            segment_type: Some("function_name".into()),
            pos_marker: None,
            class_types: vec!["function_name".to_string()],
            children: vec![leaf],
        }
    }

    /// A type-CHANGING replace (word `function_name_identifier` → bare
    /// `naked_identifier`) corrupts the `FunctionNameSegment`: its grammar
    /// can no longer re-match its own leaf, so `validate_staged` REJECTS it.
    /// This is the RF06 backtick-strip case in miniature.
    #[test]
    fn validate_staged_rejects_type_changing_replace() {
        let tree = function_name_tree(typed_leaf(
            "WordSegment",
            "function_name_identifier",
            "foo",
            &["function_name_identifier"],
            &["word", "raw", "base"],
        ));
        let mut arena = Arena::from_node(&tree);
        let leaf = leaf_by_raw(&arena, "foo");
        let leaf_uuid = arena.uuid(leaf);
        // Replace with a bare naked_identifier (DIFFERENT class_types).
        let spec = typed_raw_spec(
            leaf_uuid,
            "naked_identifier",
            "foo",
            &["naked_identifier"],
            &["identifier", "raw", "base"],
        );
        arena.stage_edit_batch(vec![op(leaf_uuid, EditKind::Replace, vec![spec])], false);
        assert!(
            !arena.validate_staged(&Dialect::Ansi, ParseLimits::default()),
            "type-changing replace should fail grammar re-validation"
        );
    }

    /// A type-PRESERVING replace (word → word, same class_types) still
    /// re-matches — `validate_staged` ACCEPTS it.  Also exercises the
    /// legitimate-unquote shape (RF06 on a real identifier).
    #[test]
    fn validate_staged_accepts_type_preserving_replace() {
        let tree = function_name_tree(typed_leaf(
            "WordSegment",
            "function_name_identifier",
            "foo",
            &["function_name_identifier"],
            &["word", "raw", "base"],
        ));
        let mut arena = Arena::from_node(&tree);
        let leaf = leaf_by_raw(&arena, "foo");
        let leaf_uuid = arena.uuid(leaf);
        // Same class_types => native marks it type-preserving => no validate
        // target => validate_staged trivially true.
        let spec = typed_raw_spec(
            leaf_uuid,
            "function_name_identifier",
            "bar",
            &["function_name_identifier"],
            &["word", "raw", "base"],
        );
        arena.stage_edit_batch(vec![op(leaf_uuid, EditKind::Replace, vec![spec])], false);
        assert!(
            arena.validate_staged(&Dialect::Ansi, ParseLimits::default()),
            "type-preserving replace should pass re-validation"
        );
    }

    /// Nothing staged / no validate-containers => trivially valid.
    #[test]
    fn validate_staged_true_when_nothing_staged() {
        let arena = Arena::from_node(&file_tree());
        assert!(arena.validate_staged(&Dialect::Ansi, ParseLimits::default()));
    }

}
