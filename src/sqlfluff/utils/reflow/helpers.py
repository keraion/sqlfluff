"""Helper utilities for reflow."""

import logging
from collections.abc import Iterable
from itertools import chain
from typing import Any

from sqlfluff.core.parser import (
    BaseSegment,
    NewlineSegment,
    RawSegment,
    WhitespaceSegment,
)
from sqlfluff.core.rules import LintFix, LintResult

# We're in the utils module, but users will expect reflow
# logs to appear in the context of rules. Hence it's a subset
# of the rules logger.
reflow_logger = logging.getLogger("sqlfluff.rules.reflow")


def fixes_from_results(results: Iterable[LintResult]) -> list[LintFix]:
    """Return a list of fixes from an iterable of LintResult."""
    return list(chain.from_iterable(result.fixes for result in results))


def pretty_segment_name(segment: BaseSegment) -> str:
    """Get a nicely formatted name of the segment."""
    if segment.is_type("symbol"):
        # In a symbol reference, show the raw value and type.
        # (With underscores as spaces)
        return segment.get_type().replace("_", " ") + f" {segment.raw!r}"
    elif segment.is_type("keyword"):
        # Reference keywords as keywords.
        return f"{segment.raw!r} keyword"
    else:
        # Reference other segments just by their type.
        # (With underscores as spaces)
        return segment.get_type().replace("_", " ")


def deduce_line_indent(raw_segment: RawSegment, root_segment: BaseSegment) -> str:
    """Given a raw segment, deduce the indent of its line."""
    seg_idx = root_segment.raw_segment_index(raw_segment)
    indent_seg = None
    # Use range and a lookup here because it's more efficient than slicing
    # as we only need a subset of the long series.
    for idx in range(seg_idx, -1, -1):
        seg = root_segment.raw_segments[idx]
        if seg.is_code:
            indent_seg = None
        elif seg.is_type("whitespace"):
            indent_seg = seg
        elif seg.is_type("newline"):
            break
    reflow_logger.debug("Deduced indent for %s as %s", raw_segment, indent_seg)
    return indent_seg.raw if indent_seg else ""


def results_from_rust(raws: list[RawSegment], found: list[Any]) -> list[LintResult]:
    """SPIKE: build LintResults from the Rust reflow's compact results.

    Each result is ``(anchor, description, source, fixes)`` and each fix is
    ``(kind, target, text, extra)``, with segments given as indices into
    ``raws`` (the root segment's ``raw_segments``).
    """
    results = []
    for anchor_idx, description, source, fix_specs in found:
        fixes = []
        for kind, target_idx, text, extra_idx in fix_specs:
            target = raws[target_idx]
            if kind == "delete":
                fixes.append(LintFix.delete(target))
            elif kind == "replace_whitespace":
                fixes.append(LintFix.replace(target, [target.edit(text)]))
            elif kind == "indent_after_newline":
                fixes.append(LintFix.replace(target, [target, WhitespaceSegment(text)]))
            elif kind == "create_before":
                new_segs: list[BaseSegment] = [NewlineSegment()]
                if text:
                    new_segs.append(WhitespaceSegment(text))
                fixes.append(LintFix.create_before(target, new_segs))
            elif kind == "replace_with_newline":
                new_segs = [NewlineSegment()]
                if text:
                    new_segs.append(target.edit(text))
                fixes.append(LintFix.replace(target, new_segs))
            elif kind == "create_after":
                new_segs = [NewlineSegment()]
                if text:
                    new_segs.append(WhitespaceSegment(text))
                fixes.append(LintFix.create_after(target, new_segs))
            elif kind == "create_space_after":
                fixes.append(LintFix("create_after", target, [WhitespaceSegment()]))
            elif kind == "create_space_before":
                fixes.append(LintFix("create_before", target, [WhitespaceSegment()]))
            else:  # move_comment_before
                new_segs = [raws[extra_idx], NewlineSegment()]
                if text:
                    new_segs.append(WhitespaceSegment(text))
                fixes.append(LintFix.create_before(target, new_segs))
        results.append(
            LintResult(
                raws[anchor_idx],
                fixes,
                description=description,
                source=source or None,
            )
        )
    return results
