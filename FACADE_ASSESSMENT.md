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
alignment, `implicit_indents = require`, LT05
`ignore_comment_lines` / `ignore_comment_clauses`.

Results, untemplated files, lint and fix output identical on 629 rule
fixtures and 1,765 files:

| | testbed | snowflake | postgres |
|---|---|---|---|
| parse + lint | 1.21× | 1.36× | 1.34× |
| fix (arena rebuilt between passes) | 1.13× | 1.16× | 1.20× |

Fix mode was held back by `apply_fixes`, 41–53% of fix time at this point.
Section 6 moves it to Rust.

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

### 6. Applying fixes on the arena

`arena_fix.rs` mirrors Python's `apply_fixes` on the arena: anchor lookup,
create-before/after pairing, positioning of new segments, pushing non-code
out of segments, and per-segment validation through the Rust re-match. The
old arena is left alone; Rust returns a new arena plus a pre-order plan, and
`rust_parser.py::apply_fixes_rust` builds the Python tree from it, reusing
unchanged subtrees. Toggle: `SQLFLUFF_NO_RUST_APPLY`
(`SQLFLUFF_RUST_APPLY_NO_REUSE` is for measuring only).

It started as reflow-only (fixes described by the Rust reflow's own spec) and
was then generalised to any rule: Python describes each `LintFix` as edit
type, the anchor's pre-order position, and its new segments. New leaves and
fresh containers (AL01's `AS` operator) are supported, and fixes at the file
root validate on the parser's own arena.

Fix mode, in process, min of 3, measured alone:

| | Python rules | Rust reflow, Python apply | Rust apply, reflow fixes only | Rust apply, all rules |
|---|---|---|---|---|
| testbed (191 files) | 7.67s | 6.50s (1.18×) | 1.14× | 3.95s (1.94×) |
| snowflake (280) | 24.23s | 21.16s (1.15×) | 1.69× | 8.91s (2.72×) |
| postgres (227) | 21.25s | 18.27s (1.16×) | 1.77× | 7.81s (2.72×) |

The reflow-only column is from the run before generalising, against that
run's own baseline.

Per-stage cost of the Rust apply, all rules:

| | Passes | Apply | Validate | Build Python tree | Python `apply_fixes` left |
|---|---|---|---|---|---|
| testbed | 527 | 135ms | 41ms | 112ms | 302ms (templated files) |
| snowflake | 621 | 525ms | 360ms | 756ms | 351ms |
| postgres | 411 | 454ms | 277ms | 783ms | 292ms |

Python `apply_fixes` was 36–43% of fix time before this and is 3–7% after.

Parity: with the Rust apply on and off, the fixed output is identical on all
2,944 rule fixture cases (every rule) and 1,765 corpus files (testbed, ansi,
postgres, bigquery, tsql, snowflake, mysql, sparksql, oracle). Each of the
4,507 Rust passes was also compared with Python's `apply_fixes` on the same
tree, node by node (class, raw, source slice, templated slice, working
position), with no mismatch.

Handed back to Python (378 passes in the parity run):

- fixes that move an existing subtree (ST06, ST09, CV11, some LT09): 358;
- anchor not in the tree: 10; validation failed (Python rejects these too): 10;
- unparsable sections never reach it. (Templated files did not either at
  this point; see "Templated fix mode: ported" at the end.)

Not ported: `max_parse_nodes` in validation, `fix_even_unparsable`. Rebuilt
containers get new uuids where Python sometimes keeps them; no parity
difference showed up.

### Where time goes now

All Rust paths on, in process, min of 3.

| Fix mode | testbed (4.02s) | snowflake (8.88s) | postgres (7.63s) |
|---|---|---|---|
| Rule crawls | 47.4% | 39.3% | 39.7% |
| Parse (lex, parse, build Python tree) | 22.7% | 22.3% | 23.2% |
| Other (noqa, loop overhead, result assembly) | 15.4% | 21.2% | 20.8% |
| Rust apply + validate + build tree | 6.4% | 12.9% | 12.7% |
| Python `apply_fixes` | 6.8% | 3.7% | 3.1% |

| Lint mode | testbed (2.38s) | snowflake (3.83s) | postgres (3.47s) |
|---|---|---|---|
| Parse (lex, parse, build Python tree) | 37.7% | 46.0% | 50.7% |
| Rule crawls | 44.8% | 36.6% | 33.9% |
| Other (noqa, loop overhead, result assembly) | 15.7% | 17.1% | 15.1% |

The crawls are a flat tail: no rule is above about 6% of lint. "Other" has
not been profiled.

## Roadmap: Python out of the lint/fix cycle

Goal: no Python in lint/fix except legacy plugin rules.

1. Rules in Rust: reflow first (done here, templated files included), then
   the remaining rule bodies.
2. Rule framework in Rust: crawl, noqa, ignore masks, result assembly.
3. Fix loop on the arena, with Rust validation: done for untemplated files
   (section 6) and for templated files (last section). Left: moved subtrees,
   unparsable sections.
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

> Superseded: the port this section scopes is done. See "Templated reflow:
> ported" at the end. The measurements here are from before it.

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

## Templated reflow: ported

LT02, LT05 and LT14 now run in Rust on every templated file (LT01 already
did). Done in three slices, each checked for identical output before the
next:

1. `a56456b5b` — files whose only templating is `{{ }}` expressions,
   `{# #}` comments and non-block tags such as `{% set %}`.
2. `3616f6cb6` — block tags and loops.
3. `964b459e1` — tags which consume whitespace (`{%-`, `-%}`).

### What it took

- **Templated newlines.** A newline a tag rendered is not a line break, not
  an indent, and not a place to insert one.
- **Deferred indent points.** An empty placeholder, or an expression
  mid-line, can't take an indent.
- **Multi-line tags.** A line starting with a tag which spans lines isn't
  linted.
- **Long lines (LT05).** Templated code is measured by its source length;
  no break goes between two segments a tag rendered, or around a comment
  with a tag in its source.
- **Keyword breaks (LT14).** A location whose neighbouring newline came from
  a tag is skipped.
- **Block tags.** Ports of `_rebalance_unrendered_block_indents`,
  `_revise_skipped_source_lines`, `_revise_templated_lines` and
  `_revise_loop_repeated_lines`, and the block check in
  `_lint_line_untaken_negative_indents`.
- **Consumed whitespace.** Whitespace a tag swallowed counts as a line break
  and as the line's indent. Changing it needs a new fix kind,
  `edit_source`: the Rust result carries a source fix and Python builds the
  edited placeholder from it.
- **Rebuilt arenas carry block uuids,** so this also holds after a fix pass.

### A bug in the parser's arena

`match_result.rs::token_to_node` stored a template placeholder's raw (always
empty) as its source string, so every tag looked empty and consumed
whitespace was never recognised. This is in main's arena build, not only the
spike, and affects anything that reads `source_str` from the arena under
`use_rust_rules`. Fixed in `3616f6cb6`.

### Results

Rust vs Python: lint results, fixes and fixed text.

| Corpus | Identical | LT02 / LT05 / LT14 answered in Rust |
|---|---|---|
| Templated rule fixture cases | 92 / 92 | 108 / 106 / 108 of 108 runs |
| Repo jinja files | 133 / 133 | 202 of 202 each |
| jaffle shop | 7 / 7 | 10 of 10 each |
| testbed templated files | 12 / 12 | 12 of 12 each |
| Untemplated regression (629 fixtures, 1,765 files) | all identical | |

The two LT05 runs left in Python use `ignore_comment_lines` /
`ignore_comment_clauses`, which were already excluded.

Parse + lint on templated files, min of 3, Rust reflow vs Python reflow:

| Corpus | Python | Rust | After slice 3 | After slice 2 | After slice 1 |
|---|---|---|---|---|---|
| Repo jinja files (136) | 1,898ms | 1,471ms | 1.29× | 1.19× | 1.02× |
| jaffle shop (7) | 160ms | 122ms | 1.31× | 1.00× | 1.13× |
| testbed templated (12) | 122ms | 103ms | 1.19× | 1.10× | not measured |

This is at or above the 1.20–1.22× ceiling estimated in the scope section.

### Not covered

- **Templated fix mode.** Not at this point; ported in the next section.
- **Trees with source fixes.** Once a tree carries source fixes from an
  earlier pass, the three rules detect in Python for that pass: the arena
  doesn't hold source fixes.
- **The dbt templater.** Everything here used the jinja templater.
- **A real dbt project of any size.** jaffle shop is 7 models and the repo
  files are test snippets.

## Templated fix mode: ported

The Rust apply (section 6) now takes templated files and trees which already
carry source fixes. Commit `033b4ced1`.

### What it took

- **Placeholder source edits.** A fix which edits a tag's source (the
  reflow's `edit_source`, and rules such as JJ01) is applied as the same
  placeholder carrying a source fix, with no validation, as Python does.
- **Templated positioning.** An expression which rendered nothing is not
  used as the end point when a new segment is positioned.
- **Rebuild scope.** Python re-positions every segment it walks through
  while fixes are left to apply, not only those above a fix. That shows only
  where positions aren't already what a rebuild gives, which is templated
  files. Rust mirrors it there and keeps the cheaper path elsewhere.
- **Condensed fixes.** Several source edits to one anchor are merged into
  one fix (`compute_anchor_edit_info`) before applying; the Rust path was
  given the unmerged list.
- **Loop check.** The linter compares the Rust result on text plus source
  fixes: a source-only fix doesn't change the rendered text.

### Results

Fix mode, in process, min of 3:

| | Python rules | Rust reflow, Python apply | Rust reflow, Rust apply |
|---|---|---|---|
| Templated repo files (127) | 4.32s | 4.02s (1.07×) | 2.59s (1.67×) |
| testbed (191 files, 12 templated) | 6.60s | 5.92s (1.11×) | 3.50s (1.89×) |

On the testbed nothing goes to Python's `apply_fixes` any more. On the repo
files 72 passes are handed back, leaving 0.34s of Python apply.

Parity, on the final build:

- Each of 4,836 Rust apply passes (2,944 rule fixture cases, templated repo
  files, testbed, and the eight dialect corpora) matches Python's
  `apply_fixes` on the same tree, node by node: class, raw, source slice,
  templated slice, working position, and for leaves the placeholder source,
  source fixes and block uuid.
- The final fixed output is identical with the Rust apply on and off for
  every fixture case and file.
- Lint parity for the four reflow rules is unchanged: identical on the
  templated corpora and on the untemplated regression.

Handed back to Python across that run (464 passes): edits which move an
existing subtree 396; fixes inside unparsable sections 51 (all from repo
files forced to the ansi dialect); anchor not in the tree 10; validation
failed, which Python rejects too, 7.

jaffle shop has nothing to fix under the default rules, so it contributed no
apply passes.
