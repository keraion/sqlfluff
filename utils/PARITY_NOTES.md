# Parity harness notes (rust engine / façade)

Context for the corpus parity harnesses in this directory
(`facade_fix_parity.py`, `facade_lint_parity.py`). Current status as of
2026-07-07 (branch `keraion/rust-cli-entry-point`, after commit
`0cb6c9f41`): whole-corpus **fix parity 0/2168**, **lint parity 0/2166**,
raw-token sweep (type + class_types + raw, pure-python vs rust)
**0/2170** — with exactly ONE recurring "error" (not a divergence),
documented below.

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

**Fix options** (none applied yet — pick one):

1. Configure `templater = raw` in the harnesses (matches what the
   dialect test suite does; makes the corpus 100% clean). Applies to
   `facade_fix_parity.py`, `facade_lint_parity.py`, and the token-sweep
   script. Downside: hides genuine templater-path behavior for every
   OTHER file (today none of them template, so this is currently free).
2. Per-file skip/allow-list with a comment.
3. Leave as-is and treat `errors=1` as the expected whole-corpus
   baseline (what we've been doing).

Note the CLI fast paths are already correct here: templated sources
(`source_str != templated_str`) and templater failures route to native
by the eligibility gates in `commands.py`, so production behavior is
identical regardless.

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
