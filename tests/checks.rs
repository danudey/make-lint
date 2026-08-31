//! End-to-end checks over real files on disk.
//!
//! Several cases here come from false positives found by running the linter
//! over the Calico and GNU make source trees, and every "make says" comment was
//! verified against GNU Make 4.4.1.

use make_lint::checks;
use make_lint::diag::{Diagnostic, Severity};
use make_lint::workspace::Workspace;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    /// `files` is a list of (relative path, contents); the first is the root.
    fn new(files: &[(&str, &str)]) -> Fixture {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("make-lint-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            let p = dir.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
        Fixture { dir }
    }

    fn run(&self, root: &str) -> Vec<Diagnostic> {
        let mut ws = Workspace::new(Vec::new());
        ws.load_root(&self.dir.join(root)).unwrap();
        let mut d = std::mem::take(&mut ws.diags);
        d.extend(checks::run(&ws));
        d
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Codes the CLI would show by default: notes are hidden without --show-notes.
fn codes(d: &[Diagnostic]) -> Vec<&str> {
    let mut c: Vec<&str> =
        d.iter().filter(|x| x.severity > Severity::Note).map(|x| x.code).collect();
    c.sort_unstable();
    c
}

/// Every code, notes included.
fn all_codes(d: &[Diagnostic]) -> Vec<&str> {
    let mut c: Vec<&str> = d.iter().map(|x| x.code).collect();
    c.sort_unstable();
    c
}

#[test]
fn clean_makefile_is_silent() {
    let f = Fixture::new(&[(
        "Makefile",
        "CC := gcc\nSRCS :=\nOBJS := $(patsubst %.c,%.o,$(SRCS))\n\n.PHONY: all\nall: $(OBJS)\n\t$(CC) -o $@ $^\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn bare_dollar_run_on_is_reported() {
    let f = Fixture::new(&[("Makefile", "CFLAGS = -O2\nall:\n\tgcc $CFLAGS\n")]);
    assert_eq!(codes(&f.run("Makefile")), vec!["MK010"]);
}

#[test]
fn automatic_variables_are_not_reported() {
    let f =
        Fixture::new(&[("Makefile", "a.c:\n\ttouch $@\nall: a.c\n\tcp $< $@x\n\techo $^y $*z\n")]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// GNU make: `$(id -u)` in a recipe is a make variable named "id -u", not shell
// command substitution, so it expands to the empty string. Found in Calico's
// api/Makefile, where it silently produced `docker run --user :`.
#[test]
fn space_in_variable_name_is_reported() {
    let f = Fixture::new(&[("Makefile", "all:\n\tdocker run --user $(id -u):$(id -g) img\n")]);
    assert_eq!(codes(&f.run("Makefile")), vec!["MK031", "MK031"]);
}

#[test]
fn mistyped_function_suggests_the_real_one() {
    let f =
        Fixture::new(&[("Makefile", "S :=\nO := $(pastsubst %.c,%.o,$(S))\nall: ; @echo $(O)\n")]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), vec!["MK031"]);
    assert!(d[0].help.as_ref().unwrap().contains("patsubst"), "{:?}", d[0].help);
}

// `$(foreach file,...)` binds `file`, so `$(file)` inside it is a variable,
// not a broken call to the `file` function. From Calico's cni-plugins Makefile.
#[test]
fn foreach_binding_shadows_a_function_name() {
    let f = Fixture::new(&[(
        "Makefile",
        "FILES := a b\nall:\n\t$(foreach file,$(FILES),cp $(file) out/$(file);)\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn too_few_function_arguments_is_reported() {
    let f = Fixture::new(&[("Makefile", "O := $(patsubst %.c)\n")]);
    assert_eq!(codes(&f.run("Makefile")), vec!["MK032"]);
}

// A macro that simply does not exist here may come from a parent makefile, so
// it is only a note. A near-miss of one that does exist is a warning.
#[test]
fn undefined_call_target_is_a_note() {
    let f = Fixture::new(&[(
        "Makefile",
        "define greet\necho hi\nendef\nall:\n\t@echo $(call greet)\n\t@echo $(call nope,x)\n",
    )]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), Vec::<&str>::new());
    assert_eq!(all_codes(&d), vec!["MK030"]);
}

#[test]
fn misspelled_call_target_is_a_warning() {
    let f = Fixture::new(&[(
        "Makefile",
        "define install_thing\necho hi\nendef\nall:\n\t@echo $(call intsall_thing,x)\n",
    )]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), vec!["MK030"]);
    assert!(d[0].message.contains("install_thing`"), "{}", d[0].message);
}

#[test]
fn duplicate_recipe_in_one_file_is_reported() {
    let f = Fixture::new(&[("Makefile", "a:\n\techo one\n\na:\n\techo two\n")]);
    assert_eq!(codes(&f.run("Makefile")), vec!["MK020"]);
}

// Verified: make emits no warning for this, because only one branch is read.
#[test]
fn same_target_in_opposite_conditional_branches_is_fine() {
    let f = Fixture::new(&[(
        "Makefile",
        "ifdef NO_DOCKER\nut:\n\techo local\nelse\nut:\n\techo docker\nendif\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn double_colon_rules_may_repeat() {
    let f = Fixture::new(&[("Makefile", "a::\n\techo one\n\na::\n\techo two\n")]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// Make reads the included file first, so the includer's recipe is the override
// and the include holds the "first" definition.
#[test]
fn cross_file_duplicate_blames_in_read_order() {
    let f = Fixture::new(&[
        ("Makefile", "include lib.mk\n\nbuild:\n\techo outer\n"),
        ("lib.mk", "build:\n\techo inner\n"),
    ]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), vec!["MK020"]);
    let sm = {
        let mut ws = Workspace::new(Vec::new());
        ws.load_root(&f.dir.join("Makefile")).unwrap();
        ws
    };
    let primary = sm.sources.get(d[0].primary.file).path.clone();
    let first = sm.sources.get(d[0].secondary[0].span.file).path.clone();
    assert!(primary.ends_with("Makefile"), "{primary:?}");
    assert!(first.ends_with("lib.mk"), "{first:?}");
}

// An `include` inside a conditional is normally paired with an alternative in
// the other branch, so a missing file there is not a finding.
#[test]
fn guarded_include_is_not_reported() {
    let f = Fixture::new(&[(
        "Makefile",
        "ifneq (\"$(wildcard ../m.mk)\", \"\")\ninclude ../m.mk\nelse\ninclude ./m.mk\nendif\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn unguarded_missing_include_is_reported() {
    let f = Fixture::new(&[("Makefile", "include nope.mk\nall:\n\techo\n")]);
    assert_eq!(codes(&f.run("Makefile")), vec!["MK050"]);
}

#[test]
fn optional_missing_include_is_silent() {
    let f = Fixture::new(&[("Makefile", "-include nope.mk\nall:\n\techo\n")]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// A line that is only an expansion is valid make; it must not be reported as
// unparsable. From Calico's kube-controllers Makefile.
#[test]
fn bare_expansion_line_parses() {
    let f = Fixture::new(&[(
        "Makefile",
        "S := x\nifeq ($(filter ok,$(S)),)\n$(error refusing to run)\nendif\nall:\n\techo\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn misindented_recipe_beats_the_bare_expansion_rule() {
    let f = Fixture::new(&[("Makefile", "all:\n\techo a\n    $(MAKE) -C sub\n")]);
    assert_eq!(codes(&f.run("Makefile")), vec!["MK025"]);
}

// Verified against make: `A := $(shell echo 'x#y')` yields `x#y`.
#[test]
fn hash_inside_a_reference_does_not_truncate_the_line() {
    let f = Fixture::new(&[(
        "Makefile",
        "V := $(shell echo a | sed -r 's#/+#-#g')\nall:\n\techo $(V)\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// A reference may span the lines of a define body. From Calico's lib.Makefile.
#[test]
fn reference_spanning_define_body_lines() {
    let f = Fixture::new(&[(
        "Makefile",
        "define call_api\n\t$(eval CMD := curl -X$(1) \\\n\t\t-H 'x: y' \\\n\t\thttps://example/$(2))\nendef\nall:\n\t@echo\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn includes_are_followed_and_linted() {
    let f = Fixture::new(&[
        ("Makefile", "include sub/lib.mk\nall:\n\techo\n"),
        ("sub/lib.mk", "X = 1\nother:\n\tgcc $CFLAGS\n"),
    ]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), vec!["MK010"]);
}

// A rule whose recipe lives entirely inside a conditional may end up with no
// recipe at all, so it does not clash with a later definition. Verified: make
// prints no "overriding recipe" warning for this shape.
#[test]
fn conditional_only_recipe_does_not_clash() {
    let f = Fixture::new(&[
        ("Makefile", "include lib.mk\n\nregister:\n\techo real\n"),
        ("lib.mk", "A :=\nB :=\nregister:\nifneq ($(A),$(B))\n\techo maybe\nendif\n"),
    ]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// `$(eval dir := ...)` defines `dir`, so `$(dir)` afterwards is that variable
// and not a broken call to the `dir` function.
#[test]
fn eval_assignment_defines_a_name() {
    let f = Fixture::new(&[(
        "Makefile",
        "define go\n\t$(eval dir := $(1))\n\t@echo $(dir)\nendef\nall:\n\t$(call go,x)\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// make remakes an included file that is itself a target, then restarts.
#[test]
fn include_of_a_remakeable_target_is_not_missing() {
    let f = Fixture::new(&[(
        "Makefile",
        "Makefile.common:\n\tcurl -o $@ https://example/common\n\ninclude Makefile.common\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// An unreadable include may hold the macro's definition, so `undefined` cannot
// be claimed.
#[test]
fn unresolved_include_suppresses_undefined_macro() {
    let f = Fixture::new(&[("Makefile", "-include gen.mk\nall:\n\t@echo $(call maybe_there,x)\n")]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// ---------------------------------------------------------------------------
// Value checks (phase 2)
// ---------------------------------------------------------------------------

// Similarity alone is not evidence: makefiles are full of deliberate families.
// A warning needs both halves of the mistake visible — one name read and never
// assigned, its near-twin assigned and never read.
#[test]
fn orphaned_misspelling_is_a_warning() {
    let f = Fixture::new(&[("Makefile", "OUTPUT_DIR := build\nall:\n\t@echo $(OUPTUT_DIR)\n")]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), vec!["MK001"]);
    assert!(d[0].message.contains("OUTPUT_DIR"), "{}", d[0].message);
}

#[test]
fn a_name_that_is_used_elsewhere_is_not_a_typo_suggestion() {
    // BUILD_IMAGE is read, so it is a real variable and BUILD_IMAGES is simply
    // supplied from outside. Note, not warning.
    let f = Fixture::new(&[(
        "Makefile",
        "BUILD_IMAGE := one\nall:\n\t@echo $(BUILD_IMAGE) $(BUILD_IMAGES)\n",
    )]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), Vec::<&str>::new());
    assert!(all_codes(&d).contains(&"MK001"));
}

#[test]
fn plain_undefined_variable_is_a_note() {
    let f = Fixture::new(&[("Makefile", "all:\n\t@echo $(GITHUB_TOKEN)\n")]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), Vec::<&str>::new());
    assert_eq!(all_codes(&d), vec!["MK001"]);
}

#[test]
fn unused_variable_is_reported_only_for_the_linted_file() {
    let f = Fixture::new(&[
        ("Makefile", "MINE := unused\nall:\n\t@echo hi\n"),
        ("lib.mk", "THEIRS := also-unused\n"),
    ]);
    let d = f.run("Makefile");
    let notes: Vec<&str> = d.iter().filter(|x| x.code == "MK002").map(|x| x.code).collect();
    assert_eq!(notes, vec!["MK002"], "only the root's own variable");
    assert!(d.iter().any(|x| x.code == "MK002" && x.message.contains("MINE")));
}

#[test]
fn overwriting_an_unread_value_is_reported() {
    let f = Fixture::new(&[("Makefile", "VER := 1\nVER := 2\nall:\n\t@echo $(VER)\n")]);
    assert_eq!(codes(&f.run("Makefile")), vec!["MK003"]);
}

#[test]
fn overwriting_after_a_read_is_fine() {
    let f = Fixture::new(&[(
        "Makefile",
        "VER := 1\nTAG := v$(VER)\nVER := 2\nall:\n\t@echo $(TAG) $(VER)\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// An override inside a conditional is the normal way to vary a value.
#[test]
fn conditional_override_is_not_a_clobber() {
    let f = Fixture::new(&[(
        "Makefile",
        "VER := 1\nifdef RELEASE\nVER := 2\nendif\nall:\n\t@echo $(VER)\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn a_default_then_override_is_not_a_clobber() {
    let f = Fixture::new(&[("Makefile", "VER ?= 1\nVER := 2\nall:\n\t@echo $(VER)\n")]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn an_include_value_overwritten_by_the_includer_is_reported() {
    let f = Fixture::new(&[
        ("Makefile", "include lib.mk\nVER := 2\nall:\n\t@echo $(VER)\n"),
        ("lib.mk", "VER := 1\n"),
    ]);
    assert_eq!(codes(&f.run("Makefile")), vec!["MK003"]);
}

#[test]
fn deferred_shell_expanded_twice_is_reported() {
    let f = Fixture::new(&[(
        "Makefile",
        "REV = $(shell git rev-parse HEAD)\nall:\n\t@echo $(REV)\n\t@echo $(REV)\n",
    )]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), vec!["MK005"]);
    assert!(d[0].message.contains("2 expansions"), "{}", d[0].message);
}

#[test]
fn immediate_shell_is_fine() {
    let f = Fixture::new(&[(
        "Makefile",
        "REV := $(shell git rev-parse HEAD)\nall:\n\t@echo $(REV)\n\t@echo $(REV)\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn deferred_shell_used_once_is_fine() {
    let f =
        Fixture::new(&[("Makefile", "REV = $(shell git rev-parse HEAD)\nall:\n\t@echo $(REV)\n")]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// make refuses this outright: "Recursive variable references itself".
#[test]
fn self_referential_recursive_variable_is_an_error() {
    let f = Fixture::new(&[("Makefile", "CFLAGS = $(CFLAGS) -O2\nall:\n\t@echo $(CFLAGS)\n")]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), vec!["MK009"]);
}

#[test]
fn simple_assignment_may_read_itself() {
    let f = Fixture::new(&[(
        "Makefile",
        "CFLAGS := -g\nCFLAGS := $(CFLAGS) -O2\nall:\n\t@echo $(CFLAGS)\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn prerequisite_with_no_rule_and_no_file_is_reported() {
    let f = Fixture::new(&[("Makefile", "all: missing/thing.txt\n\t@echo hi\n")]);
    assert_eq!(codes(&f.run("Makefile")), vec!["MK021"]);
}

#[test]
fn prerequisite_that_exists_is_fine() {
    let f = Fixture::new(&[("Makefile", "all: there.txt\n\t@echo hi\n"), ("there.txt", "x")]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// A bare word is almost always a phony target, and a makefile meant to be
// included expects its includer to define those.
#[test]
fn bare_word_prerequisite_is_not_judged() {
    let f = Fixture::new(&[("Makefile", "test: ut fv st\n\t@echo hi\n")]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// make expands wildcards in prerequisites.
#[test]
fn glob_prerequisite_is_not_judged() {
    let f = Fixture::new(&[("Makefile", "all: pkg/*.go\n\t@echo hi\n")]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

// The target is only a name once the evaluator has expanded it.
#[test]
fn prerequisite_built_by_a_variable_target_is_fine() {
    let f = Fixture::new(&[(
        "Makefile",
        "BIN := dist/bin\n$(BIN)/tool:\n\t@touch $@\nall: dist/bin/tool\n\t@echo hi\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn prerequisite_covered_by_a_pattern_rule_is_fine() {
    let f = Fixture::new(&[(
        "Makefile",
        "%.pb.go: %.proto\n\t@touch $@\nall: api.pb.go\n\t@echo hi\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), Vec::<&str>::new());
}

#[test]
fn append_before_any_definition_is_a_note() {
    let f = Fixture::new(&[("Makefile", "MYLIST += one\nall:\n\t@echo $(MYLIST)\n")]);
    let d = f.run("Makefile");
    assert_eq!(codes(&d), Vec::<&str>::new());
    assert!(all_codes(&d).contains(&"MK004"));
}
