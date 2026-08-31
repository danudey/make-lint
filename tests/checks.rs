//! End-to-end checks over real files on disk.
//!
//! Several cases here come from false positives found by running the linter
//! over the Calico and GNU make source trees, and every "make says" comment was
//! verified against GNU Make 4.4.1.

use make_lint::checks;
use make_lint::diag::Diagnostic;
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

fn codes(d: &[Diagnostic]) -> Vec<&str> {
    let mut c: Vec<&str> = d.iter().map(|x| x.code).collect();
    c.sort_unstable();
    c
}

#[test]
fn clean_makefile_is_silent() {
    let f = Fixture::new(&[(
        "Makefile",
        "CC := gcc\nOBJS := $(patsubst %.c,%.o,$(SRCS))\n\n.PHONY: all\nall: $(OBJS)\n\t$(CC) -o $@ $^\n",
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
    let f = Fixture::new(&[("Makefile", "all: a.c\n\tcp $< $@x\n\techo $^y $*z\n")]);
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
    let f = Fixture::new(&[("Makefile", "O := $(pastsubst %.c,%.o,$(S))\n")]);
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

#[test]
fn undefined_call_target_is_reported() {
    let f = Fixture::new(&[(
        "Makefile",
        "define greet\necho hi\nendef\nall:\n\t@echo $(call greet)\n\t@echo $(call nope,x)\n",
    )]);
    assert_eq!(codes(&f.run("Makefile")), vec!["MK030"]);
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
        ("lib.mk", "register:\nifneq ($(A),$(B))\n\techo maybe\nendif\n"),
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
