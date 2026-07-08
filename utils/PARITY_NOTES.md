# Parity harness notes (rust engine / façade)

Context for the corpus parity harnesses in this directory
(`facade_fix_parity.py`, `facade_lint_parity.py`). Current status as of
2026-07-07 (branch `keraion/rust-cli-entry-point`, after commit
`0cb6c9f41`): whole-corpus **fix parity 0/2168**, **lint parity 0/2166**,
raw-token sweep (type + class_types + raw, pure-python vs rust)
**0/2170** — with exactly ONE recurring "error" (not a divergence),
documented below (since fixed: the harnesses now use the raw
templater, so `errors=0` is the expected baseline).

## Known artifact: `test/fixtures/dialects/postgres/array.sql`

**Symptom** (shows up as `err=1` / `--- ERROR ... [ERROR]` in every
whole-corpus harness run; NOT a façade or rust-parser divergence):

- fix harness: native baseline raises
  `AssertionError('Fixing a string requires successful templating.')`
  (`linted_file.py:213 fix_string`).
- token sweep / anything reading `parsed_variants[0]`:
  `IndexError('list index out of range')`.

**Root cause**: line 48 of the fixture contains a Postgres array-literal
string

```sql
SELECT '[1:1][-2:-1][3:5]={{{1,2,3},{4,5,6}}}'::int[] AS f1 ...
```

The `{{{ ... }}}` inside the SQL string is interpreted by the default
**jinja** templater as a Jinja expression and fails to render → a `TMP`
violation ("Failed to parse Jinja syntax", L49), `templated_file=None`,
no parsed variants. This happens identically for pure-python,
rust-parser and rust-engine configs — the file never reaches
parse/lint/fix in ANY engine, so it cannot diverge. The dialect parse
test suite is unaffected because dialect fixture tests run the **raw**
templater.

**Repro**:

```python
from sqlfluff.core import FluffConfig, Linter
src = open("test/fixtures/dialects/postgres/array.sql").read()
c = FluffConfig(overrides={"dialect": "postgres"})
res = Linter(config=c).lint_string(src, fix=True)
# res.templated_file is None; violations == [TMP L49]
```

**Fix applied (2026-07-07)**: option 1 — `templater = raw` is now
configured in `facade_fix_parity.py` and `facade_lint_parity.py`
(matches what the dialect test suite does; makes the corpus 100% clean,
so `errors=0` is the expected baseline). Any future ad-hoc token-sweep
script over this corpus should do the same. Known downside: hides
genuine templater-path behavior for every OTHER file (today none of
them template, so this is currently free).

Options considered and not taken: per-file skip/allow-list; leaving
`errors=1` as the expected whole-corpus baseline.

Note the CLI fast paths are already correct here: templated sources
(`source_str != templated_str`) and templater failures route to native
by the eligibility gates in `commands.py`, so production behavior is
identical regardless.

## Empty files are not façade-eligible (found by the raw-templater switch)

Switching the harnesses to `templater = raw` exposed a real façade gap on
`test/fixtures/dialects/ansi/empty_file.sql` (0 bytes): a raw-templated
zero-byte render keeps one zero-length literal raw slice, which native's
lexer turns into a zero-width `placeholder` meta — and **LT12 lints it and
fixes the file to a single newline** (`'' -> '\n'`). (Under jinja the
zero-byte render has no raw slices, no placeholder is lexed, and empty
files lint clean — which is why this never showed before.) The arena's
bare `file` node carries no pos_marker, so the façade can't reproduce
that. Resolution: empty sources are routed to native everywhere — the CLI
gates (`_facade_lint_file`, `_try_facade_stdin_fix`,
`_try_facade_paths_fix`), `facade_violations` (returns `None`), and the
fix harness (`facade_fix` returns `None`). `facade_fix_loop` keeps its
empty-source short-circuit purely as a crash guard for direct callers.

Chasing the same case also exposed a Rust parser bug (affecting the
standalone `sqlfluff-rs` CLI and `engine_parse_to_tree` alike): the
single-token retag path in `MatchResult::apply` popped the sole child
node *before* checking it was a `Node::Raw`, silently dropping a lone
Meta — so a zero-byte **jinja** render (whose only token is
`end_of_file`) parsed to a bare `file` node with no children, where
native keeps `file > end_of_file`. Fixed (guard before pop, mirroring
the raw-class collapse block below it) + regression test in
`test/core/parser/rust_parser_test.py`.

