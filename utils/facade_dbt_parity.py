"""dbt-templater parity: façade fast-path lint AND fix vs native.

Sweeps the dbt templater plugin's fixture project through the production
gate functions, exactly like the jinja harnesses
(``facade_templated_{lint,fix}_parity.py``) but configured for dbt.

Requirements: run from the repo root with a dbt-capable interpreter (e.g.
``.tox/dbt1100/bin/python``), a postgres reachable per the fixture
profiles, ``dbt deps`` run in the fixture project, and the env vars the
fixture project needs (set below).

    .tox/dbt1100/bin/python utils/facade_dbt_parity.py

Expected: 0 divergences, 0 errors.
"""

import glob
import os
import traceback

os.environ.setdefault("passed_through_env", "_")
os.environ.setdefault("DBT_USE_EXPERIMENTAL_PARSER", "True")

import sqlfluffrs  # noqa: E402
from sqlfluff.core import FluffConfig, Linter  # noqa: E402
from sqlfluff.core.errors import SQLFluffSkipFile, SQLLintError  # noqa: E402
from sqlfluff.core.rules.rs_lint import (  # noqa: E402
    FACADE_SAFE_RULES,
    RsSegment,
    facade_fix_loop,
    facade_ignore_mask,
    facade_violations,
)

PLUGIN_ROOT = "plugins/sqlfluff-templater-dbt"
PROJECT = f"{PLUGIN_ROOT}/test/fixtures/dbt/dbt_project"
PROFILES = f"{PLUGIN_ROOT}/test/fixtures/dbt/profiles_yml"

RULESET = ",".join(sorted(FACADE_SAFE_RULES))


def cfg(rust: bool) -> FluffConfig:
    """Engine-on/off config for the dbt fixture project."""
    return FluffConfig(
        configs={
            "core": {
                "templater": "dbt",
                "dialect": "postgres",
                "rules": RULESET,
                "use_rust_parser": rust,
                "use_rust_engine": rust,
                "use_rust_rules": rust,
            },
            "templater": {
                "dbt": {"profiles_dir": PROFILES, "project_dir": PROJECT},
            },
        }
    )


def facade_lint(src: str, fname: str, c: FluffConfig) -> "list | None":
    """Mirror ``_facade_lint_file``'s gates (commands.py).

    Inline rather than imported: the dbt env's click version can't load
    the CLI module.
    """
    lnt = Linter(config=c)
    rule_pack = lnt.get_rulepack(config=c)
    rules = list(rule_pack.rules)
    rst = sqlfluffrs.engine_parse_to_tree(src, fname, c, None, True)
    if rst is None:
        return None
    root = RsSegment(rst.root)
    if next(root.recursive_crawl("unparsable"), None) is not None:
        return None
    tf = rst.templated_file
    if tf is None:
        return None
    if getattr(rst, "num_variants", 1) > 1:
        return None
    if getattr(rst, "templater_violations", None):
        return None
    ignore_mask, ivs = facade_ignore_mask(root, c, rule_pack.reference_map)
    violations = facade_violations(
        src, fname, c, rules, rst=rst, ignore_mask=ignore_mask
    )
    if violations is None:
        return None
    return violations + list(ivs)


def facade_fix(src: str, fname: str, c: FluffConfig) -> "str | None":
    """Mirror ``_try_facade_paths_fix``'s per-file logic (commands.py)."""
    lnt = Linter(config=c)
    rule_pack = lnt.get_rulepack(config=c)
    rules = list(rule_pack.rules)
    rst = sqlfluffrs.engine_parse_to_tree(src, fname, c, None, True)
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
    ignore_mask, _ivs = facade_ignore_mask(
        RsSegment(rst.root), c, rule_pack.reference_map
    )
    loop_state: dict = {}
    fixed = facade_fix_loop(
        src,
        fname,
        c,
        rules,
        int(c.get("runaway_limit")),
        rst=rst,
        lint_sink=[],
        loop_state=loop_state,
        ignore_mask=ignore_mask,
    )
    if loop_state.get("runaway"):
        return None
    if fixed != src and facade_violations(fixed, fname, c, rules) is None:
        return None
    return fixed


def keys(violations) -> list[tuple]:
    """Sortable comparison keys for a violation set."""
    return sorted(
        (v.rule_code(), v.line_no, v.line_pos, v.description) for v in violations
    )


n_files = n_routed = n_eligible = n_div = n_err = 0
divergences = []
files = sorted(
    glob.glob(os.path.join(PROJECT, "models", "**", "*.sql"), recursive=True)
)
for f in files:
    n_files += 1
    src = open(f, encoding="utf-8", errors="backslashreplace").read()
    try:
        # ---- LINT parity (gate logic mirrored inline) ----
        try:
            fac_v = facade_lint(src, f, cfg(True))
        except SQLFluffSkipFile:
            # dbt refuses this file (disabled model / missing var). The
            # production gates catch this and route to native, which must
            # handle it GRACEFULLY (lint_string absorbs the skip into its
            # result). Verify that — a native crash here would mean the
            # routing hands users an exception.
            Linter(config=cfg(False)).lint_string(src, fname=f)
            n_routed += 1
            continue
        if fac_v is None:
            n_routed += 1
            continue
        n_eligible += 1
        fac = keys(v for v in fac_v if isinstance(v, SQLLintError))
        nat_res = Linter(config=cfg(False)).lint_string(src, fname=f)
        nat = keys(v for v in nat_res.violations if isinstance(v, SQLLintError))
        if fac != nat:
            n_div += 1
            divergences.append((f + " [LINT]", nat, fac))
            continue

        # ---- FIX parity (byte compare) ----
        fac_fixed = facade_fix(src, f, cfg(True))
        if fac_fixed is None:
            continue  # routed for fix only
        nat_fixed = (
            Linter(config=cfg(False))
            .lint_string(src, fname=f, fix=True)
            .fix_string()[0]
        )
        if fac_fixed != nat_fixed:
            n_div += 1
            divergences.append((f + " [FIX]", nat_fixed, fac_fixed))
    except Exception as e:  # noqa: BLE001
        n_err += 1
        divergences.append((f + " [ERROR]", repr(e), traceback.format_exc()[-400:]))
    print(
        f"[{os.path.basename(f)}] routed={n_routed} eligible={n_eligible} "
        f"div={n_div} err={n_err}",
        flush=True,
    )

print(
    f"\n===== DBT PARITY: files={n_files} routed_native={n_routed} "
    f"facade_eligible={n_eligible} divergences={n_div} errors={n_err} ====="
)
for f, a, b in divergences[:20]:
    print(f"\n--- DIVERGE {f}\n    native: {a!r}\n    facade: {b!r}")
if len(divergences) > 20:
    print(f"\n... and {len(divergences) - 20} more")
