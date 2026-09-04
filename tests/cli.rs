//! End-to-end tests driving the binary, since the config file, suppression
//! comments, output formats and `--fix` only meet each other in `main`.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(files: &[(&str, &str)]) -> Project {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("make-lint-cli-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            let p = dir.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
        Project { dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(["--color", "never"])
            .args(args)
            .current_dir(&self.dir)
            .output()
            .expect("run make-lint")
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.run(args).stdout).into_owned()
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn bin() -> PathBuf {
    // The test binary lives beside the one under test.
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("make-lint")
}

const NOISY: &str = "CFLAGS = -O2\nall:\n\tgcc $CFLAGS\n";

// ---------------------------------------------------------------------------
// Suppression comments
// ---------------------------------------------------------------------------

#[test]
fn a_trailing_comment_suppresses_that_line() {
    let p = Project::new(&[(
        "Makefile",
        "CFLAGS = -O2\nall:\n\tgcc $CFLAGS # make-lint: disable=MK010\n",
    )]);
    let out = p.run(&[]);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("MK010"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 suppressed"));
    assert!(out.status.success(), "a fully suppressed run should pass");
}

#[test]
fn an_unsuppressed_run_still_reports() {
    let p = Project::new(&[("Makefile", NOISY)]);
    assert!(p.stdout(&[]).contains("MK010"));
}

#[test]
fn disable_file_covers_everything() {
    let p = Project::new(&[(
        "Makefile",
        "# make-lint: disable-file\nCFLAGS = -O2\nall:\n\tgcc $CFLAGS\n",
    )]);
    assert!(!p.stdout(&[]).contains("MK010"));
}

#[test]
fn a_suppression_naming_no_rule_is_reported() {
    let p = Project::new(&[("Makefile", "A := 1 # make-lint: disable=MK999\nall: ; @true\n")]);
    let out = p.stdout(&[]);
    assert!(out.contains("MK097"), "{out}");
    assert!(out.contains("MK999"));
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn config_can_switch_a_rule_off() {
    let p = Project::new(&[("Makefile", NOISY), (".make-lint.toml", "disable = [\"MK010\"]\n")]);
    assert!(!p.stdout(&[]).contains("MK010"));
    // ...and --no-config puts it back.
    assert!(p.stdout(&["--no-config"]).contains("MK010"));
}

#[test]
fn config_can_force_a_severity() {
    let p = Project::new(&[
        ("Makefile", NOISY),
        (".make-lint.toml", "[severity]\nMK010 = \"error\"\n"),
    ]);
    let out = p.stdout(&[]);
    assert!(out.contains("error[MK010]"), "{out}");
}

#[test]
fn config_is_found_in_a_parent_directory() {
    let p = Project::new(&[
        (".make-lint.toml", "disable = [\"bare-dollar\"]\n"),
        ("sub/Makefile", NOISY),
    ]);
    assert!(!p.stdout(&["-f", "sub/Makefile"]).contains("MK010"));
}

#[test]
fn a_bad_config_is_an_error_not_a_shrug() {
    let p = Project::new(&[("Makefile", NOISY), (".make-lint.toml", "disabel = [\"MK010\"]\n")]);
    let out = p.run(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown key"));
}

#[test]
fn config_can_turn_off_running_commands() {
    let p = Project::new(&[
        ("Makefile", "V := $(shell cat VERSION)\nall:\n\t@echo $(V)\n"),
        ("VERSION", "1.0\n"),
        (".make-lint.toml", "[exec]\nenabled = false\n"),
    ]);
    assert!(p.stdout(&["--show-notes"]).contains("MK040"));
}

// ---------------------------------------------------------------------------
// Output formats
// ---------------------------------------------------------------------------

#[test]
fn sarif_output_is_well_formed() {
    let p = Project::new(&[("Makefile", NOISY)]);
    let out = p.stdout(&["--format", "sarif"]);
    assert!(out.starts_with(r#"{"$schema":"https://json.schemastore.org/sarif-2.1.0.json""#));
    assert!(out.contains(r#""version":"2.1.0""#));
    assert!(out.contains(r#""ruleId":"MK010""#));
    assert!(out.contains(r#""level":"warning""#));
    // Paths are relative to the project so a CI viewer can match the checkout.
    assert!(out.contains(r#""uri":"Makefile""#), "{out}");
    assert!(out.contains(r#""startLine":3"#));
    assert!(out.contains("partialFingerprints"));
    // Every rule is declared, not just the ones that fired.
    assert!(out.contains(r#""id":"MK006""#));
    assert!(well_formed_json(&out), "SARIF is not well formed:\n{out}");
}

/// A string-aware structural check: brackets balance and every string closes.
/// Also exercises the escaping, which is the part most likely to break.
fn well_formed_json(s: &str) -> bool {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                // A raw control character inside a string is invalid JSON.
                c if (c as u32) < 0x20 => return false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !in_string
}

#[test]
fn json_output_still_works() {
    let p = Project::new(&[("Makefile", NOISY)]);
    let out = p.stdout(&["--format", "json"]);
    assert!(out.starts_with('['));
    assert!(out.contains(r#""code":"MK010""#));
}

// ---------------------------------------------------------------------------
// --fix
// ---------------------------------------------------------------------------

#[test]
fn fix_rewrites_a_bare_dollar() {
    let p = Project::new(&[("Makefile", "CFLAGS = -O2\nall:\n\tgcc $CFLAGS -o $@\n")]);
    let out = p.run(&["--fix"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 fix applied"));
    assert_eq!(p.read("Makefile"), "CFLAGS = -O2\nall:\n\tgcc $(CFLAGS) -o $@\n");
    // And the rewritten file is clean.
    assert!(!p.stdout(&[]).contains("MK010"));
}

#[test]
fn fix_replaces_space_indentation_with_a_tab() {
    let p = Project::new(&[("Makefile", "all:\n\techo one\n    echo two\n")]);
    p.run(&["--fix"]);
    assert_eq!(p.read("Makefile"), "all:\n\techo one\n\techo two\n");
    assert!(!p.stdout(&[]).contains("MK025"));
}

#[test]
fn without_fix_the_file_is_untouched() {
    let body = "CFLAGS = -O2\nall:\n\tgcc $CFLAGS\n";
    let p = Project::new(&[("Makefile", body)]);
    p.run(&[]);
    assert_eq!(p.read("Makefile"), body);
}

#[test]
fn a_suppressed_finding_is_not_fixed() {
    let body = "CFLAGS = -O2\nall:\n\tgcc $CFLAGS # make-lint: disable=MK010\n";
    let p = Project::new(&[("Makefile", body)]);
    p.run(&["--fix"]);
    assert_eq!(p.read("Makefile"), body, "suppressing a finding also declines its fix");
}

// ---------------------------------------------------------------------------
// Rule metadata
// ---------------------------------------------------------------------------

#[test]
fn list_rules_and_explain_agree_with_the_catalogue() {
    let p = Project::new(&[("Makefile", "all: ; @true\n")]);
    let listed = p.stdout(&["--list-rules"]);
    for r in make_lint::rules::RULES {
        assert!(listed.contains(r.code), "{} missing from --list-rules", r.code);
    }
    let explained = p.stdout(&["--explain", "MK006"]);
    assert!(explained.contains("duplicate-value"));
    assert_eq!(p.run(&["--explain", "MK999"]).status.code(), Some(2));
}

#[test]
fn exit_codes_follow_the_documented_contract() {
    let clean = Project::new(&[("Makefile", "all: ; @true\n")]);
    assert_eq!(clean.run(&[]).status.code(), Some(0));

    let noisy = Project::new(&[("Makefile", NOISY)]);
    assert_eq!(noisy.run(&[]).status.code(), Some(1));
    // A warning does not fail a run that only fails on errors.
    assert_eq!(noisy.run(&["--fail-level", "error"]).status.code(), Some(0));

    let missing = Project::new(&[("Makefile", "all: ; @true\n")]);
    assert_eq!(missing.run(&["-f", "nope.mk"]).status.code(), Some(2));
}
