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

use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::Arc;

use hashbrown::{HashMap, HashSet};
use sqlfluffrs_types::token::CaseFold;
use sqlfluffrs_types::{PositionMarker, Slice};

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
// TODO(fixing milestone): consumed by the splice engine (`stage_edit_batch`);
// the dead_code allow goes away when it lands.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct EditOp {
    pub(crate) anchor_uuid: u128,
    pub(crate) kind: EditKind,
    pub(crate) edits: Vec<NodeSpec>,
}

/// Payload for a to-be-created node, mirroring [`ArenaKind`] minus caches.
// TODO(fixing milestone): fields read by `ingest_spec` (in tests today) and
// the splice engine; allow goes away when `stage_edit_batch` lands.
#[allow(dead_code)]
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

/// A recursive description of a Python fix *edit* segment to ingest into the
/// arena.  Built by the façade (`_segment_to_spec`) from `LintFix.edit`
/// segments.  `pos_marker` is deliberately absent: native strips edit markers
/// in the `LintFix` constructor and relies on the position pass to synthesise
/// them (`fix.py:53-75`).
#[derive(Debug, Clone)]
pub(crate) struct NodeSpec {
    /// The Python segment's uuid (PYTHON_TAG space — disjoint from arena-minted
    /// RUST_TAG uuids). Preserved on ingest so identities match a native tree;
    /// a duplicate (a rule reusing one edit segment object across fixes —
    /// `BaseSegment.copy()` keeps uuids) is re-minted instead.
    pub(crate) uuid: u128,
    pub(crate) kind: SpecKind,
    pub(crate) source_fixes: Vec<SourceFixSpec>,
    pub(crate) children: Vec<NodeSpec>,
}

impl NodeSpec {
    /// Joined raw of the spec subtree — mirrors `BaseSegment.raw` for the
    /// yet-to-be-created segment (used for the `consumed_pos` match).
    // TODO(fixing milestone): used by the splice engine's consumed_pos check.
    #[allow(dead_code)]
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
}

