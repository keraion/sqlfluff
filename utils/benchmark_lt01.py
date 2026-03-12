#!/usr/bin/env python3
"""Benchmark LT01 respace: Rust reflow vs Python reflow.

Both paths use the Rust lexer + Rust parser to produce the segment tree.
The only difference is whether respace runs in Rust (RustReflowSequence) or
Python (ReflowSequence).

Usage:
    python utils/benchmark_lt01.py              # default 100 runs
    python utils/benchmark_lt01.py --runs 50
    python utils/benchmark_lt01.py --runs 200 --dialect ansi
"""

from __future__ import annotations

import argparse
import statistics
import sys
import textwrap
import time
from typing import Optional

# ─────────────────────────────────────────────────────────────────────────────
# Availability checks
# ─────────────────────────────────────────────────────────────────────────────

try:
    from sqlfluffrs import (
        rs_make_reflow_config,
        rs_respace_with_config_obj,
    )

    _HAS_RUST = True
except ImportError:
    _HAS_RUST = False
    rs_make_reflow_config = None  # type: ignore[assignment]
    rs_respace_with_config_obj = None  # type: ignore[assignment]

from sqlfluff.utils.reflow.rust_bridge import (
    _HAS_RUST_RESPACE,
    RustReflowSequence,
    convert_rust_violations,
)
from sqlfluff.utils.reflow.sequence import ReflowSequence

# ─────────────────────────────────────────────────────────────────────────────
# SQL test corpus
# ─────────────────────────────────────────────────────────────────────────────

_SMALL = textwrap.dedent("""\
    SELECT a,b,c FROM foo WHERE x=1
""")

_MEDIUM = textwrap.dedent("""\
    SELECT
        a,        b,   c,d  ,
        e  +  f   AS total,
        CASE  WHEN g>0 THEN  'yes'  ELSE  'no'  END  AS flag,
        COUNT(*)  OVER  (PARTITION  BY  h  ORDER  BY  i)  AS  cnt
    FROM   foo   AS   f
    JOIN   bar   AS   b   ON   f.id=b.fk
    LEFT   JOIN   baz   ON   baz.id=f.id
    WHERE
        f.active   =   1   AND
        b.value   >   42   OR
        (  f.name   LIKE  '%test%'  )
    GROUP   BY   1,2,3
    HAVING   COUNT(*)   >   5
    ORDER   BY   total   DESC,   cnt   ASC
    LIMIT   100
""")

# Larger synthetic query (~80 tokens of meaningful content with deliberate
# spacing violations so respace has real work to do).
_LARGE = textwrap.dedent("""\
    WITH
        cte_a  AS  (
            SELECT
                id,   name,   value,
                ROW_NUMBER()  OVER  (
                    PARTITION  BY  category  ORDER  BY  value  DESC
                )  AS  rn,
                SUM(value)  OVER  (PARTITION  BY  category)  AS  cat_total,
                value  /  NULLIF(SUM(value)  OVER  (PARTITION  BY  category),0)  AS  pct
            FROM  raw_data
            WHERE  status  =  'active'  AND  deleted_at  IS  NULL
        ),
        cte_b  AS  (
            SELECT  a.*,  b.extra_col
            FROM  cte_a  AS  a
            LEFT  JOIN  extras  AS  b  ON  a.id  =  b.ref_id  AND  b.type  =  'main'
            WHERE  a.rn  =  1
        ),
        cte_c  AS  (
            SELECT
                category,
                COUNT(*)  AS  n,
                AVG(value)  AS  avg_val,
                MIN(value)  AS  min_val,
                MAX(value)  AS  max_val,
                PERCENTILE_CONT(0.5)  WITHIN  GROUP  (ORDER  BY  value)  AS  median_val
            FROM  cte_b
            GROUP  BY  category
        )
    SELECT
        b.id,
        b.name,
        b.value,
        b.pct,
        c.n,
        c.avg_val,
        c.median_val,
        b.value  -  c.avg_val  AS  delta,
        RANK()  OVER  (ORDER  BY  b.value  DESC)  AS  overall_rank
    FROM  cte_b  AS  b
    JOIN  cte_c  AS  c  ON  b.category  =  c.category
    WHERE  b.value  >  c.avg_val
    ORDER  BY  overall_rank,  b.category,  b.name
""")