## Open upstream bug: `PY_TEMPLATED_FILE_CACHE` key ignores slicing

`PySqlFluffTemplatedFile::extract`
(`sqlfluffrs/sqlfluffrs_python/src/templater/templatefile.rs`, present on
upstream main since #7386) memoizes Python→Rust `TemplatedFile`
conversions keyed by `fname:source_str:templated_str` — the key does
**not** include `sliced_file`/`raw_sliced`. Two TemplatedFiles with the
same rendered text but different slicing (e.g. jinja vs raw templater
over the same source — a zero-byte file is the minimal case) collide, and
the second lex silently reuses the first's slices. Observable effects in
one process (found 2026-07-07 while chasing the empty-file divergence):

- `Linter(templater=raw).lint_string("")` returns different trees
  depending on whether a jinja config linted `""` first (the placeholder
  meta appears or not — LT12 flips).
- The mismatched tree/TemplatedFile pair can crash `fix_string` with
  `IndexError` in `generate_source_patches` (`raw_sliced[-1]` on `[]`).
- The cache is also never evicted (unbounded growth keyed by full source
  text).

Any parity probe comparing templaters in one process is affected. Not
fixed here (rust-side, upstream-shared); the key needs the slicing (or a
content hash), or the cache needs to go.

## Templated (jinja) sources on the façade fast paths (2026-07-07)

Templated sources are now façade-eligible for lint AND fix when the render
is single-variant and violation-free (`RsTree.num_variants` /
`.templater_violations`, populated by `engine_parse_to_tree`); multi-variant
or violation-bearing renders route to native (native lints EVERY variant and
reports TMP violations). Vetting harnesses:
`utils/facade_templated_lint_parity.py` and
`utils/facade_templated_fix_parity.py` — both sweep
`test/fixtures/templater/**` (per-fixture configs honoured) plus the
sqlfluff-testbed models through the PRODUCTION gate functions. Baseline:
0 divergences / 0 errors on both (218 eligible of 277).

Getting there fixed four real bugs, all found via fix-output byte diffs:

1. **Arena position pass `marker_eq`** compared full markers where native
   `PositionMarker.__eq__` (markers.py:67) compares ONLY the working
   location. At a jinja-loop boundary (source positions non-monotonic in
   templated order) the widen-vs-point decision then widened via
   `from_points` into an INVERTED source slice, silently suppressing that
   segment's source patch.
2. **FFI slices carried `step=1`** (`PySlice` → `slice(a, b, 1)`), and
   `slice(a, b, 1) != slice(a, b, None)` in Python — breaking equality
   against native-built slices (live victim: FixPatch dedupe over repeated
   loop regions applied a patch twice).
3. **`facade_fix_loop_v3` skipped `merge_source_patches`** — native routes
   even single-variant patches through it (dedupe + drop same-position
   conflicting insertions, e.g. two different indents at a block-tag
   boundary).
4. **`RsSegment.edit(source_fixes=...)` on a placeholder** (JJ01's fix
   shape) fell into the raw-edit branch: the staged replacement lost
   `block_type` ('skipped_source') and its summary `source_str`, misleading
   LT02's jinja-block alignment on the next crawl. Now mirrors
   `TemplateSegment.edit` (keep source_str/block_type/block_uuid, merge
   source fixes).

Detection-side: `facade_violations` now applies native's
`Linter.remove_templated_errors` (violations anchored in non-literal
regions are dropped unless semantically literal or the rule targets
templated code), and `facade_fix_loop_v3` applies it to the harvested
`lint_sink` like native's `initial_linting_errors` filter.

## noqa (ignore masks) on the façade fast paths (2026-07-07)

Sources containing ``noqa`` no longer route to native: the gates build
native's ``IgnoreMask`` from the façade tree
(``rs_lint.facade_ignore_mask``, mirroring linter.py:490-499 —
``disable_noqa``/``disable_noqa_except`` handling included) and pass it
into the rule crawls, where ``_process_lint_result`` drops masked results
and their fixes exactly like native. Malformed directives surface as
``SQLParseError`` violations (native's ``initial_linting_errors``
additions), and the mask rides on the lint path's ``LintedFile`` so
unused-noqa warnings (``--warn-unused-ignores``) work natively. The
façade wrappers duck-type everything ``IgnoreMask.from_tree`` reads —
directive extraction is byte-identical to native.

All four parity sweeps re-vetted with noqa files eligible: literal fix
2169/0, literal lint 2169/0, templated lint+fix 229 eligible / 0 each.
The sqlfluff-testbed generator now sprinkles inline masks, unused
directives and disable/enable ranges through the models so mask parity
stays exercised end-to-end in every testbed run.

## Plugin rules: explicit ``rust_compatible`` opt-in (2026-07-07)

Plugin rules run on the classic Python pipeline unless their author
declares ``rust_compatible = True`` on the rule class (see
``BaseRule.rust_compatible`` and the custom-rules guide). Effective
façade eligibility is ``rs_lint.rule_is_facade_safe``: the
centrally-vetted core allowlist (FACADE_SAFE_RULES) OR the flag — and it
applies to BOTH the lint and fix fast paths (an opted-in rule's fixes
flow through the arena like core rules'). Undeclared rules no longer
disqualify a lint run: the gate splits the crawl — safe rules on the
façade, undeclared rules crawled on a native reference parse
(``facade_unknown_rule_violations``, correct by construction, one extra
python parse per file, INFO-logged) — and merges. Undeclared rules DO
still route fix runs to native.

Why an explicit flag rather than attempt-and-fall-back: the façade's
synthetic segment classes subclass only RawSegment/BaseSegment, so a
plugin rule using ``isinstance(seg, KeywordSegment)`` finds NOTHING on
the façade *without crashing* — silent divergence, nothing catchable to
fall back on. (An earlier same-day design auto-promoted rules whose
shadow façade crawl byte-matched native; replaced by the flag — explicit
author contract over runtime guessing, and no double-crawl overhead.)
The example plugin declares the flag as a reference.

## dbt templater vetted on the façade fast paths (2026-07-07)

The gates have been templater-agnostic since the jinja milestone; this
confirms it for dbt with real compilation: ``utils/facade_dbt_parity.py``
sweeps the dbt plugin's fixture project (dbt 1.10.20, postgres adapter)
through mirrored gate logic — lint (positions + descriptions) AND fix
(byte compare) vs native. Baseline: **31 models, 28 façade-eligible, 0
divergences, 0 errors**; 3 route to native consistently (a disabled
model and a missing-CLI-var model dbt refuses — ``SQLFluffSkipFile``
propagates out of ``engine_parse_to_tree``, the gates' exception guard
routes, and native absorbs the skip gracefully; plus ``vars_from_env``).

Run requirements (see the harness docstring): a dbt-capable interpreter
(``.tox/dbt1100/bin/python`` with the working-tree sqlfluff + fresh
sqlfluffrs wheel installed), postgres reachable per the fixture
profiles, ``dbt deps`` run in the fixture project, and
``passed_through_env``/``DBT_USE_EXPERIMENTAL_PARSER`` env vars (the
harness sets them). NOTE: the dbt env's click version can't import
``sqlfluff.cli.commands``, so the harness mirrors the gate logic inline
instead of calling the production functions.

## CLI-flag carve-outs closed on the fix fast path (2026-07-07)

- ``[sqlfluff:warnings]`` (and the ``ignore`` config) now apply on both
  fix gates: native's ``ignore_if_in``/``warning_if_in`` post-processing
  runs over the harvested violations; warned/ignored drop out of every
  count while their fixes still apply, and the per-file display mirrors
  ``dispatch_file_violations`` (ignored dropped, warned shown,
  unused-noqa warnings appended under ``warn_unused_ignores``). Files
  with non-``SQLLintError`` violations (e.g. malformed-noqa parse
  errors) route to native, whose parse-error machinery owns them.
- ``fix --check`` engages the fast path: fixes are computed but held as
  pending writes; the confirmation prompt in ``_paths_fix`` writes them
  (with the FIXED dispatch) on confirm and discards on abort.
- ``--show-lint-violations``: façade-handled files contribute records,
  merged with native's and rendered in path order (native's
  ``as_records`` sort). Previously such files routed wholesale.
- ``--bench``/``--persist-timing`` still route to native BY DESIGN: they
  exist to produce native's per-file timing records.

Two integration traps caught by the CLI test suite: an empty
``remaining`` list must only produce an empty ``LintingResult`` when the
FAÇADE emptied it (a no-path invocation relies on ``lint_paths(())``
defaulting to CWD), and the unfixable-violations section must merge and
SORT façade+native records (files split across engines otherwise break
the alphabetical ordering).

## Multi-variant renders on the façade fast paths (2026-07-07)

The last templater routing carve-out. The engine parses EVERY render
variant into its own arena tree (``RsTree.alternate_trees``, each with
its own TemplatedFile; unparsable variants skipped like native's
``not alternate_variant.tree``). ``facade_violations`` crawls each
alternate with its own noqa mask (untaken branches can carry their own
directives) and merges through the shared templated filter +
source-space dedupe; ``facade_fix_loop`` runs the full mutation loop per
variant and merges per-variant patch sets via ``merge_source_patches``
(natively that IS the multi-variant mechanism). A runaway on any variant
defers the whole file. All gates dropped ``num_variants > 1`` routing.

Bug found by the sweep (via jinja_lint_unreached_code's 6-variant
chain-scoring fixture): native's ``render_string`` stops the lazy
variant generator at ``render_variant_limit`` (default 5) — the engine's
``render_via_python`` collected all of them, so the façade linted a
variant native never sees. The cap is mirrored now (a cost fix for
``parse``/``render`` too: the engine no longer over-renders).

Baselines after: templated lint AND fix 269 eligible of 282 (was 229) /
0 divergences / 0 errors; literal sweeps and full suites unchanged.

## Python API on the façade fast path (2026-07-08)

``Linter.lint_string`` (the funnel for ``sqlfluff.lint``/``sqlfluff.fix``
and ``lint_string_wrapped``) now tries the façade first when
``use_rust_engine`` permits AND the linter carries NO formatter (a
formatter implies CLI-style per-file dispatch that only the native path
performs; the CLI has its own gates). The shared per-file core moved to
``rs_lint.facade_linted_file`` (the CLI's ``_facade_lint_file`` is now a
thin wrapper); fix mode builds a ``LintedFile`` whose ``source_patches``
carry the loop's merged patches so ``fix_string()`` reconstructs
byte-identically. NOTE: a fast-pathed ``LintedFile.tree`` is the façade
root (duck-typed), not a native ``BaseSegment`` — documented on
``lint_string``. Custom templater INSTANCES on the linter route to
native (the engine renders via ``config.get_templater()``, which ignores
them — caught by the mocked-templater dedup test). ``RsSegment`` gained
``stringify()`` (materialise + delegate; the rule-testing utilities call
it).

**Major latent bug found** (via the AL09/CP02/RF06 combo test, config no
corpus run used): the façade fix loop's phase model scoped the all-rules
crawl to the first pass only. Native reassigns
``rules_this_phase = rule_pack.rules`` on the first pass and the
assignment LEAKS into every later MAIN-phase loop (linter.py:532-536 —
its own comment says post rules skip intervening loops; the code wins).
Cascades where a post-phase rule's main-loop application unlocks a
main-phase rule (CP02 lowercases ``C as C`` -> AL09 strips the redundant
alias) converged differently — latent in the CLI fast path all along.
Also mirrored: non-fix-compatible rules skip after the first pass.

Native-reference discipline reminder, now sharper: ``lint_string``
itself fast-paths — ANY probe using it as the native side must pin
``use_rust_engine = False`` (several façade tests were re-pinned).

## PY_TEMPLATED_FILE_CACHE collision + growth: FIXED (2026-07-08)

The upstream (#7386-era) conversion cache in
``sqlfluffrs_python/src/templater/templatefile.rs`` was keyed by
``fname:source_str:templated_str`` — unsound, since two TemplatedFiles
can share all three strings with different SLICINGS (jinja vs raw over
the same rendered text; minimal case an empty file), making the second
conversion silently reuse the first's slices (LT12 flips, and the
mismatched pair could crash ``fix_string`` in
``generate_source_patches``). It also never evicted.

Now keyed by PYTHON OBJECT IDENTITY (address) with a ``weakref.ref``
whose callback evicts the entry on GC — the ref itself is stored in the
entry so the callback stays alive, entries die with their objects
(bounded by live TemplatedFiles), and a recycled address can never hit
a stale entry (eviction runs during dealloc, before reuse). Identity
keying preserves the cache's real contract: one Python object → one
``Arc<TemplatedFile>`` (marker combination compares Arcs by pointer
first, value fallback second — sqlfluffrs_python/src/marker.rs:147).
Two extras: the lock is NO LONGER held across the getattr extraction
(GC-triggered eviction callbacks take the same lock — deadlock
otherwise), and actual ``RsTemplatedFile`` objects short-circuit via
downcast. Hit path got cheaper (pointer read vs three string extracts +
format!).

Cross-templater single-process probes are now reliable; the pitfall
below (same-source dual-templater comparisons) is resolved, though
native-side pinning discipline still applies for OTHER reasons.
Regression tests: test/core/rules/rs_templatedfile_cache_test.py
(collision via span-marker literalness — POINT markers can't
discriminate, both slicings report literal; eviction with a
gc.collect() BEFORE baselining, or prior tests' pending garbage drags
the count below base under xdist; the original jinja-then-raw
empty-file symptom).

## -p N on the façade fast paths (2026-07-08)

The lint and fix gates now parallelize: per-file work runs through ONE
shared unit (``facade_lint_file_unit`` / ``facade_fix_file_unit`` in
rs_lint) consumed identically by an inline generator (``-p 1``) or a
spawn-context pool with ordered ``imap`` (``-p N``, worker-count math
matching native ``get_runner``: <=0 means cpu_count+n). Serial and
parallel therefore cannot drift.

Transport: façade objects don't pickle (pyo3 handles/markers/synthetic
segment classes), so workers precompute every consumer-visible answer
WITH THE REAL OBJECTS' METHODS and return ``TransportedLintError`` /
``TransportedParseError`` (SQLLintError/SQLParseError SUBCLASSES —
isinstance-faithful — answering to_dict/rule_code/fixable/check_tuple
from stored primitives; ``fixes`` deliberately empty, ``fixable`` is a
stored bool) plus ``_TransportedTreeStats``/``_TransportedTemplatedFileStats``
shims (the only things LintedDir.add reads). IgnoreMask is plain data
and crosses as-is (parent generates unused-noqa warnings from it). The
serial path uses the SAME transports, so -p 1 exercises them too. A
worker failure or pool breakage routes the file(s) to native.

