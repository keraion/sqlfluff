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
    # noqa directives are handled ON the façade path now: the masked CP01
    # is dropped, exactly like native.
    noqa_result = _try_facade_stdin_lint(_linter("CP01"), "SeLeCt 1 -- noqa\n", None)
    assert noqa_result is not None
    assert not [v for v in noqa_result.get_violations() if isinstance(v, SQLLintError)]
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
    # Non-façade-safe rule (simulated: every shipped rule is now in the
    # set): no longer routes the run to native — it's native-crawled and
    # merged (and promoted, once it fires and byte-matches the façade).
    import sqlfluff.core.rules.rs_lint as rs_lint

    monkeypatch.setattr(
        rs_lint, "FACADE_SAFE_RULES", rs_lint.FACADE_SAFE_RULES - {"CP01"}
    )
    monkeypatch.setitem(rs_lint._UNKNOWN_RULE_VERDICTS, ("CP01", "ansi"), False)
    unknown_result = _try_facade_stdin_lint(_linter("CP01"), "SeLeCt 1\n", None)
    assert unknown_result is not None
    assert [
        (v["code"], v["start_line_no"])
        for record in unknown_result.as_records()
        for v in record["violations"]
    ] == [("CP01", 1)]
    # monkeypatch.setitem restores the pinned verdict on teardown; pop the
    # promotion the run would otherwise leak is unnecessary since we pinned.


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


def test_facade_lint_templated_eligibility(tmp_path) -> None:
    """Templated sources are façade-eligible; violation-bearing renders route.

    A clean jinja render lints on the fast path (native-identical result);
    a render with templater violations (undefined variable) must return
    ``None`` so native reports the TMP violation.
    """
    from sqlfluff.cli.commands import _facade_lint_file
    from sqlfluff.core import FluffConfig

    def cfg(rust: bool) -> FluffConfig:
        return FluffConfig(
            overrides={
                "dialect": "ansi",
                "templater": "jinja",
                "rules": "CP01",
                "use_rust_engine": rust,
                "use_rust_parser": rust,
                "use_rust_rules": rust,
            }
        )

    clean = "{% set t = 'tbl' %}select a from {{ t }}\n"
    c = cfg(True)
    linted = _facade_lint_file(clean, "clean.sql", c, Linter(config=c))
    assert linted is not None  # templated but eligible
    fac = sorted(
        (v.rule_code(), v.line_no, v.line_pos)
        for v in linted.violations
        if isinstance(v, SQLLintError)
    )
    nat = sorted(
        (v.rule_code(), v.line_no, v.line_pos)
        for v in Linter(config=cfg(False)).lint_string(clean).violations
        if isinstance(v, SQLLintError)
    )
    assert fac == nat

    undef = "select a from {{ undefined_table_name }}\n"
    assert _facade_lint_file(undef, "undef.sql", c, Linter(config=c)) is None


def _noqa_cfg(rust: bool, **extra) -> "FluffConfig":
    return FluffConfig(
        overrides={
            "dialect": "ansi",
            "rules": "LT01,LT12",
            "use_rust_engine": rust,
            "use_rust_parser": rust,
            "use_rust_rules": rust,
            **extra,
        }
    )


