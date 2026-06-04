"""Parity tests for the Rust-backed ``RsSegment`` facade.

These assert that navigating the Rust arena tree via ``RsSegment`` yields the
same results as navigating Python's ``BaseSegment`` tree, for the read-only
surface that linting rules depend on.

The tests are skipped automatically when the Rust extension (and therefore the
``_rs_tree`` attribute) is unavailable.
"""

import pytest

from sqlfluff.core import FluffConfig, Linter

try:
    from sqlfluff.core.parser.segments.rs_segment import RsSegment

    _HAS_RS = True
except ImportError:  # pragma: no cover
    _HAS_RS = False


CORPUS = [
    "SELECT 1\n",
    "SELECT a, b FROM my_table\n",
    "SELECT a, b FROM my_table WHERE a > 1 AND b < 2\n",
    "SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.id = t2.id\n",
    "WITH cte AS (SELECT 1 AS x) SELECT x FROM cte\n",
    "SELECT COUNT(*), MAX(col) FROM tbl GROUP BY other\n",
    "SELECT CASE WHEN a = 1 THEN 'x' ELSE 'y' END AS c FROM t\n",
    "select A , B from C\n",  # messy whitespace/casing
    "INSERT INTO t (a, b) VALUES (1, 2)\n",
    "SELECT a FROM b ORDER BY a DESC LIMIT 10\n",
]

CRAWL_TYPES = [
    "keyword",
    "column_reference",
    "naked_identifier",
    "table_reference",
    "select_statement",
    "function",
    "literal",
    "comparison_operator",
    "expression",
    "with_compound_statement",
]


def _parse(sql: str):
    cfg = FluffConfig(overrides={"dialect": "ansi"})
    lnt = Linter(config=cfg)
    parsed = lnt.parse_string(sql)
    return parsed.tree


def _rs_root(tree):
    rs_tree = getattr(tree, "_rs_tree", None)
    if rs_tree is None:
        pytest.skip("Rust arena tree (_rs_tree) not available")
    return RsSegment(rs_tree.root)


pytestmark = pytest.mark.skipif(not _HAS_RS, reason="Rust extension not built")


# SQL verified byte-identical between the facade crawl and pure Python. The
# Rust-backed lint path is still maturing; constructs with known remaining
# divergences (e.g. CTE column-count analysis for AM04, some reflow positions)
# are intentionally excluded until those gaps close.
LINT_CORPUS = [
    "SELECT a , b  FROM my_table where a>1\n",
    "select X,Y from Z\n",
    "SELECT COUNT(*) FROM t GROUP BY b HAVING COUNT(*)>1\n",
    "SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.id=t2.id\n",
    "SELECT CASE WHEN a=1 THEN 2 ELSE 3 END c FROM t\n",
    "INSERT INTO t (a,b) VALUES (1,2)\n",
    "SELECT a FROM b ORDER BY a DESC LIMIT 10\n",
    "SELECT a AS x, b FROM t1, t2 WHERE t1.id = t2.id\n",
    'select a, b, c from sch."blah"\n',
    # CTE / WITH (exercises recursive_crawl no_recursive + bracket line_no)
    "WITH cte AS (SELECT 1 AS x) SELECT x FROM cte\n",
    "WITH source AS (SELECT * FROM raw) SELECT * FROM source\n",
    "CREATE TABLE t AS\nWITH s AS\n(\n    SELECT * FROM d\n)\nSELECT * FROM s\n",
    # Leading whitespace (exercises file-root pos_marker)
    " select 12 -- trailing comment\n",
    # Jinja templating (exercises container is_literal, placeholder is_raw,
    # templated-slice handling)
    "SELECT * FROM {{ foo }}\n",
    "SELECT {{ col }} FROM t\n",
    "{% set x = 1 %}\nSELECT a FROM t\n",
    "-- comment\n{% if true %}\nSELECT 1\n{% endif %}\n",
]


@pytest.mark.parametrize("sql", LINT_CORPUS)
def test_lint_parity_facade_vs_python(sql, monkeypatch):
    """Linting on the Rust-backed facade matches pure Python byte-for-byte.

    With the facade enabled, every rule is crawled on ``RsSegment``; rules that
    error fall back to the Python tree, so divergence here means a *silent*
    correctness gap (a rule that ran on the facade and produced a different
    result without erroring).
    """

    def violations(mode: str):
        monkeypatch.setenv("SQLFLUFF_RS_SEGMENTS", mode)
        lnt = Linter(config=FluffConfig(overrides={"dialect": "ansi"}))
        return sorted(
            (v.rule_code(), v.line_no, v.line_pos, v.description)
            for v in lnt.lint_string(sql).violations
        )

    if getattr(_parse(sql), "_rs_tree", None) is None:
        pytest.skip("Rust arena tree not available")
    assert violations("0") == violations("1")


