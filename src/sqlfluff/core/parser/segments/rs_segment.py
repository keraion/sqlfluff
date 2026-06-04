"""A Rust-backed segment facade.

``RsSegment`` wraps a lightweight ``RsHandle`` cursor (an ``(arena, node_id)``
pair) into the Rust arena tree and duck-types the read-only subset of the
``BaseSegment`` interface that linting rules, the functional API, and reflow
depend on.  Tree navigation runs entirely Rust-side; only thin handles and
scalars cross the FFI boundary.

This is milestone 1 (read-only navigation).  Editing/fixing and source-patch
generation are added in later milestones; methods that mutate the tree are
intentionally absent here.
"""

from __future__ import annotations

import functools
import inspect
import os
import re
from contextvars import ContextVar
from typing import TYPE_CHECKING, Any, Iterator, Optional, Union

from sqlfluff.core.parser.segments.base import BaseSegment, PathStep

if TYPE_CHECKING:  # pragma: no cover
    from sqlfluffrs import RsHandle

    from sqlfluff.core.dialects.base import Dialect

# The dialect in effect for the current crawl, so the facade can resolve
# class-specific segment methods (e.g. FromClauseSegment.get_eventual_aliases)
# by the node's segment_class name.  Set by the linter around the rule crawl.
_current_dialect: ContextVar[Optional["Dialect"]] = ContextVar(
    "_rs_current_dialect", default=None
)


def set_crawl_dialect(dialect: Optional["Dialect"]) -> None:
    """Record the dialect in effect for the current rule crawl."""
    _current_dialect.set(dialect)


_MISSING = object()

_CLASS_CACHE: dict[tuple[int, str], Optional[type]] = {}


def _resolve_segment_class(dialect: "Dialect", class_name: str) -> Optional[type]:
    """Resolve a Python segment class by name from the dialect (cached)."""
    key = (id(dialect), class_name)
    if key in _CLASS_CACHE:
        return _CLASS_CACHE[key]
    try:
        item = dialect.get_segment(class_name)
    except Exception:  # pragma: no cover
        item = None
    result = item if isinstance(item, type) and issubclass(item, BaseSegment) else None
    _CLASS_CACHE[key] = result
    return result


def rs_segments_enabled() -> bool:
    """Whether to run the rule crawl on Rust-backed ``RsSegment`` facades.

    Controlled by the ``SQLFLUFF_RS_SEGMENTS`` environment variable; off by
    default so behaviour is unchanged unless explicitly opted in.
    """
    return os.environ.get("SQLFLUFF_RS_SEGMENTS", "").strip().lower() in (
        "1",
        "true",
        "yes",
        "on",
    )


def _norm_types(seg_type: tuple[str, ...]) -> list[str]:
    return [str(t) for t in seg_type]