def test_facade_lint_noqa_masks_like_native() -> None:
    """Sources with noqa lint on the fast path with native-identical masking.

    Covers: a masked violation (dropped), an unmasked one (kept), a
    malformed directive (SQLParseError reported), and disable_noqa
    (directives ignored entirely).
    """
    from sqlfluff.cli.commands import _facade_lint_file

    # LT01 fires per-site: line 1's double space is masked, line 2's is
    # not; line 3 is a malformed directive (no colon after noqa).
    src = (
        "select a ,  b from tbl -- noqa: LT01\n"
        "union all\n"
        "select c ,  d from tbl --noqa missing colon\n"
    )

    def keys(linted):
        return sorted((type(v).__name__, v.rule_code(), v.line_no) for v in linted)

    c = _noqa_cfg(True)
    linted = _facade_lint_file(src, "noqa.sql", c, Linter(config=c))
    assert linted is not None  # noqa no longer routes to native
    fac = keys(linted.get_violations(filter_ignore=True))
    nat = keys(
        Linter(config=_noqa_cfg(False))
        .lint_string(src)
        .get_violations(filter_ignore=True)
    )
    assert fac == nat
    # Line 1's LT01s are masked; line 3's report; the malformed directive
    # raises an SQLParseError violation.
    assert not any(k[1] == "LT01" and k[2] == 1 for k in fac)
    assert any(k[1] == "LT01" and k[2] == 3 for k in fac)
    assert any(k[0] == "SQLParseError" for k in fac)

    # disable_noqa: directives ignored -> the masked LT01s come back and
    # the malformed directive is no longer parsed.
    c2 = _noqa_cfg(True, disable_noqa=True)
    linted2 = _facade_lint_file(src, "noqa.sql", c2, Linter(config=c2))
    assert linted2 is not None
    fac2 = keys(linted2.get_violations(filter_ignore=True))
    nat2 = keys(
        Linter(config=_noqa_cfg(False, disable_noqa=True))
        .lint_string(src)
        .get_violations(filter_ignore=True)
    )
    assert fac2 == nat2
    assert any(k[1] == "LT01" and k[2] == 1 for k in fac2)


def test_facade_lint_unused_noqa_warns_like_native() -> None:
    """An unused noqa directive produces the same warning as native."""
    from sqlfluff.cli.commands import _facade_lint_file

    src = "SELECT a FROM tbl\n-- noqa: CP01\n"

    def warning_tuples(linted):
        return sorted(
            (v.rule_code(), v.line_no, v.description)
            for v in linted.get_violations(
                filter_warning=False, warn_unused_ignores=True
            )
            if getattr(v, "warning", False)
        )

    c = _noqa_cfg(True)
    linted = _facade_lint_file(src, "unused.sql", c, Linter(config=c))
    assert linted is not None
    nat_linted = Linter(config=_noqa_cfg(False)).lint_string(src)
    assert warning_tuples(linted) == warning_tuples(nat_linted)


def test_facade_fix_noqa_masks_fixes_like_native(tmp_path) -> None:
    """Masked violations' fixes are NOT applied on the fix fast path."""
    from sqlfluff.cli.commands import _try_facade_paths_fix
    from sqlfluff.cli.formatters import OutputStreamFormatter
    from sqlfluff.cli.outputstream import make_output_stream

    src = "select a ,  b from tbl -- noqa: LT01\nunion all\nselect c ,  d from tbl\n"
    f = tmp_path / "mixed.sql"
    f.write_text(src)

    c = _noqa_cfg(True)
    linter = Linter(config=c)
    stream = make_output_stream(c)
    formatter = OutputStreamFormatter(stream, False)
    remaining, _fixable, _unfixable = _try_facade_paths_fix(
        linter, formatter, (str(tmp_path),), "", True
    )
    assert remaining == []  # facade handled the file
    fixed = f.read_text()

    f.write_text(src)  # restore for the native run
    nat = (
        Linter(config=_noqa_cfg(False))
        .lint_string(src, fname=str(f), fix=True)
        .fix_string()[0]
    )
    assert fixed == nat
    # Line 1's spacing stays broken (masked); line 3 gets fixed.
    assert fixed.startswith("select a ,  b from tbl -- noqa: LT01\n")
    assert "select c, d from tbl" in fixed


