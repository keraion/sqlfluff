"""Parity tests for the Rust engine path of `sqlfluff parse` and `render`.

Each case runs the CLI twice: once on the pure Python path and once with
``use_rust_engine = True``. Stdout and the exit code must match.
"""

import logging
from pathlib import Path
from typing import Optional

import pytest
from click.testing import CliRunner

from sqlfluff.cli.commands import parse, render
from sqlfluff.core import FluffConfig

try:
    import sqlfluffrs

    _HAS_ENGINE = hasattr(sqlfluffrs, "engine_parse_paths")
except ImportError:
    _HAS_ENGINE = False

# NOTE: Under the `py*-rust-engine` tox envs, test/conftest.py fails the run
# if the entry points are missing, so this can only skip elsewhere.
pytestmark = pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_paths unavailable"
)

UNPARSABLE_SQL = "SELECT a FROM tbl WHERE (;\n"
JINJA_SQL = "SELECT {{ col }} FROM tbl {% if flag %}WHERE a = 1{% endif %}\n"


def _run(command, args: list[str], engine: bool, tmp_path: Path, stdin=None):
    """Invoke a CLI command with the engine forced on or off."""
    # Pin both flags on the native side. "auto" would compare Rust with Rust.
    cfg = tmp_path / f"engine_{engine}.cfg"
    cfg.write_text(
        "[sqlfluff]\n"
        f"use_rust_engine = {engine}\n"
        f"use_rust_parser = {engine}\n"
        "[sqlfluff:templater:jinja:context]\n"
        "col = my_col\n"
        "flag = True\n"
    )
    return CliRunner().invoke(command, [*args, "--config", str(cfg)], input=stdin)


def _assert_parity(command, args: list[str], tmp_path: Path, stdin=None):
    native = _run(command, args, False, tmp_path, stdin)
    engine = _run(command, args, True, tmp_path, stdin)
    assert engine.stdout == native.stdout
    assert engine.exit_code == native.exit_code
    return native


def _write(tmp_path: Path, name: str, sql: str) -> str:
    path = tmp_path / "sql" / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(sql)
    return str(path)


@pytest.mark.parametrize("fmt", ["json", "yaml"])
@pytest.mark.parametrize(
    "sql,extra_args",
    [
        ("SELECT a, b FROM tbl\n", []),
        ("SELECT a, b FROM tbl\n", ["--code-only"]),
        ("SELECT a + 1 AS c FROM tbl WHERE b IN (1, 2)\n", []),
        ("-- a comment\nSELECT 1;\n\nSELECT 2;\n", []),
        (JINJA_SQL, []),
        (UNPARSABLE_SQL, []),
        (UNPARSABLE_SQL, ["--nofail"]),
    ],
    ids=[
        "clean",
        "code_only",
        "expression",
        "multi_statement",
        "jinja",
        "unparsable",
        "unparsable_nofail",
    ],
)
def test__engine_parse__file_matches_native(
    sql: str, extra_args: list[str], fmt: str, tmp_path: Path
):
    """A single file gives the same output and exit code on both paths."""
    path = _write(tmp_path, "test.sql", sql)
    native = _assert_parity(
        parse, [path, "--dialect", "ansi", "--format", fmt, *extra_args], tmp_path
    )
    assert native.stdout.strip()


def test__engine_parse__directory_matches_native(tmp_path: Path):
    """A directory gives the same files, in the same order."""
    _write(tmp_path, "b.sql", "SELECT 2\n")
    _write(tmp_path, "a.sql", "SELECT 1\n")
    _write(tmp_path, "nested/c.sql", "SELECT 3\n")
    _write(tmp_path, "not_sql.txt", "SELECT 4\n")
    _assert_parity(
        parse,
        [str(tmp_path / "sql"), "--dialect", "ansi", "--format", "json"],
        tmp_path,
    )


@pytest.mark.parametrize(
    "stdin_filename",
    [
        None,
        pytest.param(
            "models/my_model.sql",
            marks=pytest.mark.xfail(
                strict=True,
                reason="The engine reports the --stdin-filename as the "
                "filepath; native reports 'stdin'.",
            ),
        ),
    ],
)
def test__engine_parse__stdin_matches_native(
    stdin_filename: Optional[str], tmp_path: Path
):
    """Stdin gives the same output, with and without --stdin-filename."""
    args = ["-", "--dialect", "ansi", "--format", "json"]
    if stdin_filename:
        args += ["--stdin-filename", stdin_filename]
    _assert_parity(parse, args, tmp_path, stdin="SELECT a FROM tbl\n")


def test__engine_parse__no_dialect_is_user_error(tmp_path: Path):
    """A missing dialect is reported as a user error on both paths."""
    path = _write(tmp_path, "test.sql", "SELECT 1\n")
    native = _assert_parity(parse, [path, "--format", "json"], tmp_path)
    assert native.exit_code == 2


@pytest.mark.parametrize(
    "sql", ["SELECT a FROM tbl\n", JINJA_SQL], ids=["literal", "jinja"]
)
def test__engine_render__file_matches_native(sql: str, tmp_path: Path):
    """`render` gives the same output on both paths."""
    path = _write(tmp_path, "test.sql", sql)
    native = _assert_parity(render, [path, "--dialect", "ansi"], tmp_path)
    assert "tbl" in native.stdout


def test__engine_render__stdin_matches_native(tmp_path: Path):
    """`render` from stdin gives the same output on both paths."""
    _assert_parity(render, ["-", "--dialect", "ansi"], tmp_path, stdin=JINJA_SQL)


def _engine_config(limit: int) -> FluffConfig:
    return FluffConfig(
        overrides={
            "dialect": "ansi",
            "use_rust_engine": True,
            "large_file_skip_byte_limit": limit,
        }
    )


@pytest.mark.xfail(
    strict=True,
    reason="The engine's file loading does not check large_file_skip_byte_limit yet.",
)
def test__engine_parse_paths__respects_large_file_skip(tmp_path: Path):
    """A file over the limit is skipped with native's warning, not parsed."""
    path = tmp_path / "big.sql"
    path.write_text("select col_a, col_b from some_table;\n" * 50)
    size = path.stat().st_size

    # Collect from the logger directly. caplog relies on propagation to the
    # root logger, which CLI logging setup in other tests can turn off.
    captured: list[str] = []

    class _Collect(logging.Handler):
        def emit(self, record: logging.LogRecord) -> None:
            captured.append(record.getMessage())

    handler = _Collect(level=logging.WARNING)
    linter_logger = logging.getLogger("sqlfluff.linter")
    old_level = linter_logger.level
    linter_logger.addHandler(handler)
    linter_logger.setLevel(logging.WARNING)
    try:
        records = sqlfluffrs.engine_parse_paths([str(path)], _engine_config(100), None)
    finally:
        linter_logger.removeHandler(handler)
        linter_logger.setLevel(old_level)

    assert records == []
    assert (
        f"Length of file '{path}' is {size} bytes which is over the limit of "
        "100 bytes. Skipping to avoid parser lock. Users can increase this "
        "limit in their config by setting the 'large_file_skip_byte_limit' "
        "value, or disable by setting it to zero."
    ) in captured


@pytest.mark.parametrize("limit", [0, 10_000_000])
def test__engine_parse_paths__under_limit_parses(limit: int, tmp_path: Path):
    """A disabled (0) or generous limit leaves the file parsed."""
    path = tmp_path / "small.sql"
    path.write_text("select col_a from some_table;\n")
    records = sqlfluffrs.engine_parse_paths([str(path)], _engine_config(limit), None)
    assert len(records) == 1
    assert records[0]["segments"] is not None
