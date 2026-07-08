"""Tests for the Python-API façade fast path (``Linter.lint_string``)."""

import pytest

try:
    import sqlfluffrs

    _HAS_ENGINE = hasattr(sqlfluffrs, "engine_parse_to_tree")
except ImportError:  # pragma: no cover
    _HAS_ENGINE = False

import sqlfluff
from sqlfluff.core import FluffConfig, Linter
from sqlfluff.core.errors import SQLLintError

pytestmark = pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)


def _cfg(rust: bool, **extra) -> FluffConfig:
    return FluffConfig(
        overrides={
            "dialect": "ansi",
            "use_rust_engine": rust,
            "use_rust_parser": rust,
            "use_rust_rules": rust,
            **extra,
        }
    )


def test_api_lint_string_fast_path_engages(monkeypatch) -> None:
    """lint_string takes the façade path: the native parse never runs."""
    src = "SeLeCt a ,  b from tbl\n"
    linter = Linter(config=_cfg(True, rules="CP01,LT01"))

    def _boom(*a, **k):  # pragma: no cover
        raise AssertionError("fast path must not reach parse_string")

    monkeypatch.setattr(Linter, "parse_string", _boom)
    linted = linter.lint_string(src)
    monkeypatch.undo()

    nat = Linter(config=_cfg(False, rules="CP01,LT01")).lint_string(src)
    keys = lambda lf: sorted(  # noqa: E731
        (v.rule_code(), v.line_no, v.line_pos, v.description)
        for v in lf.violations
        if isinstance(v, SQLLintError)
    )
    assert keys(linted) == keys(nat)
    assert linted.check_tuples() == nat.check_tuples()


def test_api_lint_string_fix_mode_byte_parity() -> None:
    """lint_string(fix=True).fix_string() is byte-identical to native."""
    src = "SeLeCt a ,  b from tbl -- noqa: LT01\nselect c ,  d from tbl\n"
    fac = (
        Linter(config=_cfg(True, rules="CP01,LT01"))
        .lint_string(src, fix=True)
        .fix_string()
    )
    nat = (
        Linter(config=_cfg(False, rules="CP01,LT01"))
        .lint_string(src, fix=True)
        .fix_string()
    )
    assert fac == nat  # (fixed_string, changed) tuple


def test_api_simple_lint_and_fix_parity() -> None:
    """The simple API (sqlfluff.lint / sqlfluff.fix) matches engine on/off."""
    src = "SeLeCt a ,  b from tbl\n"
    # sqlfluff.lint/fix take config overrides via a FluffConfig.
    on = sqlfluff.lint(src, config=_cfg(True, rules="CP01,LT01"))
    off = sqlfluff.lint(src, config=_cfg(False, rules="CP01,LT01"))
    assert on == off

    fixed_on = sqlfluff.fix(src, config=_cfg(True, rules="CP01,LT01"))
    fixed_off = sqlfluff.fix(src, config=_cfg(False, rules="CP01,LT01"))
    assert fixed_on == fixed_off


def test_api_custom_templater_routes_to_native() -> None:
    """A custom templater INSTANCE on the linter bypasses the fast path.

    Native rendering honours ``linter.templater``; the engine renders via
    ``config.get_templater()`` — so custom instances must route to native or
    they'd be silently ignored.
    """
    from sqlfluff.core.templaters import RawTemplater

    class TattleTemplater(RawTemplater):
        name = "tattle"
        called = False

        def process_with_variants(self, *, in_str, fname, config=None, formatter=None):
            type(self).called = True
            yield from super().process_with_variants(
                in_str=in_str, fname=fname, config=config, formatter=formatter
            )

    linter = Linter(config=_cfg(True, rules="CP01"))
    templater = TattleTemplater()
    linter.templater = templater
    linter.config._configs["core"]["templater_obj"] = templater
    linted = linter.lint_string("SeLeCt 1\n")
    assert TattleTemplater.called  # the custom instance actually rendered
    assert [v.rule_code() for v in linted.violations] == ["CP01"]


def test_api_formatter_pins_native(monkeypatch) -> None:
    """A linter WITH a formatter never takes the API fast path.

    The native path dispatches per-file output through the formatter; the
    fast path would silently skip that, so it's API-(formatter-less)-only.
    """
    import sqlfluff.core.rules.rs_lint as rs_lint

    linter = Linter(config=_cfg(True, rules="CP01"))
    from unittest import mock

    linter.formatter = mock.MagicMock()  # any non-None formatter

    def _boom(*a, **k):  # pragma: no cover
        raise AssertionError("formatter-carrying linter must not fast-path")

    monkeypatch.setattr(rs_lint, "try_facade_lint_string", _boom)
    linted = linter.lint_string("SeLeCt 1\n")
    assert [v.rule_code() for v in linted.violations] == ["CP01"]