Scaling: testbed (198 files) lint 10.05s -> 3.69s with -p 4 (2.7x,
spawn overhead included).

Findings for future probes: NATIVE's own -p N per-file output order is
completion-order — nondeterministic run to run — while the façade's
ordered imap is deterministic and byte-matches -p 1. Byte-comparing
against native multiprocess stdout is therefore never stable; compare
per-file blocks as sets (and the trailing dialect-WARNING /
"All Finished!" lines attach to whichever block prints last). JSON
record comparisons must drop ``timings`` (wall-clock floats).

## --bench / --persist-timing on the fast paths (2026-07-08)

The last flag carve-outs, now lifted. LINT needed only the gate lift:
façade LintedFiles already carry real FileTimings and merge into
``result``, so ``timing_summary()``/``persist_timing_records()`` cover
both engines. FIX: ``facade_fix_loop`` gained ``rule_timing_sink``
(a timing per crawl, EVERY loop pass, root variant only — mirroring
native linter.py:659-662 and its discarded alternate timings); the fix
unit times the engine parse + whole loop and ships a violation-free
"timing carrier" LintedFile per handled file; ``_paths_fix`` aggregates
carriers with native's result in a SEPARATE LintingResult used only by
the timing consumers (adding them to ``result`` would leak empty
records into --show-lint-violations and the stats).

