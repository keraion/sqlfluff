"""Experimental: lint & fix over the Rust parse arena via an ``RsSegment`` façade.

The Rust-driven engine parses to an arena and hands Python an ``RsTree``
(``sqlfluffrs.engine_parse_to_tree``). :class:`RsSegment` is a ``BaseSegment``
duck-type backed by the arena's ``RsHandle`` cursor, so the existing Python
rules can crawl the Rust tree directly — no Python ``BaseSegment`` tree is built.

Fixes are applied by **mutating the arena in place** (no reparse), mirroring
native ``apply_fixes`` — :func:`facade_fix_loop` stages/commits edit batches on
the Rust arena and reconstructs the source with native
``generate_source_patches``. This is ~2.3× faster than native and
byte-identical. (The legacy v1 source-patch + re-parse loop and its
``SQLFLUFF_RS_FIX_V1`` escape hatch are retired.)

This covers the rules whose ``BaseSegment`` API surface the façade implements
(see :data:`FACADE_SAFE_RULES`); other rules should stay on the Python path.
Detection and fixing match native SQLFluff for the covered rules.
"""
# The RsSegment accessors are trivial one-line delegations to the arena handle
# that mirror the documented BaseSegment interface; per-method docstrings would
# be pure noise, so D102 is disabled for this façade module.
# ruff: noqa: D102

from __future__ import annotations

import logging
import weakref
from typing import Any, Iterator, Optional, cast

import regex

from sqlfluff.core.errors import SQLBaseError, SQLLintError, SQLParseError

# Same logger native ``apply_fixes`` uses, so the "unparsable file" warning is
# emitted on the identical channel (``src/sqlfluff/core/linter/fix.py``).
linter_logger = logging.getLogger("sqlfluff.linter")

# Interning cache so the same arena node always yields the same RsSegment object.
# Keyed by node uuid (a globally-unique monotonic counter), held weakly so
# wrappers are freed once no longer referenced. This makes identity (`x is y`)
# comparisons — used across the rule engine — behave like they do on native
# BaseSegment instances, without which navigation returns a fresh wrapper each
# time and `is` never matches (causing e.g. infinite recursion in alias analysis).
_INTERN: "weakref.WeakValueDictionary[int, RsSegment]" = weakref.WeakValueDictionary()

# Rules whose façade FIX output is byte-identical to native SQLFluff. Vetted
# both per-rule across ``std_rule_cases`` AND — crucially — by the COMBINED
# multi-rule fix over the whole ``test/fixtures/dialects/*`` corpus under each
# fixture's CORRECT dialect (running everything as ``ansi`` produces unparsable
# regions and spurious divergences that are pure harness artifacts). A rule is
# only listed if adding it introduces zero new whole-corpus divergences over the
# rest of the set. This is a *fix-output* guarantee, used by the self-guarding
# stdin/path-fix fast path (which re-checks that no violations remain).
#
# DETECTION parity is now ALSO verified: ``facade_violations`` matches native
# ``lint_string`` violation-for-violation ((rule, line, pos, description)) for
# every rule across the whole dialects corpus, and the raw token streams match
# native (type + class_types) on every comparable file. Kept in check by the
# lint-parity harness and the detection-parity suite in rs_lint_test.py.
#
# RF06 is façade-safe via arena grammar re-validation. It strips backtick quotes
# from mysql/mariadb/tsql stored-procedure/function *names* that native LEAVES
# quoted: unquoting ``\`name\``` reparses as ``function_name_identifier`` not the
# ``naked_identifier`` the fix specifies, so native ``validate_segment_with_reparse``
# rejects it. The façade now reproduces that check — ``RsTree.validate_staged``
# (parser ``revalidate`` + the arena stage/commit path) re-matches each edited
# container's grammar against its own typed arena leaves and discards a batch that
# would corrupt the tree — so the façade rejects exactly the fixes native does.
# Verified byte-identical to native over the whole ``test/fixtures/dialects/*``
# corpus (2144 façade-eligible files, 0 divergences).
#
# TQ02 became façade-safe once its own non-convergence was fixed. Its fix inserts
# a loose BEGIN ... END that a reparse folds into a begin_end_block, but native's
# no-reparse apply_fixes loop never does — so the loose keyword was never seen as
# a wrap and the fix re-fired every pass → runaway_limit → native reverted to the
# ORIGINAL (leaving the body UNwrapped), while the façade stabilised and kept the
# (correct) wrap: a divergence. TQ02 now recognises a body already led by a bare
# BEGIN keyword as wrapped (see TQ02._eval), so native converges too — both apply
# the wrap once. Verified byte-identical to native over the whole dialects corpus.
# Rules whose façade LINT detection diverges from native — currently EMPTY:
# the historical members (AL04, AL10, CP01, CV09, RF02, ST03) were re-audited
# with the whole-corpus lint-parity harness and all now match native
# violation-for-violation. (Their original blockers — concrete-class
# ``isinstance`` in rule/analysis code, CP01's sparksql ``div`` double-report
# — were fixed by intervening façade work; the last two real divergences were
# a missing ``deduplicate_in_source_space`` in ``facade_violations``, and
# arena tokens losing class_types/types on some parse paths.) The constant
# and its fast-path gating remain for future rules (e.g. plugins) that may
# need quarantining before their detection is verified.
FACADE_SAFE_RULES_DETECTION_UNSAFE: frozenset[str] = frozenset()

# Core rules DELIBERATELY kept off the façade fast paths, with the reason.
# Every bundled rule code must appear in ``FACADE_SAFE_RULES`` or here — the
# drift guard (test/rules/rs_lint_test.py) enforces totality, so adding a
# core rule forces an explicit decision (run the parity sweeps and allowlist
# it, or record the exclusion) instead of silently routing to native.
FACADE_EXCLUDED: dict[str, str] = {}
# Batch added (verified 0 NEW whole-corpus divergences over the prior set):
# AL05, AM04, AM07, CV06, PG02, RF01, RF03, RF05, ST02, ST10, ST11. Triage
# surfaced (and fixed) the sole combined-run divergence, which was PRE-EXISTING
# in the prior 51-rule set: on ``tsql/datatype_methods.sql`` the Rust parser
# matched T-SQL data-type methods (``.value()`` etc.) case-INSENSITIVELY, but
# native honours their ``ignore_case=False`` (only lowercase ``.value()`` is a
# method), so ``.Value()``/``.VALUE()`` parsed differently and a CP rule
# casefolded the schema name. The parser now honours ``ignore_case`` (RegexParser
# ``CASE_SENSITIVE`` grammar flag), so the 62-rule set is divergence-clean.
# Also added 8 LAYOUT rules — LT03, LT04, LT07, LT08, LT10, LT12, LT13, LT14 —
# verified 0 corpus divergences combined with the rest.
# (LT02's only façade-safe-suite failures are Jinja-templated cases, which route
# to native in production — skipped in the parity harness — so it is safe.)
#
# The FINAL batch — LT01, LT05, LT09 and ST05 — closed out every fixable rule.
# What unblocked them:
# * ST05 needed ``RsSegment.copy``'s synthetic classes to carry the raw flag
#   attrs (``_is_whitespace``/``_is_code``/``_is_comment``): without them a
#   cloned whitespace token reported ``is_whitespace=False`` and ST05 injected a
#   duplicate space after ``FROM`` when inspecting its clone.
# * The remaining combined-run divergences were NATIVE non-idempotency bugs
#   (native's own second fix run produced the façade's output — the façade was
#   already at the fixed point). Both were fixed native-side, TQ02-precedent:
#   (1) ``BaseSegment.copy(segments=...)`` didn't re-parent the provided
#   children, so after a mid-loop fix ``path_to`` could climb stale parent refs
#   into the old tree and reflow silently lost its depth info (LT11 missing
#   ``INTERSECT (`` violations after an LT09 fix); (2) CV11 built its cast()/
#   convert() replacements as FLAT token runs, so in the no-reparse loop LT01
#   saw no ``function_name``/``function_contents`` containers and added a stray
#   space after ``cast`` that a fresh parse would remove — CV11 now constructs
#   the parse-shaped nested subtree.
# ``facade_fix_loop`` also gained native's runaway-limit revert (return the
# ORIGINAL source when the main phase never stabilises, linter.py:673-699) and
# its warning parity (``_warn_unfixable`` on a previously-seen version), so
# loop-prone reflow rules degrade exactly like native.
FACADE_SAFE_RULES: frozenset[str] = frozenset(
    {
        "AL01",
        "AL02",
        "AL03",
        "AL04",
        "AL05",
        "AL06",
        "AL07",
        "AL08",
        "AL09",
        "AL10",
        "AM01",
        "AM02",
        "AM03",
        "AM04",
        "AM05",
        "AM06",
        "AM07",
        "AM08",
        "AM09",
        "CP01",
        "CP02",
        "CP03",
        "CP04",
        "CP05",
        "CV01",
        "CV02",
        "CV03",
        "CV04",
        "CV05",
        "CV06",
        "CV07",
        "CV08",
        "CV09",
        "CV10",
        "CV11",
        "CV12",
        "JJ01",
        "LT01",
        "LT02",
        "LT03",
        "LT04",
        "LT05",
        "LT06",
        "LT07",
        "LT08",
        "LT09",
        "LT10",
        "LT11",
        "LT12",
        "LT13",
        "LT14",
        "LT15",
        "OR01",
        "PG01",
        "PG02",
        "RF01",
        "RF02",
        "RF03",
        "RF04",
        "RF05",
        "RF06",
        "ST01",
        "ST02",
        "ST03",
        "ST04",
        "ST05",
        "ST06",
        "ST07",
        "ST08",
        "ST09",
        "ST10",
        "ST11",
        "ST12",
        "TQ01",
        "TQ02",
        "TQ03",
    }
)


def _typename(t: Any) -> str:
    """Coerce a ``get_child`` arg (type-name string or segment class) to a name."""
    if isinstance(t, str):
        return t
    return getattr(t, "type", None) or getattr(t, "_surrogate_type", None) or str(t)


# Cache of synthetic segment classes used by ``RsSegment.copy`` to materialise a
# real Python ``BaseSegment`` tree from an arena subtree. Keyed by
# (class-name, type, class_types, is_raw, raw_flags) so identical arena nodes
# reuse the same class. We build synthetic classes (rather than resolving the
# concrete dialect class) because ``RsSegment`` holds no dialect reference;
# setting ``_class_types`` directly guarantees ``is_type``/``class_types`` parity
# with the façade node without needing the dialect registry.
_SYNTH_CLASSES: dict[
    tuple[str, str, frozenset[str], bool, Optional[tuple[bool, bool, bool]]], type
] = {}


def _synth_segment_class(
    name: str,
    seg_type: str,
    class_types: frozenset[str],
    is_raw: bool,
    raw_flags: Optional[tuple[bool, bool, bool]] = None,
) -> type:
    """Return (cached) a real segment class reporting the given type/class_types.

    ``raw_flags`` is ``(is_code, is_comment, is_whitespace)`` for raw classes —
    ``RawSegment`` reports these from class attributes (``_is_code`` etc.), so a
    synthetic class must carry the arena node's values or a cloned whitespace
    token would report ``is_whitespace=False`` (which e.g. makes ST05 mis-read
    its clone and inject a duplicate space). Container classes derive these from
    their children, so pass ``None``.
    """
    from sqlfluff.core.parser import RawSegment
    from sqlfluff.core.parser.segments.base import BaseSegment

    if not name:
        # Meta/whitespace/newline tokens carry no class name in the arena;
        # derive a stable one from the type so `type()` gets a valid str.
        name = "".join(p.capitalize() for p in seg_type.split("_")) + "Segment"
    key = (name, seg_type, class_types, is_raw, raw_flags)
    cls = _SYNTH_CLASSES.get(key)
    if cls is None:
        base = RawSegment if is_raw else BaseSegment
        # The SegmentMetaclass recomputes ``_class_types`` from the base hierarchy
        # on class creation, so we override it *after* creation to force an exact
        # match with the arena node's class_types.
        # Include the handful of *dialect* segment methods that rules call on
        # copied segments (which are these synthetic classes, not the concrete
        # dialect class): CTEDefinitionSegment.get_identifier (used by ST05 on a
        # cloned CTE). These are navigation-only so they work on any real segment.
        namespace: dict[str, Any] = {
            "type": seg_type,
            "get_identifier": _synth_get_identifier,
        }
        if raw_flags is not None:
            namespace["_is_code"] = raw_flags[0]
            namespace["_is_comment"] = raw_flags[1]
            namespace["_is_whitespace"] = raw_flags[2]
        cls = type(name, (base,), namespace)
        cls._class_types = class_types  # type: ignore[attr-defined]
        _SYNTH_CLASSES[key] = cls
    return cls


