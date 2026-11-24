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

## Templated reflow scope

Measured 2026-11-24 on this branch. In process, parse + full-rule lint, min
of 3, Rust parser. "Templated" = the same test as `is_untemplated()` in
`reflow.rs`. No code was ported.

### Headline

- **The testbed is not a templated corpus.** Only 12 of its 191 files are
  templated, and they are 0.5% of reflow time. The 1.21–1.28× testbed number
  is an untemplated number.
- **LT01 already runs in Rust on templated files.** Respace has no
  `is_untemplated()` guard; it answered on every templated file measured
  except 2 of 99 repo fixtures and 4 of 20 rule cases. Only LT02, LT05 and
  LT14 fall back.
- **On an all-templated project the spike is worth nothing today**
  (0.97–0.99×). LT01's Rust saving is cancelled because Python LT02 then
  pays the cache warm-up that Python LT01 used to pay (jaffle shop: LT02 is
  15.8ms after Python LT01, 39.7ms alone, 37.4ms after Rust LT01).

### Who falls back

| Corpus | Files | Templated | LT01 fallback | LT02 / LT05 / LT14 fallback | Today (Rust on vs four rules in Python) |
|---|---|---|---|---|---|
| sqlfluff-testbed (postgres) | 191 | 12 (6%) | 0 | 12 each; 6% / 4% / 5% of that rule's Python time | 1.28× |
| jaffle_shop_duckdb | 14 | 14 (100%) | 0 | 14 each; 100% | 0.97× |
| Repo fixtures (`test/fixtures`, jinja, own configs) | 99 (36 more skipped: template or parse error) | 99 (100%) | 2 | 99 each; 100% | 0.99× |

Rule fixture cases (`std_rule_cases/LT*.yml`), counts only:

| Rule | Cases | Templated | Rust answers today |
|---|---|---|---|
| LT01 | 212 | 20 (9%) | 16 |
| LT02 | 305 | 43 (14%) | 0 |
| LT05 | 77 | 28 (36%) | 0 |
| LT14 | 35 | 1 (3%) | 0 |

No fallback had a cause other than templating.

### Why files are templated

Share of templated files containing each construct (a file can be in
several). For the two timed corpora the second figure is the share of
four-rule Python time.

| Construct | testbed (12) | jaffle (14) | repo fixtures (99) | rule cases (92) |
|---|---|---|---|---|
| `{{ expr }}` | 100% | 100% | 72% / 83% | 75% |
| `{% set x = … %}` | 100% | 29% | 16% / 11% | 13% |
| `{# comment #}` | 0 | 43% | 2% | 4% |
| expression rendering a newline | 0 | 0 | 13% / 43% | 11% |
| `{% if %}` | 0 | 0 | 45% / 57% | 26% |
| unrendered source (untaken branch, skipped literal) | 0 | 71% | 29% / 12% | 30% |
| whitespace-consuming tag (`{%-`, `-%}`) | 0 | 71% | 13% / 7% | 21% |
| `{% for %}` | 100% | 29% | 26% / 62% | 33% |
| loop body repeated (`template_loop`) | 100% | 29% | 24% / 61% | 30% |
| `{% macro %}` / `{% call %}` | 0 | 0 | 1% | 8% |
| other (`raw`, `include`, `import`, `do`, block `set`) | 0 | 0 | 6% | 0 |

Files fully covered by each cumulative step (files / time):

| Step | testbed | jaffle | repo fixtures | rule cases |
|---|---|---|---|---|
| 1. expressions, comments, `set` | 0 | 29% / 22% | 39% / 25% | 46% |
| 2. + `if`, unrendered source, whitespace-consuming tags | 0 | 71% / 39% | 67% / 33% | 60% |
| 3. + `for` loops | 100% | 100% / 100% | 92% / 93% | 92% |
| 4. + macro, call, the rest | 100% | 100% | 100% | 100% |