CORPUS = {
    "small": _SMALL,
    "medium": _MEDIUM,
    "large": _LARGE,
}


# ─────────────────────────────────────────────────────────────────────────────
# Parse helper — Rust lexer + Rust parser → BaseSegment with _rs_node
# ─────────────────────────────────────────────────────────────────────────────


def _parse_with_rust(sql: str, dialect: str = "ansi"):
    """Lex with Rust, parse with Rust, return a BaseSegment tree.

    The returned root segment will have ``._rs_node`` attached, which is
    what enables the Rust respace fast-path in :class:`RustReflowSequence`.
    """
    from sqlfluff.core import FluffConfig
    from sqlfluff.core.parser import Lexer
    from sqlfluff.core.parser.rust_parser import RustParser

    config = FluffConfig.from_kwargs(dialect=dialect)
    lexer = Lexer(config=config)
    tokens, _ = lexer.lex(sql)
    parser = RustParser(config=config)
    return parser.parse(tuple(tokens), fname="<benchmark>")


# ─────────────────────────────────────────────────────────────────────────────
# Benchmark functions
# ─────────────────────────────────────────────────────────────────────────────


def _bench(fn, runs: int) -> dict[str, float]:
    """Run *fn* *runs* times and return timing statistics (milliseconds)."""
    # Warm-up
    for _ in range(min(5, runs)):
        fn()

    times = []
    for _ in range(runs):
        t0 = time.perf_counter()
        fn()
        times.append((time.perf_counter() - t0) * 1_000)

    return {
        "min": min(times),
        "mean": statistics.mean(times),
        "median": statistics.median(times),
        "stdev": statistics.stdev(times) if len(times) > 1 else 0.0,
        "max": max(times),
    }


def benchmark_rust_respace(tree, config, runs: int) -> dict[str, float]:
    """Benchmark the Rust respace path.

    Includes both Rust reflow and Python violation conversion.
    """

    def run():
        RustReflowSequence.from_root(tree, config=config).respace().get_results()

    return _bench(run, runs)


def benchmark_rust_respace_raw(tree, config, runs: int) -> dict[str, float]:
    """Benchmark just the Rust reflow computation (no Python violation conversion).

    This isolates the pure Rust cost (FFI crossing + reflow algorithm) from the
    Python-side ``convert_rust_violations`` overhead.
    """
    rs_node = getattr(tree, "_rs_node", None)
    if rs_node is None:
        return {"min": 0, "mean": 0, "median": 0, "stdev": 0, "max": 0}

    layout_dict = config.get_section(["layout", "type"])
    rs_cfg = rs_make_reflow_config(layout_dict)

    def run():
        rs_respace_with_config_obj(rs_node, rs_cfg)

    return _bench(run, runs)


def benchmark_rust_violation_conversion(tree, config, runs: int) -> dict[str, float]:
    """Benchmark just the Python violation-conversion step (subtract raw Rust time).

    Pre-compute the raw violations once, then time only the
    ``convert_rust_violations`` call to isolate Python object-creation cost.
    """
    rs_node = getattr(tree, "_rs_node", None)
    if rs_node is None:
        return {"min": 0, "mean": 0, "median": 0, "stdev": 0, "max": 0}

    layout_dict = config.get_section(["layout", "type"])
    rs_cfg = rs_make_reflow_config(layout_dict)
    violations = rs_respace_with_config_obj(rs_node, rs_cfg)
    raw_segs = tree.raw_segments

    def run():
        convert_rust_violations(violations, raw_segs)

    return _bench(run, runs)


def benchmark_python_respace(tree, config, runs: int) -> dict[str, float]:
    """Benchmark the Python respace path (ReflowSequence, ignores _rs_node)."""

    def run():
        ReflowSequence.from_root(tree, config=config).respace().get_results()

    return _bench(run, runs)


# ─────────────────────────────────────────────────────────────────────────────
# Output helpers
# ─────────────────────────────────────────────────────────────────────────────

_W = 72