def _synth_get_identifier(self: Any) -> Any:
    """Port of CTEDefinitionSegment.get_identifier for materialised copies."""
    return self.get_child("identifier")


class RsSegment:
    """A ``BaseSegment`` duck-type backed by an arena ``RsHandle``.

    Every accessor delegates to the (Rust) handle; navigation returns fresh
    ``RsSegment`` wrappers. Read-only: ``edit`` returns a real Python
    ``RawSegment`` for fix construction, but the arena itself is never mutated.
    """

    # `_uid` caches the node uuid (fetched once for interning) so __hash__/uuid
    # avoid an FFI call; `_ct` caches class_types (an interned wrapper is stable,
    # so the arena node's type set never changes under it) so is_type/class_types
    # become Python set operations instead of per-call FFI — the hot path in
    # rule crawling.
    __slots__ = ("_h", "_uid", "_segments", "_ct", "_dts", "_rwa", "__weakref__")
    _h: Any
    _uid: int
    _segments: Optional[tuple["RsSegment", ...]]
    _ct: Optional[frozenset[str]]
    _dts: Optional[frozenset[str]]
    _rwa: Optional[list[tuple["RsSegment", list[Any]]]]

    def __new__(cls, handle: Any) -> "RsSegment":
        # Intern by node uuid so the same node returns the same object (identity
        # stability). Field init happens here, not __init__, because __init__
        # still runs when __new__ returns a cached instance.
        uid = handle.uuid
        obj = _INTERN.get(uid)
        if obj is None:
            obj = object.__new__(cls)
            obj._h = handle
            obj._uid = uid
            obj._segments = None
            obj._ct = None
            obj._dts = None
            obj._rwa = None
            _INTERN[uid] = obj
        return obj

    # No __init__: all state is set in __new__. Defining a no-op __init__ would
    # add a Python-frame dispatch to every wrapper creation (the hot path); with
    # __new__ overridden and no __init__, object.__init__ ignores the argument.

    # -- identity ------------------------------------------------------------
    def __eq__(self, other: object) -> bool:
        # Interning makes same-node wrappers identical; uuid compare covers the
        # rare case where a wrapper was GC'd and re-created for the same node.
        return self is other or (
            isinstance(other, RsSegment) and self._uid == other._uid
        )

    def __hash__(self) -> int:
        return self._uid

    @property
    def uuid(self) -> int:
        return self._uid

    # -- payload -------------------------------------------------------------
    @property
    def raw(self) -> str:
        return self._h.raw

    @property
    def raw_upper(self) -> str:
        return self._h.raw_upper

    @property
    def block_type(self) -> Optional[str]:
        # A ``TemplateSegment`` (placeholder) attribute; ``None`` otherwise.
        return self._h.block_type()

    @property
    def block_uuid(self) -> Optional[int]:
        # A ``TemplateSegment`` (placeholder) attribute used by reflow reindent
        # to group template-block indents.  Native stores a ``uuid.UUID``; the
        # arena exposes it as an int (hashable + truthy), ``None`` otherwise.
        return self._h.block_uuid()

    def normalize(self, value: Optional[str] = None) -> str:
        # Mirrors ``RawSegment.normalize`` (parser/segments/raw.py): quote-strip
        # via ``quoted_value`` then apply ``escape_replacements``.
        raw_buff = value or self._h.raw
        qv = self._h.quoted_value()
        if qv:
            # The arena's quoted_value/escape patterns come from the Rust regex
            # crate, which uses ``(?<name>...)`` named-group syntax that Python's
            # ``re`` rejects ("unknown extension ?<"). The ``regex`` module (a
            # sqlfluff dependency) accepts that syntax, so use it here.
            _match = regex.match(qv[0], raw_buff)
            if _match:
                group = qv[1]
                # The arena stores the capture group as a string; a numeric
                # index arrives as e.g. ``"1"`` and must be int for ``group``.
                try:
                    group = int(group)
                except (TypeError, ValueError):
                    pass
                _group_match = _match.group(group)
                if isinstance(_group_match, str):
                    raw_buff = _group_match
        for old, new in self._h.escape_replacements() or []:
            raw_buff = regex.sub(old, new, raw_buff)
        return raw_buff

    def raw_normalized(self, casefold: bool = True) -> str:
        # Raw node: normalize then apply the dialect fold (``RawSegment``).
        # Container: join children (mirrors ``BaseSegment.raw_normalized``).
        if self._h.is_raw():
            raw_buff = self.normalize()
            fold = self._h.casefold()
            if fold and casefold:
                if fold == "upper":
                    raw_buff = raw_buff.upper()
                elif fold == "lower":
                    raw_buff = raw_buff.lower()
            return raw_buff
        return "".join(s.raw_normalized(casefold) for s in self.get_raw_segments())

    def raw_trimmed(self) -> str:
        # Mirrors ``RawSegment.raw_trimmed``: strip ``trim_start`` prefixes,
        # then ``trim_chars`` from both ends.
        raw_buff = self._h.raw
        trim_start = self._h.trim_start()
        if trim_start:
            for seq in trim_start:
                if raw_buff.startswith(seq):
                    raw_buff = raw_buff[len(seq) :]
        trim_chars = self._h.trim_chars()
        if trim_chars:
            raw_buff = self._h.raw
            for seq in trim_chars:
                while raw_buff.startswith(seq):
                    raw_buff = raw_buff[len(seq) :]
                while raw_buff.endswith(seq):
                    raw_buff = raw_buff[: -len(seq)]
            return raw_buff
        return raw_buff

    @property
    def type(self) -> str:
        # Native `BaseSegment.type` is the concrete class's `type` attribute
        # (the class-level type), NOT the instance override.  `get_type()` below
        # returns the instance type (`self._h.type`).
        return self._h.class_type()

    def get_type(self) -> str:
        return self._h.type

    def is_type(self, *seg_type: str) -> bool:
        # Membership in cached class_types — verified equivalent to the arena's
        # is_type (class_types already includes the structural hierarchy).
        ct = self._ct
        if ct is None:
            ct = self._ct = frozenset(self._h.class_types())
        if len(seg_type) == 1:  # hot path — most callers pass one type
            return seg_type[0] in ct
        return not ct.isdisjoint(seg_type)

    @property
    def class_types(self) -> frozenset[str]:
        ct = self._ct
        if ct is None:
            ct = self._ct = frozenset(self._h.class_types())
        return ct

    @property
    def instance_types(self) -> tuple[str, ...]:
        return tuple(self._h.instance_types())

    @property
    def descendant_type_set(self) -> frozenset[str]:
        # Subtree-derived (aggregates over all descendants) and hit heavily by the
        # crawler's subtree-pruning, so cache it on the wrapper. Goes stale on
        # mutation → cleared by ``_sweep_wrapper_caches`` like ``_segments``.
        dts = self._dts
        if dts is None:
            dts = self._dts = frozenset(self._h.descendant_type_set())
        return dts

    @property
    def direct_descendant_type_set(self) -> set[str]:
        # Union of the class_types of the *direct* children (BaseSegment parity).
        result: set[str] = set()
        for seg in self.segments:
            result.update(seg.class_types)
        return result

    @property
    def can_start_end_non_code(self) -> bool:
        # Class attribute in BaseSegment; only FileSegment + UnparsableSegment
        # set it True.
        return self.is_type("file", "unparsable")

    @property
    def source_fixes(self) -> list[Any]:
        # Subtree aggregate in document order (mirrors BaseSegment.source_fixes).
        # Empty straight after a parse; populated once fix edit segments carrying
        # SourceFixes are ingested into the arena by the mutation path.
        raw_fixes = self._h.source_fixes()
        if not raw_fixes:
            return []
        from sqlfluff.core.parser.segments.base import SourceFix

        return [
            SourceFix(edit, slice(s0, s1), slice(t0, t1))
            for (edit, (s0, s1), (t0, t1)) in raw_fixes
        ]

    @property
    def is_code(self) -> bool:
        return self._h.is_code

    @property
    def is_meta(self) -> bool:
        return self._h.is_meta

    @property
    def is_whitespace(self) -> bool:
        return self._h.is_whitespace

    @property
    def is_comment(self) -> bool:
        return self._h.is_comment

    def is_raw(self) -> bool:
        return self._h.is_raw()

    @property
    def is_templated(self) -> bool:
        return self._h.is_templated

    @property
    def indent_val(self) -> int:
        t = self._h.type
        return 1 if t == "indent" else (-1 if t == "dedent" else 0)

    @property
    def is_implicit(self) -> bool:
        return bool(self._h.is_implicit())

    @property
    def pos_marker(self) -> Any:
        return self._h.pos_marker

    def get_start_loc(self) -> tuple[int, int]:
        return self._h.pos_marker.working_loc

    def get_end_loc(self) -> tuple[int, int]:
        pm = self._h.pos_marker
        return pm.working_loc_after(self._h.raw)

    # -- navigation ----------------------------------------------------------
    @property
    def segments(self) -> tuple[RsSegment, ...]:
        if self._segments is None:
            self._segments = tuple(RsSegment(c) for c in self._h.children)
        return self._segments

    @property
    def raw_segments(self) -> list[RsSegment]:
        return [RsSegment(x) for x in self._h.raw_segments()]

    def get_raw_segments(self) -> list[RsSegment]:
        return self.raw_segments

    def count_segments(self, raw_only: bool = False) -> int:
        # Mirrors ``BaseSegment.count_segments`` (used by ``LintedDir.add``
        # for the per-file ``statistics`` record).
        if self.segments:
            self_count = 0 if raw_only else 1
            return self_count + sum(
                seg.count_segments(raw_only=raw_only) for seg in self.segments
            )
        return 1

    def select_children(
        self,
        start_seg: Optional["RsSegment"] = None,
        stop_seg: Optional["RsSegment"] = None,
        select_if: Any = None,
        loop_while: Any = None,
    ) -> list[RsSegment]:
        # Port of BaseSegment.select_children; index() relies on __eq__ (uuid)
        # + interning, so start/stop segments match by node identity.
        segs = self.segments
        start_index = segs.index(start_seg) if start_seg else -1
        stop_index = segs.index(stop_seg) if stop_seg else len(segs)
        buff = []
        for seg in segs[start_index + 1 : stop_index]:
            if loop_while and not loop_while(seg):
                break
            if not select_if or select_if(seg):
                buff.append(seg)
        return buff

    @property
    def raw_segments_with_ancestors(
        self,
    ) -> list[tuple[RsSegment, list[Any]]]:
        # Reflow hot path (DepthMap). Use the bulk arena traversal — one FFI call
        # returning every leaf with its full path — instead of a path_to() FFI per
        # leaf, and cache it (the arena is immutable, so successive reflow rules on
        # the same root reuse it).
        cached = self._rwa
        if cached is not None:
            return cached
        from sqlfluff.core.parser.segments.base import PathStep

        out: list[tuple[RsSegment, list[Any]]] = [
            (
                RsSegment(leaf_h),
                [
                    PathStep(RsSegment(h), idx, ln, tuple(cidx))  # type: ignore[arg-type]
                    for (h, idx, ln, cidx) in steps
                ],
            )
            for (leaf_h, steps) in self._h.raw_segments_with_ancestors()
        ]
        self._rwa = out
        return out

    def reflow_depth_info(self) -> dict[int, Any]:
        # Reflow DepthMap fast path: build the {leaf_uuid: DepthInfo} map wholly
        # from arena-side scalars (no PathStep/PyHandle marshalling). The arena
        # emits, per leaf, its top-down stack of (anc_uuid, idx, len, stack_pos)
        # plus the deduped (anc_uuid, class_types); we assemble DepthInfo directly.
        # DepthInfo/StackPosition imported lazily to avoid an import cycle.
        from sqlfluff.utils.reflow.depthmap import DepthInfo, StackPosition

        per_leaf, anc_cts = self._h.reflow_depth_info()
        ct_map = {u: frozenset(ct) for u, ct in anc_cts}
        out: dict[int, Any] = {}
        for leaf_uuid, steps in per_leaf:
            # Mirror native `stack_hashes = tuple(hash(ps.segment) for ...)`:
            # RsSegment.__hash__ returns the node uuid, and Python's hash() then
            # reduces that u128 (Mersenne modulus) — so hash(au) is byte-identical
            # to hash(RsSegment) for the same node.
            hashes = tuple(hash(au) for (au, i, ln, sp) in steps)
            out[leaf_uuid] = DepthInfo(
                stack_depth=len(steps),
                stack_hashes=hashes,
                stack_hash_set=frozenset(hashes),
                stack_class_types=tuple(ct_map[au] for (au, i, ln, sp) in steps),
                stack_positions={
                    hashes[k]: StackPosition(i, ln, sp)
                    for k, (au, i, ln, sp) in enumerate(steps)
                },
            )
        return out

    def get_parent(self) -> Optional[tuple[RsSegment, int]]:
        gp = self._h.get_parent()
        return (RsSegment(gp[0]), gp[1]) if gp else None

    def get_child(self, *seg_type: Any) -> Optional[RsSegment]:
        r = self._h.get_child([_typename(t) for t in seg_type])
        return RsSegment(r) if r is not None else None

    def get_children(self, *seg_type: Any) -> list[RsSegment]:
        return [
            RsSegment(x) for x in self._h.get_children([_typename(t) for t in seg_type])
        ]

    def get_identifier(self) -> Any:
        # Port of CTEDefinitionSegment.get_identifier: blindly the first
        # identifier child (the CTE grammar guarantees one).
        return self.get_child("identifier")

    @property
    def source_str(self) -> str:
        # TemplateSegment.source_str is a STORED attribute on the placeholder —
        # prefer the arena's stored value (correct even after a fix edits the
        # placeholder's source). Fall back to the pos-marker-derived source
        # slice for non-Template nodes (native only defines source_str on
        # TemplateSegment, but rules probe it via getattr).
        stored = self._h.source_str()
        if stored is not None:
            return stored
        pm = self.pos_marker
        return pm.source_str() if pm is not None else ""

    def path_to(self, other: "RsSegment") -> list[Any]:
        from sqlfluff.core.parser.segments.base import PathStep

        # `other` may be a freshly-constructed segment (e.g. a reflow-created
        # WhitespaceSegment) that isn't in the arena — it has no `_h`. Native
        # path_to returns [] when `other` isn't found under self; match that
        # rather than crashing.
        if not isinstance(other, RsSegment):
            return []
        return [
            PathStep(RsSegment(h), idx, ln, tuple(cidx))  # type: ignore[arg-type]
            for (h, idx, ln, cidx) in self._h.path_to(other._h)
        ]

    def recursive_crawl(
        self,
        *seg_type: str,
        recurse_into: bool = True,
        no_recursive_seg_type: Any = None,
        allow_self: bool = True,
    ) -> Iterator[RsSegment]:
        # Return an iterator (not a list) to match BaseSegment.recursive_crawl,
        # so callers can `next(...)` on it (e.g. get_alias).
        if isinstance(no_recursive_seg_type, str):
            nr = [no_recursive_seg_type]
        else:
            nr = list(no_recursive_seg_type) if no_recursive_seg_type else []
        return iter(
            [
                RsSegment(x)
                for x in self._h.recursive_crawl(
                    list(seg_type), recurse_into, nr, allow_self
                )
            ]
        )

    def recursive_crawl_all(self, reverse: bool = False) -> Iterator[RsSegment]:
        segs = [RsSegment(x) for x in self._h.recursive_crawl_all()]
        return iter(reversed(segs)) if reverse else iter(segs)

    def get_alias(self) -> Any:
        # Port of SelectClauseElementSegment.get_alias (dialect_ansi.py):
        # navigation-only, so it works over the façade. Returns ColumnAliasInfo.
        from sqlfluff.core.dialects.common import ColumnAliasInfo

        alias_expression_segment = next(
            self.recursive_crawl(
                "alias_expression", no_recursive_seg_type="select_statement"
            ),
            None,
        )
        if alias_expression_segment is None:
            return None
        alias_identifier_segment = next(
            (s for s in alias_expression_segment.segments if s.is_type("identifier")),
            None,
        )
        if alias_identifier_segment is None:
            return None
        aliased_segment = next(
            s
            for s in self.segments
            if not s.is_whitespace and not s.is_meta and s != alias_expression_segment
        )
        column_reference_segments = []
        if aliased_segment.is_type("column_reference"):
            column_reference_segments.append(aliased_segment)
        else:
            column_reference_segments.extend(
                aliased_segment.recursive_crawl("column_reference")
            )
        # RsSegment duck-types BaseSegment; cast for the typed NamedTuple.
        return ColumnAliasInfo(
            alias_identifier_name=alias_identifier_segment.raw,
            aliased_segment=cast(Any, aliased_segment),
            column_reference_segments=cast(Any, column_reference_segments),
        )

    def iter_segments(
        self, expanding: Any = None, pass_through: bool = False
    ) -> Iterator["RsSegment"]:
        # Faithful port of BaseSegment.iter_segments: expand children whose type
        # is in `expanding` (e.g. recurse into bracketed to reach a nested
        # SELECT), carrying `expanding` deeper only when pass_through is set.
        for s in self.segments:
            if expanding and s.is_type(*expanding):
                yield from s.iter_segments(
                    expanding=expanding if pass_through else None
                )
            else:
                yield s

    # -- fix support ---------------------------------------------------------
    def set_parent(self, parent: Any, idx: int) -> None:
        # No-op: the arena already encodes parentage (get_parent reads it) and a
        # façade node's parent is fixed. Native uses this to wire up freshly
        # constructed fix segments — irrelevant to detection over the arena.
        pass

    def copy(
        self,
        segments: Optional[tuple[Any, ...]] = None,
        parent: Optional[Any] = None,
        parent_idx: Optional[int] = None,
        preserve_uuid: bool = True,
    ) -> Any:
        """Materialise a real Python ``BaseSegment`` tree from this arena subtree.

        Mirrors :meth:`BaseSegment.copy`: recurses to build child copies (unless
        ``segments`` is supplied), keeps this node's ``pos_marker``, and honours
        ``parent``/``parent_idx``. The arena itself is never mutated; this returns
        a freestanding real segment tree that rules can safely hand to fixes.

        Class identity is reconstructed via a synthetic segment class whose
        ``type``/``class_types`` exactly match the arena node (see
        :func:`_synth_segment_class`), so ``is_type`` and ``recursive_crawl_all``
        line up 1:1 with the façade original (as ``ST05.SegmentCloneMap`` relies
        on) while every leaf carries the right ``raw``/``pos_marker`` and each
        node's ``pos_marker`` remains assignable.

        ``preserve_uuid`` defaults to True because native ``BaseSegment.copy``
        ALWAYS keeps the uuid (the ``__dict__`` transfer carries it) — and fix
        application depends on it: a rule may anchor one fix on a live tree
        segment that another fix's replacement CLONE contains (e.g. ST05's
        space-after-FROM ``create_after`` inside the CTE it also rewrites);
        the anchor is found inside the spliced clone by uuid.
        """
        from sqlfluff.core.helpers.identity import get_next_id

        h = self._h
        if h.is_meta:
            # Metas must clone as REAL meta segments (like native ``copy``,
            # where an ``Indent`` clone IS an ``Indent``). A synthetic RAW
            # typed "indent" loses ``is_meta`` — and once such a clone rides a
            # fix edit into the arena, grammar re-validation sees a
            # zero-length "code" leaf and wrongly rejects later batches (CV07
            # triple-nested bracket unwrap stalling at the third pass).
            import uuid as _uuid

            from sqlfluff.core.parser.segments.meta import (
                Dedent,
                EndOfFile,
                ImplicitIndent,
                Indent,
                TemplateLoop,
                TemplateSegment,
            )

            kind = h.type
            block_uuid = h.block_uuid()
            block_uuid = _uuid.UUID(int=block_uuid) if block_uuid is not None else None
            meta_segment: Any
            if kind == "placeholder":
                meta_segment = TemplateSegment(
                    pos_marker=h.pos_marker,
                    source_str=self.source_str or "",
                    block_type=self.block_type or "",
                    block_uuid=block_uuid,
                )
            elif kind == "template_loop":
                meta_segment = TemplateLoop(
                    pos_marker=h.pos_marker, block_uuid=block_uuid
                )
            elif kind == "end_of_file":
                meta_segment = EndOfFile(pos_marker=h.pos_marker)
            else:
                # Dedent subclasses Indent; classify by class types (mirrors
                # ``_segment_to_spec``).
                if self.is_type("dedent"):
                    meta_cls: type = Dedent
                elif self.is_implicit:
                    meta_cls = ImplicitIndent
                else:
                    meta_cls = Indent
                meta_segment = meta_cls(pos_marker=h.pos_marker, block_uuid=block_uuid)
            if preserve_uuid:
                meta_segment.uuid = self._uid
            if parent is not None:
                assert parent_idx is not None
                meta_segment.set_parent(parent, parent_idx)
            return meta_segment

        is_raw = h.is_raw()
        cls = _synth_segment_class(
            h.segment_class,
            h.type,
            self.class_types,
            is_raw,
            (h.is_code, h.is_comment, h.is_whitespace) if is_raw else None,
        )

        if h.is_raw():
            fold = h.casefold()
            casefold = (
                str.upper if fold == "upper" else str.lower if fold == "lower" else None
            )
            # The arena stores the quoted_value capture group as a string; a
            # numeric index arrives as e.g. "1" and must be an int for
            # ``re.Match.group`` (a numeric string is treated as a group *name*).
            # This mirrors the conversion in ``RsSegment.normalize``.
            quoted_value = h.quoted_value()
            if quoted_value:
                pattern, group = quoted_value
                try:
                    group = int(group)
                except (TypeError, ValueError):
                    pass
                quoted_value = (pattern, group)
            new_segment = cls(
                raw=h.raw,
                pos_marker=h.pos_marker,
                instance_types=tuple(h.instance_types()),
                trim_start=h.trim_start(),
                trim_chars=h.trim_chars(),
                quoted_value=quoted_value,
                escape_replacements=h.escape_replacements(),
                casefold=casefold,
            )
            if preserve_uuid:
                # Keep the arena node's uuid so native ``apply_fixes`` (which
                # matches fixes to segments by ``anchor.uuid``) lines up with the
                # façade fixes — the uuid-bridge for tree-restructuring fixes.
                new_segment.uuid = self._uid
            if parent is not None:
                assert parent_idx is not None
                new_segment.set_parent(parent, parent_idx)
            return new_segment

        # Container node: build via __new__ (like BaseSegment.copy) so we bypass
        # the parse-time validation in __init__ and just transplant state.
        new_segment = cls.__new__(cls)  # type: ignore[call-overload]
        new_segment.pos_marker = h.pos_marker
        new_segment.uuid = self._uid if preserve_uuid else get_next_id()
        if parent is not None:
            assert parent_idx is not None
            new_segment.set_parent(parent, parent_idx)
        if segments is not None:
            new_segment.segments = tuple(segments)
        else:
            new_segment.segments = tuple(
                child.copy(
                    parent=new_segment, parent_idx=idx, preserve_uuid=preserve_uuid
                )
                for idx, child in enumerate(self.segments)
            )
        return new_segment

    def stringify(self, *args: Any, **kwargs: Any) -> str:
        # Display/debug output (e.g. the rule-testing utilities print the
        # tree when an expected failure doesn't materialise, and API users
        # may too). Materialise a native copy and delegate — display-only,
        # so the copy cost is acceptable.
        return self.copy().stringify(*args, **kwargs)

    def edit(
        self,
        raw: Optional[str] = None,
        source_fixes: Any = None,
        source_str: Optional[str] = None,
    ) -> Any:
        """Return a real segment for a fix's replacement text.

        The arena is not mutated; the returned segment only carries the new raw
        (or, for a placeholder edit, the new source_str) + this node's position,
        which is all the source-patch fixer needs. Mirrors ``RawSegment.edit`` and
        ``TemplateSegment.edit``: when ``source_str`` is given we're editing a
        template placeholder, so return a ``TemplateSegment``.
        """
        if source_str is not None or self.is_type("placeholder"):
            # Mirror ``TemplateSegment.edit`` (meta.py:267-300) exactly: keep
            # the existing source_str/block_type/block_uuid when not replaced
            # and MERGE new source fixes with any existing ones. JJ01 edits a
            # placeholder passing ONLY source_fixes — branching on the passed
            # source_str alone sent that through the raw-edit path below,
            # which rebuilt the placeholder as a raw-typed segment: the
            # staged replacement lost block_type ('skipped_source') and its
            # summary source_str, and LT02's next crawl misindented the
            # surrounding jinja block.
            import uuid as _uuid

            from sqlfluff.core.parser.segments.meta import TemplateSegment

            if source_fixes or self.source_fixes:
                sf = list(source_fixes or []) + list(self.source_fixes)
            else:  # pragma: no cover
                sf = None
            block_uuid = self._h.block_uuid()
            return TemplateSegment(
                pos_marker=self.pos_marker,
                source_str=source_str if source_str is not None else self.source_str,
                block_type=self.block_type or "",
                source_fixes=sf,
                block_uuid=(
                    _uuid.UUID(int=block_uuid) if block_uuid is not None else None
                ),
            )
        # Mirror RawSegment.edit: `raw` defaults to the current raw (fixes that
        # only set source_fixes, e.g. JJ01, pass raw=None but must keep the raw)
        # and — crucially — the edited copy keeps the segment's CLASS identity
        # (native returns `self.__class__(...)`). A bare RawSegment would come
        # back typed "raw", so e.g. a reflow-edited whitespace would stop being
        # `whitespace` on the mutated tree and re-trigger spacing rules forever
        # (the v1 source-patch loop masked this by re-lexing on reparse).
        h = self._h
        cls = _synth_segment_class(
            h.segment_class,
            h.type,
            self.class_types,
            True,
            (h.is_code, h.is_comment, h.is_whitespace),
        )
        seg = cls(
            raw=raw if raw is not None else self.raw,
            pos_marker=self.pos_marker,
            instance_types=tuple(h.instance_types()),
            source_fixes=source_fixes,
        )
        return seg

    def __getattr__(self, name: str) -> Any:
        # Only fires for BaseSegment API the façade doesn't implement yet. Raising
        # keeps behaviour honest — such rules aren't in FACADE_SAFE_RULES.
        raise AttributeError(
            f"RsSegment (arena façade) does not implement {name!r}; "
            "this rule is not façade-safe yet."
        )

    def __repr__(self) -> str:
        return f"RsSegment({self._h!r})"