Whitespace-consuming tags never appear without unrendered source, so they
are not a step of their own. No small subset reaches 80%: on the repo
fixtures 50% of files needs 3 constructs, 80% needs 8, 95% needs 11; by time
50% already needs `for` loops. Loops are where the time is.

### Upper bound

If all four rules ran in Rust on templated files at the per-rule ratio
measured on files that do run in Rust (LT01 4.9–7.5×, LT02 7.2×, LT05 4.4×,
LT14 1.2×). This is arithmetic, not a measurement, and a ceiling: templated
paths do more work than untemplated ones.

| Corpus | Today | Upper bound |
|---|---|---|
| testbed | 1.28× | 1.29× |
| jaffle shop | 0.97× | 1.22× |
| repo fixtures | 0.99× | 1.20× |

So templated support does not raise the ceiling; it extends the existing
~1.2–1.3× to templated projects, which today get none of it.

### What the port needs

The arena already carries everything: `Arena::pos_marker` (source and
templated slices, `is_literal`, the templated file), `block_type`,
`source_str` and `block_uuid` for placeholders, and `is_templated`. The
sequence build already treats placeholders and whitespace-only literal
placeholders correctly (that is why LT01 works). What is missing is rule
logic, plus one new fix kind.

| Construct | Python to port | Notes |
|---|---|---|
| Expressions, comments, `set` | `_IndentLine.is_all_templates`; the templated-placeholder newline check in `_crawl_indent_points`; `_deduce_line_current_indent` templated branch; `_is_templated_safe_break` (LT05); `RebreakLocation.has_templated_newline` (LT14) | `source_char_len` is already ported. Mostly guards. |
| Whitespace-consuming tags | `has_untemplated_newline`; `get_consumed_whitespace` users in `ReflowPoint.num_newlines`, `get_indent_segment`, `get_indent`; the placeholder branch of `ReflowPoint.indent_to`; the leading-placeholder branch of `_lint_line_starting_indent` | Needs a **new fix kind**: edit a placeholder's `source_str` with a `SourceFix`. The Rust→Python result contract has none today. |
| Unrendered source | `_revise_skipped_source_lines`; `_rebalance_unrendered_block_indents` | Uses `block_type == "skipped_source"` and `block_uuid`. |
| `if` / `for` blocks | `_revise_templated_lines` (about 400 lines, the hardest function in reindent); block checks in `_lint_line_untaken_positive_indents` and `_lint_line_untaken_negative_indents` | Groups lines by `block_uuid`, uses `block_start` / `block_mid` / `block_end`. |
| Loop repeats | `_revise_loop_repeated_lines` | Groups lines by source slice. |
| Macro, call, the rest | nothing specific found | Not checked; they render as expressions or skipped source. |

### Recommended order

1. **LT02 guards for expressions, comments and `set`** — about 150 lines of
   Rust. Small, but covers no testbed file and a quarter of fixture time.
2. **Blocks: `_revise_templated_lines`, loop repeats, unrendered source** —
   about 600–700 lines. This is the step that matters: it takes coverage
   from ~25% to ~93% of time. It cannot be split usefully, because `for`
   files also have `set` and expressions.
3. **Whitespace-consuming tags and the source-fix kind** — about 150 lines
   plus the contract change. Needed for jaffle-style dbt models (71% of
   them).
4. **LT05 and LT14 templated guards** — about 50 lines; do them alongside
   step 1, they are cheap but worth little time on their own.

Sizes are estimates from the Python line counts, not from a port.

Until step 2 lands, a templated project gets nothing, and possibly slightly
less than nothing, from the Rust reflow.

### Not measured

- A real dbt project of any size. Jaffle shop is 14 files; the repo
  fixtures are test snippets, each with its own config. The construct mix
  above is the best available, not a production sample.
- The dbt templater (jinja templater only).
- Fix mode on templated files.
- Whether templated Rust paths keep the untemplated speed ratio.
- Why LT01's Rust path declines 2 fixture files and 4 rule cases.
- 36 repo fixture files that fail to template or parse.
- Script: `scope.py` and `warm.py` in the session scratchpad, not on this
  branch.
