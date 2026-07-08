"""Whole-corpus FIX parity: façade vs native over test/fixtures/dialects/*.

Runs ALL given rules together (argv[1] is a comma-separated set passed
straight into the config) — the FACADE_SAFE_RULES promotion criterion: the
façade fix output must be byte-identical to native for every façade-eligible
file (mirroring the CLI routing gates). Run from the repo root:

    python utils/facade_fix_parity.py "$(python -c 'from sqlfluff.core.rules.rs_lint import FACADE_SAFE_RULES; print(",".join(sorted(FACADE_SAFE_RULES)))')"

Optionally pass a single dialect name as argv[2]. Expected result: 0
divergences and 0 errors. Uses the raw templater (like the dialect test
suite) so fixture SQL containing jinja-lookalike text (e.g.
postgres/array.sql's ``{{{...}}}`` array literal) doesn't error out —
see PARITY_NOTES.md.
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
    facade_ignore_mask,
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
            "templater": "raw",
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
    """Return facade-fixed source, or None if the file routes to native.

    Mirrors the CLI fast path's routing (commands.py): gave-up and unfixable
    residuals COMPLETE on the fast path (their bytes are final and must be
    byte-compared here); only a runaway revert, unparsable/templated sources
    and engine-parse failures route to native.
    """
    if not src:
        return None  # empty files route to native (LT12 under raw templating)
    c = cfg(dialect, True)
    lnt = Linter(config=c)
    rule_pack = lnt.get_rulepack(config=c)
    rules = list(rule_pack.rules)
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
    # noqa masks are applied facade-side now (like the CLI gates).
    ignore_mask, _ivs = facade_ignore_mask(
        RsSegment(rst.root), c, rule_pack.reference_map
    )
    pre = []
    loop_state = {}
    fixed = facade_fix_loop(
        src,
        fname,
        c,
        rules,
        limit,
        rst=rst,
        lint_sink=pre,
        loop_state=loop_state,
        ignore_mask=ignore_mask,
        reference_map=rule_pack.reference_map,
    )
    if loop_state.get("runaway"):
        return None  # the CLI defers runaway reverts to native
    if fixed != src and facade_violations(fixed, fname, c, rules) is None:
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
