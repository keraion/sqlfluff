"""Lint (detection) parity: facade_violations vs native lint over the corpus.

For each fixture (under its own dialect), compare native ``lint_string``
violations against ``facade_violations`` — first as (rule, line, pos) tuples
(detection parity), then descriptions for position-matched pairs (text
parity). Files that route to native in production (unparsable, templated,
noqa) are skipped, mirroring the CLI gates.

Run from the repo root:

    python utils/facade_lint_parity.py [RULESET] [DIALECT]

RULESET defaults to all rules; expected result: 0 detection and 0 text
divergences and 0 errors. Uses the raw templater (like the dialect test
suite) so fixture SQL containing jinja-lookalike text (e.g.
postgres/array.sql's ``{{{...}}}`` array literal) doesn't error out —
see PARITY_NOTES.md.
"""

import glob
import os
import sys
from collections import Counter

import sqlfluffrs
from sqlfluff.core import FluffConfig, Linter
from sqlfluff.core.errors import SQLLintError
from sqlfluff.core.rules.rs_lint import RsSegment, facade_violations

CORPUS = "test/fixtures/dialects"
RULESET = sys.argv[1] if len(sys.argv) > 1 else None
DIALECT_FILTER = sys.argv[2] if len(sys.argv) > 2 else None

det_div = Counter()  # rule -> files with (rule,line,pos) set differences
txt_div = Counter()  # rule -> files with description differences
det_examples = {}
txt_examples = {}
n_files = n_eligible = n_det = n_txt = n_err = 0

for dialect in sorted(os.listdir(CORPUS)):
    dpath = os.path.join(CORPUS, dialect)
    if not os.path.isdir(dpath) or (DIALECT_FILTER and dialect != DIALECT_FILTER):
        continue
    overrides = {"dialect": dialect, "templater": "raw"}
    if RULESET:
        overrides["rules"] = RULESET
    cfg_nat = FluffConfig(overrides=dict(overrides))
    cfg_rs = FluffConfig(
        overrides={
            **overrides,
            "use_rust_parser": True,
            "use_rust_engine": True,
            "use_rust_rules": True,
        }
    )
    lnt_nat = Linter(config=cfg_nat)
    lnt_rs = Linter(config=cfg_rs)
    rules = list(lnt_rs.get_rulepack(config=cfg_rs).rules)
    for f in glob.glob(os.path.join(dpath, "**", "*.sql"), recursive=True):
        src = open(f).read()
        n_files += 1
        if "noqa" in src.lower():
            continue
        try:
            rst = sqlfluffrs.engine_parse_to_tree(src, f, cfg_rs, None, True)
            if rst is None:
                continue
            if next(RsSegment(rst.root).recursive_crawl("unparsable"), None):
                continue
            tf = rst.templated_file
            if tf is None or tf.source_str != tf.templated_str:
                continue
            fac = facade_violations(src, f, cfg_rs, rules, rst=rst)
            if fac is None:
                continue
            res = lnt_nat.lint_string(src)
            nat = [v for v in res.violations if isinstance(v, SQLLintError)]
            n_eligible += 1

            nat_keys = sorted((v.rule_code(), v.line_no, v.line_pos) for v in nat)
            fac_keys = sorted((v.rule_code(), v.line_no, v.line_pos) for v in fac)
            if nat_keys != fac_keys:
                n_det += 1
                seen = set()
                nk, fk = Counter(nat_keys), Counter(fac_keys)
                for k in set(nk) | set(fk):
                    if nk[k] != fk[k] and k[0] not in seen:
                        seen.add(k[0])
                        det_div[k[0]] += 1
                        det_examples.setdefault(k[0], (f, k, nk[k], fk[k]))
                continue  # don't double count text diffs on detection-diverged
            # position sets equal -> compare descriptions pairwise
            nat_s = sorted(nat, key=lambda v: (v.rule_code(), v.line_no, v.line_pos))
            fac_s = sorted(fac, key=lambda v: (v.rule_code(), v.line_no, v.line_pos))
            diff_rules = set()
            for a, b in zip(nat_s, fac_s):
                if a.description != b.description and a.rule_code() not in diff_rules:
                    diff_rules.add(a.rule_code())
                    txt_div[a.rule_code()] += 1
                    txt_examples.setdefault(
                        a.rule_code(), (f, a.description, b.description)
                    )
            if diff_rules:
                n_txt += 1
        except Exception as e:  # noqa: BLE001
            n_err += 1
            print(f"ERROR {f}: {e!r}", flush=True)
    print(
        f"[{dialect}] files={n_files} eligible={n_eligible} "
        f"det_div={n_det} txt_div={n_txt} err={n_err}",
        flush=True,
    )

print(
    f"\n===== LINT PARITY: files={n_files} eligible={n_eligible} "
    f"detection_diverged={n_det} text_diverged={n_txt} errors={n_err} ====="
)
print("\n-- detection divergences by rule (files) --")
for rule, cnt in det_div.most_common():
    f, k, nkc, fkc = det_examples[rule]
    print(f"  {rule}: {cnt} files | e.g. {f} at {k}: native x{nkc} vs facade x{fkc}")
print("\n-- description divergences by rule (files) --")
for rule, cnt in txt_div.most_common():
    f, a, b = txt_examples[rule]
    print(f"  {rule}: {cnt} files | e.g. {f}\n     native: {a!r}\n     facade: {b!r}")
