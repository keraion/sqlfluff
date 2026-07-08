"""Parity tests for the experimental RsSegment arena-façade lint/fix path.

For every rule in :data:`FACADE_SAFE_RULES`, run the façade's multi-pass
source-patch fix loop over each of that rule's ``std_rule_cases`` fixtures and
assert the fixed output matches the fixture's expected ``fix_str`` (or the input
unchanged for ``pass_str`` cases). Skipped unless the ``sqlfluffrs`` extension
provides ``engine_parse_to_tree``.
"""

import glob

import pytest
import yaml

try:
    import sqlfluffrs

    _HAS_ENGINE = hasattr(sqlfluffrs, "engine_parse_to_tree")
except ImportError:  # pragma: no cover
    _HAS_ENGINE = False

from sqlfluff.core import FluffConfig, Linter
from sqlfluff.core.rules.rs_lint import FACADE_SAFE_RULES, facade_fix_loop
from sqlfluff.utils.testing.rules import _setup_config, load_test_cases

_CASE_DIR = "test/fixtures/rules/std_rule_cases"


def _facade_safe_cases():
    """Collect (id, test_case) for every case of a façade-safe rule."""
    collected = []
    for yaml_path in sorted(glob.glob(f"{_CASE_DIR}/*.yml")):
        with open(yaml_path) as f:
            doc = yaml.safe_load(f)
        if not doc or doc.get("rule") not in FACADE_SAFE_RULES:
            continue
        ids, cases = load_test_cases(yaml_path)
        for cid, tc in zip(ids, cases):
            src = tc.fail_str if tc.fail_str is not None else tc.pass_str
            if not isinstance(src, str):
                continue
            # Templated (Jinja) cases route to native in production (the façade
            # path only handles files whose source == templated), so they are not
            # the fix fast path's responsibility — skip them here.
            if "{%" in src or "{{" in src:
                continue
            collected.append(pytest.param(tc, id=cid))
    return collected


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
@pytest.mark.parametrize("test_case", _facade_safe_cases())
def test_facade_fix_matches_native(test_case) -> None:
    """Façade multi-pass fix output equals the fixture's expected result."""
    src = test_case.fail_str if test_case.fail_str is not None else test_case.pass_str
    config = _setup_config(test_case.rule, test_case.configs)
    linter = Linter(config=config)
    rules = list(linter.get_rulepack(config=config).rules)
    limit = int(config.get("runaway_limit"))

    fixed = facade_fix_loop(src, "<test>", config, rules, limit)
    expected = test_case.fix_str if test_case.fix_str is not None else src
    assert fixed == expected


def _facade_fix(src, dialect, rule):
    config = FluffConfig(overrides={"dialect": dialect, "rules": rule})
    linter = Linter(config=config)
    rules = list(linter.get_rulepack(config=config).rules)
    limit = int(config.get("runaway_limit"))
    return facade_fix_loop(src, "<test>", config, rules, limit)


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_facade_rf06_keeps_quoted_routine_name() -> None:
    """RF06 façade fix must NOT strip backticks from a routine name.

    Unquoting the backtick-quoted name reparses as ``function_name_identifier``
    (not the ``naked_identifier`` the fix specifies), so native rejects the fix
    and leaves the file unchanged. The façade's grammar re-validation
    (``validate_staged``) must reproduce that rejection — the corruption this
    whole path guards against.
    """
    src = "CREATE PROCEDURE `my_proc`() BEGIN SELECT 1; END\n"
    assert _facade_fix(src, "mysql", "RF06") == src  # backticks kept, like native


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_facade_rf06_unquotes_plain_identifier() -> None:
    """RF06 façade fix STILL unquotes a legitimate identifier.

    Regression guard that the grammar re-validation doesn't over-reject.
    """
    assert (
        _facade_fix("SELECT `foo` FROM t\n", "mysql", "RF06") == "SELECT foo FROM t\n"
    )


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
@pytest.mark.parametrize("rule", ["CP01", "AL01", "RF06"])
def test_facade_fix_empty_file(rule) -> None:
    """An empty file returns unchanged instead of crashing reconstruction.

    The arena's empty ``file`` node carries no pos_marker (native's FileSegment
    gets a zero-width one), so ``generate_source_patches`` used to assert. The
    short-circuit returns the empty source, matching native.
    """
    assert _facade_fix("", "ansi", rule) == ""


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_facade_tq02_wraps_multiple_procedures() -> None:
    """TQ02 façade fix wraps a multi-procedure file, matching native.

    Regression for the non-convergence that made native runaway-revert (leaving
    the body unwrapped): the façade must produce the same wrapped result native
    now produces (both apply the BEGIN/END wrap once).
    """
    src = (
        "CREATE PROCEDURE dbo.a AS\nSELECT 1;\nSELECT 2;\nGO\n"
        "CREATE PROCEDURE dbo.b AS\nSELECT 1;\nSELECT 2;\nGO\n"
    )
    config = FluffConfig(overrides={"dialect": "tsql", "rules": "TQ02"})
    native = Linter(config=config).lint_string(src, fix=True).fix_string()[0]
    assert native != src  # native now wraps (previously reverted on runaway)
    assert _facade_fix(src, "tsql", "TQ02") == native


