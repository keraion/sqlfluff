# PR stack plan — `keraion/rust-cli-entry-point` vs `main`

108 commits (two mid-branch merges from main), reviewed 2026-07-08 and cut
into 12 stackable branches. Each branch tip is a state that was fully
validated when committed (suites green in both tox envs; the relevant parity
sweeps clean — each tip's commit message records its evidence). Branches are
pushed to the `keraion` remote; **no PRs are opened yet**. Each PR should
target the branch above it in the table (PR 01 targets `main`).

## The story

The arc is: *drive the pipeline from Rust, then earn back native behaviour
piece by piece, with byte-parity as the gate at every step.*

1. **Engine orchestration** — Rust drives discover→render→lex→parse for
   `parse`/`render` (machine formats), reverse-dispatching templating to
   Python. `use_rust_engine` config (auto/True/False) gates everything that
   follows.
2. **The arena façade** — rules crawl Rust-owned trees through duck-typed
   `RsSegment` wrappers instead of materialised Python segments. Fixing works
   by harvesting LintFixes and reconstructing source via native patch
   machinery. Rules are promoted to `FACADE_SAFE_RULES` only with
   corpus-proven byte parity (51 by the end of this element).
3. **Arena mutation (v3)** — the fix loop stops re-parsing: staged edit
   batches splice the arena in place with native loop-protection semantics.
4. **Full parity + lint wiring** — grammar re-validation, every fixable rule
   promoted, the detection-unsafe quarantine emptied, and `sqlfluff lint`
   itself wired through the façade.
5. **Robustness + infrastructure** — empty files, large-file skips, meta
   fidelity, the raw-templater corpus baseline (errors=0), and the
   `py*-rust-engine` tox envs that run the whole suite with the engine FORCED.
6. **Templated sources** — jinja renders become façade-eligible (working-loc
   marker equality, 2-arg FFI slices, render metadata for routing).
7. **noqa, plugins, dbt** — native IgnoreMask over the façade; unknown rules
   split-crawled on lint; `rust_compatible` opt-in for plugins; v1 fix
   machinery retired; dbt vetted 0-divergence.
8. **Fix-mode CLI flags** — warnings config, `--check`, `--show-lint-violations`.
9. **Multi-variant renders** — every render variant parsed and fixed;
   `render_variant_limit` honoured.
10. **Python API** — `Linter.lint_string` fast-paths (simple API inherits);
    found and fixed the native phase-loop leak (AL09/CP02/RF06).
11. **TemplatedFile cache fix** — the upstream content-key collision replaced
    by object-identity keying with weakref eviction.
12. **Parallelism + timing + perf** — `-p N` pools over shared per-file
    units, `--bench`/`--persist-timing`, per-directory config cache,
    native-inline for declined files, the classification drift guard, and
    the standalone CLI brought back to parity.

## The stack

| # | branch (tip) | commits | boundary diff | review focus |
|---|---|---|---|---|
| 01 | `rs-stack-01-engine-orchestration` | 2 | 98f +2522/−3333 | `sqlfluffrs_engine` crate split; `engine_entry.rs`; `parse`/`render` gates; the big deletion is crate code MOVING |
| 02 | `rs-stack-02-arena-facade-fix` | 41 | 25f +2039 | `rs_lint.py` façade wrappers + accessors; source-patch fix loop; stdin/paths fix gates; is_type conversions in rules |
| 03 | `rs-stack-03-arena-mutation-v3` | 8 | 46f +4623 | arena splice/position/validate (Rust); `facade_fix_loop` v3; **tip is a main-merge — diffstat inflated, review commits 44–50** |
| 04 | `rs-stack-04-full-parity-lint` | 24 | 54f +3961 | grammar re-validation; promotions to ALL; lint gate; **tip is a main-merge — same caveat** |
| 05 | `rs-stack-05-robustness-infra` | 11 | 21f +608 | edge-case routing; corpus baseline; forced-engine tox envs |
| 06 | `rs-stack-06-templated-sources` | 5 | 12f +657 | marker equality semantics; FFI slice fix; render metadata; jinja gates |
| 07 | `rs-stack-07-noqa-plugins-dbt` | 6 | 14f +861/−306 | IgnoreMask-from-façade; split crawl; `rust_compatible` contract; v1 deletion |
| 08 | `rs-stack-08-fix-cli-flags` | 1 | 4f +266 | violation post-processing on fix; pending writes; record merging |
| 09 | `rs-stack-09-multi-variant` | 1 | 12f +383 | alternate trees; per-variant masks/patches; the variant-limit cap |
| 10 | `rs-stack-10-python-api` | 1 | 9f +544 | `lint_string` hook + formatter guard; **the phase-loop leak fix** (read this one carefully — it changes fix convergence) |
| 11 | `rs-stack-11-templatedfile-cache` | 1 | 4f +205 | identity keying + weakref eviction; lock-scope change |
| 12 | `rs-stack-12-parallel-bench-perf` | 7 | 10f +1348 | transport classes; pool mechanics; config cache; drift guard |

## Review guidance

- **02 and 04 are the heavyweight reviews.** They are single arcs built
  incrementally and every intermediate commit was green, so they can be
  reviewed commit-by-commit. If a reviewer wants smaller units, natural
  sub-cuts exist at `7719bb5b5` (stdin→paths fix boundary in 02) and
  `91399a713` (lint wiring in 04) — say the word and those branches can be
  added to the stack.
- **The two main-merges sit at the tops of 03 and 04** so their upstream
  noise is localised; GitHub will render them as merge commits and the
  substantive review content precedes them. When PRs are actually opened,
  merging main into each base branch first will make the three-dot diffs
  clean.
- **Parity evidence** lives in each tip's commit message and cumulatively in
  `utils/PARITY_NOTES.md` (which lands progressively across the stack —
  final state in 12). Final numbers: literal corpus 2169/0/0 lint+fix,
  templated 269/0/0 both, dbt 29/0, testbed byte-identical, suites
  16868/16867.
- **11 is independent** of 08–10 in content (it only needs the façade to
  exist) and could be reordered earlier if a quick standalone bugfix PR is
  useful — it fixes an upstream bug that predates this branch.

## Extractions — independent of the stack (2026-07-08)

Seven commits cherry-pick onto `main` cleanly AND pass the full suite
there (`keraion/rs-picks-verify` = main + all seven, wheel rebuilt,
12,634 passed). Each is its own branch, PR-able immediately in any
order, no stacking:

| branch | commit (on branch) | what |
|---|---|---|
| `rs-pick-core-copy-reparent` | c1af221d7 | `BaseSegment.copy` re-parents provided children |
| `rs-pick-cv07-deterministic` | d1bf647fe | CV07 deterministic fix order |
| `rs-pick-cv11-parse-shaped` | 626430c23 | CV11 parse-shaped replacements |
| `rs-pick-st05-lazy-clonemap` | 856d0a1c9 | ST05 lazy SegmentCloneMap (native perf) |
| `rs-pick-regex-ignore-case` | e769c8c7e | RegexParser ignore_case — completes 2155/2155 parser parity on main |
| `rs-pick-bracketed-metas` | 4e912a64d | Bracketed direct-child Indent/Dedent metas |
| `rs-pick-templatedfile-cache` | 457931e06 | identity-keyed conversion cache + weakref eviction |

Notes from the extraction:
- The cache pick needed conflict resolution (python.rs registration
  context; PARITY_NOTES dropped — it doesn't exist on main) and a test
  adaptation: the span-marker literalness probe doesn't discriminate on
  main (whose marker semantics predate this branch's marker fixes), so
  the extracted test asserts on CACHE ENTRY COUNT instead — a more
  direct probe of the collision.
- `48c73d248` (token-typing fidelity) cherry-picks CLEANLY but does not
  COMPILE on main — it uses `MatchedClass.class_type`, added by
  stack-02's arena class-type commit. Textually-clean != independent;
  it stays stacked. (`27a6ace8b`, `0cb6c9f41` conflict outright and
  stay stacked too.)
- These commits remain in the stack elements. As each extraction merges
  to main, they vanish from the stack diffs automatically; do NOT also
  rewrite the stack branches.
