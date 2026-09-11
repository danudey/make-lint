use make_lint::config::Config;
use make_lint::diag::Severity;
use make_lint::suppress::Suppressions;
use make_lint::workspace::Workspace;
use make_lint::{DEFAULT_MAKEFILES, checks, eval, fix, render, rules, shell};

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
make-lint — a static linter for GNU Makefiles

USAGE:
    make-lint [OPTIONS] [FILE...]

With no FILE, looks for GNUmakefile, makefile, then Makefile in the current
directory. Statically resolvable `include` directives are followed.

OPTIONS:
    -f, --file <FILE>        Makefile to lint (repeatable)
    -I, --include-dir <DIR>  Extra directory to search for includes (repeatable)
        --stdin-path <PATH>  Lint stdin as the file that would be saved at
                             PATH. The file need not exist; PATH fixes where
                             its includes and config are looked for. For
                             editors linting a buffer with unsaved edits.
        --format <FORMAT>    text (default), json, or sarif
        --fail-level <LEVEL> note, warning (default), or error
        --color <WHEN>       auto (default), always, or never
        --show-notes         Include note-level diagnostics in the output
        --fix                Apply the fixes that have one obvious answer
        --no-exec            Never run a $(shell ...) command
        --allow-command <C>  Also run this command when it appears in
                             $(shell ...) (repeatable)
        --config <FILE>      Read this config instead of searching for one
        --no-config          Ignore any .make-lint.toml
        --explain <CODE>     Describe one rule and exit
        --list-rules         List every rule and exit
    -h, --help               Print this help
    -V, --version            Print version

SUPPRESSING A FINDING:
    A `# make-lint: disable=MK006` comment at the end of a line covers that
    line; on a line of its own it covers the next. `disable-file` covers the
    whole file, and omitting `=CODE` covers every rule.

RUNNING COMMANDS:
    By default make-lint runs the $(shell ...) commands it can prove are
    read-only: an allowlist of plain commands with no redirection, no
    substitution, and path arguments confined to the project directory.
    Everything else is left unresolved and reported as MK040.

EXIT CODES:
    0  no diagnostics at or above --fail-level
    1  diagnostics found
    2  could not read a makefile, or bad arguments
";

#[derive(PartialEq)]
enum Format {
    Text,
    Json,
    Sarif,
}