def _multi_rule_fix(src, dialect, rules_str):
    """Run both engines with a multi-rule set; return (native, facade)."""
    config = FluffConfig(overrides={"dialect": dialect, "rules": rules_str})
    native = Linter(config=config).lint_string(src, fix=True).fix_string()[0]
    rs_config = FluffConfig(
        overrides={
            "dialect": dialect,
            "rules": rules_str,
            "use_rust_parser": True,
            "use_rust_engine": True,
            "use_rust_rules": True,
        }
    )
    linter = Linter(config=rs_config)
    rules = list(linter.get_rulepack(config=rs_config).rules)
    limit = int(rs_config.get("runaway_limit"))
    facade = facade_fix_loop(src, "<test>", rs_config, rules, limit)
    return native, facade


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_facade_st05_clone_keeps_existing_space() -> None:
    """ST05's clone inspection must see existing whitespace as whitespace.

    ``RsSegment.copy`` builds synthetic classes; without the raw flag attrs
    (``_is_whitespace`` etc.) a cloned whitespace token reported
    ``is_whitespace=False`` and ST05 injected a duplicate space after ``FROM``
    (``FROM  A_TABLE``) that native doesn't.
    """
    src = (
        "SELECT *\nFROM A_TABLE\nINNER JOIN (\n    SELECT margin\n"
        "    FROM B_TABLE\n) USING (SOME_COLUMN)\n"
    )
    fixed = _facade_fix(src, "ansi", "ST05")
    assert "FROM A_TABLE" in fixed  # exactly one space, not two
    assert "FROM  A_TABLE" not in fixed
    config = FluffConfig(overrides={"dialect": "ansi", "rules": "ST05"})
    native = Linter(config=config).lint_string(src, fix=True).fix_string()[0]
    assert fixed == native


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_facade_runaway_limit_reverts_to_original() -> None:
    """Exhausting the loop limit returns the ORIGINAL source, like native.

    Native returns ``save_tree`` (pre-fix) when the main phase never
    stabilises within ``runaway_limit`` loops (linter.py:673-699); the façade
    must revert identically rather than emit the half-churned tree.
    """
    src = "select a FROM tbl\n"  # CP01 needs one fix loop; limit=1 can't settle
    config = FluffConfig(
        overrides={"dialect": "ansi", "rules": "CP01", "runaway_limit": 1}
    )
    native = Linter(config=config).lint_string(src, fix=True).fix_string()[0]
    assert native == src  # native reverts on runaway
    rs_config = FluffConfig(
        overrides={
            "dialect": "ansi",
            "rules": "CP01",
            "runaway_limit": 1,
            "use_rust_parser": True,
            "use_rust_engine": True,
            "use_rust_rules": True,
        }
    )
    linter = Linter(config=rs_config)
    rules = list(linter.get_rulepack(config=rs_config).rules)
    assert facade_fix_loop(src, "<test>", rs_config, rules, 1) == src


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_native_lt11_converges_after_lt09() -> None:
    """LT09+LT11 fix in one run reaches the fixed point, both engines agree.

    Regression for stale parent references after ``copy(segments=...)``:
    ``path_to`` climbed out of the fixed tree, reflow lost its depth info, and
    native LT11 missed 3 of 4 ``INTERSECT (`` violations after an LT09 fix in
    the same pass — so ``sqlfluff fix`` changed the file again on a second run.
    """
    src = (
        "SELECT DISTINCT\n    field_1\nFROM table_1\nEXCEPT (\n"
        "    SELECT DISTINCT field_1\n    FROM table_2\n);\n\n"
        "SELECT field_1\nFROM table_1\nINTERSECT (\n"
        "    SELECT field_1\n    FROM table_2\n);\n"
    )
    native, facade = _multi_rule_fix(src, "postgres", "LT09,LT11")
    assert "INTERSECT (" not in native  # all set operators get their newline
    config = FluffConfig(overrides={"dialect": "postgres", "rules": "LT09,LT11"})
    refixed = Linter(config=config).lint_string(native, fix=True).fix_string()[0]
    assert refixed == native  # native fix is idempotent again
    assert facade == native


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_native_cv11_lt01_converges() -> None:
    """CV11+LT01 fix in one run reaches the fixed point, both engines agree.

    Regression for CV11's flat fix construction: without the
    ``function_name``/``function_contents`` nesting, LT01's reflow saw no
    "touch" configuration and inserted a stray space after the constructed
    ``cast`` (``cast (col1 as integer)``) that a second run removed again.
    """
    src = (
        "select cast(col1 as integer)\nfrom tbl1;\n\n"
        "select convert(integer, col1)\nfrom tbl1;\n"
    )
    native, facade = _multi_rule_fix(src, "redshift", "CV11,LT01")
    assert "cast(col1 as integer)" in native
    assert "cast (" not in native
    config = FluffConfig(overrides={"dialect": "redshift", "rules": "CV11,LT01"})
    refixed = Linter(config=config).lint_string(native, fix=True).fix_string()[0]
    assert refixed == native  # native fix is idempotent again
    assert facade == native


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_facade_fix_loop_lint_sink_matches_native_initial() -> None:
    """``lint_sink`` collects native's ``initial_linting_errors`` equivalent.

    The CLI fast path harvests the pre-fix violation set from the fix loop's
    own first pass (instead of a separate whole-ruleset crawl), so the
    collected results must line up with what native reports for the same fix
    run — and stay empty for a clean file.
    """
    src = "select a FROM tbl\n"
    rs_config = FluffConfig(
        overrides={
            "dialect": "ansi",
            "rules": "CP01,LT12",
            "use_rust_parser": True,
            "use_rust_engine": True,
            "use_rust_rules": True,
        }
    )
    linter = Linter(config=rs_config)
    rules = list(linter.get_rulepack(config=rs_config).rules)
    sink: list = []
    fixed = facade_fix_loop(src, "<test>", rs_config, rules, 10, lint_sink=sink)

    config = FluffConfig(overrides={"dialect": "ansi", "rules": "CP01,LT12"})
    result = Linter(config=config).lint_string(src, fix=True)
    assert fixed == result.fix_string()[0]
    assert [(v.rule_code(), v.line_no, bool(v.fixes)) for v in sink] == [
        (v.rule_code(), v.line_no, bool(v.fixes)) for v in result.violations
    ]

    clean_sink: list = []
    clean = facade_fix_loop(fixed, "<test>", rs_config, rules, 10, lint_sink=clean_sink)
    assert clean == fixed
    assert clean_sink == []


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
@pytest.mark.parametrize("test_case", _facade_safe_cases())
def test_facade_lint_matches_native(test_case) -> None:
    """Façade DETECTION equals native, violation-for-violation.

    The fix suite (above) locks fix-output parity; this locks lint parity —
    (rule, line, pos, description) — which the fix fast path relies on for
    its violation counts/exit codes and any future façade ``lint`` wiring.
    """
    import sqlfluffrs
    from sqlfluff.core.errors import SQLLintError
    from sqlfluff.core.rules.rs_lint import (
        RsSegment,
        facade_ignore_mask,
        facade_violations,
    )

    src = test_case.fail_str if test_case.fail_str is not None else test_case.pass_str
    config = _setup_config(test_case.rule, test_case.configs)
    linter = Linter(config=config)
    rule_pack = linter.get_rulepack(config=config)
    rules = list(rule_pack.rules)

    # Mirror the production gate: noqa directives build an ignore mask from
    # the facade tree and masked results drop inside the crawl.
    rst = sqlfluffrs.engine_parse_to_tree(src, "<test>", config, None, True)
    if rst is None:
        pytest.skip("engine parse unavailable for this case")
    ignore_mask, _ivs = facade_ignore_mask(
        RsSegment(rst.root), config, rule_pack.reference_map
    )
    fac = facade_violations(
        src, "<test>", config, rules, rst=rst, ignore_mask=ignore_mask
    )
    if fac is None:
        # Engine can't parse this case -> routes to native in production.
        pytest.skip("engine parse unavailable for this case")
    # NATIVE reference: pin the engine off — lint_string itself fast-paths
    # via the façade now (the API integration), which would make this a
    # façade-vs-façade comparison.
    nat_config = _setup_config(test_case.rule, test_case.configs)
    nat_config.set_value(["use_rust_engine"], False)
    nat_linter = Linter(config=nat_config)
    nat = [
        v for v in nat_linter.lint_string(src).violations if isinstance(v, SQLLintError)
    ]
    assert [(v.rule_code(), v.line_no, v.line_pos, v.description) for v in fac] == [
        (v.rule_code(), v.line_no, v.line_pos, v.description) for v in nat
    ]


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_facade_lint_dedupes_like_native() -> None:
    """Duplicate lint results collapse like native's source-space dedupe.

    AL04 legitimately emits one duplicate-alias result per SELECT-clause
    subquery, anchored on the same parent alias; native collapses them via
    ``LintedFile.deduplicate_in_source_space`` and ``facade_violations``
    must too (it previously reported the same violation twice).
    """
    from sqlfluff.core.errors import SQLLintError
    from sqlfluff.core.rules.rs_lint import facade_violations

    src = "SELECT a, a IN (SELECT a FROM t1), a = all (SELECT a FROM t1) FROM t1;\n"
    rs_config = FluffConfig(
        overrides={
            "dialect": "mariadb",
            "rules": "AL04",
            "use_rust_parser": True,
            "use_rust_engine": True,
            "use_rust_rules": True,
        }
    )
    linter = Linter(config=rs_config)
    rules = list(linter.get_rulepack(config=rs_config).rules)
    fac = facade_violations(src, "<test>", rs_config, rules)
    config = FluffConfig(overrides={"dialect": "mariadb", "rules": "AL04"})
    nat = [
        v
        for v in Linter(config=config).lint_string(src).violations
        if isinstance(v, SQLLintError)
    ]
    assert len(nat) == 1  # native reports the duplicate alias exactly once
    assert [(v.rule_code(), v.line_no, v.line_pos) for v in fac] == [
        (v.rule_code(), v.line_no, v.line_pos) for v in nat
    ]


