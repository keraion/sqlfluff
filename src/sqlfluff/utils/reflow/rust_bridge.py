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
from typing import TYPE_CHECKING, Callable, Literal, Optional

# sqlfluffrs is an *optional* third-party package. It must appear before
# first-party (sqlfluff) imports to satisfy ruff's isort ordering rules.
# Preferred production API: build a RsReflowConfig once per FluffConfig
# and reuse it across all files linted with that config.
try:
    from sqlfluffrs import RsReflowConfig as _RsReflowConfig
    from sqlfluffrs import rs_make_reflow_config as _rs_make_reflow_config
    from sqlfluffrs import rs_respace_with_config_obj as _rs_respace_with_config_obj

    _HAS_RUST_RESPACE = True
except ImportError:
    _RsReflowConfig = None  # type: ignore[assignment,misc]
    _rs_make_reflow_config = None  # type: ignore[assignment]
    _rs_respace_with_config_obj = None  # type: ignore[assignment]
    _HAS_RUST_RESPACE = False

from sqlfluff.core.rules import LintFix, LintResult

if TYPE_CHECKING:
    from collections.abc import Sequence

    from sqlfluff.core.config import FluffConfig
    from sqlfluff.core.parser import BaseSegment, RawSegment
    from sqlfluff.utils.reflow.depthmap import DepthMap
    from sqlfluff.utils.reflow.sequence import ReflowSequence
    from sqlfluffrs import RsReflowConfig

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

# Sentinels for _RS_CONFIG_CACHE — distinguish a true cache miss from an
# entry that was intentionally stored as "no Rust respace for this config".
_RS_CONFIG_NOT_FOUND = object()  # returned by dict.get() on a cache miss
_RS_CONFIG_NO_RUST = object()  # config has align constraints → Python-only


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


def get_rust_reflow_config(config: FluffConfig) -> Optional["RsReflowConfig"]:
    """Return a cached ``RsReflowConfig`` for *config*, or ``None``.

    Converts ``config``'s layout type section to a Rust ``RsReflowConfig``
    on first call and caches the result keyed on ``id(config)``.  Subsequent
    calls for the same ``FluffConfig`` instance pay only a dict lookup.

    Returns ``None`` when ``sqlfluffrs`` is not installed or when the
    config cannot be converted, so callers never need to branch on
    ``_HAS_RUST_RESPACE`` themselves.

    The ``class_types_map`` previously extracted from the dialect library is
    no longer needed here: codegen now embeds ``_class_types`` directly in
    every ``Node::Segment``, so ``depthmap.rs`` reads them from the node
    itself rather than from the config.

    Configs that contain ``align:*`` spacing constraints cannot use the Rust
    respace engine (alignment requires cross-line tree traversal not yet
    implemented in Rust).  For those configs this function returns ``None``
    and caches the result so the check is paid at most once per config.
    """
    if not _HAS_RUST_RESPACE:
        return None
    cache_key = (id(config), hash(config))
    cached = _RS_CONFIG_CACHE.get(cache_key, _RS_CONFIG_NOT_FOUND)
    if cached is not _RS_CONFIG_NOT_FOUND:
        # _RS_CONFIG_NO_RUST means align constraints → return None to skip Rust.
        if cached is _RS_CONFIG_NO_RUST:
            return None
        return cached
    try:
        layout_dict = config.get_section(["layout", "type"]) or {}
        # Detect align constraints once and cache the result.  Rust silently
        # produces no violations for align-constrained pairs, which would mask
        # real issues, so fall back to Python for those configs.
        for seg_cfg in layout_dict.values():
            if not isinstance(seg_cfg, dict):
                continue
            for key in ("spacing_before", "spacing_after"):
                val = seg_cfg.get(key, "")
                if isinstance(val, str) and val.startswith("align"):
                    _RS_CONFIG_CACHE[cache_key] = _RS_CONFIG_NO_RUST
                    return None
        rs_cfg = _rs_make_reflow_config(layout_dict)
        _RS_CONFIG_CACHE[cache_key] = rs_cfg
        return rs_cfg
    except Exception:
        logger.debug(
            "Failed to build RsReflowConfig from FluffConfig, "
            "Rust respace will fall back to Python.",
            exc_info=True,
        )
        return None


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
          cross-line tree traversal not yet implemented in Rust; detected and
          cached once per ``FluffConfig`` inside :func:`get_rust_reflow_config`)
        - the Rust call raises an unexpected exception

        Template placeholders (Jinja ``{{ }}``, ``{% %}``) are handled
        correctly by the Rust engine — they are included in the node tree as
        ``MetaType::Template`` nodes and assigned ``any`` spacing constraints.
        """
        if self._rs_node is not None:
            rs_cfg = get_rust_reflow_config(self._config)
            if rs_cfg is not None:
                violations = _rs_respace_with_config_obj(self._rs_node, rs_cfg)
                self._rust_violations.extend(violations)
                self._rs_node = None  # consumed — prevent double-run
                return self
            logger.debug("Config not supported for Rust respace.")
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

    def rebreak(
        self,
        filter_type: Literal["lines", "keywords"] | None = None,
    ) -> RustReflowSequence:
        """Check and fix line breaks (Python implementation).

        Args:
            filter_type: Optional dispatch string, e.g. ``"keywords"``
                for LT14.

        .. note::
            A Rust fast-path will be wired in here once
            ``sqlfluffrs_rules`` implements rebreak.
        """
        # TODO: Rust fast-path for rebreak.
        if filter_type is None:
            self._inner = self._get_inner().rebreak()
        else:
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
