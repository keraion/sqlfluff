"""Implementation of Rule LT01."""

import os
from typing import Optional

from sqlfluff.core.parser import WhitespaceSegment
from sqlfluff.core.rules import BaseRule, LintFix, LintResult, RuleContext
from sqlfluff.core.rules.crawlers import RootOnlyCrawler
from sqlfluff.utils.reflow.config import ReflowConfig
from sqlfluff.utils.reflow.sequence import ReflowSequence

try:
    import sqlfluffrs

    _HAS_RUST_RESPACE = hasattr(sqlfluffrs, "reflow_respace")
except ImportError:  # pragma: no cover
    _HAS_RUST_RESPACE = False


class Rule_LT01(BaseRule):
    """Inappropriate Spacing.

    This rule checks for an enforces the spacing as configured in
    :ref:`layoutconfig`. This includes excessive whitespace,
    trailing whitespace at the end of a line and also the wrong
    spacing between elements on the line. Because of this wide reach
    you may find that you wish to add specific configuration in your
    project to tweak how specific elements are treated. Rather than
    configuration on this specific rule, use the `sqlfluff.layout`
    section of your configuration file to customise how this rule
    operates.

    The ``•`` character represents a space in the examples below.

    **Anti-pattern**

    .. code-block:: sql
        :force:

        SELECT
            a,        b(c) as d••
        FROM foo••••
        JOIN bar USING(a)

    **Best practice**

    * Unless an indent or preceding a comment, whitespace should
      be a single space.

    * There should also be no trailing whitespace at the ends of lines.

    * There should be a space after :code:`USING` so that it's not confused
      for a function.

    .. code-block:: sql

        SELECT
            a, b(c) as d
        FROM foo
        JOIN bar USING (a)
    """

    name = "layout.spacing"
    # NOTE: This rule combines the following legacy rules:
    # - L001: Trailing Whitespace
    # - L005 & L008: Space around commas
    # - L006: Space around operators
    # - L023: Space after AS in WITH clause
    # - L024: Space immediately after USING
    # - L039: Unnecessary Whitespace
    # - L048: Spacing around quoted literals
    # - L071: Spacing around brackets
    aliases = ("L001", "L005", "L006", "L008", "L023", "L024", "L039", "L048", "L071")
    groups = ("all", "core", "layout")
    crawl_behaviour = RootOnlyCrawler()
    _rebuild_rust_arena = True
    is_fix_compatible = True

    def _eval_rust(self, context: RuleContext) -> Optional[list[LintResult]]:
        """SPIKE: respace in Rust over the arena. See ``BaseRule._eval_rust``.

        Returns ``None`` (Python path) when the extension has no respace, or
        when Rust reports a case it doesn't cover.
        """
        if not _HAS_RUST_RESPACE or os.environ.get("SQLFLUFF_NO_RUST_LT01"):
            return None
        rs_tree = getattr(context.segment, "_rs_tree", None) or getattr(
            context.segment, "_rs_tree_rebuilt", None
        )
        if rs_tree is None:
            return None  # pragma: no cover
        reflow_config = ReflowConfig.from_fluff_config(context.config)
        layout = {
            seg_type: {k: str(v) for k, v in cfg.items() if v is not None}
            for seg_type, cfg in reflow_config._config_dict.items()
        }
        found = sqlfluffrs.reflow_respace(  # type: ignore[attr-defined]
            rs_tree,
            layout,
            str(reflow_config.indent_unit),
            int(reflow_config.tab_space_size),
        )
        if found is None:
            return None
        raws = context.segment.raw_segments
        results = []
        for anchor_idx, description, kind, fix_idx, edit in found:
            target = raws[fix_idx]
            if kind == "delete":
                fix = LintFix.delete(target)
            elif kind == "replace":
                fix = LintFix("replace", anchor=target, edit=[target.edit(edit)])
            else:
                fix = LintFix(kind, anchor=target, edit=[WhitespaceSegment()])
            results.append(LintResult(raws[anchor_idx], [fix], description=description))
        return results

    def _eval(self, context: RuleContext) -> Optional[list[LintResult]]:
        """Unnecessary whitespace."""
        sequence = ReflowSequence.from_root(context.segment, config=context.config)
        return sequence.respace().get_results()
