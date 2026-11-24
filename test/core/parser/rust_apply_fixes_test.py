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


@pytest.mark.skipif(
    not hasattr(getattr(sqlfluffrs, "RsParser", None), "apply_leaf_fixes"),
    reason="extension built without the arena apply",
)
def test_rust_apply_matches_python(monkeypatch):
    """Same fixed text and leaf positions with the Rust apply on and off."""
    rust_parser.APPLY_STATS.clear()
    on = _fix(monkeypatch, True)
    assert rust_parser.APPLY_STATS["applied"] > 0
    assert on == _fix(monkeypatch, False)
    assert on[0] != SQL
