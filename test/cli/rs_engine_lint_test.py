"""Wiring tests for the experimental Rust-engine façade lint fast path.

Exercises ``_try_facade_stdin_lint`` / ``_try_facade_paths_lint``: they must
produce native-identical violations when all selected rules are façade-safe
and the engine is enabled, and fall back (return ``None`` / route files to
``remaining``) otherwise. Skipped unless the ``sqlfluffrs`` engine is built.
"""

import pytest

try:
    import sqlfluffrs

    _HAS_ENGINE = hasattr(sqlfluffrs, "engine_parse_to_tree")
except ImportError:  # pragma: no cover
    _HAS_ENGINE = False

from sqlfluff.cli.commands import _try_facade_paths_lint, _try_facade_stdin_lint
from sqlfluff.cli.formatters import OutputStreamFormatter
from sqlfluff.cli.outputstream import make_output_stream
from sqlfluff.core import FluffConfig, Linter
from sqlfluff.core.errors import SQLLintError

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


def _native_violation_tuples(linter_rules: str, src: str) -> list[tuple]:
    config = FluffConfig(overrides={"dialect": "ansi", "rules": linter_rules})
    return [
        (v.rule_code(), v.line_no, v.line_pos, v.description)
        for v in Linter(config=config).lint_string(src).violations
        if isinstance(v, SQLLintError)
    ]


@pytest.mark.parametrize(
    "src,rule",
    [
        ("SeLeCt 1 from b\n", "CP01"),  # violations found
        ("SELECT 1 FROM b\n", "CP01"),  # clean
        ("select * from tbl\n", "AM04"),  # lint-only rule
    ],
)
def test_facade_stdin_lint_fast_path(src: str, rule: str) -> None:
    """Fast path lints with native-identical violations."""
    result = _try_facade_stdin_lint(_linter(rule), src, None)
    assert result is not None
    records = result.as_records()
    fac = [
        (v["code"], v["start_line_no"], v["start_line_pos"], v["description"])
        for record in records
        for v in record["violations"]
    ]
    assert fac == _native_violation_tuples(rule, src)


def test_facade_stdin_lint_falls_back(monkeypatch) -> None:
    """Fall back to the Python path when the façade can't safely cover it."""
    # Engine disabled.
    assert (
        _try_facade_stdin_lint(_linter("CP01", engine="false"), "SeLeCt 1\n", None)
        is None
    )
    # noqa directives are not applied on the façade path.
    assert _try_facade_stdin_lint(_linter("CP01"), "SeLeCt 1 -- noqa\n", None) is None
    # Templated source routes to native (templater violations).
    assert (
        _try_facade_stdin_lint(
            _linter("CP01"), "create TABLE {{ params.undefined_var }}.t (a int)\n", None
        )
        is None
    )
    # Unparsable source routes to native (PRS violations, non-zero exit).
    assert (
        _try_facade_stdin_lint(_linter("CP01"), "select select select\n", None) is None
    )
    # Non-façade-safe rule (simulated: every shipped rule is now in the set).
    import sqlfluff.core.rules.rs_lint as rs_lint

    monkeypatch.setattr(
        rs_lint, "FACADE_SAFE_RULES", rs_lint.FACADE_SAFE_RULES - {"CP01"}
    )
    assert _try_facade_stdin_lint(_linter("CP01"), "SeLeCt 1\n", None) is None


def test_facade_paths_lint_routing(tmp_path) -> None:
    """Literal files are façade-linted; templated/unparsable route to native."""
    (tmp_path / "dirty.sql").write_text("SeLeCt col from tbl\n")
    (tmp_path / "clean.sql").write_text("SELECT col FROM tbl\n")
    (tmp_path / "templated.sql").write_text("select {{ x }} from tbl\n")
    (tmp_path / "unparsable.sql").write_text("select select select\n")

    linter = _linter("CP01")
    config = linter.config
    formatter = OutputStreamFormatter(make_output_stream(config), False, verbosity=0)
    result = _try_facade_paths_lint(linter, formatter, (str(tmp_path),), True)
    assert result is not None
    dirs, remaining = result
    handled = sorted(
        record["filepath"].rsplit("/", 1)[-1]
        for linted_dir in dirs
        for record in linted_dir.as_records()
    )
    assert handled == ["clean.sql", "dirty.sql"]
    assert sorted(p.rsplit("/", 1)[-1] for p in remaining) == [
        "templated.sql",
        "unparsable.sql",
    ]
    # The dirty file's violations match native.
    records = [
        record
        for linted_dir in dirs
        for record in linted_dir.as_records()
        if record["filepath"].endswith("dirty.sql")
    ]
    assert [v["code"] for v in records[0]["violations"]] == ["CP01", "CP01"]
