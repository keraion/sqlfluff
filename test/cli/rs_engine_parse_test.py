"""Wiring tests for the Rust-engine `parse` path (``engine_parse_paths``).

Skipped unless the ``sqlfluffrs`` engine is built.
"""

import pytest

try:
    import sqlfluffrs

    _HAS_ENGINE = hasattr(sqlfluffrs, "engine_parse_paths")
except ImportError:  # pragma: no cover
    _HAS_ENGINE = False

from sqlfluff.core import FluffConfig

pytestmark = pytest.mark.skipif(
    not _HAS_ENGINE, reason="sqlfluffrs.engine_parse_paths unavailable"
)


def _cfg(limit: int) -> FluffConfig:
    return FluffConfig(
        overrides={
            "dialect": "ansi",
            "use_rust_engine": True,
            "large_file_skip_byte_limit": limit,
        }
    )


def test__engine_parse_paths__respects_large_file_skip(tmp_path, caplog):
    """Over-limit files are skipped with native's warning, not parsed.

    Regression test: the engine's Rust-side file loading bypassed
    ``large_file_skip_byte_limit`` (checked natively in
    ``Linter.load_raw_file_and_config`` — "to avoid parser lock"), so
    ``sqlfluff parse`` with the engine parsed files native refuses.
    """
    f = tmp_path / "big.sql"
    f.write_text("select col_a, col_b from some_table;\n" * 50)
    size = f.stat().st_size

    with caplog.at_level("WARNING", logger="sqlfluff.linter"):
        records = sqlfluffrs.engine_parse_paths([str(f)], _cfg(100), None)
    assert records == []  # excluded from output, like native
    warning = next(
        (
            r.message
            for r in caplog.records
            if "Skipping to avoid parser lock" in r.message
        ),
        None,
    )
    assert warning is not None
    # Byte-match native's SQLFluffSkipFile message.
    assert warning == (
        f"Length of file '{f}' is {size} bytes which is over the limit of "
        "100 bytes. Skipping to avoid parser lock. Users can increase this "
        "limit in their config by setting the 'large_file_skip_byte_limit' "
        "value, or disable by setting it to zero."
    )


@pytest.mark.parametrize("limit", [0, 10_000_000])
def test__engine_parse_paths__under_limit_parses(tmp_path, limit):
    """Disabled (0) or generous limits leave the file parsed as before."""
    f = tmp_path / "small.sql"
    f.write_text("select col_a from some_table;\n")

    records = sqlfluffrs.engine_parse_paths([str(f)], _cfg(limit), None)
    assert len(records) == 1
    assert records[0]["segments"] is not None