struct Args {
    files: Vec<PathBuf>,
    include_dirs: Vec<PathBuf>,
    format: Format,
    fail_level: Severity,
    color: Option<bool>,
    show_notes: bool,
    apply_fixes: bool,
    exec: Option<shell::Mode>,
    allow_commands: Vec<String>,
    config_path: Option<PathBuf>,
    no_config: bool,
    /// Lint stdin as if it were the file saved at this path.
    stdin_path: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("make-lint: {e}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let Some(mut args) = parse_args()? else { return Ok(ExitCode::SUCCESS) };

    // The config is looked for next to the makefile, so linting a subdirectory
    // still picks up the project's settings.
    let anchor = args
        .stdin_path
        .as_ref()
        .or_else(|| args.files.first())
        .and_then(|f| f.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let config = match (&args.config_path, args.no_config) {
        (_, true) => Config::default(),
        (Some(p), _) => Config::load(p)?,
        (None, _) => Config::discover(&anchor)?,
    };
    apply_config(&mut args, &config);

    let mut ws = Workspace::new(args.include_dirs.clone());
    if let Some(p) = &args.stdin_path {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        ws.load_root_text(p, text);
    } else {
        for f in &args.files {
            ws.load_root(f).map_err(|e| format!("cannot read {}: {e}", f.display()))?;
        }
    }

    let opts = eval::Options {
        exec: args.exec.unwrap_or(shell::Mode::Allowlist),
        allow_commands: args.allow_commands.clone(),
    };
    let mut diags = std::mem::take(&mut ws.diags);
    diags.extend(checks::run_with(&ws, &opts));

    // Rules switched off entirely, then severities forced.
    diags.retain(|d| !config.disable.iter().any(|c| c == d.code));
    for d in &mut diags {
        if let Some(&s) = config.severity.get(d.code) {
            d.severity = s;
        }
    }

    let suppressions = Suppressions::scan(&ws.sources);
    let suppressed = suppressions.apply(&mut diags, &ws.sources);
    suppressions.report_unknown(&ws.sources, &mut diags);

    let applied = if args.apply_fixes { Some(fix::apply(&diags, &ws.sources)?) } else { None };

    if !args.show_notes {
        diags.retain(|d| d.severity > Severity::Note);
    }
    render::sort(&mut diags, &ws.sources);

    match args.format {
        Format::Json => print!("{}", render::json(&diags, &ws.sources)),
        Format::Sarif => print!("{}", render::sarif(&diags, &ws.sources, &ws.base_dir())),
        Format::Text => {
            let color = args.color.unwrap_or_else(|| std::io::stdout().is_terminal());
            print!("{}", render::text(&diags, &ws.sources, &render::Style { color }));
            summarise(diags.len(), suppressed, applied.as_ref());
        }
    }

    Ok(if diags.iter().any(|d| d.severity >= args.fail_level) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn summarise(shown: usize, suppressed: usize, applied: Option<&fix::Applied>) {
    let mut parts = Vec::new();
    if shown > 0 {
        parts.push(format!("{shown} diagnostic{}", plural(shown)));
    }
    if suppressed > 0 {
        parts.push(format!("{suppressed} suppressed"));
    }
    if let Some(a) = applied {
        parts.push(format!(
            "{} fix{} applied in {} file{}",
            a.fixes,
            if a.fixes == 1 { "" } else { "es" },
            a.files,
            plural(a.files)
        ));
        if a.skipped > 0 {
            parts.push(format!("{} skipped as overlapping", a.skipped));
        }
    }
    if !parts.is_empty() {
        eprintln!("{}", parts.join(", "));
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The command line wins wherever it said something.
fn apply_config(args: &mut Args, config: &Config) {
    if args.exec.is_none()
        && let Some(enabled) = config.exec
    {
        args.exec = Some(if enabled { shell::Mode::Allowlist } else { shell::Mode::Deny });
    }
    args.allow_commands.extend(config.allow_commands.iter().cloned());
    args.include_dirs.extend(config.include_dirs.iter().cloned());
    if let Some(l) = config.fail_level {
        args.fail_level = l;
    }
    if let Some(n) = config.show_notes {
        args.show_notes |= n;
    }
    if args.fail_level == Severity::Note {
        args.show_notes = true;
    }
}

fn list_rules() {
    for r in rules::RULES {
        println!("{}  {:<7}  {:<26}  {}", r.code, r.severity.as_str(), r.name, r.summary);
    }
}

fn explain(key: &str) -> Result<(), String> {
    let r = rules::lookup(key).ok_or_else(|| format!("`{key}` is not a rule; try --list-rules"))?;
    println!("{}  {}  [{}]\n", r.code, r.name, r.severity.as_str());
    println!("{}\n", r.summary);
    println!("{}", r.explanation);
    Ok(())
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args {
        files: Vec::new(),
        include_dirs: Vec::new(),
        format: Format::Text,
        fail_level: Severity::Warning,
        color: None,
        show_notes: false,
        apply_fixes: false,
        exec: None,
        allow_commands: Vec::new(),
        config_path: None,
        no_config: false,
        stdin_path: None,
    };
    let mut it = std::env::args().skip(1);
    let mut positional = Vec::new();

    while let Some(arg) = it.next() {
        let mut next = |flag: &str| -> Result<String, String> {
            it.next().ok_or(format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("make-lint {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--list-rules" => {
                list_rules();
                return Ok(None);
            }
            "--explain" => {
                explain(&next("--explain")?)?;
                return Ok(None);
            }
            "-f" | "--file" => a.files.push(PathBuf::from(next("--file")?)),
            "-I" | "--include-dir" => a.include_dirs.push(PathBuf::from(next("--include-dir")?)),
            "--show-notes" => a.show_notes = true,
            "--fix" => a.apply_fixes = true,
            "--no-exec" => a.exec = Some(shell::Mode::Deny),
            "--allow-command" => a.allow_commands.push(next("--allow-command")?),
            "--stdin-path" => a.stdin_path = Some(PathBuf::from(next("--stdin-path")?)),
            "--config" => a.config_path = Some(PathBuf::from(next("--config")?)),
            "--no-config" => a.no_config = true,
            "--format" => {
                a.format = match next("--format")?.as_str() {
                    "text" => Format::Text,
                    "json" => Format::Json,
                    "sarif" => Format::Sarif,
                    other => return Err(format!("unknown format `{other}`")),
                }
            }
            "--fail-level" => {
                let v = next("--fail-level")?;
                a.fail_level = Severity::parse(&v).ok_or(format!("unknown level `{v}`"))?;
            }
            "--color" => {
                a.color = match next("--color")?.as_str() {
                    "auto" => None,
                    "always" => Some(true),
                    "never" => Some(false),
                    other => return Err(format!("unknown color mode `{other}`")),
                }
            }
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unknown option `{s}`; try --help"));
            }
            s => positional.push(PathBuf::from(s)),
        }
    }

    a.files.extend(positional);
    if a.stdin_path.is_some() {
        if !a.files.is_empty() {
            return Err(
                "--stdin-path already says which file stdin is; do not also name one".into()
            );
        }
        if a.apply_fixes {
            // The file on disk is not what was linted, so writing to it would
            // clobber whatever the editor has not saved yet.
            return Err("--fix cannot write a file whose text came from stdin; \
                 read the `fix` ranges from --format json and apply them instead"
                .into());
        }
    } else if a.files.is_empty() {
        let found = DEFAULT_MAKEFILES
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .ok_or("no makefile found in the current directory")?;
        a.files.push(found);
    }
    if a.fail_level == Severity::Note {
        a.show_notes = true;
    }
    Ok(Some(a))
}
