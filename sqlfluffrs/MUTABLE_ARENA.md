<!-- Design + phasing doc for the mutable-arena fix milestone on
     keraion/rust-cli-entry-point. Kept in-repo as an implementation record. -->

## Status — COMPLETE (2026-07-04)

Phases 1–5 + 7 landed (commits `45cfb419b` → `13335514e`); `facade_fix_loop_v3`
(mutate the arena, no reparse) is now the **default** façade fix path. Results:
fix **2.3× faster than native** at 8–30 KB (was ~1.5× *slower* under the v1
source-patch loop), **byte-identical**, and **guard-clean over the whole 2159-file
dialect corpus** (0 wrong-fix cases). Detection unchanged (~1.5×, 74/74).

Deviation from the plan below: **Phase 6 (grammar validation) was attempted and
found non-viable, then reverted.** Re-lex-and-match can't reproduce native's
`validate_segment_with_reparse` (it re-parses from text, which always conforms, so
it can't see native's typed-leaf rejection) *and* it false-rejects legitimate
fixes; the token-synthesis alternative is blocked because the Rust matcher consumes
lexer-typed tokens, not the arena's parser-typed leaves. Faithful validation would
need deep matcher rework for a ~1-rule payoff, so RF06/TQ02 remain on the native
fallback (as they were before). Follow-up: retire the legacy v1 source-patch
machinery (behind `SQLFLUFF_RS_FIX_V1=1` for one bisection cycle).

**2026-07-07: v1 RETIRED.** `apply_source_fixes`, `_native_apply_fixes` (the
uuid-bridge materialisation) and the `SQLFLUFF_RS_FIX_V1` dispatcher are
deleted; `facade_fix_loop_v3` is now simply `facade_fix_loop`. (Grammar
re-validation later landed in Rust after all — `revalidate.rs`, commit
4141dd03f — so RF06/TQ02 are façade-safe too.)

---

# Phase 2 — Mutable arena: native-semantics fixing over the Rust parse arena

(Supersedes the completed CLI-entry-point plan: Milestone 0 — standalone binary —
and Milestone 1 — Rust-driven orchestration + arena façade — are done and committed
on `keraion/rust-cli-entry-point`; history in git log and project memory.)

## Context

The arena façade runs unchanged Python lint rules over the Rust parse arena.
**Detection** is shipped and wins (~1.5× native on 2–30 KB files, ~6× on tiny-file
batches, 74/74 rules byte-identical). **Fixing** works (49 rules byte-identical,
guard-clean over the 2159-file dialect corpus, wired into stdin + path
`sqlfluff fix`) but is ~1.5× **slower** than native on real files: the arena is
read-only, so `facade_fix_loop` patches source text and re-parses the whole file
after every applied fix — `O(fixes × parse)`.

Native never reparses: `apply_fixes` (src/sqlfluff/core/linter/fix.py) splices edit
segments into the tree, `_position_segments` positions them, and only changed
subtrees are grammar-re-validated. A measured prototype (Rust-parse → materialise →
native loop) already beats native 1.16–1.22×; mutating the arena directly removes
the materialisation too and should let fixing inherit most of detection's 1.5×.

Mutability also erases the three divergence classes we dropped rules for:
- **Reparse-vs-mutate fixed points** (TQ02, multi-rule interactions): native loop
  semantics over a mutated tree reproduce native's oscillation-detect/revert.
- **No per-fix grammar validation** (RF06 backtick-names): in scope here, Rust-side.
- Source-patch edge-case tail (ordering, subsumption, boundary inserts): retired
  with the source-patch path itself.

Decisions (confirmed with user):
1. **Rust gets mutation + position pass + validation only.** Fix loop stays Python
   (`facade_fix_loop` mirroring `Linter.lint_fix_parsed`); source reconstruction
   reuses native `patch.py` + `LintedFile` static methods over the mutated façade (they
   only need `.raw/.pos_marker/.segments/.source_fixes`).
