use make_lint::diag::Severity;
use make_lint::workspace::Workspace;
use make_lint::{DEFAULT_MAKEFILES, checks, render};

use std::io::IsTerminal;
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
        --format <FORMAT>    text (default) or json
        --fail-level <LEVEL> note, warning (default), or error
        --color <WHEN>       auto (default), always, or never
        --show-notes         Include note-level diagnostics in the output
    -h, --help               Print this help
    -V, --version            Print version

EXIT CODES:
    0  no diagnostics at or above --fail-level
    1  diagnostics found
    2  could not read a makefile, or bad arguments
";

#[derive(PartialEq)]
enum Format {
    Text,
    Json,
}

struct Args {
    files: Vec<PathBuf>,
    include_dirs: Vec<PathBuf>,
    format: Format,
    fail_level: Severity,
    color: Option<bool>,
    show_notes: bool,
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("make-lint: {e}");
            eprintln!("try `make-lint --help`");
            return ExitCode::from(2);
        }
    };

    let mut ws = Workspace::new(args.include_dirs);
    let mut failed = false;
    for f in &args.files {
        if let Err(e) = ws.load_root(f) {
            eprintln!("make-lint: cannot read {}: {e}", f.display());
            failed = true;
        }
    }
    if failed {
        return ExitCode::from(2);
    }

    let mut diags = std::mem::take(&mut ws.diags);
    diags.extend(checks::run(&ws));
    if !args.show_notes {
        diags.retain(|d| d.severity > Severity::Note);
    }
    render::sort(&mut diags, &ws.sources);

    match args.format {
        Format::Json => print!("{}", render::json(&diags, &ws.sources)),
        Format::Text => {
            let color = args.color.unwrap_or_else(|| std::io::stdout().is_terminal());
            print!("{}", render::text(&diags, &ws.sources, &render::Style { color }));
            let n = diags.len();
            if n > 0 {
                eprintln!("{n} diagnostic{}", if n == 1 { "" } else { "s" });
            }
        }
    }

    if diags.iter().any(|d| d.severity >= args.fail_level) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args {
        files: Vec::new(),
        include_dirs: Vec::new(),
        format: Format::Text,
        fail_level: Severity::Warning,
        color: None,
        show_notes: false,
    };
    let mut it = std::env::args().skip(1);
    let mut positional = Vec::new();

    while let Some(arg) = it.next() {
        let mut next = |flag: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{flag} needs a value"))
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
            "-f" | "--file" => a.files.push(PathBuf::from(next("--file")?)),
            "-I" | "--include-dir" => a.include_dirs.push(PathBuf::from(next("--include-dir")?)),
            "--show-notes" => a.show_notes = true,
            "--format" => {
                a.format = match next("--format")?.as_str() {
                    "text" => Format::Text,
                    "json" => Format::Json,
                    other => return Err(format!("unknown format `{other}`")),
                }
            }
            "--fail-level" => {
                let v = next("--fail-level")?;
                a.fail_level = Severity::parse(&v).ok_or_else(|| format!("unknown level `{v}`"))?;
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
                return Err(format!("unknown option `{s}`"));
            }
            s => positional.push(PathBuf::from(s)),
        }
    }

    a.files.extend(positional);
    if a.files.is_empty() {
        let found = DEFAULT_MAKEFILES
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .ok_or("no makefile found in the current directory")?;
        a.files.push(found);
    }
    // `--show-notes` only makes sense if notes can still fail the run.
    if a.fail_level == Severity::Note {
        a.show_notes = true;
    }
    Ok(Some(a))
}
