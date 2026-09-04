//! Running the read-only `$(shell ...)` commands, and refusing everything else.
//!
//! A makefile is not necessarily trustworthy: it may come from a third-party
//! repository or an unreviewed pull request. So the gate is built to be
//! default-deny at every step, and the steps run in this order:
//!
//! 1. The command must already be fully expanded. What cannot be seen cannot be
//!    vetted.
//! 2. It must be *recognised*, not parsed. A deliberately tiny grammar accepts a
//!    pipeline of plain commands and nothing else — no redirection, no
//!    substitution, no `;`, no globbing. Using a real shell parser and then
//!    denying the dangerous parts gets this backwards: anything the recogniser
//!    has not been taught is refused because it was never taught it.
//! 3. The program must be a bare name resolving inside a system bindir, so a
//!    `uname` dropped in the repository cannot be picked up.
//! 4. It must be in the allowlist, and satisfy that entry's argument policy.
//!    Path arguments are confined to the project directory: `cat VERSION` is
//!    the idiom worth supporting, `cat ~/.ssh/id_rsa` is not.
//! 5. Only then is it run: no stdin, no environment, cleared locale, a timeout,
//!    an output cap, and a budget across the whole run.
//!
//! Anything refused leaves the value `Unknown` and is reported by MK040, so a
//! skipped command is visible rather than silently empty.

use crate::funcs;
use crate::value::Stability;

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Directories a program may be resolved from. Deliberately not the caller's
/// `PATH`: a repository must not be able to put itself first.
const SAFE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Characters allowed in an unquoted word. Everything the shell treats
/// specially is absent, so a word can never grow into a second command.
const WORD_CHARS: &str = "_./:@%+,=^-";

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
pub const DEFAULT_MAX_OUTPUT: usize = 64 * 1024;
/// Commands run in one pass over a workspace.
pub const DEFAULT_BUDGET: u32 = 400;
/// Wall clock spent running commands across the whole run.
pub const DEFAULT_TOTAL_TIME: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CommandPolicy {
    pub name: &'static str,
    pub stability: Stability,
    /// Flags that are never accepted, matched exactly or up to `=`.
    pub deny_flags: &'static [&'static str],
    /// When non-empty, the first operand must be one of these.
    pub subcommands: &'static [&'static str],
    /// Leading operands that are not paths: a `grep` pattern, a `sed` script.
    pub operand_offset: usize,
    /// Operands are paths and must stay inside the project directory. False for
    /// commands that never open a file.
    pub confine_paths: bool,
}

const fn cmd(name: &'static str, stability: Stability) -> CommandPolicy {
    CommandPolicy {
        name,
        stability,
        deny_flags: &[],
        subcommands: &[],
        operand_offset: 0,
        // A command declared deterministic does not read the filesystem, so it
        // has no path arguments to confine. `basename /a/b/c` is string
        // manipulation, not a file access.
        confine_paths: !matches!(stability, Deterministic),
    }
}

use Stability::{Deterministic, Filesystem, Volatile};

