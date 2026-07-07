"""Whole-corpus FIX parity: façade vs native over test/fixtures/dialects/*.

Runs ALL given rules together (argv[1] is a comma-separated set passed
straight into the config) — the FACADE_SAFE_RULES promotion criterion: the
façade fix output must be byte-identical to native for every façade-eligible
file (mirroring the CLI routing gates). Run from the repo root:

    python utils/facade_fix_parity.py "$(python -c 'from sqlfluff.core.rules.rs_lint import FACADE_SAFE_RULES; print(",".join(sorted(FACADE_SAFE_RULES)))')"

Optionally pass a single dialect name as argv[2]. Expected result: 0
divergences (postgres/array.sql errors in the NATIVE baseline — it needs a
templater this harness doesn't configure — and is a known artifact).
"""

import glob
import os
import sys
import traceback

import sqlfluffrs
from sqlfluff.core import FluffConfig, Linter
from sqlfluff.core.rules.rs_lint import (
    RsSegment,
    facade_fix_loop,
    facade_violations,
)

CORPUS = "test/fixtures/dialects"
RULESET = sys.argv[1]  # comma-separated, passed as one config value
DIALECT_FILTER = sys.argv[2] if len(sys.argv) > 2 else None


def cfg(dialect, rust):
    """Config for one engine (rust=True -> the Rust engine/facade)."""
    return FluffConfig(
        overrides={
            "dialect": dialect,
            "rules": RULESET,
            "use_rust_parser": rust,
            "use_rust_engine": rust,
            "use_rust_rules": rust,
        }
    )


def native_fix(src, dialect):
    """Native fixed output for ``src``."""
    c = cfg(dialect, False)
    return Linter(config=c).lint_string(src, fix=True).fix_string()[0]


def facade_fix(src, fname, dialect):
    """Return facade-fixed source, or None if the file routes to native."""
    c = cfg(dialect, True)
    lnt = Linter(config=c)
    rules = list(lnt.get_rulepack(config=c).rules)
    limit = int(c.get("runaway_limit"))
    rst = sqlfluffrs.engine_parse_to_tree(src, fname, c, None, True)
    if rst is None:
        return None
    root = RsSegment(rst.root)
    if next(root.recursive_crawl("unparsable"), None) is not None:
        return None
    tf = rst.templated_file
    if tf is None or getattr(tf, "source_str", None) != getattr(
        tf, "templated_str", None
    ):
        return None
    pre = facade_violations(src, fname, c, rules)
    if pre is None:
        return None
    fixed = facade_fix_loop(src, fname, c, rules, limit)
    post = facade_violations(fixed, fname, c, rules)
    if post is None or post:
        return None
    return fixed


def dialects():
    """Yield (dialect, path) corpus directories."""
    for d in sorted(os.listdir(CORPUS)):
        p = os.path.join(CORPUS, d)
        if os.path.isdir(p) and (DIALECT_FILTER is None or d == DIALECT_FILTER):
            yield d, p


n_files = n_eligible = n_div = n_err = 0
divergences = []
for dialect, dpath in dialects():
    for f in glob.glob(os.path.join(dpath, "**", "*.sql"), recursive=True):
        src = open(f).read()
        n_files += 1
        try:
            nat = native_fix(src, dialect)
            fac = facade_fix(src, f, dialect)
            if fac is None:
                continue
            n_eligible += 1
            if fac != nat:
                n_div += 1
                divergences.append((f, nat, fac))
        except Exception as e:  # noqa: BLE001
            n_err += 1
            divergences.append((f + " [ERROR]", repr(e), traceback.format_exc()[-400:]))
    print(
        f"[{dialect}] done: files={n_files} eligible={n_eligible} "
        f"div={n_div} err={n_err}",
        flush=True,
    )

print(
    f"\n===== COMBINED [{RULESET}]: files={n_files} facade_eligible={n_eligible} "
    f"divergences={n_div} errors={n_err} ====="
)
for f, a, b in divergences[:40]:
    print(f"\n--- DIVERGE {f}")
    print(f"    native: {a!r}")
    print(f"    facade: {b!r}")
if len(divergences) > 40:
    print(f"\n... and {len(divergences) - 40} more")
