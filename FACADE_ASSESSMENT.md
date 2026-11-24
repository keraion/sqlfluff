# Façade viability assessment and Rust reflow spike

Session notes, 2026-11-23 to 2026-11-24. Base: main `63534d45a`.
Everything on this branch is spike code: measured, parity-checked, not
cleaned up for review.

## Question

Is the `RsSegment` arena façade (Python rules crawling the Rust parse arena
instead of a Python segment tree) viable, or should it be thrown out?

## Verdict

**Correct, but not worth shipping.** The speed case eroded as main improved.

| CLI lint, sqlfluff-testbed (min of 3) | pure Python | Rust parser + Python tree | engine + façade |
|---|---|---|---|
| Stack tip `rs-stack-12` (July code) | 12.46s | 8.73s | 5.75s |
| main `63534d45a` | 8.77s | **4.09s** | not measurable (stack unrebased) |

- Main's default path (4.09s) already beats the July façade (5.75s).
- Façade crawl vs native crawl near main (PR 9272 branch, 67/79 comparable
  rules, identical violations): 2.325s vs 2.243s = **1.04×**, i.e. parity.
- Python tree materialisation is 12.8% of lint, so the façade's ceiling on
  main is about **1.15×**, for a ~13.8K-line stack.
- The pieces already merged (arena, metadata, `is_type`/uuid refactors,
  parity fixes) keep their value.

Caveat: `sqlfluff-testbed/bench/run.py`'s "native" config is the pure-Python
parser, so its recorded speedups are not against the shipping default.

## Where lint time goes on main

Rust parser, in process: rule crawl 66%, parse 20% (of which tree
materialisation 12.8%). Inside the crawl the reflow engine
(`utils/reflow`, LT01/LT02/LT05/LT14) dominates.

## What was prototyped

### 1. Python-side wins (no Rust)

- **Shared walk** (`core/rules/crawlers.py`): one pre-order walk per tree
  (`_WalkIndex`), reused by every `SegmentSeekerCrawler`.
  Toggle: `SQLFLUFF_NO_SHARED_WALK`.
- **Select-analysis cache** (`utils/analysis/select.py`):
  `get_select_statement_info` cached per segment and dialect.
  Toggle: `SQLFLUFF_NO_SELECT_CACHE`.

Together: CLI lint 4.22s → 3.69s.

### 2. Reflow engine in Rust

`sqlfluffrs/sqlfluffrs_rules/src/reflow.rs` (~2,550 lines): sequence build,
depth map, block config, then

| Rule | Ported | Toggle |
|---|---|---|
| LT01 | respace | `SQLFLUFF_NO_RUST_LT01` |
| LT02 | reindent | `SQLFLUFF_NO_RUST_LT02` |
| LT05 | long lines, including fixes | `SQLFLUFF_NO_RUST_LT05` |
| LT14 | keyword rebreak, including fixes | `SQLFLUFF_NO_RUST_LT14` |

Contract: Rust returns leaf indexes (1:1 with Python `raw_segments`),
descriptions and fix specs; Python builds the `LintResult`/`LintFix`
(`utils/reflow/helpers.py::results_from_rust`). The built sequence is shared
across the four rules through a thread-local cache keyed on root uuid and
layout config.

Rust returns `None`, and Python runs as before, for anything not covered:
templated files, alignment, `implicit_indents = require`, LT05
`ignore_comment_lines` / `ignore_comment_clauses`.

Results, untemplated files, lint and fix output identical on 629 rule
fixtures and 1,765 files:

| | testbed | snowflake | postgres |
|---|---|---|---|
| parse + lint | 1.21× | 1.36× | 1.34× |
| fix (arena rebuilt between passes) | 1.13× | 1.16× | 1.20× |

Fix mode is held back by `apply_fixes`, which is 41–53% of fix time.

### 3. Arena rebuild between fix passes

`RsTree.from_segments` (`arena_py.rs`) rebuilds an arena from the fixed
Python tree; `rust_parser.py::rebuild_rs_tree` drives it; rules opt in with
`_rebuild_rust_arena = True`. Toggle: `SQLFLUFF_NO_ARENA_REBUILD`.

### 4. Fix validation in Rust

Fix validation re-matches a segment class's grammar against its already
typed leaves. Added `Parser::rematch_segment`, `PyParser::rematch_segment`
and `rematch_arena_leaves`, plus `Token::typed_leaf`.

After the two parser fixes below: **8,861 / 8,861** real validations agree
with Python (8,761 accept, 100 reject); repeated-token inputs 4,151 agree,
0 disagree; about 15× faster than Python.

### 5. Rust parser parity fixes

Found by the validation comparison. Pushed separately to `upstream` as
`keraion/rs-parser-gap-and-terminator-parity` (no PR opened), and also
applied on this branch:

- `8bc2bee7c` fix(rust): stop OneOf and Ref skipping a leading gap
  (`oneof.rs`, `ref_grammar.rs`). Origin: #7342, #7566; the inline path
  copied it in #8168.
- `a921118f8` fix(rust): don't require whitespace before a keyword
  terminator (`helpers.rs`). Origin: #7566.

## Roadmap: Python out of the lint/fix cycle

Goal: no Python in lint/fix except legacy plugin rules.

1. Rules in Rust: reflow first (done here for untemplated files), then
   templated reflow, then the remaining rule bodies.
2. Rule framework in Rust: crawl, noqa, ignore masks, result assembly.
3. Fix loop on a mutable arena, with Rust validation (shown feasible above),
   which removes `apply_fixes` and the per-pass rebuild.
4. Reporting straight from Rust results.
5. Plugin bridge: build the Python tree on demand, only when a Python rule
   is loaded.

The tree-materialisation cost only disappears once step 5 makes the Python
tree optional.

## Known issues

- Main's Rust CP01/CP03/CP04 give different violation text from Python
  under `use_rust_rules`. Found, not fixed.
- `RF04_test_pass_postgres_collation_words_as_columns` already fails on
  main; unrelated.
- An early "repeat a token" validation mismatch was a harness artefact
  (the same leaf inserted twice collides in Python's position-keyed parse
  cache), not a parser divergence.

## Build notes

- `uv` is not installed here: use `tox -e build-rs` with `~/.cargo/bin` on
  `PATH`, then `pip install --force-reinstall --no-deps dist/sqlfluffrs-*.whl`
  into the tox env.
- Run scripts from outside the repo root, or `import sqlfluffrs` picks up
  the source directory.
- Measurement scripts lived in a session scratchpad and are not on this
  branch.
