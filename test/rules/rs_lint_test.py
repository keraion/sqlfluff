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
            if isinstance(src, str):
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