/// The default allowlist: commands that read, transform, or report, and cannot
/// modify anything. Anything not named here is refused.
///
/// `awk` and `perl` are deliberately absent: both can write files and run
/// commands from inside their program text, which the argument policy cannot
/// see. So are `env`, `xargs`, `sudo`, `sh` and friends, which exist to launch
/// something else.
pub static DEFAULT_COMMANDS: &[CommandPolicy] = &[
    // Pure text, no file access.
    cmd("echo", Deterministic),
    cmd("printf", Deterministic),
    cmd("basename", Deterministic),
    cmd("dirname", Deterministic),
    cmd("expr", Deterministic),
    cmd("seq", Deterministic),
    cmd("true", Deterministic),
    cmd("false", Deterministic),
    cmd("tr", Deterministic),
    cmd("rev", Deterministic),
    cmd("pwd", Deterministic),
    // Reads files or the machine.
    cmd("cut", Filesystem),
    cmd("cat", Filesystem),
    cmd("uniq", Filesystem),
    cmd("wc", Filesystem),
    cmd("head", Filesystem),
    cmd("tail", Filesystem),
    cmd("test", Filesystem),
    cmd("arch", Filesystem),
    cmd("nproc", Filesystem),
    cmd("getconf", Filesystem),
    cmd("id", Filesystem),
    cmd("whoami", Filesystem),
    cmd("uname", Filesystem),
    cmd("realpath", Filesystem),
    cmd("readlink", Filesystem),
    cmd("file", Filesystem),
    cmd("stat", Filesystem),
    cmd("ls", Filesystem),
    CommandPolicy { deny_flags: &["-o", "--output"], ..cmd("sort", Filesystem) },
    // A `grep` pattern is not a path.
    CommandPolicy { operand_offset: 1, ..cmd("grep", Filesystem) },
    CommandPolicy { operand_offset: 1, ..cmd("egrep", Filesystem) },
    CommandPolicy { operand_offset: 1, ..cmd("fgrep", Filesystem) },
    // `sed -i` edits in place, and the `w` command writes wherever it likes.
    CommandPolicy {
        deny_flags: &["-i", "--in-place", "-f", "--file", "-s", "--separate"],
        operand_offset: 1,
        ..cmd("sed", Filesystem)
    },
    CommandPolicy {
        deny_flags: &[
            "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprintf", "-fls",
            "-printf",
        ],
        ..cmd("find", Filesystem)
    },
    // `hostname NAME` sets it; only the bare read is allowed.
    CommandPolicy { deny_flags: &["-b", "--boot", "-F", "--file"], ..cmd("hostname", Filesystem) },
    // Only subcommands that cannot write to the repository or the config.
    // Volatile, not Filesystem: everything git reports about a working tree is
    // checkout state. Two variables holding the same branch name today say
    // nothing about how the makefile is written.
    CommandPolicy {
        deny_flags: &["-C", "--git-dir", "--work-tree", "--exec-path", "-c", "--namespace"],
        subcommands: &[
            "rev-parse",
            "describe",
            "show-ref",
            "symbolic-ref",
            "rev-list",
            "log",
            "ls-files",
            "ls-tree",
            "merge-base",
            "cat-file",
            "diff",
            "show",
        ],
        operand_offset: usize::MAX,
        ..cmd("git", Volatile)
    },
    cmd("date", Volatile),
    cmd("uptime", Volatile),
];

/// Programs whose whole purpose is to run another program. Never allowed, even
/// if a user adds them, because the argument policy cannot see through them.
pub static WRAPPERS: &[&str] = &[
    "env", "xargs", "sudo", "doas", "su", "nice", "ionice", "timeout", "nohup", "setsid", "sh",
    "bash", "dash", "zsh", "ksh", "csh", "chroot", "unshare", "stdbuf", "script", "watch", "flock",
    "time", "strace", "ltrace", "make", "awk", "gawk", "mawk", "perl", "python", "python3", "ruby",
    "node", "eval", "exec", "command", "busybox",
];

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DenyReason {
    Disabled,
    /// The command text still contained something unresolved.
    NotExpanded,
    /// The tiny grammar rejected it; the string says what it tripped on.
    NotRecognised(String),
    NotAllowed(String),
    Wrapper(String),
    DeniedFlag {
        program: String,
        flag: String,
    },
    DeniedSubcommand {
        program: String,
        sub: String,
    },
    EscapesProject(String),
    NotFound(String),
    Timeout,
    Budget,
    Failed(String),
}

