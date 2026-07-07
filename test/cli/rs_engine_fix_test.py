"""Wiring tests for the experimental Rust-engine façade fix fast path (stdin).

Exercises ``_try_facade_stdin_fix``: it must produce native-identical fixes when
all selected rules are façade-safe and the engine is enabled, and fall back
(return ``None``) otherwise. Skipped unless the ``sqlfluffrs`` engine is built.
"""

import pytest

try:
    import sqlfluffrs

    _HAS_ENGINE = hasattr(sqlfluffrs, "engine_parse_to_tree")
except ImportError:  # pragma: no cover
    _HAS_ENGINE = False

from sqlfluff.cli.commands import _try_facade_stdin_fix
from sqlfluff.core import FluffConfig, Linter

pytestmark = pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)


def _linter(rules: str, engine: str = "true", dialect: str = "ansi") -> Linter:
    return Linter(
        config=FluffConfig(
            overrides={
                "dialect": dialect,
                "rules": rules,
                "use_rust_engine": engine,
            }
        )
    )


@pytest.mark.parametrize(
    "src,rule,expected",
    [
        ("SeLeCt 1 from b\n", "CP01", "SELECT 1 FROM b\n"),
        ("select 1 from b\n", "CP01", "select 1 from b\n"),  # clean -> unchanged
    ],
)
def test_facade_stdin_fix_fast_path(src: str, rule: str, expected: str) -> None:
    """Fast path fixes correctly (matching the fixture expectation)."""
    assert _try_facade_stdin_fix(_linter(rule), src, None) == expected


def test_facade_stdin_fix_falls_back(monkeypatch) -> None:
    """Fall back to the Python path when the façade can't safely cover it."""
    # Non-façade-safe rule. Every shipped fixable rule is now in
    # FACADE_SAFE_RULES, so simulate a not-yet-vetted rule (e.g. from a
    # plugin) by removing one from the set.
    import sqlfluff.core.rules.rs_lint as rs_lint

    monkeypatch.setattr(
        rs_lint, "FACADE_SAFE_RULES", rs_lint.FACADE_SAFE_RULES - {"LT01"}
    )
    assert _try_facade_stdin_fix(_linter("LT01"), "select  1 from b\n", None) is None
    monkeypatch.undo()
    # Engine disabled.
    assert (
        _try_facade_stdin_fix(_linter("CP01", engine="false"), "SeLeCt 1\n", None)
        is None
    )
    # noqa directives are not applied on the façade path.
    assert _try_facade_stdin_fix(_linter("CP01"), "SeLeCt 1 -- noqa\n", None) is None
    # A templated source (jinja) routes to native: the façade path can't
    # observe templater violations (e.g. undefined variables must abort).
    assert (
        _try_facade_stdin_fix(
            _linter("CP01"), "create TABLE {{ params.undefined_var }}.t (a int)\n", None
        )
        is None
    )
    # An unparsable source routes to native (PRS violations, non-zero exit).
    assert (
        _try_facade_stdin_fix(_linter("CP01"), "select select select\n", None) is None
    )
