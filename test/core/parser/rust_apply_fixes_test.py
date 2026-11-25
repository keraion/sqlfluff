"""SPIKE: fixes applied on the Rust arena give what `apply_fixes` gives."""

import pytest

from sqlfluff.core import FluffConfig, Linter
from sqlfluff.core.parser import rust_parser

sqlfluffrs = pytest.importorskip("sqlfluffrs")

SQL = "select a,b   from tbl\nwhere a=1 and\nb = 2  -- note\n  order by a\n"


def _fix(monkeypatch, rust_apply: bool):
    if rust_apply:
        monkeypatch.delenv("SQLFLUFF_NO_RUST_APPLY", raising=False)
    else:
        monkeypatch.setenv("SQLFLUFF_NO_RUST_APPLY", "1")
    cfg = FluffConfig(
        overrides={
            "dialect": "ansi",
            "rules": "LT01,LT02,LT05,LT14",
            "max_line_length": 20,
            "use_rust_parser": "True",
            "use_rust_rules": "True",
        }
    )
    linted = Linter(config=cfg).lint_string(SQL, fix=True)
    positions = [
        (seg.raw, seg.pos_marker.source_slice, seg.pos_marker.working_loc)
        for seg in linted.tree.raw_segments
    ]
    return linted.fix_string()[0], positions


_needs_arena_apply = pytest.mark.skipif(
    not hasattr(getattr(sqlfluffrs, "RsParser", None), "apply_leaf_fixes"),
    reason="extension built without the arena apply",
)


@_needs_arena_apply
def test_rust_apply_matches_python(monkeypatch):
    """Same fixed text and leaf positions with the Rust apply on and off."""
    rust_parser.APPLY_STATS.clear()
    on = _fix(monkeypatch, True)
    assert rust_parser.APPLY_STATS["applied"] > 0
    assert on == _fix(monkeypatch, False)
    assert on[0] != SQL


def _lint_fix(monkeypatch, sql: str, rust: bool, **overrides):
    """Lint and fix with every Rust reflow and apply path on, or all off."""
    for name in ("LT01", "LT02", "LT05", "LT14", "APPLY"):
        if rust:
            monkeypatch.delenv(f"SQLFLUFF_NO_RUST_{name}", raising=False)
        else:
            monkeypatch.setenv(f"SQLFLUFF_NO_RUST_{name}", "1")
    cfg = FluffConfig(
        overrides={
            "dialect": "ansi",
            "use_rust_parser": "True",
            "use_rust_rules": "True",
            **overrides,
        }
    )
    linter = Linter(config=cfg)
    violations = [
        (v.rule_code(), v.line_no, v.line_pos, v.desc())
        for v in linter.lint_string(sql).get_violations()
    ]
    linted = linter.lint_string(sql, fix=True)
    return violations, linted.fix_string()[0], linted.tree


@_needs_arena_apply
def test_rust_fixed_tree_pickles(monkeypatch):
    """A tree fixed on the arena goes between processes (`--processes`)."""
    import pickle

    _, _, tree = _lint_fix(monkeypatch, SQL, True, rules="LT01,LT02")
    assert pickle.loads(pickle.dumps(tree)).raw == tree.raw


@_needs_arena_apply
def test_rust_reindent_forced_indent_needs_a_result(monkeypatch):
    """An untaken indent which gives no result isn't counted as forced."""
    sql = "UPDATE t1 AS a\nINNER JOIN t2 AS b ON a.id = b.id\nSET a.x = 1;\n"
    kwargs = {"dialect": "mysql", "rules": "LT02"}
    assert (
        _lint_fix(monkeypatch, sql, True, **kwargs)[:2]
        == _lint_fix(monkeypatch, sql, False, **kwargs)[:2]
    )


@_needs_arena_apply
def test_rust_apply_positions_by_stored_location(monkeypatch):
    """A new segment is placed by where its siblings are, not where text puts them.

    In a templated loop two whitespace segments can share a rendered slice,
    so the tree's working locations don't follow the text.
    """
    sql = "SELECT {% for col in ['a', 'b', 'c'] %}{{col}}, {% endfor %} FROM tbl\n"
    kwargs = {"rules": "CV03,LT01,LT02,LT05,LT14"}
    assert (
        _lint_fix(monkeypatch, sql, True, **kwargs)[:2]
        == _lint_fix(monkeypatch, sql, False, **kwargs)[:2]
    )
