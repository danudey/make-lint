//! Differential test: the evaluator's variable table against real GNU Make.
//!
//! Unix only. Windows runners do carry GNU Make, but the comparison is built
//! out of a POSIX shell: values are read back through `printf`, the fixtures
//! call `uname`, `tr` and `wc`, and make is run with `PATH=/usr/bin:/bin` so
//! the probe cannot pick up anything from the host. None of that is portable,
//! and rebuilding it around `cmd.exe` would compare the evaluator against a
//! make configured differently from the one it models.
#![cfg(unix)]
//!
//! Every construct the evaluator models is written into a makefile, then make
//! is asked to print each variable's *expanded* value and the two tables are
//! compared. `make -p` is not used for this: it prints a recursive variable's
//! unexpanded text, whereas the evaluator resolves it, so the two would not be
//! comparing the same thing.
//!
//! The test is skipped when GNU Make is not on PATH.
//!
//! Distros ship a wide spread of GNU Make (4.2.1 on RHEL 8 through 4.4.1 on
//! current Debian/Arch), and make grew functions over that range. Comparing a
//! construct against a make that predates it would fail against the evaluator,
//! which models current make: an unknown `$(intcmp ...)` is just an undefined
//! variable reference to make 4.3, so make prints nothing and is "wrong". Each
//! such construct therefore lives in its own test, gated on the version that
//! introduced it, rather than in the shared fixtures.

use make_lint::eval;
use make_lint::workspace::Workspace;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn gnu_make() -> Option<String> {
    for cand in ["make", "gmake"] {
        let out = Command::new(cand).arg("--version").output().ok();
        if let Some(o) = out
            && o.status.success()
            && String::from_utf8_lossy(&o.stdout).starts_with("GNU Make")
        {
            return Some(cand.to_string());
        }
    }
    None
}