def test_facade_lint_plugin_rule_split_crawl() -> None:
    """A plugin rule no longer routes the run to native.

    The example plugin's Example_L001 (not in FACADE_SAFE_RULES) is
    native-crawled while unproven, merged with façade-crawled core rules —
    results native-identical — and PROMOTED once it fires and byte-matches,
    after which it crawls the façade directly.
    """
    import sqlfluff.core.rules.rs_lint as rs_lint
    from sqlfluff.cli.commands import _facade_lint_file

    # `bar` is in the plugin's default forbidden_columns -> Example_L001
    # fires; the lowercase keywords make CP01 fire (façade side).
    src = "SELECT a FROM foo ORDER BY bar\n"

    def cfg(rust: bool) -> FluffConfig:
        return FluffConfig(
            overrides={
                "dialect": "ansi",
                "rules": "Example_L001,CP01",
                "use_rust_engine": rust,
                "use_rust_parser": rust,
                "use_rust_rules": rust,
            }
        )

    rs_lint._UNKNOWN_RULE_VERDICTS.pop(("Example_L001", "ansi"), None)

    def keys(violations):
        return sorted(
            (v.rule_code(), v.line_no, v.line_pos, v.description)
            for v in violations
            if isinstance(v, SQLLintError)
        )

    c = cfg(True)
    linted = _facade_lint_file(src, "plugin.sql", c, Linter(config=c))
    assert linted is not None  # plugin rule no longer disqualifies the file
    fac = keys(linted.violations)
    nat = keys(Linter(config=cfg(False)).lint_string(src).violations)
    assert fac == nat
    assert any(k[0] == "Example_L001" for k in fac)  # the plugin rule fired
    # Fired and byte-matched -> promoted for the rest of the process.
    assert rs_lint._UNKNOWN_RULE_VERDICTS.get(("Example_L001", "ansi")) is True

    # Promoted: the native reference parse is dropped (parse_string unused).
    def _boom(*a, **k):  # pragma: no cover
        raise AssertionError("promoted rule must not trigger a native parse")

    orig = Linter.parse_string
    Linter.parse_string = _boom  # type: ignore[method-assign]
    try:
        linted2 = _facade_lint_file(src, "plugin.sql", c, Linter(config=c))
    finally:
        Linter.parse_string = orig  # type: ignore[method-assign]
    assert linted2 is not None
    assert keys(linted2.violations) == nat


def test_facade_unknown_rule_divergence_pins_to_native() -> None:
    """A silently-diverging rule is caught by comparison, not by crashing.

    A rule that only 'works' on native trees (an isinstance-style failure
    mode: it finds nothing on the façade) must still REPORT the native
    results, and its verdict pins it to native crawls.
    """
    import sqlfluff.core.rules.rs_lint as rs_lint
    import sqlfluffrs
    from sqlfluff.core.errors import SQLLintError as LintErr
    from sqlfluff.core.rules.rs_lint import (
        RsSegment,
        facade_unknown_rule_violations,
    )

    class FakeDivergentRule:
        code = "XT01"
        name = "fake.divergent"

        def crawl(self, tree, dialect, fix, templated_file, ignore_mask, fname, config):
            if isinstance(tree, RsSegment):
                return [], (), [], None  # silently finds nothing on the façade
            seg = next(tree.recursive_crawl("keyword"))
            return (
                [LintErr(description="fake finding", segment=seg, rule=self)],
                (),
                [],
                None,
            )

    src = "SELECT a FROM tbl\n"
    cfg = FluffConfig(overrides={"dialect": "ansi", "rules": "CP01"})
    rst = sqlfluffrs.engine_parse_to_tree(src, "<t>", cfg, None, True)
    assert rst is not None
    rule = FakeDivergentRule()
    rs_lint._UNKNOWN_RULE_VERDICTS.pop(("XT01", "ansi"), None)

    out = facade_unknown_rule_violations(
        src, "<t>", cfg, [rule], RsSegment(rst.root), rst.templated_file
    )
    # The NATIVE results are reported despite the façade crawl finding nothing.
    assert out is not None and [v.description for v in out] == ["fake finding"]
    # And the mismatch pins the rule to native for the process.
    assert rs_lint._UNKNOWN_RULE_VERDICTS.get(("XT01", "ansi")) is False