impl DenyReason {
    pub fn describe(&self) -> String {
        match self {
            DenyReason::Disabled => "running commands is disabled".into(),
            DenyReason::NotExpanded => "the command text could not be fully resolved".into(),
            DenyReason::NotRecognised(what) => format!("the command uses {what}"),
            DenyReason::NotAllowed(p) => format!("`{p}` is not in the allowlist"),
            DenyReason::Wrapper(p) => format!("`{p}` runs another program, so it is never allowed"),
            DenyReason::DeniedFlag { program, flag } => {
                format!("`{program} {flag}` can modify things")
            }
            DenyReason::DeniedSubcommand { program, sub } => {
                format!("`{program} {sub}` is not one of the read-only subcommands")
            }
            DenyReason::EscapesProject(a) => {
                format!("`{a}` points outside the project directory")
            }
            DenyReason::NotFound(p) => format!("`{p}` was not found in a system directory"),
            DenyReason::Timeout => "the command took too long".into(),
            DenyReason::Budget => "the limit on commands for this run was reached".into(),
            DenyReason::Failed(e) => format!("the command could not be started: {e}"),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Ran { output: String, stability: Stability },
    Refused(DenyReason),
}

// ---------------------------------------------------------------------------
// The recogniser
// ---------------------------------------------------------------------------

/// A pipeline of plain commands, each already split into words.
pub type Pipeline = Vec<Vec<String>>;

/// Accept only the tiny grammar, or say what was found that is not in it.
///
/// Accepted: `word+ ( '|' word+ )*`, where a word is unquoted text drawn from a
/// fixed character set, a single-quoted string, or a double-quoted string with
/// no expansion in it.
pub fn recognise(input: &str) -> Result<Pipeline, DenyReason> {
    let refuse = |what: &str| Err(DenyReason::NotRecognised(what.to_string()));
    let bytes = input.as_bytes();
    let mut pipeline: Pipeline = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' => i += 1,
            b'\n' | b'\r' => return refuse("more than one line"),
            b'|' => {
                // `||` is a second command, not a pipe.
                if bytes.get(i + 1) == Some(&b'|') {
                    return refuse("`||`");
                }
                if current.is_empty() {
                    return refuse("an empty pipeline stage");
                }
                pipeline.push(std::mem::take(&mut current));
                i += 1;
            }
            b'\'' => {
                let end = match input[i + 1..].find('\'') {
                    Some(e) => i + 1 + e,
                    None => return refuse("an unterminated quote"),
                };
                current.push(input[i + 1..end].to_string());
                i = end + 1;
            }
            b'"' => {
                let end = match input[i + 1..].find('"') {
                    Some(e) => i + 1 + e,
                    None => return refuse("an unterminated quote"),
                };
                let body = &input[i + 1..end];
                // Inside double quotes the shell still expands these.
                if body.contains(['$', '`', '\\']) {
                    return refuse("expansion inside double quotes");
                }
                current.push(body.to_string());
                i = end + 1;
            }
            b'$' => return refuse("`$`, which the shell would expand"),
            b'`' => return refuse("a backquote"),
            b'>' | b'<' => return refuse("redirection"),
            b';' => return refuse("`;`"),
            b'&' => return refuse("`&`"),
            b'(' | b')' => return refuse("a subshell"),
            b'{' | b'}' => return refuse("brace expansion"),
            b'\\' => return refuse("a backslash escape"),
            b'*' | b'?' | b'[' | b']' => return refuse("a glob"),
            b'~' => return refuse("`~`, which the shell would expand"),
            b'#' => return refuse("`#`"),
            b'!' => return refuse("`!`"),
            _ => {
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i] as char;
                    if c.is_ascii_alphanumeric() || WORD_CHARS.contains(c) {
                        i += 1;
                    } else {
                        break;
                    }
                }
                if i == start {
                    return Err(DenyReason::NotRecognised(format!(
                        "the character `{}`",
                        input[start..].chars().next().unwrap_or('?')
                    )));
                }
                current.push(input[start..i].to_string());
            }
        }
    }

    if !current.is_empty() {
        pipeline.push(current);
    }
    if pipeline.is_empty() || pipeline.iter().any(Vec::is_empty) {
        return refuse("no command");
    }
    // `FOO=bar cmd` sets the environment for the command.
    for stage in &pipeline {
        let head = &stage[0];
        if head.contains('=') {
            return refuse("an environment assignment");
        }
    }
    Ok(pipeline)
}

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Refuse everything; the value stays unknown.
    Deny,
    /// Run what the allowlist permits.
    Allowlist,
}