2. **Grammar validation in scope**, implemented **in Rust**. (Python-callback
   validation is disproven: the RF06 chase showed real container classes over
   synthetic leaves still pass validation native fails.)
3. **Mutation replaces the source-patch path** as the default; the old path stays
   behind a temporary flag for bisection, retired in a follow-up.

## Parity-critical native semantics (verified against code this session)
These are the details byte-parity lives or dies on — replicate exactly:
- Fixes keyed by **anchor uuid** (`compute_anchor_edit_info` — reuse verbatim in
  Python), applied **from the parent**; root-anchored fixes never apply.
- **consumed_pos**: on replace, the first edit whose raw == anchor raw inherits the
  anchor's full pos_marker (fix.py:209-217).
- Only multi-fix-per-anchor case: create_before+create_after pair, before first.
- Edit segments arrive pos_marker=None; `_position_segments` (base.py:448-562)
  synthesizes markers that are **not always zero-length** — forward-scan can widen
  via `PositionMarker.from_points`, end-point descends to `fwd.raw_segments[0]`,
  and there's a zero-templated-slice placeholder skip (issue #6261).
- **Non-code bubble-up**: containers that can't start/end non-code (everything but
  file/unparsable) trim boundary whitespace up to the parent (fix.py:280-293).
- **Unparsable guard is a silent revert**: `requires_validate` + pre-existing
  unparsable in scope + not fix_even_unparsable → restore that container's original
  children; rest of tree keeps its fixes (fix.py:317-330).
- Loop protection is **two** mechanisms: `(tree.raw, tuple(source_fixes))` version
  set AND `fixes == last_fixes` consecutive-identical check (linter.py:597-656).
- `BaseSegment.copy()` **preserves uuid** → fixes built from a shared edit segment
  carry duplicate uuids → arena ingest needs a collision policy.
- Native applies each rule's fixes **functionally** and adopts the new tree only
  after loop/no-op checks — hence the stage/commit API below.

## Design

### A. Arena storage + mutation primitives (`sqlfluffrs_parser/src/parser/arena.rs`)
- Add to `Arena`: `epoch: u64` (bumped per commit), `source_fixes:
  HashMap<NodeId, Vec<SourceFixSpec>>` (sparse side-table on leaves — don't fatten
  every `ArenaKind::Raw`), `staged: Option<StagedBatch>`.
- **Tombstone deletion**: detach (`parent = None`) + purge subtree uuids from
  `by_uuid`; never `Vec::remove`, never reuse slots → NodeIds stay valid forever,
  **no generational keys needed** (confirmed). Detached nodes keep payload so
  outstanding handles still read (mirrors native). `is_detached()` helper.
- Insertion appends (`alloc_with_uuid`); splice rebuilds `children` + renumbers
  `parent`/`parent_idx` for every entry; cache invalidation clears `cached_raw` +
  `descendant_types` on the node and **every ancestor to root**.

### B. Stage/commit edit API (`arena.rs` + `arena_py.rs`)
```
PyTree.stage_edit_batch(ops, fix_even_unparsable) -> StageSummary
PyTree.commit_staged() -> ApplyOutcome        PyTree.discard_staged()
```
- `EditOp = (anchor_uuid, kind ∈ {delete,replace,create_before,create_after},
  edits: [NodeSpec])`; **NodeSpec** = recursive spec-tuple built by Python
  (`_segment_to_spec`): uuid, SpecKind (Raw{class,type,raw,instance/class_types,
  kwargs} | Segment{...} | Meta{meta_type incl. Template source_str, block_uuid,
  is_implicit}), source_fixes, children.
- **Stage** plans without mutating (splice per §parity semantics, visiting exactly
  the union of anchor root-paths + created subtrees; ops crossing a planned
  tombstone are dropped → `unapplied_anchors`, matching native's never-visited
  children). Returns predicted `(staged_raw, staged_source_fixes, changed, ...)` so
  Python can run native's loop-detection gates **before** committing — no undo
  journal needed.