# ============================================================================
# Templated-source (jinja) façade support
# ============================================================================

_JINJA_LOOP_SRC = (
    "{% set cols = ['a', 'b'] %}\n"
    "select\n"
    "    id,\n"
    "{% for c in cols %}\n"
    "    sum({{ c }}) as total_{{ c }}{{ ',' if not loop.last }}\n"
    "{% endfor %}\n"
    "from tbl\n"
    "group by id;\n"
)


def _jinja_cfg(rust: bool, rules: str) -> FluffConfig:
    return FluffConfig(
        overrides={
            "dialect": "ansi",
            "templater": "jinja",
            "rules": rules,
            "use_rust_parser": rust,
            "use_rust_engine": rust,
            "use_rust_rules": rust,
        }
    )


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_facade_lint_templated_source_matches_native() -> None:
    """facade_violations on a jinja source matches native.

    Including the templated-area filter: violations anchored in non-literal
    regions are dropped unless the rule targets templated code.
    """
    from sqlfluff.core.errors import SQLLintError
    from sqlfluff.core.rules.rs_lint import facade_violations

    rules_str = "CP01,LT01,LT02,LT09,JJ01"
    cfg = _jinja_cfg(True, rules_str)
    lnt = Linter(config=cfg)
    rules = list(lnt.get_rulepack(config=cfg).rules)
    fac = facade_violations(_JINJA_LOOP_SRC, "<test>", cfg, rules)
    assert fac is not None
    fac_keys = sorted((v.rule_code(), v.line_no, v.line_pos) for v in fac)

    nat_cfg = _jinja_cfg(False, rules_str)
    nat = [
        v
        for v in Linter(config=nat_cfg).lint_string(_JINJA_LOOP_SRC).violations
        if isinstance(v, SQLLintError)
    ]
    nat_keys = sorted((v.rule_code(), v.line_no, v.line_pos) for v in nat)
    assert fac_keys == nat_keys


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_facade_fix_templated_source_matches_native() -> None:
    """facade_fix_loop on a jinja source is byte-identical to native.

    Patches only land in literal regions, and same-position conflicting
    insertions at block-tag boundaries dedupe like native
    (merge_source_patches).
    """
    rules_str = "CP01,LT01,LT02,LT09,JJ01"
    cfg = _jinja_cfg(True, rules_str)
    lnt = Linter(config=cfg)
    rules = list(lnt.get_rulepack(config=cfg).rules)
    fixed = facade_fix_loop(
        _JINJA_LOOP_SRC, "<test>", cfg, rules, int(cfg.get("runaway_limit"))
    )

    nat_cfg = _jinja_cfg(False, rules_str)
    nat_res = Linter(config=nat_cfg).lint_string(_JINJA_LOOP_SRC, fix=True)
    assert fixed == nat_res.fix_string()[0]


@pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)
def test_facade_placeholder_edit_preserves_template_metadata() -> None:
    """Placeholder edit(source_fixes=...) mirrors TemplateSegment.edit.

    It keeps source_str/block_type/block_uuid and merges source fixes —
    JJ01's fix path. Previously it fell through to the raw-edit branch and
    the staged replacement lost 'skipped_source' block_type, misleading
    LT02's jinja-block alignment.
    """
    import sqlfluffrs
    from sqlfluff.core.parser import SourceFix
    from sqlfluff.core.parser.segments.meta import TemplateSegment
    from sqlfluff.core.rules.rs_lint import RsSegment

    src = "select\n    {% for c in ['a'] %}\n    {{c}}\n    {% endfor %}\nfrom t;\n"
    cfg = _jinja_cfg(True, "JJ01")
    rst = sqlfluffrs.engine_parse_to_tree(src, "<test>", cfg, None, True)
    assert rst is not None
    placeholder = next(
        s
        for s in RsSegment(rst.root).recursive_crawl("placeholder")
        if s.block_type == "block_start"
    )
    fix = SourceFix("{{ c }}", slice(0, 5), slice(0, 0))
    edited = placeholder.edit(source_fixes=[fix])
    assert isinstance(edited, TemplateSegment)
    assert edited.block_type == "block_start"
    assert edited.source_str == placeholder.source_str
    # The façade exposes block_uuid as an int (hashable + truthy for reflow
    # grouping); the edited native TemplateSegment carries the real UUID.
    import uuid as _uuid

    assert edited.block_uuid == _uuid.UUID(int=placeholder.block_uuid)
    assert edited.source_fixes == [fix]


