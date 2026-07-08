"""Tests for the -p N (multiprocess) façade fast path.

Comparison strategy: the façade's own output is deterministic (ordered
``imap``), so ``-p N`` must BYTE-match ``-p 1``. Native's parallel output
order is completion-order (nondeterministic), and files routed to native
print after façade-handled ones (the documented split-ordering deviation) —
so comparisons against native treat the per-file blocks as unordered sets.
"""

import re

import pytest

try:
    import sqlfluffrs

    _HAS_ENGINE = hasattr(sqlfluffrs, "engine_parse_to_tree")
except ImportError:  # pragma: no cover
    _HAS_ENGINE = False

from click.testing import CliRunner

from sqlfluff.cli.commands import fix, lint

pytestmark = pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_to_tree unavailable"
)

# Deliberately mixed: fixable (CP01/LT01), a noqa mask, an unparsable file
# (routes to native even under the pool), and a clean file.
_FILES = {
    "a_mixed.sql": "SeLeCt a ,  b from tbl\n",
    "b_noqa.sql": "SeLeCt 1 -- noqa: CP01\n",
    "c_broken.sql": "select from from\n",
    "d_clean.sql": "SELECT 1 FROM tbl\n",
    "e_more.sql": "select   c from other\n",
}


def _write_project(tmp_path, engine: bool):
    tmp_path.mkdir(parents=True, exist_ok=True)
    for name, src in _FILES.items():
        (tmp_path / name).write_text(src)
    (tmp_path / ".sqlfluff").write_text(
        f"[sqlfluff]\ndialect = ansi\nrules = CP01,LT01\nuse_rust_engine = {engine}\n"
    )
    return tmp_path


def _run(cmd, args):
    return CliRunner().invoke(cmd, args)


def _blocks(stdout: str, root: str) -> set:
    """Per-file output blocks as an order-insensitive set.

    Trailing summary lines (the dialect WARNING, "All Finished!") attach to
    whichever block happens to print last — drop them before splitting.
    """
    body = "\n".join(
        ln
        for ln in stdout.replace(root, "X").splitlines()
        if not ln.startswith("WARNING: ") and ln != "All Finished!"
    )
    parts = re.split(r"(?m)^(?=== \[)", body)
    return {p.rstrip() for p in parts if p.startswith("== [")}


def test_parallel_lint_matches_serial_facade_bytes(tmp_path) -> None:
    """Lint -p 2: façade output is BYTE-identical to -p 1 façade output."""
    proj_a = str(_write_project(tmp_path / "one", engine=True))
    proj_b = str(_write_project(tmp_path / "two", engine=True))
    serial = _run(lint, ["--disable-progress-bar", "-p", "1", proj_a])
    parallel = _run(lint, ["--disable-progress-bar", "-p", "2", proj_b])
    assert serial.exit_code == parallel.exit_code == 1
    assert serial.output.replace(proj_a, "X") == parallel.output.replace(proj_b, "X")


@pytest.mark.parametrize("processes", ["2", "0"])
def test_parallel_lint_matches_native_blocks(tmp_path, processes) -> None:
    """Lint -p N: façade == native: same per-file blocks and exit code."""
    proj = str(_write_project(tmp_path / "rs", engine=True))
    nat_proj = str(_write_project(tmp_path / "nat", engine=False))
    got = _run(lint, ["--disable-progress-bar", "-p", processes, proj])
    ref = _run(lint, ["--disable-progress-bar", "-p", processes, nat_proj])
    assert got.exit_code == ref.exit_code == 1
    assert _blocks(got.output, proj) == _blocks(ref.output, nat_proj)


def test_parallel_fix_matches_serial_facade_bytes(tmp_path) -> None:
    """Fix -f -p 2: output and written bytes match the -p 1 façade run."""
    proj_a = _write_project(tmp_path / "one", engine=True)
    proj_b = _write_project(tmp_path / "two", engine=True)
    serial = _run(fix, ["--disable-progress-bar", "-f", "-p", "1", str(proj_a)])
    parallel = _run(fix, ["--disable-progress-bar", "-f", "-p", "2", str(proj_b)])
    assert serial.exit_code == parallel.exit_code
    assert serial.output.replace(str(proj_a), "X") == parallel.output.replace(
        str(proj_b), "X"
    )
    for name in _FILES:
        assert (proj_a / name).read_text() == (proj_b / name).read_text(), name


def test_parallel_fix_written_bytes_match_native(tmp_path) -> None:
    """Fix -f -p 2: every written file is byte-identical to native's."""
    proj = _write_project(tmp_path / "rs", engine=True)
    nat_proj = _write_project(tmp_path / "nat", engine=False)
    got = _run(fix, ["--disable-progress-bar", "-f", "-p", "2", str(proj)])
    ref = _run(fix, ["--disable-progress-bar", "-f", "-p", "2", str(nat_proj)])
    assert got.exit_code == ref.exit_code
    for name in _FILES:
        assert (proj / name).read_text() == (nat_proj / name).read_text(), name


def test_parallel_lint_json_records_match(tmp_path) -> None:
    """--format json under -p 2: identical records (as_records sorts)."""
    import json

    proj = str(_write_project(tmp_path / "rs", engine=True))
    nat_proj = str(_write_project(tmp_path / "nat", engine=False))
    got = _run(lint, ["--format", "json", "-p", "2", proj])
    ref = _run(lint, ["--format", "json", "-p", "2", nat_proj])

    def norm(out, root):
        recs = json.loads(out)
        for r in recs:
            r["filepath"] = r["filepath"].replace(root, "X")
            r.pop("timings", None)  # wall-clock floats: nondeterministic
        # Record ordering differs when files split across engines (and
        # native's own -p ordering is completion-order): sort for comparison.
        return sorted(recs, key=lambda r: r["filepath"])

    assert norm(got.output, proj) == norm(ref.output, nat_proj)