def facade_ignore_mask(
    root: "RsSegment", config: Any, reference_map: dict[str, set[str]]
) -> tuple[Any, list[Any]]:
    """Build native's ``IgnoreMask`` from a façade tree.

    Mirrors ``Linter.lint_fix_parsed`` (linter.py:490-499) exactly:
    ``disable_noqa`` (unless ``disable_noqa_except`` is set) suppresses the
    mask entirely; otherwise ``IgnoreMask.from_tree`` scans the tree's comment
    segments — the façade wrappers duck-type everything it reads
    (``recursive_crawl``/``is_type``/``raw_trimmed``/``source_position``).

    Returns ``(mask_or_None, malformed_noqa_violations)`` — the violations are
    native ``SQLParseError``s that the caller must report like native's
    ``initial_linting_errors`` additions. ``reference_map`` is the rule pack's
    (``rule_pack.reference_map``), so rule aliases/groups resolve identically.
    """
    from sqlfluff.core.linter import Linter as _Linter
    from sqlfluff.core.rules.noqa import IgnoreMask

    disable_noqa_except = config.get("disable_noqa_except")
    if config.get("disable_noqa") and not disable_noqa_except:
        return None, []
    allowed_rules_ref_map = _Linter.allowed_rule_ref_map(
        reference_map, disable_noqa_except
    )
    return IgnoreMask.from_tree(root, allowed_rules_ref_map)  # type: ignore[arg-type]


