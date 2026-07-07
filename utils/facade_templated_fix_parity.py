"""Templated-source FIX parity: façade vs native fix bytes, jinja corpus.

Companion to ``facade_templated_lint_parity.py`` — exercises TEMPLATED
sources through the production fix gates: files the gates route to native
count as routed; files the façade finishes must be byte-identical to
native's ``fix_string()`` output.

Run from the repo root:

    python utils/facade_templated_fix_parity.py [CORPUS_DIR ...]

Expected: 0 divergences, 0 errors.
"""

import glob
import os
import sys
import traceback

import sqlfluffrs
from sqlfluff.core import FluffConfig, Linter
from sqlfluff.core.errors import SQLFluffSkipFile
from sqlfluff.core.linter.discovery import paths_from_path
from sqlfluff.core.rules.rs_lint import (
    FACADE_SAFE_RULES,
    RsSegment,
    facade_fix_loop,
    facade_ignore_mask,
    facade_violations,
)

DEFAULT_CORPORA = [
    "test/fixtures/templater",
    os.path.expanduser("~/repos/sqlfluff-testbed/models"),
]
CORPORA = sys.argv[1:] or [d for d in DEFAULT_CORPORA if os.path.isdir(d)]

RULESET = ",".join(sorted(FACADE_SAFE_RULES))

RUST_ROOT = {
    "dialect": "ansi",
    "templater": "jinja",
    "rules": RULESET,
    "use_rust_parser": True,
    "use_rust_engine": True,
    "use_rust_rules": True,
}
NATIVE_ROOT = {
    "dialect": "ansi",
    "templater": "jinja",
    "rules": RULESET,
    "use_rust_parser": False,
    "use_rust_engine": False,
    "use_rust_rules": False,
}


def facade_fix(src: str, fname: str, cfg) -> "str | None":
    """Façade-fixed source, or None when the gates route the file to native.

    Mirrors ``_try_facade_paths_fix``'s per-file logic (commands.py).
    """
    lnt = Linter(config=cfg)
    rule_pack = lnt.get_rulepack(config=cfg)
    rules = list(rule_pack.rules)
    if not rules or any(r.code not in FACADE_SAFE_RULES for r in rules):
        return None
    rst = sqlfluffrs.engine_parse_to_tree(src, fname, cfg, None, True)
    if rst is None:
        return None
    if next(RsSegment(rst.root).recursive_crawl("unparsable"), None) is not None:
        return None
    tf = rst.templated_file
    if tf is None:
        return None
    if getattr(rst, "num_variants", 1) > 1:
        return None
    if getattr(rst, "templater_violations", None):
        return None
    # noqa masks are applied facade-side now (like the CLI gates).
    ignore_mask, _ivs = facade_ignore_mask(
        RsSegment(rst.root), cfg, rule_pack.reference_map
    )
    pre: list = []
    loop_state: dict = {}
    fixed = facade_fix_loop(
        src,
        fname,
        cfg,
        rules,
        int(cfg.get("runaway_limit")),
        rst=rst,
        lint_sink=pre,
        loop_state=loop_state,
        ignore_mask=ignore_mask,
    )
    if loop_state.get("runaway"):
        return None
    if fixed != src and facade_violations(fixed, fname, cfg, rules) is None:
        return None
    return fixed


n_files = n_routed = n_eligible = n_div = n_err = 0
divergences = []
for corpus in CORPORA:
    for f in sorted(glob.glob(os.path.join(corpus, "**", "*.sql"), recursive=True)):
        n_files += 1
        try:
            if not list(paths_from_path(f, target_file_exts=(".sql",))):
                n_routed += 1
                continue
            root_cfg = FluffConfig(overrides=dict(RUST_ROOT))
            try:
                raw, rust_cfg, _enc = Linter.load_raw_file_and_config(f, root_cfg)
            except SQLFluffSkipFile:
                n_routed += 1
                continue
            fac = facade_fix(raw, f, rust_cfg)
            if fac is None:
                n_routed += 1
                continue
            n_eligible += 1

            nat_cfg = FluffConfig(overrides=dict(NATIVE_ROOT)).make_child_from_path(f)
            nat_res = Linter(config=nat_cfg).lint_string(raw, fname=f, fix=True)
            nat = nat_res.fix_string()[0]
            if fac != nat:
                n_div += 1
                divergences.append((f, nat, fac))
        except Exception as e:  # noqa: BLE001
            n_err += 1
            divergences.append((f + " [ERROR]", repr(e), traceback.format_exc()[-500:]))
    print(
        f"[{corpus}] files={n_files} routed_native={n_routed} "
        f"eligible={n_eligible} div={n_div} err={n_err}",
        flush=True,
    )

print(
    f"\n===== TEMPLATED FIX PARITY: files={n_files} routed_native={n_routed} "
    f"facade_eligible={n_eligible} divergences={n_div} errors={n_err} ====="
)
for f, a, b in divergences[:20]:
    print(f"\n--- DIVERGE {f}\n    native: {a!r}\n    facade: {b!r}")
if len(divergences) > 20:
    print(f"\n... and {len(divergences) - 20} more")