/// Stable, arena-local identity for a node.  A plain index for milestone 1
/// (read-only); generational keys will be introduced alongside deletion in the
/// fixing milestone so that stale ids fail loudly instead of aliasing.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct NodeId(u32);

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
pub(crate) struct Arena {
    nodes: Vec<ArenaNode>,
    root: NodeId,
    by_uuid: HashMap<u128, NodeId>,
    /// Mutation epoch: bumped once per committed edit batch.  Python-side
    /// wrapper caches key their validity off this (and tests assert on it).
    epoch: u64,
    /// Source fixes per *leaf* node, kept as a sparse side-table rather than a
    /// field on every `ArenaKind::Raw` — fixes are rare (a handful per file at
    /// most) while nodes number in the tens of thousands.  Containers aggregate
    /// over their subtree on read (mirroring `BaseSegment.source_fixes`).
    source_fixes: HashMap<NodeId, Vec<SourceFixSpec>>,
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
            epoch: 0,
            source_fixes: HashMap::new(),
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
    // TODO(fixing milestone): lib callers arrive with the splice engine.
    #[allow(dead_code)]
    fn alloc_with_uuid(
        &mut self,
        kind: ArenaKind,
        pos_marker: Option<PositionMarker>,
        parent: Option<NodeId>,
        parent_idx: usize,
        uuid: u128,
    ) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        let uuid = if self.by_uuid.contains_key(&uuid) {
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
    // TODO(fixing milestone): lib callers arrive with the splice engine.
    #[allow(dead_code)]
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
    pub(crate) fn root(&self) -> NodeId {
        self.root
    }

    #[inline]
    fn node(&self, id: NodeId) -> &ArenaNode {
        &self.nodes[id.idx()]
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    // Companion to `len()` for the accessor surface; not yet wired up on the
    // Python side (the façade will use it), so allow it to be unused for now.
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub(crate) fn node_by_uuid(&self, uuid: u128) -> Option<NodeId> {
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
    // TODO(fixing milestone): called by `commit_staged`.
    #[allow(dead_code)]
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
    // TODO(fixing milestone): used by the splice engine (tests exercise it now).
    #[allow(dead_code)]
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

    #[inline]
    pub(crate) fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id.idx()].children
    }

    #[inline]
    pub(crate) fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id.idx()].parent
    }

    #[inline]
    pub(crate) fn uuid(&self, id: NodeId) -> u128 {
        self.nodes[id.idx()].uuid
    }

    #[inline]
    pub(crate) fn pos_marker(&self, id: NodeId) -> Option<PositionMarker> {
        self.nodes[id.idx()].pos_marker.clone()
    }

    // -- payload accessors (mirror Node / BaseSegment) -----------------------

    /// Joined raw text (cached for containers).
    pub(crate) fn raw(&self, id: NodeId) -> String {
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

    pub(crate) fn raw_upper(&self, id: NodeId) -> String {
        self.raw(id).to_uppercase()
    }

    /// Semantic type string (mirrors [`Node::get_type`]).
    pub(crate) fn get_type(&self, id: NodeId) -> String {
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
    pub(crate) fn is_type(&self, id: NodeId, target: &str) -> bool {
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

    pub(crate) fn is_any_type(&self, id: NodeId, targets: &[String]) -> bool {
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

    pub(crate) fn class_types(&self, id: NodeId) -> Vec<String> {
        self.node_type_set(id)
    }

    pub(crate) fn instance_types(&self, id: NodeId) -> Vec<String> {
        match &self.node(id).kind {
            ArenaKind::Raw { instance_types, .. } => instance_types.clone(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn segment_class(&self, id: NodeId) -> Option<String> {
        match &self.node(id).kind {
            ArenaKind::Raw { segment_class, .. } | ArenaKind::Segment { segment_class, .. } => {
                Some(segment_class.to_string())
            }
            _ => None,
        }
    }

    /// `is_implicit` flag for Indent/Dedent meta nodes (`None` for non-metas).
    pub(crate) fn is_implicit(&self, id: NodeId) -> Option<bool> {
        match &self.node(id).kind {
            ArenaKind::Meta {
                meta_type: MetaType::Indent { is_implicit } | MetaType::Dedent { is_implicit },
                ..
            } => Some(*is_implicit),
            _ => None,
        }
    }

    /// Characters to trim from both ends of a raw token (if set on the token).
    pub(crate) fn trim_chars(&self, id: NodeId) -> Option<Vec<String>> {
        match &self.node(id).kind {
            ArenaKind::Raw { kwargs, .. } => kwargs.trim_chars.clone(),
            _ => None,
        }
    }

    /// The `(pattern, group)` quote-extraction spec for a quoted raw token.
    pub(crate) fn quoted_value(&self, id: NodeId) -> Option<(String, String)> {
        match &self.node(id).kind {
            ArenaKind::Raw { kwargs, .. } => kwargs.quoted_value.clone(),
            _ => None,
        }
    }

    /// The escape `(pattern, replacement)` pairs for a raw token.
    pub(crate) fn escape_replacements(&self, id: NodeId) -> Option<Vec<(String, String)>> {
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

    pub(crate) fn is_raw(&self, id: NodeId) -> bool {
        // Mirrors `BaseSegment.is_raw` (`len(self.segments) == 0`): any leaf,
        // including meta segments (which are RawSegment subclasses in Python).
        self.children(id).is_empty()
    }

    pub(crate) fn is_meta(&self, id: NodeId) -> bool {
        matches!(self.node(id).kind, ArenaKind::Meta { .. })
    }

    /// Mirrors [`Node::is_code`].
    pub(crate) fn is_code(&self, id: NodeId) -> bool {
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

    pub(crate) fn is_whitespace(&self, id: NodeId) -> bool {
        match &self.node(id).kind {
            ArenaKind::Raw { instance_types, .. } => instance_types
                .iter()
                .any(|t| matches!(t.as_str(), "whitespace" | "newline")),
            ArenaKind::Segment { .. } | ArenaKind::Unparsable { .. } => {
                let kids = self.children(id);
                !kids.is_empty() && kids.iter().all(|&c| self.is_whitespace(c))
            }
            _ => false,
        }
    }

    pub(crate) fn is_comment(&self, id: NodeId) -> bool {
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
    pub(crate) fn is_templated(&self, id: NodeId) -> bool {
        match self.node(id).pos_marker.as_ref() {
            Some(pm) => !pm.is_literal() && !pm.is_point(),
            None => false,
        }
    }

    // -- navigation ----------------------------------------------------------

    /// First child matching any of `seg_type` (mirrors `get_child`).
    pub(crate) fn get_child(&self, id: NodeId, seg_type: &[String]) -> Option<NodeId> {
        self.children(id)
            .iter()
            .copied()
            .find(|&c| self.is_any_type(c, seg_type))
    }

    /// All children matching any of `seg_type` (mirrors `get_children`).
    pub(crate) fn get_children(&self, id: NodeId, seg_type: &[String]) -> Vec<NodeId> {
        self.children(id)
            .iter()
            .copied()
            .filter(|&c| self.is_any_type(c, seg_type))
            .collect()
    }

    /// Depth-first leaf nodes (mirrors `raw_segments` / `get_raw_segments`).
    pub(crate) fn raw_segments(&self, id: NodeId) -> Vec<NodeId> {
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
    pub(crate) fn recursive_crawl(
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
    pub(crate) fn recursive_crawl_all(&self, id: NodeId) -> Vec<NodeId> {
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
    pub(crate) fn descendant_type_set(&self, id: NodeId) -> Arc<HashSet<String>> {
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
    pub(crate) fn get_parent(&self, id: NodeId) -> Option<(NodeId, usize)> {
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
}