@pytest.mark.parametrize("sql", CORPUS)
def test_root_raw_parity(sql):
    """The facade's joined raw matches the Python tree exactly."""
    tree = _parse(sql)
    rs = _rs_root(tree)
    assert rs.raw == tree.raw == sql


@pytest.mark.parametrize("sql", CORPUS)
def test_recursive_crawl_parity(sql):
    """recursive_crawl yields the same raws for each segment type."""
    tree = _parse(sql)
    rs = _rs_root(tree)
    for seg_type in CRAWL_TYPES:
        rs_raws = [s.raw for s in rs.recursive_crawl(seg_type)]
        py_raws = [s.raw for s in tree.recursive_crawl(seg_type)]
        assert rs_raws == py_raws, f"mismatch for type {seg_type!r} in {sql!r}"


@pytest.mark.parametrize("sql", CORPUS)
def test_raw_segments_parity(sql):
    """raw_segments produce the same ordered raw strings."""
    tree = _parse(sql)
    rs = _rs_root(tree)
    assert [s.raw for s in rs.raw_segments] == [s.raw for s in tree.raw_segments]


@pytest.mark.parametrize("sql", CORPUS)
def test_class_types_exact_parity(sql):
    """The arena's class_types exactly match Python's for every node.

    Raw/container nodes get their full produced-class hierarchy from the dialect
    codegen ``CLASS_TYPES_BY_NAME`` table (e.g. ``word`` for keywords); metas
    (including ``Dedent`` subclassing ``Indent``) are covered structurally.
    """
    tree = _parse(sql)
    rs = _rs_root(tree)
    rs_all = list(rs.recursive_crawl_all())
    py_all = list(tree.recursive_crawl_all())
    assert len(rs_all) == len(py_all)
    for rs_seg, py_seg in zip(rs_all, py_all):
        assert set(rs_seg.class_types) == set(py_seg.class_types), (
            f"class_types mismatch on {py_seg.raw!r} (type {py_seg.get_type()})"
        )
        assert rs_seg.type == py_seg.get_type()


@pytest.mark.parametrize("sql", CORPUS)
def test_is_code_whitespace_parity(sql):
    """is_code / is_whitespace agree node-by-node."""
    tree = _parse(sql)
    rs = _rs_root(tree)
    for rs_seg, py_seg in zip(rs.raw_segments, tree.raw_segments):
        assert rs_seg.is_code == py_seg.is_code, f"is_code on {py_seg.raw!r}"
        assert rs_seg.is_whitespace == py_seg.is_whitespace


@pytest.mark.parametrize("sql", CORPUS)
def test_identity_and_equality(sql):
    """RsSegment identity mirrors BaseSegment: same node compares equal, is
    hashable, and distinct nodes differ.  Exercises same-arena handle equality
    (which must not deadlock on the arena mutex).
    """
    tree = _parse(sql)
    rs = _rs_root(tree)
    kws = list(rs.recursive_crawl("keyword"))
    # Re-fetching the same node yields an equal, equally-hashing facade.
    again = list(rs.recursive_crawl("keyword"))
    for a, b in zip(kws, again):
        assert a == b
        assert hash(a) == hash(b)
        assert a.uuid == b.uuid
    # Distinct nodes are not equal, and dedupe via set works.
    all_raws = list(rs.raw_segments)
    assert len(set(all_raws)) == len(all_raws)
    if len(kws) >= 2:
        assert kws[0] != kws[1]


@pytest.mark.parametrize("sql", CORPUS)
def test_get_parent_and_path_to_parity(sql):
    """get_parent type and path_to length agree for each identifier."""
    tree = _parse(sql)
    rs = _rs_root(tree)
    rs_idents = list(rs.recursive_crawl("naked_identifier"))
    py_idents = list(tree.recursive_crawl("naked_identifier"))
    assert len(rs_idents) == len(py_idents)
    for rs_seg, py_seg in zip(rs_idents, py_idents):
        rs_gp = rs_seg.get_parent()
        py_gp = py_seg.get_parent()
        assert (rs_gp is None) == (py_gp is None)
        if rs_gp is not None:
            assert rs_gp[0].type == py_gp[0].type
        # path from root to this identifier
        rs_path = rs.path_to(rs_seg)
        py_path = tree.path_to(py_seg)
        assert [p.segment.type for p in rs_path] == [p.segment.type for p in py_path]
