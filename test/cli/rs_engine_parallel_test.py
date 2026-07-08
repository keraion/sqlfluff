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


def test_inline_directive_isolation_under_config_cache(tmp_path) -> None:
    """The per-directory config cache must not leak inline directives.

    Two same-directory files: one carries ``-- sqlfluff:rules:LT01`` (private
    child config), its sibling must still lint with the directory rules —
    at -p 1 and -p 2, matching native.
    """
    for engine in (True, False):
        d = tmp_path / ("rs" if engine else "nat")
        d.mkdir()
        # Directive file sorts FIRST so a leaked child would poison the cache.
        (d / "a_directive.sql").write_text(
            "-- sqlfluff:rules:LT01\nSeLeCt a ,  b from tbl\n"
        )
        (d / "b_plain.sql").write_text("SeLeCt a ,  b from tbl\n")
        (d / ".sqlfluff").write_text(
            f"[sqlfluff]\ndialect = ansi\nrules = CP01,LT01\n"
            f"use_rust_engine = {engine}\n"
        )
    outs = {}
    for p in ("1", "2"):
        got = _run(lint, ["--disable-progress-bar", "-p", p, str(tmp_path / "rs")])
        ref = _run(lint, ["--disable-progress-bar", "-p", p, str(tmp_path / "nat")])
        assert got.exit_code == ref.exit_code == 1
        assert _blocks(got.output, str(tmp_path / "rs")) == _blocks(
            ref.output, str(tmp_path / "nat")
        )
        outs[p] = _blocks(got.output, str(tmp_path / "rs"))
    assert outs["1"] == outs["2"]
    # The directive file got ONLY LT01; the sibling got CP01 too.
    directive_block = next(b for b in outs["1"] if "a_directive" in b)
    plain_block = next(b for b in outs["1"] if "b_plain" in b)
    assert "CP01" not in directive_block and "LT01" in directive_block
    assert "CP01" in plain_block


@pytest.mark.parametrize("processes", ["1", "2"])
def test_large_file_skip_accounting_on_fast_path(
    tmp_path, monkeypatch, processes
) -> None:
    """SQLFluffSkipFile skips count toward large_file_skip_fail's exit code.

    NOTE: ``large_file_skip_fail`` is read from the ROOT (cwd-discovered)
    config — native semantics, verified engine-off — so the test runs from
    inside the project directory.
    """
    (tmp_path / "small.sql").write_text("SELECT 1 FROM tbl\n")
    (tmp_path / "big.sql").write_text("SELECT 1 FROM tbl\n" * 200)
    (tmp_path / ".sqlfluff").write_text(
        "[sqlfluff]\ndialect = ansi\nrules = CP01\nuse_rust_engine = True\n"
        "large_file_skip_byte_limit = 500\nlarge_file_skip_fail = True\n"
    )
    monkeypatch.chdir(tmp_path)
    result = _run(lint, ["--disable-progress-bar", "-p", processes, "."])
    # small.sql is clean; the exit failure comes from the skipped big file.
    assert result.exit_code == 1