def rule_is_facade_safe(rule: Any) -> bool:
    """Whether ``rule`` may run over the arena façade.

    Two ways in: membership of the centrally-vetted core allowlist
    (:data:`FACADE_SAFE_RULES` — byte-parity proven over the corpora), or an
    EXPLICIT opt-in by the rule author via ``BaseRule.rust_compatible`` (the
    plugin contract: the rule asserts it behaves identically over the façade
    wrappers — ``is_type`` navigation, no ``isinstance`` against concrete
    segment classes). Anything else runs on the classic Python pipeline.
    """
    return rule.code in FACADE_SAFE_RULES or bool(
        getattr(rule, "rust_compatible", False)
    )


def facade_unknown_rule_violations(
    source: str,
    fname: str,
    config: Any,
    unknown_rules: list[Any],
    ignore_mask: Any = None,
    rule_timing_sink: Optional[list[tuple[str, str, float]]] = None,
) -> Optional[list[Any]]:
    """Violations for rules NOT façade-safe (see :func:`rule_is_facade_safe`).

    These run on the classic Python pipeline BY DESIGN: a native reference
    parse of the source, crawled natively — correct by construction. (Rules
    can opt out of this cost by declaring ``rust_compatible = True``; without
    the declaration the façade is never crawled — an incompatible rule, e.g.
    one using ``isinstance`` against concrete segment classes, would diverge
    silently rather than crash, so there is nothing safe to "attempt".)

    Returns ``None`` when the native reference parse fails — the caller
    routes the file to native.
    """
    import time

    from sqlfluff.core.linter import Linter as _Linter

    dialect_obj = config.get("dialect_obj")

    # Native reference parse — a python tree, by design. INFO: expected, but
    # visible so per-file python parses are attributable when profiling.
    linter_logger.info(
        "Façade lint: native reference parse of %s for non-rust_compatible rules %s",
        fname,
        [r.code for r in unknown_rules],
    )
    # A bare Linter: the caller's may carry a formatter, and ``parse_string``
    # dispatches output through it.
    parsed = _Linter(config=config).parse_string(source, fname=fname)
    nat_tree = parsed.tree
    if nat_tree is None:
        return None
    nat_variant = parsed.parsed_variants[0] if parsed.parsed_variants else None
    nat_tf = getattr(nat_variant, "templated_file", None)

    out: list[Any] = []
    for rule in unknown_rules:
        t0 = time.monotonic()
        lints, _, _, _ = rule.crawl(
            tree=nat_tree,
            dialect=dialect_obj,
            fix=False,
            templated_file=nat_tf,
            ignore_mask=ignore_mask,
            fname=fname,
            config=config,
        )
        if rule_timing_sink is not None:
            rule_timing_sink.append((rule.code, rule.name, time.monotonic() - t0))
        out.extend(lints)
    return out


def facade_violations(
    source: str,
    fname: str,
    config: Any,
    rules: list[Any],
    rst: Any = None,
    rule_timing_sink: Optional[list[tuple[str, str, float]]] = None,
    ignore_mask: Any = None,
    reference_map: Optional[dict[str, set[str]]] = None,
) -> Optional[list[Any]]:
    """Crawl ``rules`` over the arena façade and return their ``SQLLintError``s.

    Returns ``None`` if the source can't be parsed via the engine (the caller
    should fall back to the Python path).

    ``ignore_mask``, if given, is passed to each rule crawl exactly like
    native (``_process_lint_result`` drops masked results and marks the
    directives used). Build one with :func:`facade_ignore_mask`; the caller
    owns reporting the malformed-noqa violations it returns and storing the
    mask on the ``LintedFile`` (unused-noqa warnings).

    Multi-variant renders: every ALTERNATE variant tree is crawled too and
    the results merge (native lints every variant, linter.py:779-805).
    ``reference_map`` (the rule pack's), when given, builds each alternate's
    own noqa mask — untaken template branches can carry their own
    directives — and their malformed-directive violations join the output.

    ``rst`` may be a tree already parsed from ``source`` (the crawl is read-only,
    so a caller can share one parse across the gate checks, the pre-count and the
    fix loop instead of re-parsing the same source each time).

    ``rule_timing_sink``, if given, collects ``(code, name, seconds)`` per rule
    crawl — the shape of native ``rule_timings`` (linter.py:659-662), used by
    the lint fast path to populate the per-file ``timings`` record.
    """
    import time

    import sqlfluffrs
    from sqlfluff.core.linter.linted_file import LintedFile

    # An empty file is not façade-eligible: the arena's empty ``file`` node
    # carries no pos_marker (native's gets a zero-width one), which some rule
    # crawls assert on (e.g. JJ01) — and native is NOT always violation-free
    # here: under a templater whose zero-byte render keeps a raw slice (e.g.
    # ``raw``), native's lexer emits a zero-width placeholder that LT12 lints
    # and fixes to a single newline. Fall back to native rather than report
    # a clean result the native path may not produce.
    if not source:
        return None
    if rst is None:
        rst = sqlfluffrs.engine_parse_to_tree(source, fname, config, None, True)
    if rst is None:
        return None
    dialect_obj = config.get("dialect_obj")
    root = RsSegment(rst.root)
    # The engine's TemplatedFile — required by rules that read raw_slices /
    # source_str (e.g. CV10) and for correct source-position mapping.
    templated_file = rst.templated_file
    out: list[Any] = []
    # The same per-rule progress bar native shows while crawling
    # (linter.py:536-541/556). Read the configuration through the linter
    # module at call time — that's the reference native's bars use (and the
    # one tests patch), so enable/disable behaviour stays uniform.
    from tqdm import tqdm

    import sqlfluff.core.linter.linter as _linter_module

    progress_bar_crawler = tqdm(
        rules,
        desc="lint by rules",
        leave=False,
        disable=_linter_module.progress_bar_configuration.disable_progress_bar,  # type: ignore[attr-defined]  # noqa: E501
    )
    for rule in progress_bar_crawler:
        progress_bar_crawler.set_description(f"rule {rule.code}")
        t0 = time.monotonic()
        lints, _, _, _ = rule.crawl(
            tree=root,
            dialect=dialect_obj,
            fix=False,
            templated_file=templated_file,
            ignore_mask=ignore_mask,
            fname=fname,
            config=config,
        )
        if rule_timing_sink is not None:
            rule_timing_sink.append((rule.code, rule.name, time.monotonic() - t0))
        out.extend(lints)
    # ALTERNATE variants (native lints every variant and merges the results
    # — linter.py:779-805): crawl each alternate tree with its own noqa mask
    # and templated file, appending after the root's results so the shared
    # templated-area filter + source-space dedupe below merge them exactly
    # like native's LintedFile handling. Native discards alternate timings.
    for alt in getattr(rst, "alternate_trees", []) or []:
        alt_tf = alt.templated_file
        if alt_tf is None:  # pragma: no cover
            continue
        alt_root = RsSegment(alt.root)
        alt_mask = None
        if reference_map is not None:
            alt_mask, alt_ivs = facade_ignore_mask(alt_root, config, reference_map)
            out.extend(alt_ivs)
        for rule in rules:
            lints, _, _, _ = rule.crawl(
                tree=alt_root,
                dialect=dialect_obj,
                fix=False,
                templated_file=alt_tf,
                ignore_mask=alt_mask,
                fname=fname,
                config=config,
            )
            out.extend(lints)
    # Native filters out lint errors anchored in templated (non-literal)
    # sections unless the anchor is semantically literal or the rule targets
    # templated code (``Linter.remove_templated_errors``, applied in
    # ``lint_fix_parsed`` gated on ``ignore_templated_areas``). The façade
    # markers carry ``is_literal`` and the Rust raw-slice objects duck-type
    # the accessors it uses, so reuse it verbatim. No-op for literal sources
    # (every marker is literal).
    if config.get("ignore_templated_areas", default=True):
        from sqlfluff.core.linter import Linter as _Linter

        out = _Linter.remove_templated_errors(out)
    # Native passes every file's violations through
    # ``deduplicate_in_source_space`` (linter.py:848): dedupe on
    # ``source_signature()`` + sort by position. Without it a rule that
    # legitimately emits the same result twice diverges — e.g. AL04 anchors one
    # duplicate-alias result per SELECT-clause subquery on the SAME parent
    # alias segment, which native collapses to one.
    return LintedFile.deduplicate_in_source_space(out)