- **Commit** allocates spec nodes (preserve Python uuids — tag spaces are disjoint;
  on collision mint a Rust uuid), installs children, renumbers, tombstones,
  applies consumed_pos, invalidates caches up ancestor chains, runs the position
  pass, bumps epoch.
- `PyHandle.source_fixes()` (subtree aggregate), `PyHandle.source_str()` (stored
  Template source_str — fixes a latent façade nuance), `PyTree.epoch`. Update
  `sqlfluffrs/sqlfluffrs.pyi`.

### C. Position pass (Rust port of `_position_segments`)
Single top-down pass from root at commit time; marker primitives (`from_points`,
`start/end_point_marker`, `with_working_position`, `infer_next_position`) already
exist in `sqlfluffrs_types/src/marker.rs`. Per container: cursor from parent's
working pos; position-less children get start = prev sibling end / parent start,
end = forward-scan (with the #6261 placeholder skip, descending to the fwd node's
first raw); widen via from_points when start ≠ end. Recurse into a child iff its
marker changed **or** it's in the dirty set (covers native's per-level reform of
spliced-but-top-unchanged subtrees). Root marker never touched. Empty containers
skipped (native asserts; deliberate safe superset).

### D. Grammar validation (Rust; in scope)
After staging, for each planned container where native would set
`requires_validate` (and grammar exists for its `segment_class`): re-match the
container's rule against its planned leaf content; incomplete match or **new**
unparsables → fail those fixes (Python skips them, mirroring `_valid=False`).
- Primary approach: **re-lex + match** — lex the planned subtree raw with the
  dialect lexer, call the rule (`call_rule_as_root`-style entry,
  parser/core.rs) — simpler and semantically equivalent for the verdict.
- Alternative if verdicts diverge: token synthesis from leaves (risk: token typing
  for Python-created leaves; parser-matched raws lex generically).
- Ground truth: extract native `validate_segment_with_reparse` verdicts on the
  known cases (RF06 backtick procedure names → reject; CV11 multi-`::` casts;
  TQ02) and assert Rust verdicts match.

