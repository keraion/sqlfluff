"""Templated-source LINT parity: the façade fast path vs native, jinja corpus.

Unlike ``facade_lint_parity.py`` (raw templater, dialect corpus), this sweep
exercises TEMPLATED sources through the production gate function itself
(``_facade_lint_file``): files the gate routes to native are counted as such;
files it handles must produce native-identical violations.

Corpus: ``test/fixtures/templater/**`` (per-fixture ``.sqlfluff`` configs are
honoured via ``make_child_from_path``) plus, if present, the sqlfluff-testbed
repo's ``models/`` tree.

Run from the repo root:

    python utils/facade_templated_lint_parity.py [CORPUS_DIR ...]

Expected: 0 divergences, 0 errors.
"""

import glob
import os
import sys
import traceback

from sqlfluff.cli.commands import _facade_lint_file
from sqlfluff.core import FluffConfig, Linter
from sqlfluff.core.errors import SQLLintError
from sqlfluff.core.rules.rs_lint import (
    FACADE_SAFE_RULES,
    FACADE_SAFE_RULES_DETECTION_UNSAFE,
)

DEFAULT_CORPORA = [
    "test/fixtures/templater",
    os.path.expanduser("~/repos/sqlfluff-testbed/models"),
]
CORPORA = sys.argv[1:] or [d for d in DEFAULT_CORPORA if os.path.isdir(d)]

# The lint fast path requires every selected rule to be façade-safe AND
# detection-verified — with the default all-rules set the gate always routes
# to native. Use the eligible set, like a fast-path run would.
RULESET = ",".join(sorted(FACADE_SAFE_RULES - FACADE_SAFE_RULES_DETECTION_UNSAFE))

# Root configs; per-file .sqlfluff files layer on via make_child_from_path /
# lint_paths. ``dialect`` is only the fallback for fixtures without one.
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


def keys(violations) -> list[tuple]:
    """Sortable comparison keys for a violation set."""
    return sorted(
        (v.rule_code(), v.line_no, v.line_pos, v.description) for v in violations
    )


n_files = n_routed = n_eligible = n_div = n_err = 0
divergences = []
for corpus in CORPORA:
    for f in sorted(glob.glob(os.path.join(corpus, "**", "*.sql"), recursive=True)):
        n_files += 1
        try:
            # Mirror production discovery + loading: files excluded by ignore
            # files never reach the fast-path gate, and over-limit files raise
            # SQLFluffSkipFile in load_raw_file_and_config (skipped by BOTH
            # engines identically).
            from sqlfluff.core.errors import SQLFluffSkipFile
            from sqlfluff.core.linter.discovery import paths_from_path

            if not list(paths_from_path(f, target_file_exts=(".sql",))):
                n_routed += 1
                continue
            root_cfg = FluffConfig(overrides=dict(RUST_ROOT))
            try:
                raw, rust_cfg, _enc = Linter.load_raw_file_and_config(f, root_cfg)
            except SQLFluffSkipFile:
                n_routed += 1
                continue
            linted = _facade_lint_file(raw, f, rust_cfg, Linter(config=rust_cfg))
            if linted is None:
                n_routed += 1
                continue
            n_eligible += 1
            fac = keys(v for v in linted.violations if isinstance(v, SQLLintError))

            nat_linter = Linter(config=FluffConfig(overrides=dict(NATIVE_ROOT)))
            nat_result = nat_linter.lint_paths((f,))
            nat_all = nat_result.get_violations()
            nat = keys(v for v in nat_all if isinstance(v, SQLLintError))
            nat_other = [v for v in nat_all if not isinstance(v, SQLLintError)]
            if nat_other:
                # The gate should have routed anything with TMP/PRS output.
                n_div += 1
                divergences.append(
                    (f, f"native has non-lint violations: {nat_other}", fac)
                )
            elif fac != nat:
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
    f"\n===== TEMPLATED LINT PARITY: files={n_files} routed_native={n_routed} "
    f"facade_eligible={n_eligible} divergences={n_div} errors={n_err} ====="
)
for f, a, b in divergences[:30]:
    print(f"\n--- DIVERGE {f}\n    native: {a!r}\n    facade: {b!r}")
if len(divergences) > 30:
    print(f"\n... and {len(divergences) - 30} more")