def _segment_to_spec(seg: Any) -> tuple[Any, ...]:
    """Convert a fix *edit* segment into the arena NodeSpec tuple.

    Layout (matching ``extract_node_spec`` in arena_py.rs)::

        (tag, uuid, segment_class, segment_type, class_type, raw,
         instance_types, class_types, kwargs, meta, source_fixes, children,
         marker)

    Works for real dialect segments, ``RawSegment``s from ``RsSegment.edit``,
    and the synthetic classes from ``RsSegment.copy``.

    ``marker`` carries the segment's ``pos_marker`` (or ``None``). ``LintFix``
    strips markers from the TOP-LEVEL edit segments, but descendants keep
    theirs (e.g. ST05 nests clones of real tree segments inside a new CTE) and
    native ``apply_fixes`` splices them in as-is — rules then read those
    positions off the mutated tree (ST05's ``_is_child`` CTE ordering), so
    the arena must preserve them identically.
    """
    seg_type = seg.get_type()
    cls_type = getattr(type(seg), "type", None) or seg_type
    src_fixes = [
        (
            sf.edit,
            (sf.source_slice.start, sf.source_slice.stop),
            (sf.templated_slice.start, sf.templated_slice.stop),
        )
        for sf in (getattr(seg, "source_fixes", None) or [])
    ]
    class_types = sorted(seg.class_types)
    pm = seg.pos_marker
    marker = (
        (
            pm.source_slice.start,
            pm.source_slice.stop,
            pm.templated_slice.start,
            pm.templated_slice.stop,
            pm.working_line_no,
            pm.working_line_pos,
        )
        if pm is not None
        else None
    )
    if seg.is_meta:
        kind = seg_type
        if kind not in (
            "indent",
            "dedent",
            "placeholder",
            "template_loop",
            "end_of_file",
        ):
            # Dedent subclasses Indent; classify by class types.
            kind = "dedent" if seg.is_type("dedent") else "indent"
        block_uuid = getattr(seg, "block_uuid", None)
        meta = (
            kind,
            getattr(seg, "source_str", None) if kind == "placeholder" else None,
            getattr(seg, "block_type", None) if kind == "placeholder" else None,
            bool(getattr(seg, "is_implicit", False)),
            block_uuid.int if block_uuid is not None else None,
        )
        return (
            "meta",
            seg.uuid,
            type(seg).__name__,
            seg_type,
            cls_type,
            "",
            [],
            class_types,
            None,
            meta,
            src_fixes,
            [],
            marker,
        )
    if not seg.segments:
        # Raw leaf.
        fold = getattr(seg, "casefold", None)
        casefold = (
            "upper" if fold is str.upper else "lower" if fold is str.lower else None
        )
        qv = getattr(seg, "quoted_value", None)
        kwargs = (
            list(seg.trim_chars) if getattr(seg, "trim_chars", None) else None,
            list(seg.trim_start) if getattr(seg, "trim_start", None) else None,
            (qv[0], str(qv[1])) if qv else None,
            [tuple(e) for e in getattr(seg, "escape_replacements", None) or []] or None,
            casefold,
        )
        return (
            "raw",
            seg.uuid,
            type(seg).__name__,
            seg_type,
            cls_type,
            seg.raw,
            # ORDER MATTERS: instance_types[0] is the segment's primary type
            # (what ``get_type()`` reports). Sorting here flipped e.g. a
            # ``numeric_literal`` leaf to ``literal`` on the SECOND
            # clone→ingest generation ('l' < 'n'), after which grammar
            # re-validation's ``TypedParser("numeric_literal")`` re-match
            # failed and multi-pass fixes (CV07 triple-nested brackets) were
            # wrongly rejected.
            list(getattr(seg, "instance_types", ()) or ()),
            class_types,
            kwargs,
            None,
            src_fixes,
            [],
            marker,
        )
    return (
        "segment",
        seg.uuid,
        type(seg).__name__,
        seg_type,
        cls_type,
        None,
        [],
        class_types,
        None,
        None,
        src_fixes,
        [_segment_to_spec(c) for c in seg.segments],
        marker,
    )


def _anchor_info_to_ops(anchor_info: Any) -> list[tuple[int, str, list[Any]]]:
    """Convert native ``compute_anchor_edit_info`` output to arena EditOps."""
    ops: list[tuple[int, str, list[Any]]] = []
    for uuid, info in anchor_info.items():
        for fx in info.fixes:
            ops.append(
                (
                    uuid,
                    fx.edit_type,
                    [_segment_to_spec(e) for e in (fx.edit or [])],
                )
            )
    return ops


def _sweep_wrapper_caches() -> None:
    """Invalidate interned ``RsSegment`` caches after an arena commit.

    ``_segments`` (children tuple), ``_dts`` (descendant_type_set) and ``_rwa``
    (raw_segments_with_ancestors) are subtree-derived and go stale on mutation;
    ``_ct``/``_uid`` stay — a surviving node never changes kind or uuid in place
    (replace creates new nodes).  An explicit sweep at the single mutation point
    beats per-access epoch checks, which would tax the hot crawl path.
    """
    for seg in list(_INTERN.values()):
        seg._segments = None
        seg._dts = None
        seg._rwa = None