/// GNU Make's version as `(major, minor)`, from the `--version` banner.
///
/// The banner's first line is `GNU Make 4.4.1`; a trailing patch level is
/// ignored, as no function has ever arrived in one.
fn make_version(make: &str) -> Option<(u32, u32)> {
    let out = Command::new(make).arg("--version").output().ok()?;
    let banner = String::from_utf8_lossy(&out.stdout);
    let first = banner.lines().next()?;
    let num = first.strip_prefix("GNU Make ")?.trim();
    let mut parts = num.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

/// Variable names assigned at the start of a line, in order.
fn probed_names(src: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in src.lines() {
        let line = line.strip_prefix("$(eval ").unwrap_or(line);
        // '!' is here for the `!=` shell-assignment operator.
        let Some(cut) = line.find(['=', ':', '+', '?', '!']) else { continue };
        let name = line[..cut].trim();
        if name.is_empty()
            || !name.starts_with(|c: char| c.is_ascii_uppercase())
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        // Must really be an assignment, not a rule.
        if !line[cut..].starts_with(['=', ':', '+', '?', '!']) {
            continue;
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn make_values(make: &str, dir: &Path, names: &[String]) -> BTreeMap<String, String> {
    let mut probe = String::from("include Makefile\n\nprobe:\n");
    for n in names {
        // Single quotes keep the shell from touching the expansion.
        probe.push_str(&format!("\t@printf '%s\\n' '{n}=$({n})'\n"));
    }
    std::fs::write(dir.join("probe.mk"), probe).unwrap();

    let out = Command::new(make)
        .args(["-s", "-f", "probe.mk", "probe"])
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("run make");
    assert!(out.status.success(), "make failed: {}", String::from_utf8_lossy(&out.stderr));

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

fn our_values(dir: &Path) -> BTreeMap<String, String> {
    let mut ws = Workspace::new(Vec::new());
    ws.load_root(&dir.join("Makefile")).unwrap();
    let a = eval::analyse(&ws);
    a.vars
        .iter()
        .filter_map(|(k, v)| v.value.as_known().map(|t| (k.clone(), t.to_string())))
        .collect()
}

/// A fresh empty directory of its own, so tests can run in parallel.
fn fixture_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("make-lint-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The make to compare against, or `None` when this host's make is too old to
/// know the construct under test — in which case the test has nothing to say.
fn make_at_least(min: (u32, u32)) -> Option<String> {
    let Some(make) = gnu_make() else {
        eprintln!("skipping: GNU Make not found");
        return None;
    };
    match make_version(&make) {
        Some(v) if v >= min => Some(make),
        Some(v) => {
            eprintln!(
                "skipping: GNU Make {}.{} is older than the {}.{} this construct needs",
                v.0, v.1, min.0, min.1
            );
            None
        }
        None => {
            eprintln!("skipping: could not read GNU Make's version");
            None
        }
    }
}

#[track_caller]
fn agrees_with_make(src: &str) {
    agrees_with_make_since((0, 0), src);
}

/// As [`agrees_with_make`], but skipped unless make is at least `min`.
#[track_caller]
fn agrees_with_make_since(min: (u32, u32), src: &str) {
    let Some(make) = make_at_least(min) else { return };
    let dir = fixture_dir("diff");
    std::fs::write(dir.join("Makefile"), src).unwrap();

    let names = probed_names(src);
    assert!(!names.is_empty(), "nothing to probe");
    let theirs = make_values(&make, &dir, &names);
    let ours = our_values(&dir);

    let mut wrong = Vec::new();
    for (k, want) in &theirs {
        match ours.get(k) {
            None => wrong.push(format!("{k}: make={want:?}, we could not resolve it")),
            Some(got) if got != want => wrong.push(format!("{k}: make={want:?}, ours={got:?}")),
            Some(_) => {}
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        wrong.is_empty(),
        "{} of {} variables disagree with GNU Make:\n  {}",
        wrong.len(),
        theirs.len(),
        wrong.join("\n  ")
    );
}

#[test]
fn functions_and_operators() {
    agrees_with_make(
        r#"
CC := gcc
SRCS := a.c b.c dir/c.c
OBJS := $(SRCS:.c=.o)
OBJS2 = $(patsubst %.c,%.o,$(SRCS))
NAMES := $(notdir $(SRCS))
BASES := $(basename $(NAMES))
SUFFS := $(suffix $(SRCS))
DIRS := $(sort $(dir $(SRCS)))
FILT := $(filter %.c,$(SRCS) x.h)
FOUT := $(filter-out a.c,$(SRCS))
W := $(word 2,$(SRCS))
WL := $(wordlist 1,2,$(SRCS))
N := $(words $(SRCS))
FW := $(firstword $(SRCS))
LW := $(lastword $(SRCS))
SUB := $(subst .c,.cpp,$(SRCS))
FS := $(findstring b.c,$(SRCS))
AP := $(addprefix o/,$(NAMES))
AS := $(addsuffix .bak,$(NAMES))
JN := $(join a b,1 2 3)
STRIPPED := $(strip   a   b   )
IF1 := $(if $(SRCS),yes,no)
IF2 := $(if ,yes,no)
ORV := $(or ,,third)
ANDV := $(and a,b,c)
FE := $(foreach s,$(SRCS),[$(s)])
all: ; @true
"#,
    );
}

// $(intcmp) and $(let) both arrived in make 4.4. To anything older they parse
// as a reference to an undefined variable, which expands to nothing.
#[test]
fn intcmp_and_let() {
    agrees_with_make_since(
        (4, 4),
        r#"
IC := $(intcmp 3,5,less,eq,more)
ICE := $(intcmp 4,4,less,eq,more)
ICM := $(intcmp 9,5,less,eq,more)
ICSHORT := $(intcmp 1,2,only-lt)
LETV := $(let a b,1 2 3,[$(a)][$(b)])
LETREST := $(let first rest,x y z,$(first)/$(rest))
all: ; @true
"#,
    );
}

#[test]
fn flavours_and_appends() {
    agrees_with_make(
        r#"
CC := gcc
REC = $(CC) -c
REC2 = $(REC) -O2
SIMPLE := $(CC) -c
APPEND := one
APPEND += two
RAPPEND = one
RAPPEND += $(CC)
COND ?= defaulted
COND ?= ignored
SET := first
SET ?= not-applied
define macro
<$(1)|$(2)>
endef
CALLV := $(call macro,x,y)
FLAV := $(flavor REC) $(flavor APPEND) $(flavor NOPE)
ORIG := $(origin CC) $(origin NOPE) $(origin AR)
all: ; @true
"#,
    );
}

// The built-in ARFLAGS gained its leading dash in make 4.4 ("rv" before,
// "-rv" after). The evaluator models current make, so older makes disagree.
#[test]
fn builtin_variable_defaults() {
    agrees_with_make_since(
        (4, 4),
        r#"
DEFAULTED := $(AR) $(ARFLAGS)
all: ; @true
"#,
    );
}

#[test]
fn conditionals_and_nesting() {
    agrees_with_make(
        r#"
OS := Linux
EMPTY :=
ifeq ($(OS),Linux)
PLAT := linux
else
PLAT := other
endif
ifneq ($(EMPTY),)
NEG := taken
else
NEG := nottaken
endif
ifdef OS
HAVE := yes
else
HAVE := no
endif
ifdef NEVER_SET_ANYWHERE
MISS := yes
else
MISS := no
endif
NESTED := $(subst a,b,$(subst c,a,cxc))
define wrap
[$(1)$(call inner,$(2))]
endef
define inner
{$(1)}
endef
CALLN := $(call wrap,A,B)
FE2 := $(foreach x,1 2,$(foreach y,a b,$(x)$(y)))
SREF := $(NESTED:b=z)
PREF := $(patsubst %,pre-%,x y)
CHAIN = $(A1)
A1 = $(A2)
A2 = deep
CHAINV := $(CHAIN)
DOLLAR := a$$b
PCT := 100%
COMMA := ,
LIST := $(subst $(COMMA), ,a,b,c)
MULTI := $(strip $(if $(filter linux,$(PLAT)),L,O))
all: ; @true
"#,
    );
}

#[test]
fn eval_defines_a_variable() {
    agrees_with_make(
        r#"
$(eval EV := from-eval)
EVUSE := $(EV)
BASE := x
$(eval DERIVED := $(BASE)-y)
all: ; @true
"#,
    );
}

#[test]
fn wildcard_reads_the_tree() {
    let Some(_) = gnu_make() else { return };
    let dir = fixture_dir("wild");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.c"), "").unwrap();
    std::fs::write(dir.join("src/b.c"), "").unwrap();
    std::fs::write(dir.join("Makefile"), "SRCS := $(sort $(wildcard src/*.c))\nall: ; @true\n")
        .unwrap();

    let ours = our_values(&dir);
    assert_eq!(ours.get("SRCS").map(String::as_str), Some("src/a.c src/b.c"));
    let _ = std::fs::remove_dir_all(&dir);
}

// Only allowlisted commands appear here: make will really run whatever is in
// the file, so the fixture must be safe to execute.
#[test]
fn shell_output_matches_make() {
    let Some(make) = gnu_make() else { return };
    let dir = fixture_dir("sh");
    std::fs::write(dir.join("VERSION"), "v3.29.1\n").unwrap();
    std::fs::write(
        dir.join("Makefile"),
        r#"
VER := $(shell cat VERSION)
KERNEL := $(shell uname -s)
LOWER := $(shell uname -s | tr A-Z a-z)
BASE := $(shell basename /a/b/c.txt)
DIR := $(shell dirname a/b/c.txt)
WORDS := $(shell echo one two three)
MULTILINE := $(shell printf 'a\nb\n')
TRAILING := $(shell printf 'x\n\n')
EMPTYOUT := $(shell printf '')
QUOTED := $(shell echo "hello world")
CHAINED := $(shell echo one two | wc -w)
DEFERRED = $(shell echo deferred)
all: ; @true
"#,
    )
    .unwrap();

    let names: Vec<String> = [
        "VER",
        "KERNEL",
        "LOWER",
        "BASE",
        "DIR",
        "WORDS",
        "MULTILINE",
        "TRAILING",
        "EMPTYOUT",
        "QUOTED",
        "CHAINED",
        "DEFERRED",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let theirs = make_values(&make, &dir, &names);
    let ours = our_values(&dir);
    let mut wrong = Vec::new();
    for (k, want) in &theirs {
        match ours.get(k) {
            None => wrong.push(format!("{k}: make={want:?}, we did not resolve it")),
            Some(got) if got != want => wrong.push(format!("{k}: make={want:?}, ours={got:?}")),
            Some(_) => {}
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(wrong.is_empty(), "{} disagree:\n  {}", wrong.len(), wrong.join("\n  "));
}

// The `!=` shell-assignment operator arrived in make 4.0. Older makes read the
// line as an ordinary recursive assignment to a variable named "BANGED !",
// leaving BANGED itself undefined.
#[test]
fn shell_assignment_operator() {
    agrees_with_make_since(
        (4, 0),
        r#"
BANGED != echo from-bang
BANGEDWORDS != echo one two three
all: ; @true
"#,
    );
}