class RsSegment:
    """Read-only facade over a node in the Rust arena tree.

    Identity (``uuid``, ``__eq__``, ``__hash__``) mirrors ``BaseSegment`` so the
    facade can be used interchangeably as a fix anchor in later milestones.
    """

    __slots__ = ("_h",)

    def __init__(self, handle: "RsHandle") -> None:
        self._h = handle

    # -- identity ------------------------------------------------------------

    @property
    def uuid(self) -> int:
        return self._h.uuid

    def __eq__(self, other: object) -> bool:
        return isinstance(other, RsSegment) and self._h == other._h

    def __hash__(self) -> int:
        return hash(self._h)

    def __repr__(self) -> str:
        return f"<RsSegment: ({self.type}) {self.raw!r}>"

    def _python_class(self) -> Optional[type]:
        """The Python segment class this node would have been (by class name)."""
        dialect = _current_dialect.get()
        if dialect is None:
            return None
        class_name = self._h.segment_class
        if class_name is None:
            return None
        return _resolve_segment_class(dialect, class_name)

    def __getattr__(self, name: str) -> Any:
        """Delegate methods/properties not on the facade to the segment class.

        Dialect segment classes (e.g. ``FromClauseSegment``) define behaviour
        beyond the generic ``BaseSegment`` interface. Resolve such members on
        the Python class matching this node's ``segment_class`` and run them
        bound to this facade, so their bodies execute against the facade's
        duck-typed primitives without reimplementation.
        """
        # Never delegate dunders — they drive copy/pickle/iteration protocols
        # and must resolve through normal lookup (or genuinely be absent).
        if name.startswith("__") and name.endswith("__"):
            raise AttributeError(name)
        cls = self._python_class()
        if cls is not None:
            # Inspect the raw descriptor (without invoking it) so we bind only
            # plain instance methods; classmethods/staticmethods are already
            # complete, and properties compute against this facade.
            raw = inspect.getattr_static(cls, name, _MISSING)
            if raw is not _MISSING:
                if isinstance(raw, property):
                    return raw.fget(self)
                if isinstance(raw, (classmethod, staticmethod)):
                    return getattr(cls, name)
                # Nested classes/enums (e.g. ObjectReferenceLevel) are callable
                # but must be returned as-is, not bound like a method.
                if isinstance(raw, type):
                    return raw
                if callable(raw):
                    return functools.partial(raw, self)
                return raw
        raise AttributeError(f"'RsSegment' ({self._h.type}) has no attribute {name!r}")

    # -- payload -------------------------------------------------------------

    @property
    def raw(self) -> str:
        return self._h.raw

    @property
    def raw_upper(self) -> str:
        return self._h.raw_upper

    @property
    def type(self) -> str:
        return self._h.type

    def get_type(self) -> str:
        return self._h.type

    @property
    def class_types(self) -> frozenset[str]:
        return frozenset(self._h.class_types())

    @property
    def instance_types(self) -> tuple[str, ...]:
        return tuple(self._h.instance_types())

    @property
    def descendant_type_set(self) -> frozenset[str]:
        return frozenset(self._h.descendant_type_set())

    @property
    def indent_val(self) -> int:
        """Indentation delta: +1 for Indent, -1 for Dedent, else 0."""
        t = self._h.type
        if t == "indent":
            return 1
        if t == "dedent":
            return -1
        return 0

    def iter_segments(
        self, expanding: Optional[list[str]] = None, pass_through: bool = False
    ) -> Iterator["RsSegment"]:
        """Iterate children, optionally expanding some by type."""
        for s in self.segments:
            if expanding and s.is_type(*expanding):
                yield from s.iter_segments(
                    expanding=expanding if pass_through else None
                )
            else:
                yield s

    @property
    def name(self) -> str:
        # ``name`` defaults to the segment type for raw/leaf segments. Rules
        # that rely on richer class-level names are handled in later milestones.
        return self._h.type

    def is_type(self, *seg_type: str) -> bool:
        return self._h.is_type(_norm_types(seg_type))

    def is_raw(self) -> bool:
        return self._h.is_raw()

    @property
    def is_code(self) -> bool:
        return self._h.is_code

    @property
    def is_comment(self) -> bool:
        return self._h.is_comment

    @property
    def is_whitespace(self) -> bool:
        return self._h.is_whitespace

    @property
    def is_meta(self) -> bool:
        return self._h.is_meta

    @property
    def is_templated(self) -> bool:
        return self._h.is_templated

    @property
    def pos_marker(self):  # noqa: ANN201 - RsPositionMarker (duck-typed)
        return self._h.pos_marker

    # -- structure / navigation ---------------------------------------------

    @property
    def segments(self) -> tuple["RsSegment", ...]:
        return tuple(RsSegment(c) for c in self._h.children)

    @property
    def raw_segments(self) -> list["RsSegment"]:
        return [RsSegment(c) for c in self._h.raw_segments()]

    def get_raw_segments(self) -> list["RsSegment"]:
        return self.raw_segments

    @property
    def _code_indices(self) -> tuple[int, ...]:
        return tuple(i for i, s in enumerate(self.segments) if s.is_code)

    @property
    def raw_segments_with_ancestors(
        self,
    ) -> list[tuple["RsSegment", list[PathStep]]]:
        """Raw segments in this segment paired with their ancestor path."""
        buffer: list[tuple["RsSegment", list[PathStep]]] = []
        segments = self.segments
        code_idxs = self._code_indices
        for idx, seg in enumerate(segments):
            new_step = [PathStep(self, idx, len(segments), code_idxs)]
            if seg.is_type("raw"):
                buffer.append((seg, new_step))
            else:
                buffer.extend(
                    (raw_seg, new_step + stack)
                    for raw_seg, stack in seg.raw_segments_with_ancestors
                )
        return buffer

    def select_children(
        self,
        start_seg: Optional["RsSegment"] = None,
        stop_seg: Optional["RsSegment"] = None,
        select_if=None,
        loop_while=None,
    ) -> list["RsSegment"]:
        """Retrieve a filtered subset of children within a range."""
        segments = self.segments
        start_index = segments.index(start_seg) if start_seg else -1
        stop_index = segments.index(stop_seg) if stop_seg else len(segments)
        buff = []
        for seg in segments[start_index + 1 : stop_index]:
            if loop_while and not loop_while(seg):
                break
            if not select_if or select_if(seg):
                buff.append(seg)
        return buff

    # -- RawSegment / meta internals.  Those the arena carries are read from
    #    the handle; the rest (trim_start, source_fixes, casefold) are not yet
    #    threaded into the arena and use safe defaults verified via the oracle.

    @property
    def is_implicit(self) -> bool:
        """Implicit flag for Indent/Dedent metas (absent on other segments)."""
        val = self._h.is_implicit()
        if val is None:
            raise AttributeError("is_implicit")
        return val

    @property
    def trim_chars(self):  # noqa: ANN201
        """Characters to trim from both ends (from the token, else None)."""
        tc = self._h.trim_chars()
        return tuple(tc) if tc is not None else None

    @property
    def quoted_value(self):  # noqa: ANN201
        """Quote-extraction spec ``(pattern, group)`` (None unless quoted)."""
        return self._h.quoted_value()

    @property
    def escape_replacements(self):  # noqa: ANN201
        """Escape replacement pairs (None unless set on the token)."""
        return self._h.escape_replacements()

    @property
    def trim_start(self):  # noqa: ANN201
        """Characters to trim from the start (not yet carried by the arena)."""
        return None

    @property
    def source_fixes(self) -> list:
        """Source fixes carried by this segment (none on the read-only path)."""
        return []

    @property
    def _source_fixes(self) -> tuple:
        return ()

    def raw_normalized(self, casefold: bool = True) -> str:
        """Normalized raw content: quote-stripped + escapes applied.

        Mirrors ``RawSegment.normalize`` using the arena's ``quoted_value`` and
        ``escape_replacements``. Casefolding is not yet threaded into the arena,
        so the ``casefold`` argument is currently a no-op (does not affect rules
        that compare structure/characters rather than case).
        """
        raw_buff = self._h.raw
        qv = self._h.quoted_value()
        if qv:
            pattern, group = qv
            match = re.match(pattern, raw_buff)
            if match:
                try:
                    grp: Union[int, str] = int(group)
                except (ValueError, TypeError):
                    grp = group
                raw_buff = match.group(grp)
        er = self._h.escape_replacements()
        if er:
            for old, new in er:
                raw_buff = re.sub(old, new, raw_buff)
        return raw_buff

    def get_child(self, *seg_type: str) -> Optional["RsSegment"]:
        h = self._h.get_child(_norm_types(seg_type))
        return RsSegment(h) if h is not None else None

    def get_children(self, *seg_type: str) -> list["RsSegment"]:
        return [RsSegment(h) for h in self._h.get_children(_norm_types(seg_type))]

    def recursive_crawl(
        self,
        *seg_type: str,
        recurse_into: bool = True,
        no_recursive_seg_type: Optional[Union[str, list[str]]] = None,
        allow_self: bool = True,
    ) -> Iterator["RsSegment"]:
        if no_recursive_seg_type is None:
            no_recursive: list[str] = []
        elif isinstance(no_recursive_seg_type, str):
            no_recursive = [no_recursive_seg_type]
        else:
            no_recursive = list(no_recursive_seg_type)
        for h in self._h.recursive_crawl(
            _norm_types(seg_type),
            recurse_into,
            no_recursive,
            allow_self,
        ):
            yield RsSegment(h)

    def recursive_crawl_all(self, reverse: bool = False) -> Iterator["RsSegment"]:
        """Yield self and all descendants (mirrors BaseSegment ordering)."""
        if reverse:
            for seg in reversed(self.segments):
                yield from seg.recursive_crawl_all(reverse=True)
            yield self
        else:
            for h in self._h.recursive_crawl_all():
                yield RsSegment(h)

    def get_parent(self) -> Optional[tuple["RsSegment", int]]:
        gp = self._h.get_parent()
        if gp is None:
            return None
        handle, idx = gp
        return RsSegment(handle), idx

    def path_to(self, other: "RsSegment") -> list[PathStep]:
        steps = self._h.path_to(other._h)
        return [
            PathStep(RsSegment(handle), idx, length, tuple(code_idxs))
            for (handle, idx, length, code_idxs) in steps
        ]
