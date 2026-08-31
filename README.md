# make-lint

A static linter for GNU Makefiles. It parses and evaluates; it does not run
your build, and it does not run `$(shell ...)`.

```
cargo build --release
./target/release/make-lint              # lints ./Makefile and its includes
./target/release/make-lint --show-notes # include the quieter findings
./target/release/make-lint -f lib.mk --format json
```

Exit code `0` when nothing at or above `--fail-level` (default `warning`) was
found, `1` when something was, `2` on a bad argument or unreadable file.

## Status

Phases 1 and 2 of 5, with no dependencies.

* **Phase 1** — lexer, parser, and the checks that need syntax only.
* **Phase 2** — the evaluator: make's variable table, built-in functions, and
  the checks that need values.

Still to come: duplicate-value detection (phase 3) and the allowlisted
`$(shell ...)` oracle (phase 4).

## Checks

Notes are hidden unless `--show-notes` is passed.

| Code | Severity | What it finds |
| --- | --- | --- |
| MK001 | warning / note | A variable read but never assigned. A **warning** only when it looks like a genuine misspelling; otherwise a note (see below) |
| MK002 | note | A variable the linted file assigns and nothing reads |
| MK003 | warning | An assignment that discards a value nothing had read |
| MK004 | note | `+=` before the variable has any definition, so it acts as `=` |
| MK005 | warning | `VAR = $(shell ...)`, which re-runs the command on every expansion |
| MK009 | error | A recursive variable that refers to itself; make refuses to expand it |
| MK010 | warning | `$FOO`, which make reads as `$(F)` followed by the literal `OO` |
| MK020 | warning | Two recipes for one target; make silently discards the first |
| MK021 | warning | A prerequisite with no rule, no file, and no pattern that covers it |
| MK025 | error | Recipe line indented with spaces, or with spaces before the tab |
| MK026 | error | Recipe line before any rule |
| MK030 | warning / note | `$(call f,...)` where `f` is never defined; warning only for a likely misspelling |
| MK031 | warning | `$(patsubst ...)` misspelt, `$(strip)` with no argument, or a name containing a space such as `$(id -u)` |
| MK032 | error | Too few arguments to a built-in function |
| MK033 | error | Unterminated `$(` or `${` |
| MK034 | error | `define` without `endef`, or the reverse |
| MK035 | error | Unmatched `else` / `endif`, or a missing `endif` |
| MK036 | error | Malformed `ifeq` / `ifdef` condition |
| MK050 | warning | Non-optional `include` of a file that does not exist and is not a target |

## Design notes

### Never report from an unknown

A makefile cannot always be evaluated: `$(shell)` runs a command, a variable may
arrive from the environment, a conditional may be undecidable. Evaluation
therefore produces a `Value` that is `Known`, `Partial` (literal text around
holes), or `Unknown` *with the reason why*. Every value check stops at an
`Unknown`. Precision is given up freely to keep that rule true, because a linter
nobody trusts is worse than a quieter one.

### Conditionals: union for existence, per-branch for conflict

A condition make could decide is decided, and only that branch is read. A
condition that cannot be decided is **forked**: each branch is read against its
own copy of the variable table and the results merged, so a variable the
branches disagree about becomes `Unknown` rather than silently taking one
answer. Rules and recipe lines carry the path of conditional branches enclosing
them, so two definitions in opposite branches never count as a conflict.

### Undefined variables are mostly a note, on purpose

Make has no way to declare "this comes from the environment". `$(HOME)`,
`$(GOPATH)` and `$(GITHUB_TOKEN)` are read by design; so are the interface
variables a library makefile expects its includer to set. Warning on every
unassigned read produced 912 findings across the test corpus, essentially all of
them wrong.

What *is* sound is the shape of a real typo: one name read and never assigned,
and its near-twin assigned and never read — both halves of the mistake visible
at once. Similarity alone is not enough, because makefiles are full of
deliberate families (`BUILD_IMAGE` and `BUILD_IMAGES`, `X_C_FILES` and
`X_O_FILES`). With that rule the same corpus yields 4 warnings. The rest are
still there under `--show-notes`.

### Ground truth from make, not from the manual

Several fine points were settled by experiment against GNU Make 4.4.1, and the
tests record which:

- `#` inside `$(...)` is not a comment: `A := $(shell echo 'x#y')` yields `x#y`.
- Make counts bare brackets inside a reference, which is why it rejects
  `$(shell echo '(' )#c` as an unterminated call rather than reading a comment.
- A `define` body is stored verbatim, comments included, and a reference may
  span its lines.

`tests/differential.rs` goes further: it writes makefiles exercising every
function, operator and flavour the evaluator models, asks make to print each
variable's expanded value, and compares. It is skipped when make is absent.

### Other notes

**Includes are followed in make's read order.** An included file is read before
the rest of its includer, so MK020 and MK003 blame the right definition. An
`include` of a file that is itself a target is make's remaking-makefiles
feature, not a missing file.

**Spans survive the lexer.** Comment stripping and line continuations change
byte offsets, so each logical line carries a segment table mapping back to the
original file, and diagnostics point at real source even inside a continued
line.

**`$(wildcard)` is evaluated**; it only lists directories. Values derived from
it are tagged `Filesystem` stability, which phase 3 will use to avoid treating a
coincidence as a duplicate.

## Known limits

- Only GNU Make. BSD make's `.if` / `.for` is a different language.
- A makefile fragment that is only ever included by another (a kernel
  subdirectory `Makefile`, say) cannot see the macros its parent supplies. Lint
  the real entry point.
- `$(eval ...)` is applied for plain `NAME = value` statements read at parse
  time. Rules and directives inside `eval`, and `eval` reached only through
  `$(call)`, are recognised syntactically but not evaluated.
- Target-specific variables are parsed but not folded into the table; they apply
  only while their target is being built.
- Make's built-in implicit rules are used to excuse a prerequisite, but the
  chain is not followed: `foo.o` is accepted without checking that `foo.c`
  exists.

## Tests

```
cargo test
```

111 tests. `tests/checks.rs` is largely regressions for false positives found by
running the linter across ~400 real makefiles from the Calico, GNU make, and
Linux kernel trees; `tests/differential.rs` checks the evaluator against make
itself. `examples/dump.rs` prints the resolved variable table, which is how the
differential comparison is done by hand.