def test_facade_rule_classification_is_total_and_current() -> None:
    """Drift guard: every bundled rule has an explicit façade decision.

    The behavioral suites enumerate their work FROM ``FACADE_SAFE_RULES``,
    so a rule missing from the classification is invisible to them: it
    silently routes to native and coverage never starts. This test makes the
    classification total (new rules fail here with instructions) and current
    (renamed/removed codes can't linger, silently shedding coverage).
    """
    from sqlfluff.core.rules.rs_lint import (
        FACADE_EXCLUDED,
        FACADE_SAFE_RULES,
        FACADE_SAFE_RULES_DETECTION_UNSAFE,
    )

    config = FluffConfig(overrides={"dialect": "ansi", "rules": "all"})
    pack = Linter(config=config).get_rulepack(config=config)
    # Bundled rules only: plugins are the ``rust_compatible`` flag's domain.
    bundled = {
        rule.code
        for rule in pack.rules
        if type(rule).__module__.startswith("sqlfluff.rules")
    }
    classified = FACADE_SAFE_RULES | set(FACADE_EXCLUDED)

    missing = bundled - classified
    assert not missing, (
        f"New core rule(s) {sorted(missing)} need a façade decision: verify "
        "byte-parity with the sweeps in utils/facade_*_parity.py and add to "
        "FACADE_SAFE_RULES, or add to FACADE_EXCLUDED with a reason."
    )
    stale = classified - bundled
    assert not stale, (
        f"Stale code(s) {sorted(stale)} in the façade classification "
        "(renamed or removed rule?) — coverage for them has silently ended."
    )
    overlap = FACADE_SAFE_RULES & set(FACADE_EXCLUDED)
    assert not overlap, f"Codes in BOTH safe and excluded: {sorted(overlap)}"
    assert FACADE_SAFE_RULES_DETECTION_UNSAFE <= FACADE_SAFE_RULES, (
        "DETECTION_UNSAFE quarantines a subset of the SAFE list; a code "
        "outside it has no effect."
    )