pub struct Oracle {
    pub mode: Mode,
    base_dir: PathBuf,
    policies: HashMap<String, CommandPolicy>,
    cache: HashMap<String, Outcome>,
    budget: u32,
    started: Instant,
    total_time: Duration,
    timeout: Duration,
    max_output: usize,
}

impl Oracle {
    pub fn new(mode: Mode, base_dir: PathBuf, extra_allowed: &[String]) -> Oracle {
        let mut policies: HashMap<String, CommandPolicy> =
            DEFAULT_COMMANDS.iter().map(|p| (p.name.to_string(), p.clone())).collect();
        for name in extra_allowed {
            // A user-added command still gets path confinement and can never be
            // a wrapper.
            if WRAPPERS.contains(&name.as_str()) {
                continue;
            }
            let leaked: &'static str = Box::leak(name.clone().into_boxed_str());
            policies.insert(name.clone(), cmd(leaked, Filesystem));
        }
        Oracle {
            mode,
            base_dir,
            policies,
            cache: HashMap::new(),
            budget: DEFAULT_BUDGET,
            started: Instant::now(),
            total_time: DEFAULT_TOTAL_TIME,
            timeout: DEFAULT_TIMEOUT,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }

    /// Vet and run one command string, memoised.
    pub fn run(&mut self, command: &str) -> Outcome {
        if self.mode == Mode::Deny {
            return Outcome::Refused(DenyReason::Disabled);
        }
        if let Some(hit) = self.cache.get(command) {
            return hit.clone();
        }
        let outcome = self.vet_and_run(command);
        self.cache.insert(command.to_string(), outcome.clone());
        outcome
    }

    fn vet_and_run(&mut self, command: &str) -> Outcome {
        let pipeline = match recognise(command) {
            Ok(p) => p,
            Err(e) => return Outcome::Refused(e),
        };

        let mut programs = Vec::with_capacity(pipeline.len());
        let mut stability = Deterministic;
        for stage in &pipeline {
            let policy = match self.vet_stage(stage) {
                Ok(p) => p,
                Err(e) => return Outcome::Refused(e),
            };
            stability = stability.max(policy.stability);
            match resolve_program(&stage[0]) {
                Some(path) => programs.push(path),
                None => return Outcome::Refused(DenyReason::NotFound(stage[0].clone())),
            }
        }

        if self.budget == 0 || self.started.elapsed() > self.total_time {
            return Outcome::Refused(DenyReason::Budget);
        }
        self.budget -= 1;

        match self.spawn(&pipeline, &programs) {
            Ok(output) => Outcome::Ran { output, stability },
            Err(e) => Outcome::Refused(e),
        }
    }

    fn vet_stage(&self, argv: &[String]) -> Result<&CommandPolicy, DenyReason> {
        let program = &argv[0];
        if program.contains('/') {
            return Err(DenyReason::NotRecognised("a program path rather than a bare name".into()));
        }
        if WRAPPERS.contains(&program.as_str()) {
            return Err(DenyReason::Wrapper(program.clone()));
        }
        let policy =
            self.policies.get(program).ok_or_else(|| DenyReason::NotAllowed(program.clone()))?;

        let mut operand = 0usize;
        for arg in &argv[1..] {
            if arg.starts_with('-') && arg.len() > 1 {
                let bare = arg.split('=').next().unwrap_or(arg);
                if policy.deny_flags.iter().any(|d| *d == bare || *d == arg.as_str()) {
                    return Err(DenyReason::DeniedFlag {
                        program: program.clone(),
                        flag: bare.to_string(),
                    });
                }
                continue;
            }
            if operand == 0 && !policy.subcommands.is_empty() {
                if !policy.subcommands.contains(&arg.as_str()) {
                    return Err(DenyReason::DeniedSubcommand {
                        program: program.clone(),
                        sub: arg.clone(),
                    });
                }
                operand += 1;
                continue;
            }
            let is_path = operand >= policy.operand_offset;
            if policy.confine_paths && is_path && !confined(&self.base_dir, arg) {
                return Err(DenyReason::EscapesProject(arg.clone()));
            }
            operand += 1;
        }
        Ok(policy)
    }

    fn spawn(&self, pipeline: &Pipeline, programs: &[PathBuf]) -> Result<String, DenyReason> {
        let mut children: Vec<Child> = Vec::with_capacity(pipeline.len());
        let mut upstream: Option<std::process::ChildStdout> = None;

        for (i, argv) in pipeline.iter().enumerate() {
            let mut c = Command::new(&programs[i]);
            c.args(&argv[1..])
                .current_dir(&self.base_dir)
                // No inherited environment: nothing of the caller's leaks in,
                // and the result does not depend on where it was run from.
                .env_clear()
                .env("PATH", SAFE_PATH)
                .env("LC_ALL", "C")
                .env("LANG", "C")
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            match upstream.take() {
                Some(prev) => c.stdin(Stdio::from(prev)),
                None => c.stdin(Stdio::null()),
            };
            let mut child = match c.spawn() {
                Ok(ch) => ch,
                Err(e) => {
                    for ch in &mut children {
                        let _ = ch.kill();
                    }
                    return Err(DenyReason::Failed(e.to_string()));
                }
            };
            upstream = child.stdout.take();
            children.push(child);
        }

        // Read on another thread so a child filling the pipe cannot deadlock the
        // wait loop, and stop at the cap.
        let cap = self.max_output as u64;
        let reader = upstream.expect("last stage has piped stdout");
        let handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = reader.take(cap).read_to_end(&mut buf);
            buf
        });