Timing semantics on the engine: template+lex+parse is one Rust call, so
it all lands under "parsing" (templating/lexing report 0.0 — that IS
the engine's truth, and the CSV signature the tests pin).

Traps hit: a str.replace edited BOTH commands' byte-identical bench
blocks (lint's got fix-local variables — NameError); and _paths_fix's
façade-handled-everything early exit skipped the bench/persist section
that was unreachable while the gate existed (condition now includes
``not bench and not persist_timing``).

Nothing routes for CLI flags anymore.

## Benchmark snapshot (2026-07-08, post -p N + flags + API)

Min-of-3 subprocess CLI wall-clock, 20-core box, release wheel.

Testbed (jinja-heavy, 191 small files + 7 large-file skips):
  parse 3.58x | lint -p1 1.32x | lint -p8 ~0.9x | fix -p1 ~1.0x | fix -p8 ~0.9x
Literal corpus (100 x 8KB, nothing routes):
  parse 3.84x | lint -p1 1.50x | lint -p8 2.31x | fix -p1 1.82x | fix -p8 1.64x
Python API (Linter.lint_string, in-process, per-call):
  lint 2.3-2.5x | fix 1.8-2.2x (holds from 0.7KB to 23KB)

Reading the shape:
- parse-type work (the engine's core) is a steady ~3.6-3.8x end-to-end.
- lint/fix wins scale with file size and literal-ness: per-rule PYTHON
  crawl overhead (shared by both engines) dominates small files —
  --persist-timing showed testbed per-file work at 4.81s engine vs
  6.74s native (1.40x) with wall diluted to 1.32x by startup/config.
- lint -p8 LITERAL 2.31x > -p1 1.50x: the façade pool parallelizes the
  WHOLE pipeline per worker while native renders in its main process.
- testbed -p8 slightly UNDER native: fixed-cost regime (spawn + import
  ~= the total useful work for 191 tiny files); ruled OUT the
  second-pool theory by deleting the 7 skip files (still ~0.9x).
- Profiling found + fixed a real waste: facade_lint_file_unit built the
  rulepack TWICE per file (~10% of the small-file unit) — dedup took
  testbed lint -p1 from 1.19x to 1.32x.

Known follow-up opportunities (not taken): per-directory child-config
caching (dialect_selector is ~5ms/file in FluffConfig construction —
but inline `-- sqlfluff:` directives mutate the child, so a cache needs
copy-on-use); running routed files natively INSIDE the façade pool
workers (needs SQLFluffSkipFile accounting to ride the transport).

## Perf follow-ups: config cache + native-inline lint (2026-07-08)

Two changes from the benchmark findings:

1. ``load_raw_file_and_config_cached``: same-directory files share the
   child config AND rulepack — but ONLY directive-free files
   (``-- sqlfluff`` lines mutate the child; directive-free processing is
   a no-op, so the shared child never mutates; directive-carrying files
   take the private native loader and never seed the cache).
   ``FluffConfig.copy()`` measured 5x MORE expensive than a fresh
   ``make_child_from_path`` (deepcopy of ``_configs`` deep-copies the
   dialect object!) — copy-on-use was scrapped for the no-directive
   share.

   Rulepack sharing means RULE INSTANCES are reused across same-config
   files (native rebuilds per file). ``context.memory`` is crawl-scoped
   (fresh ``RuleContext`` per crawl, base.py:626-631) so it can't leak;
   a full audit of core-rule ``self`` writes found only config/dialect-
   derived attrs (CP01 family, CV09, RF02-05 — idempotent under
   same-config sharing), per-evaluation helper classes (ST05's
   ``_CTEBuilder``/clone map), and eval-start resets (ST06). NOTE the
   parity harnesses do NOT exercise sharing (they call the façade
   functions directly with per-file packs) — the CLI-path tests and the
   testbed gate (real CLI, 191 files byte-identical) are what cover it.
   The ``rust_compatible`` contract documents the reuse constraint for
   plugin rules.

2. Lint files the façade declines now run the NATIVE pipeline inside
   the unit ("native" status; parse_string + lint_parsed with
   formatter=None; native LintedFiles pickle — that's how native's own
   runners transport them). No second native pool for leftovers, and
   per-file output stays in discovery order. SPAWN TRAP: config
   pickling deliberately nulls ``templater_obj`` and ``Linter.__init__``
   reads it — the pool initializer must rehydrate
   (``config.get_templater()``, sanctioned by ``__getstate__``'s
   comment) or every native-inline render dies with
   ``'NoneType' has no attribute 'name'``. Skips transport as a
   ("skipped", msg) status; the caller adds to
   ``result.files_skipped``. The fix command instead clamps the native
   remainder's worker count to the file count.

Parity findings en route: the OLD façade lint gate MISSED native's
unset-dialect warning for files whose only parse errors were malformed
noqa directives (linter.py:866-874) — now dispatched uniformly (paths +
stdin). And ``large_file_skip_fail`` reads from the ROOT
(cwd-discovered) config, not the target directory's — verified
engine-off before asserting; tests must chdir into the project.

Testbed lint after: -p1 1.44x (was 1.19x at yesterday's numbers), -p8
at parity with native (2.50s vs 2.43s; was 0.84x).

## Pitfall: pinning the native side of ANY parity probe

`use_rust_parser` defaults to AUTO and silently enables the rust parser
when the extension is importable. Two real regressions were blessed by
sweeps whose "native" side auto-enabled it (comparing rust-vs-rust):
the Token re-mint (`48c73d248`, fixed by `0cb6c9f41`) and the
pivot/unpivot "order-dependent" mystery (fixed by `4e912a64d`). Any
ad-hoc probe MUST set, on the native side:

```python
FluffConfig(overrides={
    "dialect": ...,
    "use_rust_parser": False,
    "use_rust_engine": False,
})
```

The committed harnesses in this directory already do this.
