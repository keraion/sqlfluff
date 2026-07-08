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
    config = FluffConfig(
        overrides={
            "dialect": "ansi",
            "rules": linter_rules,
            "use_rust_engine": False,
        }
    )
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
    # set): no longer routes the run to native — without a
    # ``rust_compatible`` opt-in it's crawled on a native reference parse
    # and merged.
    import sqlfluff.core.rules.rs_lint as rs_lint

    monkeypatch.setattr(
        rs_lint, "FACADE_SAFE_RULES", rs_lint.FACADE_SAFE_RULES - {"CP01"}
    )
    unknown_result = _try_facade_stdin_lint(_linter("CP01"), "SeLeCt 1\n", None)
    assert unknown_result is not None
    assert [
        (v["code"], v["start_line_no"])
        for record in unknown_result.as_records()
        for v in record["violations"]
    ] == [("CP01", 1)]


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
    dirs, remaining, skipped = result
    handled = sorted(
        record["filepath"].rsplit("/", 1)[-1]
        for linted_dir in dirs
        for record in linted_dir.as_records()
    )
    # Files the façade declines (templater violations, parse errors) run the
    # NATIVE pipeline inside the same unit — nothing is left for a second
    # native pass.
    assert handled == [
        "clean.sql",
        "dirty.sql",
        "templated.sql",
        "unparsable.sql",
    ]
    assert remaining == []
    assert skipped == 0

    def codes(name):
        return [
            v["code"]
            for linted_dir in dirs
            for record in linted_dir.as_records()
            if record["filepath"].endswith(name)
            for v in record["violations"]
        ]

    # The dirty file's violations match native; the natively-run files carry
    # native's TMP/PRS reporting.
    assert codes("dirty.sql") == ["CP01", "CP01"]
    assert codes("unparsable.sql") == ["PRS"]
    assert codes("templated.sql") == ["PRS", "TMP"]


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
    remaining, _fixable, _unfixable, _records, _pending, _timing = (
        _try_facade_paths_fix(linter, formatter, (str(tmp_path),), "", True)
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


def test_facade_lint_plugin_rule_opt_in(monkeypatch) -> None:
    """Plugin rules opt in to rust via ``rust_compatible = True``.

    The example plugin's Example_L001 declares the flag, so it crawls the
    façade directly — no native reference parse at all — with results
    native-identical. With the flag off (simulating an undeclared plugin),
    the rule is crawled on a native reference parse instead, still merged
    on the fast path.
    """
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

    def keys(violations):
        return sorted(
            (v.rule_code(), v.line_no, v.line_pos, v.description)
            for v in violations
            if isinstance(v, SQLLintError)
        )

    nat = keys(Linter(config=cfg(False)).lint_string(src).violations)
    assert any(k[0] == "Example_L001" for k in nat)  # the plugin rule fires

    # Flag ON (as shipped): façade-only — a native parse would be a bug.
    def _boom(*a, **k):  # pragma: no cover
        raise AssertionError("rust_compatible rule must not native-parse")

    c = cfg(True)
    monkeypatch.setattr(Linter, "parse_string", _boom)
    linted = _facade_lint_file(src, "plugin.sql", c, Linter(config=c))
    monkeypatch.undo()
    assert linted is not None
    assert keys(linted.violations) == nat

    # Flag OFF: the classic-python split — native reference crawl, merged.
    from sqlfluff_plugin_example.rules import Rule_Example_L001

    monkeypatch.setattr(Rule_Example_L001, "rust_compatible", False)
    linted2 = _facade_lint_file(src, "plugin.sql", c, Linter(config=c))
    assert linted2 is not None
    assert keys(linted2.violations) == nat


def test_facade_unknown_rule_never_crawls_facade() -> None:
    """Rules without the opt-in run on python — the façade is never crawled.

    An isinstance-style incompatible rule (finds nothing on façade wrappers,
    without crashing) is exactly why: there is nothing safe to "attempt".
    Its results come from a native reference parse, correct by construction.
    """
    from sqlfluff.core.errors import SQLLintError as LintErr
    from sqlfluff.core.rules.rs_lint import (
        RsSegment,
        facade_unknown_rule_violations,
    )

    crawled_trees = []

    class FakeIncompatibleRule:
        code = "XT01"
        name = "fake.incompatible"
        rust_compatible = False

        def crawl(self, tree, dialect, fix, templated_file, ignore_mask, fname, config):
            crawled_trees.append(type(tree).__name__)
            if isinstance(tree, RsSegment):  # pragma: no cover
                return [], (), [], None  # would silently find nothing
            seg = next(tree.recursive_crawl("keyword"))
            return (
                [LintErr(description="fake finding", segment=seg, rule=self)],
                (),
                [],
                None,
            )

    src = "SELECT a FROM tbl\n"
    cfg = FluffConfig(overrides={"dialect": "ansi", "rules": "CP01"})
    out = facade_unknown_rule_violations(src, "<t>", cfg, [FakeIncompatibleRule()])
    assert out is not None and [v.description for v in out] == ["fake finding"]
    assert "RsSegment" not in crawled_trees  # façade never attempted


def test_facade_lint_multi_variant_render_matches_native() -> None:
    """Multi-variant renders lint every variant tree and merge like native.

    An untaken jinja branch's violations only surface via the alternate
    variants (native lints every parsed variant, capped at
    render_variant_limit); the façade must produce the identical merged set.
    """
    from sqlfluff.cli.commands import _facade_lint_file

    src = (
        "{% if true %}\n"
        "select a ,  b from tbl\n"
        "{% else %}\n"
        "select c ,  d from other_tbl\n"
        "{% endif %}\n"
    )

    def cfg(rust: bool) -> FluffConfig:
        return FluffConfig(
            overrides={
                "dialect": "ansi",
                "templater": "jinja",
                "rules": "LT01",
                "use_rust_engine": rust,
                "use_rust_parser": rust,
                "use_rust_rules": rust,
            }
        )

    def keys(violations):
        return sorted(
            (v.rule_code(), v.line_no, v.line_pos)
            for v in violations
            if isinstance(v, SQLLintError)
        )

    c = cfg(True)
    linted = _facade_lint_file(src, "multi.sql", c, Linter(config=c))
    assert linted is not None  # multi-variant no longer routes to native
    fac = keys(linted.violations)
    nat = keys(Linter(config=cfg(False)).lint_string(src).violations)
    assert fac == nat
    # The untaken else-branch's LT01s are present (only an alternate
    # variant can see them).
    assert any(k[1] == 4 for k in fac)


def test_lint_bench_and_persist_timing_on_fast_path(tmp_path) -> None:
    """--bench/--persist-timing no longer route lint to native."""
    import csv

    from click.testing import CliRunner

    from sqlfluff.cli.commands import lint as lint_cmd

    for i in range(2):
        (tmp_path / f"q{i}.sql").write_text("SeLeCt 1 from tbl\n")
    (tmp_path / ".sqlfluff").write_text(
        "[sqlfluff]\ndialect = ansi\nrules = CP01\nuse_rust_engine = True\n"
    )
    csv_path = tmp_path / "timings.csv"
    result = CliRunner().invoke(
        lint_cmd,
        [
            "--disable-progress-bar",
            "--bench",
            "--persist-timing",
            str(csv_path),
            str(tmp_path),
        ],
    )
    assert result.exit_code == 1  # CP01 violations
    assert "==== overall timings ====" in result.output
    with open(csv_path) as f:
        rows = list(csv.DictReader(f))
    assert len(rows) == 2
    for row in rows:
        # The engine's signature: whole front-of-pipeline under "parsing".
        assert float(row["parsing"]) > 0
        assert float(row["templating"]) == 0.0
        assert float(row["CP01"]) > 0