def facade_fix_loop(
    source: str,
    fname: str,
    config: Any,
    rules: list[Any],
    limit: int,
    rst: Any = None,
    lint_sink: Optional[list[Any]] = None,
    loop_state: Optional[dict[str, Any]] = None,
    ignore_mask: Any = None,
    reference_map: Optional[dict[str, set[str]]] = None,
    patch_sink: Optional[list[Any]] = None,
    rule_timing_sink: Optional[list[tuple[str, str, float]]] = None,
) -> str:
    """Iteratively fix ``source`` by MUTATING the arena (no reparse).

    (Formerly ``facade_fix_loop_v3``; the legacy v1 source-patch +
    re-parse loop and its ``SQLFLUFF_RS_FIX_V1`` escape hatch are retired —
    this is the only implementation.)

    Mirrors ``Linter.lint_fix_parsed`` (linter.py:457-660): parse once; per
    rule crawl the same (mutated) façade tree, stage the fix batch on the
    arena, gate the commit on native's loop-protections — the
    ``(raw, source_fixes)`` version set AND the consecutive-identical-fixes
    check — then reconstruct the fixed source with native patch generation
    over the mutated façade.

    ``lint_sink``, if given, collects the lint results from the FIRST pass —
    exactly native's ``initial_linting_errors`` (linter.py:532-535/573-574:
    all rules, crawled in fix mode over the progressively-mutated tree). This
    lets callers get the pre-fix violation set from the crawl the loop already
    does, instead of paying a separate whole-ruleset sweep. Only valid when
    the loop actually ran: the caller must have gated on a parseable tree
    (the early bails below leave the sink empty).

    ``loop_state``, if given, is set with ``{"runaway": True}`` when a phase
    exhausted its loop limit and the ORIGINAL source was returned. Native's
    bookkeeping for that case differs from ordinary gave-up fixes (it strips
    the fixes from every reported violation, making them all unfixable —
    linter.py:683-693), so callers must not treat the result as final.

    ``ignore_mask`` (see :func:`facade_ignore_mask`) drops masked results
    and their fixes inside the crawls, exactly like native.

    Multi-variant renders run the SAME loop over every variant tree
    (native ``lint_parsed`` runs ``lint_fix_parsed`` per variant,
    linter.py:779-805) and the per-variant patches merge via
    ``merge_source_patches``. ``reference_map`` (the rule pack's), when
    given, builds each ALTERNATE variant's own noqa mask — untaken
    template branches can carry their own directives; the root's mask
    arrives via ``ignore_mask``. A runaway on ANY variant defers the whole
    file (native reverts just that variant; deferral reproduces its output
    exactly via the native pipeline).
    """
    import time

    import sqlfluffrs
    from sqlfluff.core.linter.fix import compute_anchor_edit_info
    from sqlfluff.core.linter.linted_file import LintedFile
    from sqlfluff.core.linter.patch import generate_source_patches

    # Crash guard for direct callers: the Rust engine's empty ``file`` node
    # carries no pos_marker (native's FileSegment gets a zero-width one), so
    # the reconstruction pass (``generate_source_patches``) would assert on
    # it. NOTE: this does NOT always match native — under a templater whose
    # zero-byte render keeps a raw slice (e.g. ``raw``), native lexes a
    # zero-width placeholder that LT12 fixes to a single newline. The CLI
    # gates therefore route empty sources to native before reaching here.
    if not source:
        return source

    dialect_obj = config.get("dialect_obj")
    dialect_name = config.get("dialect")
    # ``rst`` may be a fresh (unmutated) tree already parsed from ``source`` by
    # the caller — reuse it rather than re-parsing the same source. The loop
    # mutates it in place, so it must not be crawled again by the caller after.
    if rst is None:
        rst = sqlfluffrs.engine_parse_to_tree(source, fname, config, None, True)
    if rst is None:
        return source
    tf = rst.templated_file
    if tf is None:
        return source
    feu = bool(config.get("fix_even_unparsable"))

    def _fix_tree(rst_v: Any, tf_v: Any, mask_v: Any, timing_sink: Any = None) -> bool:
        """Run the phase loop over one variant tree; True = runaway."""
        root = RsSegment(rst_v.root)
        root_handle = rst_v.root

        def current_version() -> tuple[str, tuple[Any, ...]]:
            return (root.raw, tuple(root_handle.source_fixes()))

        previous_versions: set[tuple[str, tuple[Any, ...]]] = {current_version()}
        last_fixes: Any = None
        by_phase = {
            "main": [r for r in rules if r.lint_phase == "main"],
            "post": [r for r in rules if r.lint_phase == "post"],
        }

        for phase in ("main", "post"):
            rules_this_phase = by_phase[phase]
            nloops = limit if phase == "main" else 2
            for loop in range(nloops):
                first_pass = phase == "main" and loop == 0
                if first_pass:
                    # Native reassigns to ALL rules on the first pass "to
                    # compute initial_linting_errors correctly"
                    # (linter.py:532-536) — and that assignment LEAKS into
                    # every later loop of the MAIN phase, so post-phase rules
                    # keep crawling during main loops (the docstring above it
                    # says otherwise; the code wins). Mirrored deliberately:
                    # e.g. AL09/CP02/RF06 need CP02's main-loop applications
                    # for AL09 to see the aliases they make redundant.
                    rules_this_phase = rules
                changed = False
                for rule in rules_this_phase:
                    # Native skips non-fix-compatible rules after the first
                    # pass (linter.py:545-553): their added results aren't
                    # reported to the user anyway.
                    if not first_pass and not rule.is_fix_compatible:
                        continue
                    _rt0 = time.monotonic()
                    _v, _r, fixes, _m = rule.crawl(
                        tree=root,
                        dialect=dialect_obj,
                        fix=True,
                        templated_file=tf_v,
                        # Masked results (and their fixes) are dropped inside
                        # the crawl (_process_lint_result), exactly like
                        # native.
                        ignore_mask=mask_v,
                        fname=fname,
                        config=config,
                    )
                    if timing_sink is not None:
                        # Native records EVERY crawl, every loop
                        # (linter.py:659-662).
                        timing_sink.append(
                            (rule.code, rule.name, time.monotonic() - _rt0)
                        )
                    if lint_sink is not None and first_pass:
                        # Native's ``initial_linting_errors``: only the first
                        # pass of the main phase reports (linter.py:573-574).
                        # Alternates report too (violations merge,
                        # linter.py:798).
                        lint_sink.extend(_v)
                    if not fixes:
                        continue
                    anchor_info = compute_anchor_edit_info(fixes)
                    if any(not info.is_valid for info in anchor_info.values()):
                        continue  # conflicting fixes on one anchor
                    if fixes == last_fixes:
                        # Same fixes twice in a row -> we're looping; stop
                        # applying (native linter.py:597-608).
                        continue
                    last_fixes = fixes
                    ops = _anchor_info_to_ops(anchor_info)
                    (
                        staged_raw,
                        staged_sfx,
                        _applied,
                        _unapplied,
                        _reverted,
                        st_changed,
                    ) = rst_v.stage_edit_batch(ops, feu)
                    staged_version = (staged_raw, tuple(staged_sfx))
                    if not st_changed or staged_version == current_version():
                        rst_v.discard_staged()
                        continue
                    # Native ``apply_fixes`` grammar re-validation
                    # (linter.py:637-645): reject a staged batch that would
                    # produce an unparsable file, leave the tree untouched,
                    # and warn on the same channel/text. Ordered before the
                    # previous-versions check, like native, so a batch that is
                    # both invalid and version-revisiting warns the same way
                    # on both engines.
                    if not rst_v.validate_staged(
                        dialect_name,
                        int(config.get("max_parse_depth") or 0),
                        int(config.get("max_parse_nodes") or 0),
                    ):
                        rst_v.discard_staged()
                        linter_logger.warning(
                            "Fixes for %s not applied, as it would result in "
                            "an unparsable file. Please report this as a bug "
                            "with a minimal query which demonstrates this "
                            "warning.",
                            rule.code,
                        )
                        continue
                    if staged_version in previous_versions:
                        # Applying these fixes would take us back to a state
                        # we've seen before -> we're in a loop; don't apply
                        # (native linter.py:653-657 + ``_warn_unfixable``).
                        rst_v.discard_staged()
                        linter_logger.warning(
                            "One fix for %s not applied, it would re-cause "
                            "the same error.",
                            rule.code,
                        )
                        continue
                    rst_v.commit_staged()
                    _sweep_wrapper_caches()
                    previous_versions.add(staged_version)
                    changed = True
                if not changed:
                    break
            else:
                # The phase hit its loop limit while fixes were still being
                # applied — one or more rules aren't converging. Native
                # (linter.py:673-699) warns and reverts that variant's tree;
                # the ``loop_state`` signal makes the CLI fast path defer the
                # file to the native fixer (byte- and exit-code parity).
                linter_logger.warning("Loop limit on fixes reached [%s].", limit)
                return True
        return False

    # The work-list: the root variant, then every ALTERNATE variant tree
    # (native lints/fixes every variant and merges — linter.py:779-805).
    # Alternates build their own noqa masks (untaken template branches can
    # carry their own directives); their malformed-directive violations
    # report like native's per-variant initial_linting_errors.
    trees: list[tuple[Any, Any, Any]] = [(rst, tf, ignore_mask)]
    for alt in getattr(rst, "alternate_trees", []) or []:
        alt_tf = alt.templated_file
        if alt_tf is None:  # pragma: no cover
            continue
        alt_mask = None
        if reference_map is not None:
            alt_mask, alt_ivs = facade_ignore_mask(
                RsSegment(alt.root), config, reference_map
            )
            if lint_sink is not None:
                lint_sink.extend(alt_ivs)
        trees.append((alt, alt_tf, alt_mask))

    patch_sets: list[list[Any]] = []
    for _idx, (rst_v, tf_v, mask_v) in enumerate(trees):
        # Only the ROOT variant reports rule timings (native discards
        # alternates' — linter.py:786-798 "_,  # Timings").
        if _fix_tree(rst_v, tf_v, mask_v, rule_timing_sink if _idx == 0 else None):
            if loop_state is not None:
                loop_state["runaway"] = True
            return source
        patch_sets.append(
            generate_source_patches(RsSegment(rst_v.root), tf_v)  # type: ignore[arg-type]
        )

    # Native's post-loop filter on ``initial_linting_errors`` (linter.py:701):
    # drop templated-anchored violations from reporting. Native does NOT apply
    # this on the runaway path (its early return precedes the filter), and
    # neither do we — runaway defers to native anyway.
    if lint_sink is not None and config.get("ignore_templated_areas", default=True):
        from sqlfluff.core.linter import Linter as _Linter

        lint_sink[:] = _Linter.remove_templated_errors(lint_sink)

    # Reconstruction: native patch generation over the mutated façade(s).
    # Native routes patches through ``merge_source_patches`` even for a
    # single variant (linter.py:770-808 -> LintedFile.source_patches): it
    # sorts in source space, dedupes ACROSS VARIANTS, and drops
    # same-position conflicting patches. All variants share one source, so
    # the root's TemplatedFile drives the slicing.
    from sqlfluff.core.linter.patch import merge_source_patches

    patches = merge_source_patches(patch_sets)
    if patch_sink is not None:
        # For callers building a ``LintedFile`` (the API fast path): the
        # merged patches are what native stores as ``source_patches`` for
        # ``fix_string`` to apply.
        patch_sink[:] = patches
    source_only = tf.source_only_slices()
    slices = LintedFile._slice_source_file_using_patches(
        patches, source_only, tf.source_str
    )
    return LintedFile._build_up_fixed_source_string(slices, patches, tf.source_str)


def engine_enabled(config: Any) -> bool:
    """Whether ``core.use_rust_engine`` permits the engine (and it imports).

    Mirrors the CLI's ``_use_rust_engine`` minus the output-format concern:
    explicit False disables; anything else (``auto``/True) requires the
    extension to import with the engine entrypoints present.
    """
    val = config.get("use_rust_engine")
    if val in (False, "False", "false", 0, "0"):
        return False
    try:
        import sqlfluffrs

        return hasattr(sqlfluffrs, "engine_parse_to_tree")
    except ImportError:  # pragma: no cover
        return False


def facade_linted_file(
    raw_file: str,
    fname: str,
    cfg: Any,
    linter: Any,
    encoding: str = "utf-8",
    rule_pack: Any = None,
) -> Optional[Any]:
    """Lint one source over the arena façade into a ``LintedFile``, or ``None``.

    The per-file core of the lint fast path (shared by the CLI gates and
    ``Linter.lint_string``): eligibility routing (engine enabled, parses via
    the engine with no unparsable section, violation-free render), noqa
    masks, the safe/unknown rule crawl split, and native's violation
    post-processing. Returns a real ``LintedFile`` so every downstream
    consumer (formatters, ``as_records``, stats, exit codes, the API)
    behaves natively — NOTE its ``tree`` is the façade root (a duck-typed
    ``RsSegment``), not a native ``BaseSegment``.
    """
    import time

    import sqlfluffrs
    from sqlfluff.core.linter import Linter as _Linter
    from sqlfluff.core.linter.linted_file import FileTimings, LintedFile

    if not engine_enabled(cfg):
        return None
    # A custom templater INSTANCE on the linter is authoritative for
    # native rendering (``render_string`` uses ``self.templater``); the
    # engine renders via ``config.get_templater()`` and would silently use
    # the CONFIGURED templater instead. Route custom-templater linters to
    # native (e.g. ``Linter(templater=...)`` API usage).
    _custom_templater = getattr(linter, "templater", None)
    if (
        _custom_templater is not None
        and type(_custom_templater) is not cfg.get_templater_class()
    ):
        return None
    if not raw_file:
        # Not façade-eligible: under a templater whose zero-byte render keeps
        # a raw slice (e.g. ``raw``), native lexes a zero-width placeholder
        # that LT12 lints — and the arena's bare ``file`` node also makes the
        # statistics record differ (no end-of-file meta). Native handles it
        # trivially fast.
        return None
    if rule_pack is None:
        rule_pack = linter.get_rulepack(config=cfg)
    rules = list(rule_pack.rules)
    if not rules:
        return None
    # Façade-safe rules (core allowlist or an explicit plugin
    # ``rust_compatible`` opt-in) crawl the façade; anything else runs on
    # the classic Python pipeline via ``facade_unknown_rule_violations``.
    safe_rules = [
        r
        for r in rules
        if rule_is_facade_safe(r) and r.code not in FACADE_SAFE_RULES_DETECTION_UNSAFE
    ]
    unknown_rules = [r for r in rules if r not in safe_rules]
    t0 = time.monotonic()
    rst = sqlfluffrs.engine_parse_to_tree(raw_file, fname, cfg, None, True)
    parse_time = time.monotonic() - t0
    if rst is None:
        return None  # unrenderable / lexer error -> native (TMP reporting)
    root = RsSegment(rst.root)
    if next(root.recursive_crawl("unparsable"), None) is not None:
        return None  # parse error -> native (PRS violations + exit code)
    tf = rst.templated_file
    if tf is None:
        return None
    # Templated sources are eligible (arena markers carry the full source
    # mapping; multi-variant renders crawl every variant tree) EXCEPT when
    # the render produced templater violations, which native reports
    # alongside lint results (the façade LintedFile below would drop them).
    if getattr(rst, "templater_violations", None):
        return None
    # ``noqa`` handling, exactly like native (linter.py:490-499): build the
    # ignore mask from the parsed tree's comments; masked results are dropped
    # inside the rule crawls and the mask rides on the LintedFile for
    # unused-noqa warnings. Malformed directives are SQLParseErrors reported
    # alongside the lint results.
    ignore_mask, noqa_violations = facade_ignore_mask(
        root, cfg, rule_pack.reference_map
    )
    rule_timings: list[tuple[str, str, float]] = []
    t1 = time.monotonic()
    violations = facade_violations(
        raw_file,
        fname,
        cfg,
        safe_rules,
        rst=rst,
        rule_timing_sink=rule_timings,
        ignore_mask=ignore_mask,
        reference_map=rule_pack.reference_map,
    )
    if violations is None:
        return None
    if unknown_rules:
        unknown_violations = facade_unknown_rule_violations(
            raw_file,
            fname,
            cfg,
            unknown_rules,
            ignore_mask=ignore_mask,
            rule_timing_sink=rule_timings,
        )
        if unknown_violations is None:
            return None  # native reference parse failed -> native
        # The same templated-area filter facade_violations applies.
        if cfg.get("ignore_templated_areas", default=True):
            unknown_violations = _Linter.remove_templated_errors(unknown_violations)
        violations = violations + unknown_violations
    lint_time = time.monotonic() - t1
    violations = LintedFile.deduplicate_in_source_space(violations + noqa_violations)
    # Native's violation post-processing (linter.py:840-843).
    for violation in violations:
        violation.ignore_if_in(cfg.get("ignore"))
        violation.warning_if_in(cfg.get("warnings"))
    return LintedFile(
        fname,
        violations,
        # The engine templates/lexes/parses in one call, so the whole
        # front-of-pipeline time lands under "parsing" (the record keys still
        # match native's; --bench and --persist-timing route to native).
        timings=FileTimings(
            {
                "templating": 0.0,
                "lexing": 0.0,
                "parsing": parse_time,
                "linting": lint_time,
            },
            rule_timings,
        ),
        # ``LintedDir.add`` reads these for the per-file ``statistics`` record
        # (char and segment counts); the engine's TemplatedFile and the façade
        # root duck-type the accessors it uses.
        tree=root,  # type: ignore[arg-type]
        ignore_mask=ignore_mask,
        templated_file=tf,
        encoding=encoding,
    )