        let start = Instant::now();
        loop {
            let done = children.iter_mut().all(|c| matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
            if done {
                break;
            }
            if start.elapsed() > self.timeout {
                for c in &mut children {
                    let _ = c.kill();
                }
                let _ = handle.join();
                return Err(DenyReason::Timeout);
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        let buf = handle.join().unwrap_or_default();
        // A non-zero exit is not an error: make uses whatever was written.
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// Resolve a bare program name against the fixed system directories.
fn resolve_program(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return None;
    }
    SAFE_PATH.split(':').map(|d| Path::new(d).join(name)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// True when `arg`, read as a path relative to the project, stays inside it.
fn confined(base: &Path, arg: &str) -> bool {
    if arg.starts_with('/') || arg.starts_with('~') {
        return false;
    }
    funcs::normalise(&base.join(arg)).starts_with(base)
}

/// make's own treatment of command output: drop trailing newlines, then turn
/// every remaining newline into a space.
pub fn shell_output(raw: &str) -> String {
    raw.trim_end_matches('\n').replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Pipeline {
        recognise(s).unwrap_or_else(|e| panic!("{s:?} refused: {}", e.describe()))
    }

    fn refused(s: &str) -> String {
        match recognise(s) {
            Ok(p) => panic!("{s:?} was accepted as {p:?}"),
            Err(e) => e.describe(),
        }
    }

    #[test]
    fn plain_commands_are_recognised() {
        assert_eq!(ok("uname -r"), vec![vec!["uname", "-r"]]);
        assert_eq!(ok("  git   describe --tags  "), vec![vec!["git", "describe", "--tags"]]);
        assert_eq!(ok("cat VERSION"), vec![vec!["cat", "VERSION"]]);
    }

    #[test]
    fn pipelines_are_recognised() {
        assert_eq!(
            ok("uname -s | tr A-Z a-z"),
            vec![vec!["uname", "-s"], vec!["tr", "A-Z", "a-z"]]
        );
    }

    #[test]
    fn quoting_is_recognised() {
        assert_eq!(ok("grep -E 'a b' f"), vec![vec!["grep", "-E", "a b", "f"]]);
        assert_eq!(ok("echo \"hello world\""), vec![vec!["echo", "hello world"]]);
    }

    // Everything below would let a command become a different command.
    #[test]
    fn redirection_is_refused() {
        assert!(refused("uname -r > /etc/passwd").contains("redirection"));
        assert!(refused("uname >> f").contains("redirection"));
        assert!(refused("cat < f").contains("redirection"));
    }

    #[test]
    fn command_separators_are_refused() {
        assert!(refused("uname; rm -rf /").contains("`;`"));
        assert!(refused("uname && rm -rf /").contains("`&`"));
        assert!(refused("uname || true").contains("`||`"));
        assert!(refused("uname &").contains("`&`"));
    }

    #[test]
    fn substitution_is_refused() {
        assert!(refused("echo $(rm -rf /)").contains("`$`"));
        assert!(refused("echo `rm -rf /`").contains("backquote"));
        assert!(refused("echo $HOME").contains("`$`"));
        assert!(refused("echo \"$(id)\"").contains("expansion inside double quotes"));
        assert!(refused("echo \"a\\`id\\`\"").contains("expansion inside double quotes"));
    }

    #[test]
    fn shell_syntax_that_could_hide_a_command_is_refused() {
        assert!(refused("(rm -rf /)").contains("subshell"));
        assert!(refused("echo {a,b}").contains("brace"));
        assert!(refused("echo a\\;b").contains("backslash"));
        assert!(refused("cat ~/.ssh/id_rsa").contains("`~`"));
        assert!(refused("echo a # b").contains("`#`"));
        assert!(refused("uname -r\nrm -rf /").contains("more than one line"));
        assert!(refused("FOO=bar uname").contains("environment assignment"));
        assert!(refused("cat *.c").contains("glob"));
        assert!(refused("uname '").contains("unterminated quote"));
    }

    #[test]
    fn make_output_processing_matches_make() {
        assert_eq!(shell_output("a\n"), "a");
        assert_eq!(shell_output("a\nb\n"), "a b");
        assert_eq!(shell_output("a\n\n"), "a");
        assert_eq!(shell_output("a\nb"), "a b");
        assert_eq!(shell_output(""), "");
    }

    #[test]
    fn confinement_keeps_paths_in_the_project() {
        let base = Path::new("/proj");
        assert!(confined(base, "VERSION"));
        assert!(confined(base, "src/x.c"));
        assert!(confined(base, "./a/../b"));
        assert!(!confined(base, "/etc/passwd"));
        assert!(!confined(base, "../outside"));
        assert!(!confined(base, "a/../../outside"));
        assert!(!confined(base, "~/.ssh/id_rsa"));
    }

    fn oracle() -> Oracle {
        Oracle::new(Mode::Allowlist, std::env::temp_dir(), &[])
    }

    #[test]
    fn wrappers_are_refused_even_with_safe_arguments() {
        for c in ["env uname", "xargs uname", "sh -c uname", "sudo uname", "timeout 1 uname"] {
            let o = oracle().run(c);
            assert!(matches!(o, Outcome::Refused(DenyReason::Wrapper(_))), "{c} gave {o:?}");
        }
    }

    #[test]
    fn commands_outside_the_allowlist_are_refused() {
        for c in ["curl https:", "docker ps", "rm -rf x", "awk BEGIN"] {
            assert!(matches!(oracle().run(c), Outcome::Refused(_)), "{c} was not refused");
        }
    }

    #[test]
    fn dangerous_flags_of_allowed_commands_are_refused() {
        let cases = [
            "find . -delete",
            "find . -exec rm {} +",
            "sed -i s/a/b/ f",
            "sort -o out f",
            "hostname -F f",
            "git -C /elsewhere rev-parse HEAD",
        ];
        for c in cases {
            let o = oracle().run(c);
            assert!(matches!(o, Outcome::Refused(_)), "{c} gave {o:?}");
        }
    }

    #[test]
    fn git_is_limited_to_read_only_subcommands() {
        assert!(matches!(
            oracle().run("git push origin main"),
            Outcome::Refused(DenyReason::DeniedSubcommand { .. })
        ));
        assert!(matches!(
            oracle().run("git tag v1"),
            Outcome::Refused(DenyReason::DeniedSubcommand { .. })
        ));
    }

    #[test]
    fn a_program_path_is_refused() {
        assert!(matches!(oracle().run("./uname"), Outcome::Refused(_)));
        assert!(matches!(oracle().run("/usr/bin/uname"), Outcome::Refused(_)));
    }

    #[test]
    fn allowed_commands_actually_run() {
        let Outcome::Ran { output, .. } = oracle().run("echo hello") else {
            panic!("echo did not run");
        };
        assert_eq!(shell_output(&output), "hello");

        let Outcome::Ran { output, .. } = oracle().run("echo one two | tr a-z A-Z") else {
            panic!("pipeline did not run");
        };
        assert_eq!(shell_output(&output), "ONE TWO");
    }

    // Everything git reports about a working tree is checkout state, so two
    // variables agreeing today is not evidence about the makefile.
    #[test]
    fn git_output_is_volatile() {
        let p = DEFAULT_COMMANDS.iter().find(|p| p.name == "git").unwrap();
        assert_eq!(p.stability, Volatile);
    }

    #[test]
    fn pure_string_commands_are_not_path_confined() {
        for name in ["basename", "dirname", "echo", "printf"] {
            let p = DEFAULT_COMMANDS.iter().find(|p| p.name == name).unwrap();
            assert!(!p.confine_paths, "{name} takes strings, not paths");
        }
        for name in ["cat", "head", "grep", "find"] {
            let p = DEFAULT_COMMANDS.iter().find(|p| p.name == name).unwrap();
            assert!(p.confine_paths, "{name} opens files");
        }
    }

    #[test]
    fn basename_takes_a_string_not_a_path() {
        let Outcome::Ran { output, .. } = oracle().run("basename /a/b/c.txt") else {
            panic!("basename was refused");
        };
        assert_eq!(shell_output(&output), "c.txt");
    }

    #[test]
    fn stability_is_the_worst_stage_in_the_pipeline() {
        let Outcome::Ran { stability, .. } = oracle().run("echo x") else { panic!() };
        assert_eq!(stability, Deterministic);
        let Outcome::Ran { stability, .. } = oracle().run("date | cat") else { panic!() };
        assert_eq!(stability, Volatile);
    }

    #[test]
    fn results_are_memoised() {
        let mut o = oracle();
        let first = o.run("echo memo");
        let second = o.run("echo memo");
        assert!(matches!((first, second), (Outcome::Ran { .. }, Outcome::Ran { .. })));
        // One run consumed exactly one unit of budget.
        assert_eq!(o.budget, DEFAULT_BUDGET - 1);
    }

    #[test]
    fn deny_mode_runs_nothing() {
        let mut o = Oracle::new(Mode::Deny, std::env::temp_dir(), &[]);
        assert!(matches!(o.run("echo hi"), Outcome::Refused(DenyReason::Disabled)));
    }

    #[test]
    fn a_user_added_command_still_cannot_be_a_wrapper() {
        let mut o = Oracle::new(Mode::Allowlist, std::env::temp_dir(), &["sh".to_string()]);
        assert!(matches!(o.run("sh -c id"), Outcome::Refused(DenyReason::Wrapper(_))));
    }
}