### E. Python integration (`src/sqlfluff/core/rules/rs_lint.py`)
`facade_fix_loop` v3 — parse **once**, mirror `lint_fix_parsed`:
- Phases main/post, runaway_limit, all-rules-first-pass; per rule: crawl the same
  (mutated) façade tree → `compute_anchor_edit_info` → reject invalid anchors →
  `fixes == last_fixes` check → `_anchor_info_to_ops` → `stage_edit_batch` →
  compare `(staged_raw, staged_source_fixes)` vs current + `previous_versions` →
  commit or discard. Non-convergence never commits a looping state → the arena is
  always last-good (native's revert-to-original emerges naturally).
- After each commit: `_sweep_wrapper_caches()` — iterate `_INTERN`, clear
  `_segments`/`_rwa` (`_ct`/`_uid` stay: surviving nodes never change kind/uuid).
  Explicit sweep, not per-access epoch checks — keeps the hot crawl path untouched.
- `RsSegment.source_fixes` property → real `SourceFix` objects from the handle
  (replaces hardcoded `[]`); `source_str` → stored value.
- Final reconstruction: `generate_source_patches(root_facade, templated_file)` +
  `LintedFile._slice_source_file_using_patches` / `_build_up_fixed_source_string`
  (native, unchanged).
- Default path = v3; `apply_source_fixes` + `_native_apply_fixes` kept behind a
  fallback flag for one cycle (bisection), then retired. CLI gates
  (`_try_facade_stdin_fix` / `_try_facade_paths_fix`) keep interface + self-guard.

## Edge cases (design answers)
Anchor inside same-batch-deleted subtree → dropped (AL07 nested case = canonical
test). Meta-anchored fixes → metas are ordinary children; reflow Indent edits need
Meta ingest. Root anchor / unknown uuid → unapplied, logged. Duplicate spec uuids →
mint fresh. Multiple batches → each commit leaves consistent markers (no
compounding). Templated files → synthesized markers inherit neighbours'
`Arc<TemplatedFile>`; JJ01 source_fixes persist on nodes. Empty containers →
tolerated. `reflow_depth_info` → computed per call, safe; `_rwa` swept.

## Critical files
- `sqlfluffrs/sqlfluffrs_parser/src/parser/arena.rs` — primitives, side-table,
  stage/commit splice, position pass, epoch, validation entry.
- `sqlfluffrs/sqlfluffrs_parser/src/parser/arena_py.rs` — FFI (EditOp/NodeSpec
  extraction, stage/commit/discard, epoch, source_fixes/source_str).
- `sqlfluffrs/sqlfluffrs_types/src/marker.rs` — existing marker primitives (reuse).
- `sqlfluffrs/sqlfluffrs_parser/src/parser/core.rs` — rule-match entry for
  validation.
- `src/sqlfluff/core/rules/rs_lint.py` — `_segment_to_spec`,
  `_anchor_info_to_ops`, `_sweep_wrapper_caches`, facade_fix_loop v3,
  source_fixes/source_str properties.
- Reference semantics (read, don't modify): `src/sqlfluff/core/linter/fix.py`,
  `src/sqlfluff/core/parser/segments/base.py:448-562`,
  `src/sqlfluff/core/linter/linter.py:457-660`.
- `sqlfluffrs/sqlfluffrs.pyi` — stub updates.

## Phasing (landable commits)
1. **Arena groundwork** (no behaviour change): epoch, `is_detached`, source_fixes
   side-table + accessors, `PyHandle.source_fixes`/`source_str`,
   `RsSegment.source_fixes` wiring, stubs; Rust unit tests.
2. **Spec ingest**: NodeSpec/EditOp/SourceFixSpec FFI, `alloc_with_uuid` +
   collision policy, `spec_raw`; tests.
3. **Splice engine**: stage/commit/discard, consumed_pos, bubble-up, unparsable
   revert, parent_idx/by_uuid upkeep, cache invalidation; structural-invariant
   tests (incl. stage→discard bit-identity).
4. **Position pass** + golden tests (point/widened/recursive/#6261 cases).
5. **Python integration**: facade_fix_loop v3 behind flag, conversion helpers,
   cache sweep, native-patch.py reconstruction. Gate: std_rule_cases fix sweep ≥
   current 49-rule parity.
6. **Grammar validation**: Rust re-match, verdict parity vs native
   (RF06/CV11/TQ02), wire skip-on-invalid into the loop.
7. **Re-vet + perf + flip**: corpus vetting under **correct per-fixture dialects**
   (the ansi-forced sweep lesson) to re-grow FACADE_SAFE_RULES (expect TQ02 + RF06
   back; likely LT01/LT05/LT14/CV06/LT09/AL05/ST02 too — their divergences were
   reparse-loop artifacts); benchmark; make v3 default; retire v1 in follow-up.

## Verification
- **Rust unit tests**: structural invariants after every op kind (children/parent/
  parent_idx coherence, by_uuid exactness, tombstone detachment, consumed_pos,
  create ordering, bubble-up into file root, unparsable revert, root-anchor
  unapplied, stage→discard bit-identical, cache invalidation up the chain,
  empty containers, duplicate uuids).
- **Position golden tests** vs `_position_segments` behaviour.
- **Validation verdict parity**: native vs Rust on RF06/CV11/TQ02 fixture set.
- **Byte-parity harness** (methodology from this session, documented in
  rs_lint.py): per-rule std_rule_cases sweep + combined multi-rule whole-corpus run
  over `test/fixtures/dialects/*` under each fixture's correct dialect;
  guard-missed must be 0. Differential v1-vs-v3 corpus check during transition.
- **Perf gate**: fix on 8/30 KB synthetic files (min-of-trials; box noise ±5–11%):
  must beat native (<1.0× of native's time; prototype ceiling to beat:
  1.16–1.22×). Detection must not regress (~1.5×).
- **Suites**: `test/rules/rs_lint_test.py` + `test/cli/rs_engine_fix_test.py`
  (893) and `test/cli/` stdin/path-fix gates.