def facade_fixed_linted_file(
    raw_file: str,
    fname: str,
    cfg: Any,
    linter: Any,
    encoding: str = "utf-8",
) -> Optional[Any]:
    """FIX one source over the arena façade into a ``LintedFile``, or ``None``.

    The fix-mode counterpart of :func:`facade_linted_file` for
    ``Linter.lint_string(fix=True)``: runs the arena mutation loop, stores
    the merged source patches on the ``LintedFile`` (native's
    ``source_patches``) so ``fix_string()`` reconstructs identically, and
    reports the pre-fix violations like native's ``initial_linting_errors``.
    Fix requires EVERY rule to be façade-safe (no split-crawl); anything
    else — or a runaway loop revert — returns ``None`` for native handling.
    """
    import time

    import sqlfluffrs
    from sqlfluff.core.linter.linted_file import FileTimings, LintedFile

    if not engine_enabled(cfg):
        return None
    # A custom templater INSTANCE on the linter is authoritative for
    # native rendering (``render_string`` uses ``self.templater``); the
    # engine renders via ``config.get_templater()`` and would silently use
    # the CONFIGURED templater instead. Route custom-templater linters to
    # native (e.g. ``Linter(templater=...)`` API usage).
    _custom_templater = getattr(linter, "templater", None)
    if (
        _custom_templater is not None
        and type(_custom_templater) is not cfg.get_templater_class()
    ):
        return None
    if not raw_file:
        return None
    rule_pack = linter.get_rulepack(config=cfg)
    rules = list(rule_pack.rules)
    if not rules or any(not rule_is_facade_safe(r) for r in rules):
        return None
    t0 = time.monotonic()
    rst = sqlfluffrs.engine_parse_to_tree(raw_file, fname, cfg, None, True)
    parse_time = time.monotonic() - t0
    if rst is None:
        return None
    root = RsSegment(rst.root)
    if next(root.recursive_crawl("unparsable"), None) is not None:
        return None
    tf = rst.templated_file
    if tf is None:
        return None
    if getattr(rst, "templater_violations", None):
        return None
    ignore_mask, noqa_violations = facade_ignore_mask(
        root, cfg, rule_pack.reference_map
    )
    pre: list[Any] = list(noqa_violations)
    loop_state: dict[str, Any] = {}
    patches: list[Any] = []
    t1 = time.monotonic()
    facade_fix_loop(
        raw_file,
        fname,
        cfg,
        rules,
        int(cfg.get("runaway_limit")),
        rst=rst,
        lint_sink=pre,
        loop_state=loop_state,
        ignore_mask=ignore_mask,
        reference_map=rule_pack.reference_map,
        patch_sink=patches,
    )
    lint_time = time.monotonic() - t1
    if loop_state.get("runaway"):
        # Native's runaway bookkeeping (strip fixes, all-unfixable) is not
        # reproduced here -> native owns the file.
        return None
    violations = LintedFile.deduplicate_in_source_space(pre)
    # Native's violation post-processing (linter.py:840-843).
    for violation in violations:
        violation.ignore_if_in(cfg.get("ignore"))
        violation.warning_if_in(cfg.get("warnings"))
    return LintedFile(
        fname,
        violations,
        timings=FileTimings(
            {
                "templating": 0.0,
                "lexing": 0.0,
                "parsing": parse_time,
                "linting": lint_time,
            },
            [],
        ),
        # The MUTATED façade root: ``fix_string()`` uses the stored
        # ``source_patches`` below (native's merged-variant mechanism), so it
        # never re-walks the tree — but stats/serialisation duck-type it.
        tree=root,  # type: ignore[arg-type]
        ignore_mask=ignore_mask,
        templated_file=tf,
        encoding=encoding,
        source_patches=patches,
    )


def try_facade_lint_string(
    in_str: str,
    fname: str,
    config: Any,
    linter: Any,
    fix: bool = False,
    encoding: str = "utf8",
) -> Optional[Any]:
    """The ``Linter.lint_string`` fast-path entry: a ``LintedFile`` or ``None``.

    Never raises — ANY failure defers to the native pipeline.
    """
    try:
        if fix:
            return facade_fixed_linted_file(in_str, fname, config, linter, encoding)
        return facade_linted_file(in_str, fname, config, linter, encoding)
    except Exception:  # noqa: BLE001 — any uncertainty -> native handles it
        return None


# --------------------------------------------------------------------------
# Multiprocess transport & per-file work units (the -p N fast path).
#
# Façade objects (RsSegment anchors, RsTemplatedFile-bearing markers,
# synthetic segment classes) don't pickle, so per-file results computed in a
# worker process cross back as "transported" equivalents: every consumer-
# visible answer (to_dict records, rule codes, positions, fixability,
# warning/ignore flags) is PRECOMPUTED with the real objects' own methods in
# the worker, then served from stored primitives in the parent. The serial
# path uses the same units and transports, so ``-p 1`` and ``-p N`` share one
# code path by construction.
# --------------------------------------------------------------------------


class TransportedLintError(SQLLintError):
    """A picklable stand-in for a façade ``SQLLintError``.

    Subclasses ``SQLLintError`` so ``isinstance`` filters (e.g.
    ``get_violations(types=...)``) behave identically, but never touches
    ``segment``/``rule``/``fixes`` — every answer comes from primitives
    computed by the REAL error in the producing process.
    """

    def __init__(
        self,
        description: str,
        line_no: int,
        line_pos: int,
        rule_code: str,
        rule_name: str,
        record: dict,
        fixable: bool,
        ignore: bool = False,
        warning: bool = False,
        fatal: bool = False,
    ) -> None:
        SQLBaseError.__init__(
            self,
            description=description,
            line_no=line_no,
            line_pos=line_pos,
            ignore=ignore,
            fatal=fatal,
            warning=warning,
        )
        self.segment = None  # type: ignore[assignment]
        self.rule = None  # type: ignore[assignment]
        self.fixes = []
        self._rule_code = rule_code
        self._rule_name = rule_name
        self._record = record
        self._fixable = fixable

    def rule_code(self) -> str:
        return self._rule_code

    def rule_name(self) -> str:
        return self._rule_name

    def to_dict(self) -> dict:
        return self._record

    @property
    def fixable(self) -> bool:
        return self._fixable

    def __reduce__(self):
        return type(self), (
            self.description,
            self.line_no,
            self.line_pos,
            self._rule_code,
            self._rule_name,
            self._record,
            self._fixable,
            self.ignore,
            self.warning,
            self.fatal,
        )


class TransportedParseError(SQLParseError):
    """A picklable stand-in for a façade-anchored ``SQLParseError``.

    (Malformed-noqa directives anchor on façade comment segments.)
    """

    def __init__(
        self,
        description: str,
        line_no: int,
        line_pos: int,
        record: dict,
        ignore: bool = False,
        warning: bool = False,
        fatal: bool = False,
    ) -> None:
        SQLBaseError.__init__(
            self,
            description=description,
            line_no=line_no,
            line_pos=line_pos,
            ignore=ignore,
            fatal=fatal,
            warning=warning,
        )
        self.segment = None
        self._record = record

    def to_dict(self) -> dict:
        return self._record

    def __reduce__(self):
        return type(self), (
            self.description,
            self.line_no,
            self.line_pos,
            self._record,
            self.ignore,
            self.warning,
            self.fatal,
        )


def transport_violation(v: Any) -> Any:
    """A picklable equivalent of one façade violation (see module note)."""
    if isinstance(v, SQLLintError):
        return TransportedLintError(
            description=v.description or "",
            line_no=v.line_no,
            line_pos=v.line_pos,
            rule_code=v.rule_code(),
            rule_name=v.rule_name(),
            record=v.to_dict(),
            fixable=v.fixable,
            ignore=v.ignore,
            warning=v.warning,
            fatal=v.fatal,
        )
    if isinstance(v, SQLParseError):
        return TransportedParseError(
            description=v.description or "",
            line_no=v.line_no,
            line_pos=v.line_pos,
            record=v.to_dict(),
            ignore=v.ignore,
            warning=v.warning,
            fatal=v.fatal,
        )
    return v  # segment-less SQLBaseErrors pickle natively


class _TransportedTreeStats:
    """The two answers ``LintedDir.add`` reads from ``LintedFile.tree``."""

    def __init__(self, segments: int, raw_segments: int) -> None:
        self._segments = segments
        self._raw_segments = raw_segments

    def count_segments(self, raw_only: bool = False) -> int:
        return self._raw_segments if raw_only else self._segments


class _TransportedTemplatedFileStats:
    """The two answers ``LintedDir.add`` reads from ``.templated_file``."""

    def __init__(self, source_str: str, templated_str: str) -> None:
        self.source_str = source_str
        self.templated_str = templated_str


def transport_linted_file(lf: Any) -> Any:
    """A picklable equivalent of a façade ``LintedFile``.

    ``IgnoreMask`` is plain data and crosses as-is (the formatter generates
    unused-noqa warnings from it in the parent); ``FileTimings`` likewise.
    """
    from sqlfluff.core.linter.linted_file import LintedFile

    return LintedFile(
        path=lf.path,
        violations=[transport_violation(v) for v in lf.violations],
        timings=lf.timings,
        tree=(
            _TransportedTreeStats(  # type: ignore[arg-type]
                lf.tree.count_segments(raw_only=False),
                lf.tree.count_segments(raw_only=True),
            )
            if lf.tree is not None
            else None
        ),
        ignore_mask=lf.ignore_mask,
        templated_file=(
            _TransportedTemplatedFileStats(  # type: ignore[arg-type]
                lf.templated_file.source_str, lf.templated_file.templated_str
            )
            if lf.templated_file is not None
            else None
        ),
        encoding=lf.encoding,
    )


_INLINE_CONFIG_MARKERS = ("-- sqlfluff", "--sqlfluff")


def _has_inline_config(raw: str) -> bool:
    """Whether ``process_raw_file_for_config`` would find any directive."""
    return any(ln.startswith(_INLINE_CONFIG_MARKERS) for ln in raw.splitlines())


