"""Implementation of Rule LT02."""

import os
from typing import Optional

from sqlfluff.core.rules import BaseRule, LintResult, RuleContext
from sqlfluff.core.rules.crawlers import RootOnlyCrawler
from sqlfluff.utils.reflow.config import ReflowConfig
from sqlfluff.utils.reflow.helpers import results_from_rust
from sqlfluff.utils.reflow.sequence import ReflowSequence

try:
    import sqlfluffrs

    _HAS_RUST_REINDENT = hasattr(sqlfluffrs, "reflow_reindent")
except ImportError:  # pragma: no cover
    _HAS_RUST_REINDENT = False


class Rule_LT02(BaseRule):
    """Incorrect Indentation.

    **Anti-pattern**

    The ``•`` character represents a space and the ``→`` character represents a tab.
    In this example, the third line contains five spaces instead of four and
    the second line contains two spaces and one tab.

    .. code-block:: sql
       :force:

        SELECT
        ••→a,
        •••••b
        FROM foo


    **Best practice**

    Change the indentation to use a multiple of four spaces. This example also
    assumes that the ``indent_unit`` config value is set to ``space``. If it
    had instead been set to ``tab``, then the indents would be tabs instead.

    .. code-block:: sql
       :force:

        SELECT
        ••••a,
        ••••b
        FROM foo

    """

    name = "layout.indent"
    # NOTE: We're combining three legacy rules here into one.
    aliases = ("L002", "L003", "L004")
    groups = ("all", "core", "layout")
    crawl_behaviour = RootOnlyCrawler()
    _rebuild_rust_arena = True
    is_fix_compatible = True
    targets_templated = True
    template_safe_fixes = True
    _adjust_anchors = True

    def _eval_rust(self, context: RuleContext) -> Optional[list[LintResult]]:
        """SPIKE: reindent in Rust over the arena. See ``BaseRule._eval_rust``.

        Returns ``None`` (Python path) when the extension has no reindent, or
        when Rust reports a case it doesn't cover (e.g. a templated file).
        """
        if not _HAS_RUST_REINDENT or os.environ.get("SQLFLUFF_NO_RUST_LT02"):
            return None
        rs_tree = getattr(context.segment, "_rs_tree", None) or getattr(
            context.segment, "_rs_tree_rebuilt", None
        )
        if rs_tree is None:
            return None  # pragma: no cover
        # Source fixes from an earlier pass aren't in the arena.
        templated_file = context.templated_file
        if (
            templated_file
            and templated_file.source_str != templated_file.templated_str
            and context.segment.source_fixes
        ):
            return None
        reflow_config = ReflowConfig.from_fluff_config(context.config)
        layout = {
            seg_type: {k: str(v) for k, v in cfg.items() if v is not None}
            for seg_type, cfg in reflow_config._config_dict.items()
        }
        found = sqlfluffrs.reflow_reindent(  # type: ignore[attr-defined]
            rs_tree,
            layout,
            str(reflow_config.indent_unit),
            int(reflow_config.tab_space_size),
            str(reflow_config.implicit_indents),
            bool(reflow_config.ignore_comment_lines),
            list(reflow_config.skip_indentation_in),
            list(reflow_config.skip_implicit_indents_in),
        )
        if found is None:
            return None
        return results_from_rust(context.segment.raw_segments, found)

    def _eval(self, context: RuleContext) -> list[LintResult]:
        """Indentation not consistent with previous lines.

        To set the default tab size, set the `tab_space_size` value
        in the appropriate configuration. To correct indents to tabs
        use the `indent_unit` value set to `tab`.

        """
        return (
            ReflowSequence.from_root(context.segment, context.config)
            .reindent()
            .get_results()
        )
