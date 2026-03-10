"""Bridge module for Rust-accelerated reflow operations.

This module provides :class:`RustReflowSequence`, a drop-in replacement
for :class:`~sqlfluff.utils.reflow.sequence.ReflowSequence` that
transparently uses the Rust reflow engine when available and falls back
to the pure-Python implementation otherwise.

Rule developers can adopt it with a minimal code change::

    # Before
    from sqlfluff.utils.reflow import ReflowSequence

    class Rule_LT01(BaseRule):
        def _eval(self, context: RuleContext):
            return (
                ReflowSequence.from_root(context.segment, config=context.config)
                .respace()
                .get_results()
            )

    # After — Rust-accelerated when available, identical fallback otherwise
    from sqlfluff.utils.reflow import RustReflowSequence

    class Rule_LT01(BaseRule):
        def _eval(self, context: RuleContext):
            return (
                RustReflowSequence.from_root(context.segment, config=context.config)
                .respace()
                .get_results()
            )

The :func:`reflow_respace`, :func:`reflow_reindent`,
:func:`reflow_rebreak`, and :func:`reflow_rebreak_around_target` helper
functions are thin wrappers around :class:`RustReflowSequence` for rules
that prefer a one-liner call.

**Currently Rust-accelerated:** ``respace``.

**Currently Python-only:** ``reindent``, ``rebreak``.
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING, Callable, Optional

# sqlfluffrs is an *optional* third-party package. It must appear before
# first-party (sqlfluff) imports to satisfy ruff's isort ordering rules.
# Each symbol is imported in its own try/except so that a missing symbol
# in one build variant never poisons the flags for others.
try:
    from sqlfluffrs import rs_respace_node as _rs_respace_node

    _HAS_RUST_RESPACE = True
except ImportError:
    _rs_respace_node = None  # type: ignore[assignment]
    _HAS_RUST_RESPACE = False

try:
    from sqlfluffrs import (
        rs_respace_node_with_config as _rs_respace_node_with_config,
    )

    _HAS_RUST_RESPACE_WITH_CONFIG = True
except ImportError:
    _rs_respace_node_with_config = None  # type: ignore[assignment]
    _HAS_RUST_RESPACE_WITH_CONFIG = False

# Preferred production API: build a RsReflowConfig once per FluffConfig
# and reuse it across all files linted with that config.
try:
    from sqlfluffrs import RsReflowConfig as _RsReflowConfig
    from sqlfluffrs import rs_make_reflow_config as _rs_make_reflow_config
    from sqlfluffrs import rs_respace_with_config_obj as _rs_respace_with_config_obj

    _HAS_RUST_RESPACE_OBJ = True
except ImportError:
    _RsReflowConfig = None  # type: ignore[assignment,misc]
    _rs_make_reflow_config = None  # type: ignore[assignment]
    _rs_respace_with_config_obj = None  # type: ignore[assignment]
    _HAS_RUST_RESPACE_OBJ = False

from sqlfluff.core.rules import LintFix, LintResult

if TYPE_CHECKING:
    from collections.abc import Sequence

    from sqlfluff.core.config import FluffConfig
    from sqlfluff.core.parser import BaseSegment, RawSegment
    from sqlfluff.core.rules import RuleContext
    from sqlfluff.utils.reflow.depthmap import DepthMap
    from sqlfluff.utils.reflow.sequence import ReflowSequence

logger = logging.getLogger("sqlfluff.utils.reflow.rust_bridge")

# ---------------------------------------------------------------------------
# Per-session RsReflowConfig cache
# ---------------------------------------------------------------------------
# Maps id(FluffConfig) → RsReflowConfig so the layout dict → Rust struct
# conversion happens at most once per distinct FluffConfig instance.
# FluffConfig objects are long-lived within a lint session, so using their
# id as a key is safe in practice.  The cache is intentionally unbounded
# since the number of distinct configs per process is small (typically 1
# per directory with a .sqlfluff file).
#
# NOTE: In test suites Python may recycle object IDs after GC, which can
# return a stale cached value.  We guard against this by also storing the
# config's hash (identity-based) so a recycled id with a different hash
# is a cache miss.
_RS_CONFIG_CACHE: dict[tuple[int, int], object] = {}


# ---------------------------------------------------------------------------
# Public helpers
# ---------------------------------------------------------------------------


def has_rust_reflow() -> bool:
    """Return ``True`` if the Rust reflow extension is available.

    When this returns ``False`` every :class:`RustReflowSequence` operation
    falls back silently to the pure-Python :class:`ReflowSequence`.
    """
    return _HAS_RUST_RESPACE


def get_rust_node(segment: BaseSegment) -> object:
    """Return the Rust node attached to *segment*, or ``None``.

    Returns ``None`` whenever ``sqlfluffrs`` is not installed, so callers
    never need to guard on :func:`has_rust_reflow` themselves.
    """
    if not _HAS_RUST_RESPACE:
        return None
    return getattr(segment, "_rs_node", None)


def get_rust_reflow_config(config: FluffConfig) -> object:
    """Return a cached ``RsReflowConfig`` for *config*, or ``None``.

    Converts ``config``'s layout type section to a Rust ``RsReflowConfig``
    on first call and caches the result keyed on ``id(config)``.  Subsequent
    calls for the same ``FluffConfig`` instance pay only a dict lookup.

    Returns ``None`` when ``sqlfluffrs`` is not installed or when the
    config cannot be converted, so callers never need to branch on
    ``_HAS_RUST_RESPACE_OBJ`` themselves.

    This is the correct way to respect per-directory ``.sqlfluff`` overrides:
    each ``FluffConfig`` already carries the merged config for its scope, so
    we must build the Rust config from it rather than from the hard-coded
    ``ReflowConfig::default_ansi()``.
    """
    if not _HAS_RUST_RESPACE_OBJ:
        return None
    cache_key = (id(config), hash(config))
    cached = _RS_CONFIG_CACHE.get(cache_key)
    if cached is not None:
        return cached
    try:
        layout_dict = config.get_section(["layout", "type"])
        class_types_map = _extract_class_types_map(config)
        rs_cfg = _rs_make_reflow_config(layout_dict, class_types_map)
        _RS_CONFIG_CACHE[cache_key] = rs_cfg
        return rs_cfg
    except Exception:
        logger.debug(
            "Failed to build RsReflowConfig from FluffConfig, "
            "Rust respace will fall back to Python.",
            exc_info=True,
        )
        return None


def _extract_class_types_map(config: FluffConfig) -> dict[str, list[str]]:
    """Build a segment_type → extra_class_types mapping from the dialect.

    Walks the dialect's segment library and, for each segment class that
    has a ``type`` attribute, computes the *extra* inherited class types
    (i.e. ``_class_types - {own_type}``).  The result tells Rust which
    parent-class types to inject when building DepthMap class_types sets.
    """
    from sqlfluff.core.parser.segments.base import BaseSegment

    dialect = config.get("dialect_obj")
    if dialect is None:
        return {}
    result: dict[str, list[str]] = {}
    for _name, cls in dialect._library.items():
        if not isinstance(cls, type) or not issubclass(cls, BaseSegment):
            continue
        seg_type = getattr(cls, "type", None)
        if not seg_type:
            continue
        extras = cls._class_types - {seg_type}
        if extras:
            result[seg_type] = sorted(extras)
    return result


def segment_has_templates(segment: BaseSegment) -> bool:
    """Return ``True`` if *segment* contains any template placeholder segments.

    Template (Jinja) files produce ``TemplateSegment`` (type ``"placeholder"``)
    nodes whose rendered indices do not align with the Rust node's flat raw list.
    The Rust respace engine cannot correctly handle these files yet, so we fall
    back to the Python path when any placeholder is present.
    """
    return any(s.is_type("placeholder") for s in segment.raw_segments)


def config_has_align_constraints(config: FluffConfig) -> bool:
    """Return ``True`` if *config* has any ``align:*`` spacing constraints.

    Alignment constraints require cross-line tree traversal that is not yet
    implemented in the Rust reflow engine.  When any ``spacing_before`` or
    ``spacing_after`` value starts with ``"align"`` the caller should fall
    back to the Python reflow sequence so alignment violations are correctly
    detected and fixed.
    """
    try:
        layout_dict = config.get_section(["layout", "type"]) or {}
        for _seg_type, seg_cfg in layout_dict.items():
            if not isinstance(seg_cfg, dict):
                continue
            for key in ("spacing_before", "spacing_after"):
                val = seg_cfg.get(key, "")
                if isinstance(val, str) and val.startswith("align"):
                    return True
        return False
    except Exception:
        return False


# ---------------------------------------------------------------------------
# Violation conversion
# ---------------------------------------------------------------------------


def convert_rust_violations(
    violations: list,
    raw_segments: list,
) -> list[LintResult]:
    """Convert Rust ``RsLintViolation`` objects to Python ``LintResult`` objects.

    Args:
        violations: List of ``RsLintViolation`` from the Rust reflow module.
            Each violation exposes ``.anchor_idx``, ``.fix_type``,
            ``.edit_text``, and ``.description``.
        raw_segments: Flattened list of ``RawSegment`` from the Python
            segment tree (i.e. ``segment.raw_segments``).

    Returns:
        A list of ``LintResult`` objects ready for the SQLFluff rule engine.
    """
    from sqlfluff.core.parser import WhitespaceSegment

    results: list[LintResult] = []
    n_raws = len(raw_segments)

    for v in violations:
        anchor_idx: int = v.anchor_idx
        if anchor_idx >= n_raws:
            logger.warning(
                "Rust violation anchor index %d out of range (max %d), skipping.",
                anchor_idx,
                n_raws - 1,
            )
            continue

        anchor_seg = raw_segments[anchor_idx]
        fix_type: str = v.fix_type
        fixes: list[LintFix] = []

        if fix_type == "delete":
            fixes.append(LintFix("delete", anchor_seg))
            result_anchor = anchor_seg
        elif fix_type == "replace":
            edit_text = v.edit_text
            if edit_text is not None:
                new_seg = WhitespaceSegment(raw=edit_text)
                fixes.append(LintFix("replace", anchor_seg, edit=[new_seg]))
            result_anchor = anchor_seg
        elif fix_type == "create_before":
            edit_text = v.edit_text
            if edit_text is not None:
                new_seg = WhitespaceSegment(raw=edit_text)
                fixes.append(LintFix("create_before", anchor_seg, edit=[new_seg]))
            result_anchor = anchor_seg
        elif fix_type == "create_after":
            edit_text = v.edit_text
            if edit_text is not None:
                new_seg = WhitespaceSegment(raw=edit_text)
                fixes.append(LintFix("create_after", anchor_seg, edit=[new_seg]))
            # Python anchors the LintResult at the *next* segment for better
            # CLI display (shows the error at the point of insertion).
            # Rust's anchor_idx points to the prev segment (where we insert after).
            result_anchor = (
                raw_segments[anchor_idx + 1] if anchor_idx + 1 < n_raws else anchor_seg
            )
        else:
            result_anchor = anchor_seg

        results.append(
            LintResult(
                anchor=result_anchor,
                fixes=fixes,
                description=v.description,
                source="respace",
            )
        )

    return results


# ---------------------------------------------------------------------------
# RustReflowSequence
# ---------------------------------------------------------------------------


class RustReflowSequence:
    """Drop-in replacement for :class:`ReflowSequence` with Rust acceleration.

    Presents the same chainable interface as :class:`ReflowSequence` so
    rules require only a one-line import change::

        from sqlfluff.utils.reflow import RustReflowSequence

        # In a rule's _eval method:
        return (
            RustReflowSequence.from_root(context.segment, config=context.config)
            .respace()
            .get_results()
        )

    Operations with a Rust implementation run entirely in Rust.
    Operations not yet ported to Rust delegate transparently to the
    Python :class:`ReflowSequence` — rule code never branches on Rust
    availability.

    The inner Python :class:`ReflowSequence` is constructed *lazily*: it
    is only built when needed (i.e. when the Rust fast-path is unavailable
    or when ``get_raw()`` is called), preserving the performance benefit of
    the Rust path for operations like ``respace``.

    **Currently Rust-accelerated:** ``respace``.

    **Currently Python-only:** ``reindent``, ``rebreak``.
    """

    def __init__(
        self,
        root_segment: BaseSegment,
        config: FluffConfig,
        *,
        rs_node: object = None,
        _inner: Optional[ReflowSequence] = None,
        _build_fn: Optional[Callable[[], ReflowSequence]] = None,
    ) -> None:
        self._root_segment = root_segment
        self._config = config
        # Rust node attached by the parser; None → no Rust fast-path.
        self._rs_node = rs_node
        # Python ReflowSequence — built lazily when Rust is unavailable.
        self._inner = _inner
        self._build_fn = _build_fn
        # Violations accumulated from Rust operations.
        self._rust_violations: list = []

    # ------------------------------------------------------------------
    # Constructors — mirror ReflowSequence class-methods
    # ------------------------------------------------------------------

    @classmethod
    def from_root(
        cls,
        root_segment: BaseSegment,
        config: FluffConfig,
    ) -> RustReflowSequence:
        """Generate a sequence from a root segment.

        Mirrors :meth:`ReflowSequence.from_root`.  Uses the Rust fast-path
        when the segment carries an attached Rust node; falls back silently
        to the Python :class:`ReflowSequence` when ``sqlfluffrs`` is not
        installed or the segment has no Rust node attached.
        """
        # get_rust_node() already guards on _HAS_RUST_RESPACE; it returns
        # None when sqlfluffrs is not installed.
        rs_node = get_rust_node(root_segment)
        return cls(root_segment, config, rs_node=rs_node)

    @classmethod
    def from_around_target(
        cls,
        target_segment: BaseSegment,
        root_segment: BaseSegment,
        config: FluffConfig,
        sides: str = "both",
    ) -> RustReflowSequence:
        """Generate a sequence around a specific target segment.

        Mirrors :meth:`ReflowSequence.from_around_target`.
        Delegates to the Python implementation (no Rust fast-path yet).
        """
        from sqlfluff.utils.reflow.sequence import ReflowSequence

        def _build() -> ReflowSequence:
            return ReflowSequence.from_around_target(
                target_segment, root_segment, config, sides
            )

        return cls(root_segment, config, _build_fn=_build)

    @classmethod
    def from_raw_segments(
        cls,
        segments: Sequence[RawSegment],
        root_segment: BaseSegment,
        config: FluffConfig,
        depth_map: Optional[DepthMap] = None,
    ) -> RustReflowSequence:
        """Construct from a sequence of raw segments.

        Mirrors :meth:`ReflowSequence.from_raw_segments`.
        Delegates to the Python implementation (no Rust fast-path yet).
        """
        from sqlfluff.utils.reflow.sequence import ReflowSequence

        def _build() -> ReflowSequence:
            return ReflowSequence.from_raw_segments(
                segments, root_segment, config, depth_map
            )

        return cls(root_segment, config, _build_fn=_build)

    # ------------------------------------------------------------------
    # Lazy inner-sequence accessor
    # ------------------------------------------------------------------

    def _get_inner(self) -> ReflowSequence:
        """Return the Python :class:`ReflowSequence`, constructing it lazily."""
        if self._inner is None:
            if self._build_fn is not None:
                self._inner = self._build_fn()
            else:
                from sqlfluff.utils.reflow.sequence import ReflowSequence

                self._inner = ReflowSequence.from_root(
                    self._root_segment, config=self._config
                )
        return self._inner

    # ------------------------------------------------------------------
    # Chainable operations — mirror ReflowSequence
    # ------------------------------------------------------------------

    def respace(self) -> RustReflowSequence:
        """Check and fix whitespace spacing.

        Uses the Rust implementation when a Rust node is available.
        The Rust path uses the actual ``FluffConfig`` passed at construction
        time (via :func:`get_rust_reflow_config`), so per-directory
        ``.sqlfluff`` overrides are fully respected.

        Falls back to the Python :class:`ReflowSequence` when:
        - ``sqlfluffrs`` is not installed
        - the segment has no attached Rust node
        - the config has ``align:*`` spacing constraints (alignment requires
          cross-line tree traversal not yet implemented in Rust)
        - the segment contains template placeholders (placeholder segments
          are inserted by the Python templater and not present in the Rust
          node tree, causing index misalignment)
        - the Rust call raises an unexpected exception
        """
        if (
            self._rs_node is not None
            and not config_has_align_constraints(self._config)
            and not segment_has_templates(self._root_segment)
        ):
            try:
                # Prefer the cached-config path (correct + fast).
                rs_cfg = get_rust_reflow_config(self._config)
                if rs_cfg is not None:
                    violations = _rs_respace_with_config_obj(self._rs_node, rs_cfg)
                else:
                    # sqlfluffrs installed but RsReflowConfig unavailable
                    # (older build) — fall back to the no-config variant.
                    violations = _rs_respace_node(self._rs_node)
                self._rust_violations.extend(violations)
                self._rs_node = None  # consumed — prevent double-run
                return self
            except Exception:
                logger.debug(
                    "Rust respace failed, falling back to Python.", exc_info=True
                )
                self._rs_node = None

        # Python fallback
        self._inner = self._get_inner().respace()
        return self

    def reindent(self) -> RustReflowSequence:
        """Check and fix indentation (Python implementation).

        .. note::
            A Rust fast-path will be wired in here once
            ``sqlfluffrs_rules`` implements reindent.
        """
        # TODO: Rust fast-path for reindent.
        self._inner = self._get_inner().reindent()
        return self

    def rebreak(self, filter_type: Optional[str] = None) -> RustReflowSequence:
        """Check and fix line breaks (Python implementation).

        Args:
            filter_type: Optional dispatch string, e.g. ``"keywords"``
                for LT14.

        .. note::
            A Rust fast-path will be wired in here once
            ``sqlfluffrs_rules`` implements rebreak.
        """
        # TODO: Rust fast-path for rebreak.
        self._inner = self._get_inner().rebreak(filter_type)
        return self

    # ------------------------------------------------------------------
    # Terminal accessors — mirror ReflowSequence
    # ------------------------------------------------------------------

    def get_results(self) -> list[LintResult]:
        """Return accumulated lint results."""
        if self._rust_violations:
            raw_segs = self._root_segment.raw_segments
            return convert_rust_violations(self._rust_violations, raw_segs)
        return self._get_inner().get_results()

    def get_fixes(self) -> list[LintFix]:
        """Return accumulated lint fixes."""
        from sqlfluff.utils.reflow.helpers import fixes_from_results

        return fixes_from_results(self.get_results())

    def get_raw(self) -> str:
        """Return the current raw text representation of the sequence."""
        return self._get_inner().get_raw()


# ---------------------------------------------------------------------------
# Convenience wrappers (thin wrappers around RustReflowSequence)
# ---------------------------------------------------------------------------


def reflow_respace(context: RuleContext) -> Optional[list[LintResult]]:
    """Run **respace** on the root segment.

    Thin wrapper around::

        RustReflowSequence.from_root(context.segment, config=context.config)
            .respace().get_results()

    Used by: **LT01**.
    """
    return (
        RustReflowSequence.from_root(context.segment, config=context.config)
        .respace()
        .get_results()
    )


def reflow_reindent(context: RuleContext) -> Optional[list[LintResult]]:
    """Run **reindent** on the root segment.

    Thin wrapper around::

        RustReflowSequence.from_root(context.segment, config=context.config)
            .reindent().get_results()

    Used by: **LT02**.
    """
    return (
        RustReflowSequence.from_root(context.segment, config=context.config)
        .reindent()
        .get_results()
    )


def reflow_rebreak(
    context: RuleContext,
    filter_type: Optional[str] = None,
) -> Optional[list[LintResult]]:
    """Run **rebreak** on the root segment.

    Args:
        context: The rule evaluation context.
        filter_type: Optional filter string (e.g. ``"keywords"`` for LT14).

    Thin wrapper around::

        RustReflowSequence.from_root(context.segment, config=context.config)
            .rebreak(filter_type).get_results()

    Used by: **LT14**.
    """
    return (
        RustReflowSequence.from_root(context.segment, config=context.config)
        .rebreak(filter_type)
        .get_results()
    )


def reflow_rebreak_around_target(
    context: RuleContext,
    target_segment: BaseSegment,
    root_segment: BaseSegment,
    filter_type: Optional[str] = None,
) -> Optional[list[LintResult]]:
    """Run **rebreak** around a specific target segment.

    Args:
        context: The rule evaluation context.
        target_segment: The segment to center the reflow around.
        root_segment: The root of the tree (usually
            ``context.parent_stack[0]``).
        filter_type: Optional filter string.

    Thin wrapper around::

        RustReflowSequence.from_around_target(
            target_segment, root_segment=root_segment, config=context.config
        ).rebreak(filter_type).get_results()

    Used by: **LT03**, **LT04**.
    """
    return (
        RustReflowSequence.from_around_target(
            target_segment,
            root_segment=root_segment,
            config=context.config,
        )
        .rebreak(filter_type)
        .get_results()
    )
