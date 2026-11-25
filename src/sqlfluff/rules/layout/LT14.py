"""Implementation of Rule LT14."""

import os
from typing import Optional

from sqlfluff.core.rules import BaseRule, LintResult, RuleContext
from sqlfluff.core.rules.crawlers import RootOnlyCrawler
from sqlfluff.utils.reflow.config import ReflowConfig
from sqlfluff.utils.reflow.helpers import results_from_rust
from sqlfluff.utils.reflow.sequence import ReflowSequence

try:
    import sqlfluffrs

    _HAS_RUST_KEYWORD_REBREAK = hasattr(sqlfluffrs, "reflow_rebreak_keywords")
except ImportError:  # pragma: no cover
    _HAS_RUST_KEYWORD_REBREAK = False


class Rule_LT14(BaseRule):
    """Keyword clauses should follow a standard for being before/after newlines.

    **Anti-pattern**

    In this example, the keyword are not at the beginning of or alone on the line.

    .. code-block:: sql

        SELECT 'a' AS col FROM tab WHERE x = 4 ORDER BY y LIMIT 5

    **Best practice**

    .. code-block:: sql

        SELECT 'a' AS col
        FROM tab
        WHERE x = 4
        ORDER BY y
        LIMIT 5

    .. code-block:: sql

        SELECT 'a' AS col
        FROM
            tab
        WHERE
            x = 4
        ORDER BY
            y
        LIMIT
            5

    """

    name = "layout.keyword_newline"
    groups = ("all", "layout")
    crawl_behaviour = RootOnlyCrawler()
    _rebuild_rust_arena = True
    is_fix_compatible = True

    def _eval_rust(self, context: RuleContext) -> Optional[list[LintResult]]:
        """SPIKE: keyword rebreak in Rust. See ``BaseRule._eval_rust``.

        Returns ``None`` (Python path) when the extension has no support or
        Rust reports a case it doesn't cover (e.g. a templated file).
        """
        if not _HAS_RUST_KEYWORD_REBREAK or os.environ.get("SQLFLUFF_NO_RUST_LT14"):
            return None
        rs_tree = getattr(context.segment, "_rs_tree", None) or getattr(
            context.segment, "_rs_tree_rebuilt", None
        )
        if rs_tree is None:
            return None  # pragma: no cover
        layout = {
            seg_type: {k: str(v) for k, v in cfg.items() if v is not None}
            for seg_type, cfg in ReflowConfig.from_fluff_config(
                context.config
            )._config_dict.items()
        }
        found = sqlfluffrs.reflow_rebreak_keywords(rs_tree, layout)  # type: ignore[attr-defined]
        if found is None:
            return None
        return results_from_rust(context.segment.raw_segments, found)

    def _eval(self, context: RuleContext) -> Optional[list[LintResult]]:
        """Keyword clauses should begin on a newline."""
        return (
            ReflowSequence.from_root(context.segment, config=context.config)
            .rebreak("keywords")
            .get_results()
        )
