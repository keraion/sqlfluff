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
    assert _try_facade_stdin_fix(_linter(rule), src, None) == (expected, 0)


def test_facade_stdin_fix_reports_unfixable() -> None:
    """Unfixable violations complete on the fast path, with a count.

    A lint-only finding (AM04: unknown result columns from ``*``) can't be
    fixed by anyone — deferring to native would just re-produce the same
    output, so the fast path finishes the file and returns the unfixable
    count for the caller's "Unfixable violations detected." + failure exit.
    """
    src = "select * from tbl\n"
    result = _try_facade_stdin_fix(_linter("AM04"), src, None)
    assert result == (src, 1)


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
    # noqa directives are handled ON the façade path now: the masked CP01's
    # fix is NOT applied, exactly like native.
    noqa_result = _try_facade_stdin_fix(_linter("CP01"), "SeLeCt 1 -- noqa\n", None)
    assert noqa_result is not None
    fixed, num_unfixable = noqa_result
    assert fixed == "SeLeCt 1 -- noqa\n"  # masked -> unchanged
    assert num_unfixable == 0
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


def test_facade_stdin_fix_completes_gave_up_files() -> None:
    """Files where the loop gave up on a fix complete on the fast path.

    Native's bookkeeping for a grammar-rejected or loop-detected fix is to
    keep the violation (counted fixable), write what DID apply, and not fail
    the exit code — the façade loop gives up in exactly the same places, so
    the output is final and deferring to native would just re-produce it.
    ``ansi/modulo.sql`` is such a file (LT02 fixes get grammar-rejected).
    """
    src = open("test/fixtures/dialects/ansi/modulo.sql").read()
    from sqlfluff.core.rules.rs_lint import FACADE_SAFE_RULES

    rules = ",".join(sorted(FACADE_SAFE_RULES))
    result = _try_facade_stdin_fix(_linter(rules), src, None)
    assert result is not None
    fixed, num_unfixable = result
    config = FluffConfig(overrides={"dialect": "ansi", "rules": rules})
    native = Linter(config=config).lint_string(src, fix=True).fix_string()[0]
    assert fixed == native
    assert num_unfixable == 0  # gave-up fixes stay "fixable", like native


def test_facade_stdin_fix_defers_on_runaway() -> None:
    """A loop-limit (runaway) revert defers to native.

    Native's runaway bookkeeping differs from ordinary gave-up fixes: it
    strips the fixes from every reported violation (all become unfixable and
    the exit code fails) — so the fast path must not present the reverted
    source as a final result.
    """
    linter = Linter(
        config=FluffConfig(
            overrides={
                "dialect": "ansi",
                "rules": "CP01",
                "use_rust_engine": "true",
                "runaway_limit": 1,
            }
        )
    )
    assert _try_facade_stdin_fix(linter, "select a FROM tbl\n", None) is None


def test_format_command_uses_facade_fast_path(tmp_path, monkeypatch) -> None:
    """`sqlfluff format` engages the façade fix fast path.

    format shares _stdin_fix/_paths_fix with fix and its forced ruleset is
    entirely façade-safe, so it fast-paths BY CONSTRUCTION — this test pins
    that: with the engine on, the native fixing path must never run.
    Verified separately: whole-testbed format output is byte-identical
    engine-on vs native.
    """
    from click.testing import CliRunner

    from sqlfluff.cli.commands import cli_format

    (tmp_path / ".sqlfluff").write_text(
        "[sqlfluff]\ndialect = ansi\nuse_rust_engine = True\nuse_rust_parser = True\n"
    )
    src = "select a ,  b from tbl\n"
    f = tmp_path / "dirty.sql"
    f.write_text(src)

    # If the façade hands ANY file back to native, these raise and the
    # command exits non-zero.
    def _boom(*a, **k):  # pragma: no cover
        raise AssertionError("format must not reach the native fixing path")

    monkeypatch.setattr(Linter, "lint_paths", _boom)
    monkeypatch.setattr(Linter, "lint_string_wrapped", _boom)

    formatted = "select\n    a,\n    b\nfrom tbl\n"
    result = CliRunner().invoke(cli_format, [str(f)])
    assert result.exit_code == 0, result.output
    assert f.read_text() == formatted

    # stdin flavour: output on stdout, same guarantee. (Explicit dialect:
    # stdin doesn't see tmp_path's .sqlfluff, and no-dialect bails the gate.)
    result = CliRunner().invoke(cli_format, ["--dialect", "ansi", "-"], input=src)
    assert result.exit_code == 0, result.output
    assert result.output == formatted