def load_raw_file_and_config_cached(
    fname: str, root_config: Any, linter: Any, cache: Optional[dict]
) -> tuple:
    """``load_raw_file_and_config`` + rulepack, with per-directory sharing.

    ``make_child_from_path`` is directory-derived, so same-directory files
    share the child config AND its rulepack — UNLESS the file carries inline
    ``-- sqlfluff`` directives, which mutate the child; directive-carrying
    files always take the private native loader and a fresh pack (and never
    seed the cache). Directive-free processing is a no-op, so the shared
    child never mutates. Over-limit files fall through to the native loader
    purely so its canonical ``SQLFluffSkipFile`` message raises.

    Returns ``(raw, cfg, encoding, rule_pack)``.
    """
    import os

    from sqlfluff.core.helpers.file import get_encoding
    from sqlfluff.core.linter import Linter as _Linter

    entry = None
    dirkey = None
    if cache is not None:
        dirkey = os.path.dirname(os.path.abspath(fname))
        entry = cache.get(dirkey)
    if entry is not None:
        cfg, pack = entry
        limit = cfg.get("large_file_skip_byte_limit")
        if not limit or os.path.getsize(fname) <= int(limit):
            config_encoding = cfg.get("encoding", default="autodetect")
            encoding = get_encoding(fname=fname, config_encoding=config_encoding)
            with open(fname, encoding=encoding, errors="backslashreplace") as f:
                raw = f.read()
            if not _has_inline_config(raw):
                return raw, cfg, encoding, pack
        # Over-limit (native loader raises the canonical SQLFluffSkipFile)
        # or directive-carrying: private load below.
    raw, cfg, encoding = _Linter.load_raw_file_and_config(fname, root_config)
    pack = linter.get_rulepack(config=cfg)
    if cache is not None and not _has_inline_config(raw):
        cache[dirkey] = (cfg, pack)
    return raw, cfg, encoding, pack


def facade_lint_file_unit(
    fname: str, root_config: Any, linter: Any, loader_cache: Optional[dict] = None
) -> tuple:
    """The full per-file LINT fast-path unit (read → gate → crawl → transport).

    Returns one of (the last element is the per-file dialect string, for
    native's unset-dialect safety warning — linter.py:866-874)::

        ("handled", fname, transported_linted_file, rule_codes, warn_unused, dialect)
        ("native",  fname, native_linted_file,      rule_codes, warn_unused, dialect)
        ("skipped", fname, skip_message,            None,       False,       None)
        ("routed",  fname, None,                    None,       False,       None)

    "native": the façade declined the file, so the NATIVE pipeline ran right
    here (parse_string + lint_parsed — native LintedFiles pickle, which is
    how native's own runners transport them). That keeps every file in ONE
    pool/loop: no second native pool spawn for a handful of leftovers, and
    per-file output stays in discovery order. "skipped" carries the
    ``SQLFluffSkipFile`` message; the consumer owns the warning + the
    ``files_skipped`` accounting. "routed" only remains as the escape hatch
    for unexpected failures (the consumer hands those to native
    ``lint_paths``).
    """
    from sqlfluff.core.errors import SQLFluffSkipFile

    try:
        raw, cfg, encoding, rule_pack = load_raw_file_and_config_cached(
            fname, root_config, linter, loader_cache
        )
    except SQLFluffSkipFile as e:
        return ("skipped", fname, str(e), None, False, None)
    codes = sorted(r.code for r in rule_pack.rules)
    warn_unused = bool(cfg.get("warn_unused_ignores"))
    dialect = cfg.get("dialect")
    lf = facade_linted_file(
        raw, fname, cfg, linter, encoding=encoding, rule_pack=rule_pack
    )
    if lf is None:
        parsed = linter.parse_string(raw, fname=fname, config=cfg, encoding=encoding)
        native_lf = linter.lint_parsed(
            parsed, rule_pack, fix=False, formatter=None, encoding=encoding
        )
        return ("native", fname, native_lf, codes, warn_unused, dialect)
    return ("handled", fname, transport_linted_file(lf), codes, warn_unused, dialect)


def facade_fix_file_unit(
    fname: str, root_config: Any, linter: Any, loader_cache: Optional[dict] = None
) -> tuple:
    """The full per-file FIX fast-path unit — shared by serial and pool paths.

    Mirrors the gating previously inline in ``_try_facade_paths_fix``
    (every rule façade-safe, engine parse with no unparsable section,
    violation-free render, no runaway, no phantom-prone unfixables) and
    returns ``("routed", fname, None)`` or ``("handled", fname, payload)``
    with a PICKLABLE payload::

        {"fixed": str, "changed": bool, "encoding": str,
         "pre": [transported violations post-dedupe/demotion, in order],
         "mask": IgnoreMask or None, "warn_unused": bool}

    Display filtering, counts, records and writes all happen in the parent
    from this payload (once — the same consumption for -p 1 and -p N).
    """
    import time

    import sqlfluffrs
    from sqlfluff.core.errors import SQLLintError as _SQLLintError
    from sqlfluff.core.linter.linted_file import FileTimings, LintedFile

    routed = ("routed", fname, None)
    raw_file, cfg, encoding, rule_pack = load_raw_file_and_config_cached(
        fname, root_config, linter, loader_cache
    )
    if not engine_enabled(cfg):
        return routed
    if not raw_file:
        # Empty file -> native (LT12 lints raw-templated zero-byte renders).
        return routed
    rules = list(rule_pack.rules)
    # Core allowlist or explicit plugin ``rust_compatible`` opt-in.
    if not rules or any(not rule_is_facade_safe(r) for r in rules):
        return routed
    _t0 = time.monotonic()
    rst = sqlfluffrs.engine_parse_to_tree(raw_file, fname, cfg, None, True)
    parse_time = time.monotonic() - _t0
    if rst is None:
        return routed  # unparsable / templater error
    # Parse errors (an ``unparsable`` section) are native's: PRS violations,
    # non-zero exit, no fix.
    root = RsSegment(rst.root)
    if next(root.recursive_crawl("unparsable"), None) is not None:
        return routed
    tf = rst.templated_file
    if tf is None:
        return routed
    if getattr(rst, "templater_violations", None):
        return routed  # TMP reporting stays native
    # ``noqa``: masked results/fixes drop inside the loop's crawls; malformed
    # directives report like native's initial_linting_errors.
    ignore_mask, noqa_violations = facade_ignore_mask(
        root, cfg, rule_pack.reference_map
    )
    pre: list = list(noqa_violations)
    loop_state: dict = {}
    rule_timings: list[tuple[str, str, float]] = []
    _t1 = time.monotonic()
    fixed = facade_fix_loop(
        raw_file,
        fname,
        cfg,
        rules,
        int(cfg.get("runaway_limit")),
        rst=rst,
        lint_sink=pre,
        loop_state=loop_state,
        ignore_mask=ignore_mask,
        reference_map=rule_pack.reference_map,
        rule_timing_sink=rule_timings,
    )
    lint_time = time.monotonic() - _t1
    # A runaway revert is the one gave-up case whose bookkeeping we don't
    # reproduce (native strips fixes from every reported violation —
    # linter.py:683-693): let native own the file.
    if loop_state.get("runaway"):
        return routed
    # Native's reporting pipeline: source-space dedupe (linter.py:848), then
    # the ``ignore`` config and ``sqlfluff:warnings`` demotion (840-843).
    pre = LintedFile.deduplicate_in_source_space(pre)
    for v in pre:
        v.ignore_if_in(cfg.get("ignore"))
        v.warning_if_in(cfg.get("warnings"))
    # Anything that isn't a lint error (e.g. a malformed-noqa SQLParseError)
    # reports through native's parse-error machinery -> defer.
    if any(not isinstance(v, _SQLLintError) for v in pre):
        return routed
    kept = [
        v
        for v in pre
        if not getattr(v, "warning", False) and getattr(v, "ignore", False) is not True
    ]
    # Self-guard: the fixed output must still reparse cleanly over a FRESH
    # engine parse (catches any mutate-vs-reparse divergence). If the fix
    # changed nothing the residual is identical to ``pre``, so skip it.
    if fixed != raw_file:
        post = facade_violations(fixed, fname, cfg, rules)
        if post is None:
            return routed
    else:
        post = pre
    pre_unfixable = [v for v in kept if not getattr(v, "fixes", None)]
    if pre_unfixable or post:
        # An unfixable violation from a rule with known detection
        # over-reporting on the façade may be phantom — reporting it would
        # leak a wrong exit code, so defer.
        if any(
            v.rule_code() in FACADE_SAFE_RULES_DETECTION_UNSAFE
            for v in (*pre_unfixable, *post)
        ):
            return routed
    # A violation-free "timing carrier" LintedFile: --bench and
    # --persist-timing aggregate step + rule timings (and the per-file
    # statistics columns) through LintedDir records. The engine
    # templates/lexes/parses in one call, so the whole front-of-pipeline
    # lands under "parsing"; "linting" is the whole fix loop, like native's.
    # Tree counts read the post-fix (mutated) tree, matching native's fixed
    # LintedFile.
    timing_file = LintedFile(
        path=fname,
        violations=[],
        timings=FileTimings(
            {
                "templating": 0.0,
                "lexing": 0.0,
                "parsing": parse_time,
                "linting": lint_time,
            },
            rule_timings,
        ),
        tree=_TransportedTreeStats(  # type: ignore[arg-type]
            root.count_segments(raw_only=False),
            root.count_segments(raw_only=True),
        ),
        ignore_mask=None,
        templated_file=_TransportedTemplatedFileStats(  # type: ignore[arg-type]
            tf.source_str, tf.templated_str
        ),
        encoding=encoding,
    )
    return (
        "handled",
        fname,
        {
            "fixed": fixed,
            "changed": fixed != raw_file,
            "encoding": encoding,
            "pre": [transport_violation(v) for v in pre],
            "mask": ignore_mask,
            "warn_unused": bool(cfg.get("warn_unused_ignores")),
            "timing_file": timing_file,
        },
    )


# Per-worker state for the multiprocess fast path (set by the pool
# initializer; keyed lookups keep the worker functions picklable-by-name).
_POOL_STATE: dict[str, Any] = {}


def _facade_pool_init(config: Any) -> None:
    """Pool worker initializer: mirror native ``ParallelRunner._init_global``.

    Marks the process non-main (plugin machinery), disables SIGINT so the
    parent owns Ctrl-C handling, and builds the per-worker linter once.
    """
    import signal

    from sqlfluff.core import Linter as _Linter
    from sqlfluff.core.plugin.host import is_main_process

    is_main_process.set(False)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    # Pickling deliberately nulls ``templater_obj`` (FluffConfig.__getstate__
    # sanctions rehydrating it in the child) — the native-inline path in the
    # lint unit renders through ``linter.templater``, so rebuild it here.
    config._configs["core"]["templater_obj"] = config.get_templater()
    _POOL_STATE["config"] = config
    _POOL_STATE["linter"] = _Linter(config=config)


def _facade_lint_pool_worker(fname: str) -> tuple:
    """Run one file's lint unit in a pool worker; failures route to native."""
    try:
        return facade_lint_file_unit(
            fname,
            _POOL_STATE["config"],
            _POOL_STATE["linter"],
            loader_cache=_POOL_STATE.setdefault("loader_cache", {}),
        )
    except Exception as e:  # noqa: BLE001 — worker failure -> native owns it
        linter_logger.info("Façade pool worker failed on %s: %s", fname, e)
        return ("routed", fname, None, None, False, None)


def _facade_fix_pool_worker(fname: str) -> tuple:
    """Run one file's fix unit in a pool worker; failures route to native."""
    try:
        return facade_fix_file_unit(
            fname,
            _POOL_STATE["config"],
            _POOL_STATE["linter"],
            loader_cache=_POOL_STATE.setdefault("loader_cache", {}),
        )
    except Exception as e:  # noqa: BLE001 — worker failure -> native owns it
        linter_logger.info("Façade pool worker failed on %s: %s", fname, e)
        return ("routed", fname, None)


def resolve_facade_processes(config: Any, processes: Optional[int]) -> int:
    """Resolve the worker count exactly like native ``get_runner``.

    Positive = that many processes; zero/negative = ``cpu_count + n``
    (min 1). ``None`` falls back to the ``processes`` config (default 1).
    """
    import multiprocessing

    procs = processes if processes is not None else int(config.get("processes") or 1)
    if procs <= 0:
        procs = max(multiprocessing.cpu_count() + procs, 1)
    return procs


def facade_pool(processes: int, config: Any) -> Any:
    """A spawn-context pool matching native ``MultiProcessRunner``'s choice."""
    import multiprocessing

    return multiprocessing.get_context("spawn").Pool(
        processes=processes, initializer=_facade_pool_init, initargs=(config,)
    )
