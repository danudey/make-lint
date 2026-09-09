# make-lint

A static linter for GNU Makefiles. It parses, evaluates, and runs only the
`$(shell ...)` commands it can prove are read-only.

```
cargo build --release
./target/release/make-lint              # lints ./Makefile and its includes
./target/release/make-lint --show-notes # include the quieter findings
./target/release/make-lint -f lib.mk --format json
./target/release/make-lint --no-exec       # run no commands at all
./target/release/make-lint --format sarif  # for CI
./target/release/make-lint --fix           # apply the unambiguous fixes
./target/release/make-lint --explain MK006
```

Exit code `0` when nothing at or above `--fail-level` (default `warning`) was
found, `1` when something was, `2` on a bad argument or unreadable file.

## Status

All five phases, with no dependencies.

* **Phase 1** — lexer, parser, and the checks that need syntax only.
* **Phase 2** — the evaluator: make's variable table, built-in functions, and
  the checks that need values.
* **Phase 3** — duplicate-value detection.
* **Phase 4** — the `$(shell ...)` oracle.
* **Phase 5** — SARIF, configuration, suppression comments, and `--fix`.

## Checks

Notes are hidden unless `--show-notes` is passed.

| Code | Severity | What it finds |
| --- | --- | --- |
| MK001 | warning / note | A variable read but never assigned. A **warning** only when it looks like a genuine misspelling; otherwise a note (see below) |
| MK002 | note | A variable the linted file assigns and nothing reads |
| MK003 | warning | An assignment that discards a value nothing had read |
| MK004 | note | `+=` before the variable has any definition, so it acts as `=` |
| MK005 | warning | `VAR = $(shell ...)`, which re-runs the command on every expansion |
| MK006 | warning | Two or more variables that independently resolve to the same distinctive value |
| MK007 | note | `A = $(B)`: a second live name for the same value |
| MK008 | note | Two variables defined by the same expression, where the value could not be resolved |
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
| MK040 | note | A `$(shell ...)` that was not run, and why |
| MK097 | warning | A suppression comment names a rule that does not exist |
| MK098 | note | The file announces itself as generated, so advice about how it is written was suppressed |

`make-lint --list-rules` prints the table above; `--explain MK006` prints the
reasoning behind one rule.

## Suppressing a finding

```makefile
CFLAGS = -O2   # make-lint: disable=MK010

# make-lint: disable=duplicate-value
REGISTRY := quay.io/calico
```

A marker at the end of a line covers that line; on a line of its own it covers
the next. `disable-file` covers the whole file, and omitting `=CODE` covers
every rule. Codes and slugs both work. A comment naming a rule that does not
exist is reported (MK097) rather than silently doing nothing, and the run
summary says how many findings were suppressed so they do not simply vanish.

## Configuration

`.make-lint.toml`, looked for beside the makefile and then in each parent
directory, so linting a subdirectory still picks up the project's settings.

```toml
disable = ["MK002", "append-before-definition"]
fail-level = "error"
include-dirs = ["mk"]

[severity]
MK006 = "note"

[exec]
enabled = true
allow = ["cksum"]
```

An unknown key, an unknown rule, or a value of the wrong type is an error, not a
shrug: a setting that silently does nothing is worse than one that fails, since
the reader believes it worked. The command line wins wherever it said something.

The parser is a small TOML subset written here rather than pulled in as a
dependency — a linter that runs commands should be able to account for every
line of code it ships, and the config surface is not big enough to be worth a
crate.

## Fixing

`--fix` applies only the rewrites with a single obvious answer: `$FOO` becomes
`$(FOO)` (MK010), and a space-indented recipe line becomes a tab-indented one
(MK025). Nothing else offers a fix, because nothing else has one answer a linter
should pick on someone's behalf. Overlapping fixes are resolved before anything
is written, and suppressing a finding also declines its fix.

`--format json` reports the same rewrites without applying them, so an editor
can offer them one at a time:

```json
{
  "code": "MK010", "severity": "warning", "file": "Makefile",
  "line": 3, "column": 6, "endLine": 3, "endColumn": 8,
  "message": "`$C` refers to the one-character variable `C`, ...",
  "fix": {
    "description": "`$CFLAGS` to `$(CFLAGS)`", "file": "Makefile",
    "line": 3, "column": 6, "endLine": 3, "endColumn": 13,
    "replacement": "$(CFLAGS)"
  }
}
```

The `fix` range is its own, and is usually wider than the range the diagnostic
underlines: MK010 points at the `$C` that misleads, but the rewrite has to
cover the whole of `$CFLAGS`. Applying the fix to the diagnostic's own range
would corrupt the line.

## Editors

`--stdin-path` lints text on stdin as though it were the file saved at that
path:

```
make-lint --stdin-path /repo/Makefile --format json --no-exec < buffer
```

That is what an editor needs to lint a buffer with unsaved edits. The path
never has to exist — a new, never-saved file lints fine — but it still decides
which directory `include` directives and `.make-lint.toml` are searched from,
and it is the path reported back in the output, so diagnostics land on the
buffer the text came from. Included files are read from disk as usual.

