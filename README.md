# make-lint

A static linter for GNU Makefiles. It parses; it does not evaluate and it does
not run anything.

```
cargo build --release
./target/release/make-lint            # lints ./Makefile and its includes
./target/release/make-lint -f lib.mk --format json
```

Exit code `0` when nothing at or above `--fail-level` (default `warning`) was
found, `1` when something was, `2` on a bad argument or unreadable file.

## Status

Phase 1 of 5: parser plus the checks that need syntax only. No dependencies.

Later phases add the evaluator (undefined/redefined variables, unreachable
prerequisites), duplicate-value detection, and the allowlisted `$(shell ...)`
oracle. The parser is built for that: it keeps both sides of every conditional,
stores values unexpanded, and records a byte span for everything.

## Checks

| Code | Severity | What it finds |
| --- | --- | --- |
| MK010 | warning | `$FOO`, which make reads as `$(F)` followed by the literal `OO` |
| MK020 | warning | Two recipes for one target; make silently discards the first |
| MK025 | error | Recipe line indented with spaces, or with spaces before the tab |
| MK026 | error | Recipe line before any rule ("recipe commences before first target") |
| MK030 | warning | `$(call f,...)` where `f` is never defined |
| MK031 | warning | `$(patsubst ...)` misspelt, `$(strip)` with no argument, or a name containing a space such as `$(id -u)` |
| MK032 | error | Too few arguments to a built-in function |
| MK033 | error | Unterminated `$(` or `${` |
| MK034 | error | `define` without `endef`, or the reverse |
| MK035 | error | Unmatched `else` / `endif`, or a missing `endif` |
| MK036 | error | Malformed `ifeq` / `ifdef` condition |
| MK050 | warning | Non-optional `include` of a file that does not exist and is not a target |
| MK099 | note | A line the parser could not classify |

Notes are hidden unless `--show-notes` is passed.

## Design notes

**Conditionals are kept, not resolved.** Definedness unions every branch, so a
variable set only under `ifeq ($(OS),Darwin)` is not reported as undefined.
Redefinition is per-branch: every rule and recipe line carries the path of
conditional branches enclosing it, and two definitions in opposite branches of
one conditional never clash. A rule whose recipe lines all sit inside a
conditional is treated as possibly having no recipe at all.

**Includes are followed in make's read order.** An included file is read before
the rest of its includer, so `MK020` blames the right definition. An `include`
of a file that is itself a target is make's remaking-makefiles feature, not a
missing file.

**Nothing is claimed from an unknown.** If any `include` could not be resolved,
`MK030` goes quiet: the missing file may hold the definition.

**Spans survive the lexer.** Comment stripping and line continuations change
byte offsets, so each logical line carries a segment table mapping back to the
original file. Diagnostics always point at real source, including inside a
continued line.

Several fine points of make's own lexer were settled by experiment against GNU
Make 4.4.1 rather than from the manual, and the tests say which:

- `#` inside `$(...)` is not a comment. `A := $(shell echo 'x#y')` yields `x#y`.
- Make counts bare brackets inside a reference, which is why it rejects
  `$(shell echo '(' )#c` as an unterminated call rather than reading a comment.
- A `define` body is stored verbatim, comments included, and a reference may
  span its lines.

## Known limits

- Only GNU Make. BSD make's `.if` / `.for` is a different language.
- A makefile fragment that is only ever included by another (a kernel
  subdirectory `Makefile`, say) has no visible definition for the macros its
  parent supplies, so `MK030` reports them. Lint the real entry point.
- Variables and rules created by `$(eval ...)` are recognised only when the
  name appears as literal text.
- Conditional structure *within* a recipe is not preserved; the lines are
  attached to the rule with their branch paths.

## Tests

```
cargo test
```

70 tests. The integration tests in `tests/checks.rs` are largely regressions for
false positives found by running the linter across ~400 real makefiles from the
Calico, GNU make, and Linux kernel trees.