def _header(title: str) -> None:
    print("\n" + "═" * _W)
    print(f"  {title}")
    print("═" * _W)


def _row(label: str, stats: dict[str, float], baseline: Optional[dict] = None) -> None:
    speedup = ""
    if baseline is not None:
        ratio = baseline["mean"] / stats["mean"]
        speedup = (
            f"  ({ratio:+.2f}x vs baseline)"
            if ratio >= 1
            else f"  ({ratio:.2f}x vs baseline)"
        )
    print(
        f"  {label:<20}  "
        f"mean={stats['mean']:7.3f}ms  "
        f"median={stats['median']:7.3f}ms  "
        f"stdev={stats['stdev']:6.3f}ms  "
        f"[min={stats['min']:.3f} max={stats['max']:.3f}]"
        f"{speedup}"
    )


# ─────────────────────────────────────────────────────────────────────────────
# Main
# ─────────────────────────────────────────────────────────────────────────────


def main() -> int:
    """Run the LT01 respace benchmarks and print a comparison table."""
    arg_parser = argparse.ArgumentParser(
        description="Benchmark LT01 respace: Rust vs Python reflow"
    )
    arg_parser.add_argument("--runs", type=int, default=100, help="Iterations per case")
    arg_parser.add_argument("--dialect", default="ansi", help="SQL dialect")
    arg_parser.add_argument(
        "--size",
        choices=["small", "medium", "large", "all"],
        default="all",
        help="Which SQL size(s) to benchmark",
    )
    args = arg_parser.parse_args()

    # ── Pre-flight checks ──────────────────────────────────────────────────
    if not _HAS_RUST:
        print("ERROR: sqlfluffrs is not installed.  Build it first:")
        print("  cd sqlfluffrs && maturin develop --features python")
        return 1

    if not _HAS_RUST_RESPACE:
        print("WARNING: rs_respace_with_config_obj not available in sqlfluffrs.")
        print("  The 'Rust respace' column will show the Python fallback time.")

    from sqlfluff.core import FluffConfig

    config = FluffConfig.from_kwargs(dialect=args.dialect)

    sizes = list(CORPUS.keys()) if args.size == "all" else [args.size]

    print(f"\nLT01 respace benchmark  —  dialect={args.dialect!r}  runs={args.runs}")
    print(f"Rust respace available : {_HAS_RUST_RESPACE}")
    print(f"Sizes                  : {', '.join(sizes)}")

    for size in sizes:
        sql = CORPUS[size]
        token_count = len(sql.split())  # rough word count as proxy

        _header(f"Size: {size!r}  (~{len(sql)} chars, ~{token_count} words)")

        # Parse once — both paths share the same tree
        tree = _parse_with_rust(sql, dialect=args.dialect)
        if tree is None:
            print(f"  SKIP: parse returned None for {size!r}")
            continue

        has_rs_node = getattr(tree, "_rs_node", None) is not None
        print(f"  _rs_node attached      : {has_rs_node}")

        # Run benchmarks
        py_stats = benchmark_python_respace(tree, config, args.runs)
        rs_stats = benchmark_rust_respace(tree, config, args.runs)
        rs_raw_stats = benchmark_rust_respace_raw(tree, config, args.runs)
        rs_conv_stats = benchmark_rust_violation_conversion(tree, config, args.runs)

        print()
        _row("Python respace", py_stats)
        _row("Rust respace (total)", rs_stats, baseline=py_stats)
        if has_rs_node:
            _row("  ↳ Rust FFI only", rs_raw_stats)
            _row("  ↳ violation conv", rs_conv_stats)

        if has_rs_node:
            speedup = py_stats["mean"] / rs_stats["mean"]
            if speedup >= 1:
                print(f"\n  → Rust is {speedup:.2f}x faster than Python")
            else:
                ffi_pct = rs_raw_stats["mean"] / rs_stats["mean"] * 100
                conv_pct = rs_conv_stats["mean"] / rs_stats["mean"] * 100
                print(
                    f"\n  → Python is {1 / speedup:.2f}x faster than Rust"
                    f"  (FFI={ffi_pct:.0f}%  conv={conv_pct:.0f}%  of Rust total)"
                )

    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