Passing `--fix` with `--stdin-path` is an error rather than a write: the text
linted is not the text on disk, so applying the fix would overwrite whatever
the editor has not saved. Editors should read the `fix` ranges and apply them
themselves.

Two other things matter when driving make-lint from an editor. Exit code 1
means findings, not failure — only 2 is an error. And `--no-exec` is worth
considering as the default: the `$(shell ...)` oracle is careful (see below),
but a lint on every keystroke is a different proposition from a lint in CI.

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

### Duplicate values: derivation, then distinctiveness

Two variables holding the same value is only interesting when neither got it
from the other. The evaluator therefore records, for each variable, which
variables its value was read from, and MK006 drops any member of an equal-value
group that derives from another member — so `TAG := $(VERSION)` matching
`VERSION` is arithmetic, not a coincidence. A pure alias (`A = $(B)`) is
reported separately and quietly as MK007.

What is left is filtered on how distinctive the shared value is. A compound
value — `github.com/projectcalico/calico/api`, `.crds/enterprise`,
`quay.io/calico` — appearing under two names is duplication. A bare word like
`latest`, `master` or `amd64` is a token many variables hold for unrelated
reasons; pairing those up produced most of the noise in testing, so a value with
no internal structure has to be long to qualify. That took MK006 from 285
findings on the corpus to 95, and the survivors are things like
`KINDEST_NODE_VERSION` and `K8S_VERSION` having to be bumped together.

MK008 covers the case where the value cannot be resolved at all. It compares the
*expression* rather than the value, so its claim stays exact: the same
expression is written twice. It only compares definitions of the same flavour,
since `:=` freezes a value where it stands.

### Generated makefiles are left alone

A makefile whose header says a tool wrote it gets syntax and correctness checks
but no advice about how it is written — the generator decides that, and the
duplication in autoconf output is inherent to how it substitutes. MK098 says so
rather than letting the run look clean.

### Running commands without becoming a way to run commands

A makefile may come from a third-party repository or an unreviewed pull request,
so the gate is default-deny at every step:

1. The command must already be fully expanded. What cannot be seen cannot be
   vetted.
2. It must be **recognised, not parsed**. A deliberately tiny grammar accepts a
   pipeline of plain commands and nothing else — no redirection, no
   substitution, no `;`, no globbing, no `~`, no environment prefix. Using a
   real shell parser and then denying the dangerous parts gets this backwards;
   anything the recogniser was never taught is refused because it was never
   taught it.
3. The program must be a **bare name** resolving inside a system directory, from
   a fixed `PATH` rather than the caller's. `$(shell ./hack/gen.sh)` and
   `$(shell bin/yq ...)` are repository-supplied executables and are refused —
   running those is exactly the thing being avoided.
4. It must be in the allowlist and satisfy that entry's argument policy:
   `find -delete`, `sed -i`, `sort -o`, `git push` and `git ls-remote` are out,
   and path arguments are confined to the project directory, so `cat VERSION`
   works and `cat ~/.ssh/id_rsa` does not. Commands that only manipulate strings
   (`basename`, `dirname`) have no paths to confine.
5. Only then does it run: no stdin, no inherited environment, `LC_ALL=C`, a
   two-second timeout, a 64 KiB output cap, and a budget across the whole run.

`env`, `xargs`, `sudo`, `sh`, `awk` and `perl` are never allowed even if a user
adds them: they exist to run something else, and the argument policy cannot see
through them.

There is no "run everything" switch. `--allow-command NAME` adds one command,
still subject to confinement and the wrapper ban, which covers the real need
without offering a way to execute an untrusted file wholesale.

Refusals are not silent. MK040 names the command and the reason, so an unknown
value is distinguishable from an empty one:

```
Makefile:12:14: note[MK040]: `$(shell curl https://example.com)` was not run:
                             `curl` is not in the allowlist
  = help: values derived from it stay unknown; `--allow-command NAME` permits it
```

Values carry how reproducible they are. `git` output is marked volatile, not
merely filesystem-dependent, because everything git reports about a working tree
is checkout state — two variables holding today's branch name say nothing about
how the makefile is written, and MK006 ignores them.

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
- Recipes are expanded to find variable uses, so a `$(shell ...)` inside a
  recipe runs during linting even though make would only run it when building
  that target. Everything the allowlist permits is read-only, but it is more
  commands than a plain `make -n` would run.
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

189 tests. `tests/checks.rs` is largely regressions for false positives found by
running the linter across ~400 real makefiles from the Calico, GNU make, and
Linux kernel trees; `tests/differential.rs` checks the evaluator against make
itself, including the values `$(shell ...)` produces; `tests/cli.rs` drives the
binary, since the config file, suppression comments, output formats and `--fix`
only meet each other in `main`. One test writes a makefile that tries ten ways
to run `touch` from `$(shell)` and asserts the file never appears.
`examples/dump.rs` prints the resolved variable table, which is how the
differential comparison is done by hand.