def test_facade_fix_plugin_rule_opt_in() -> None:
    """A ``rust_compatible`` plugin rule is eligible for the FIX fast path.

    Example_L001 (lint-only here — its finding is unfixable) plus CP01: the
    run engages the fast path, CP01's fixes apply, and the plugin finding
    counts as unfixable — byte- and count-identical to native.
    """
    src = "select a from foo order by bar\n"
    linter = _linter("Example_L001,CP01")
    result = _try_facade_stdin_fix(linter, src, None)
    assert result is not None  # flagged plugin rule doesn't disqualify
    fixed, num_unfixable = result

    nat_cfg = FluffConfig(overrides={"dialect": "ansi", "rules": "Example_L001,CP01"})
    nat_res = Linter(config=nat_cfg).lint_string(src, fix=True)
    assert fixed == nat_res.fix_string()[0]
    assert num_unfixable == 1  # the Example_L001 finding has no fixes


def test_facade_fix_warnings_config_matches_native() -> None:
    """[sqlfluff:warnings] demotion works on the fix fast path.

    A warned rule's violations drop out of the unfixable count (no failure
    exit) but its fixes still apply — exactly like native.
    """
    src = "SeLeCt * from tbl\n"  # AM04: unfixable; CP01: fixable
    linter = Linter(
        config=FluffConfig(
            overrides={
                "dialect": "ansi",
                "rules": "AM04,CP01",
                "warnings": "AM04",
                "use_rust_engine": "true",
            }
        )
    )
    result = _try_facade_stdin_fix(linter, src, None)
    assert result is not None  # warnings config no longer routes to native
    fixed, num_unfixable = result
    nat = (
        Linter(config=FluffConfig(overrides={"dialect": "ansi", "rules": "AM04,CP01"}))
        .lint_string(src, fix=True)
        .fix_string()[0]
    )
    assert fixed == nat  # CP01 fixes still applied
    assert fixed != src
    assert num_unfixable == 0  # the AM04 finding is demoted to a warning

    # Same config WITHOUT the demotion: the finding counts.
    linter2 = _linter("AM04,CP01")
    result2 = _try_facade_stdin_fix(linter2, src, None)
    assert result2 is not None
    assert result2[1] == 1


def test_fix_check_mode_uses_facade_and_defers_writes(tmp_path) -> None:
    """`fix --check` engages the fast path; writes wait for confirmation."""
    from click.testing import CliRunner

    from sqlfluff.cli.commands import fix

    (tmp_path / ".sqlfluff").write_text(
        "[sqlfluff]\ndialect = ansi\nuse_rust_engine = True\nuse_rust_parser = True\n"
    )
    src = "select a ,  b from tbl\n"
    f = tmp_path / "dirty.sql"

    # Decline: nothing written.
    f.write_text(src)
    result = CliRunner().invoke(fix, ["--check", "--rules", "LT01", str(f)], input="n")
    assert "Aborting" in result.output
    assert f.read_text() == src

    # Confirm: the façade's held-back fix is written.
    f.write_text(src)
    result = CliRunner().invoke(fix, ["--check", "--rules", "LT01", str(f)], input="y")
    assert result.exit_code == 0, result.output
    assert f.read_text() == "select a, b from tbl\n"


def test_fix_show_lint_violations_renders_facade_records(tmp_path) -> None:
    """--show-lint-violations renders unfixables from façade-handled files."""
    from click.testing import CliRunner

    from sqlfluff.cli.commands import fix

    (tmp_path / ".sqlfluff").write_text(
        "[sqlfluff]\ndialect = ansi\nuse_rust_engine = True\nuse_rust_parser = True\n"
    )
    # AM04 (unfixable) + LT01 (fixable) in one file.
    f = tmp_path / "mixed.sql"
    f.write_text("select  * from tbl\n")

    result = CliRunner().invoke(
        fix,
        ["--show-lint-violations", "--rules", "AM04,LT01", str(f)],
    )
    assert "==== lint for unfixable violations ====" in result.output
    assert "AM04" in result.output  # the unfixable finding is rendered
    assert f.read_text() == "select * from tbl\n"  # LT01 fix applied
