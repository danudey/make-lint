//! `.make-lint.toml`.
//!
//! A deliberately small TOML subset — strings, booleans, arrays of strings, and
//! one level of table — parsed here rather than pulled in as a dependency. A
//! linter that runs commands should be able to justify every line of code it
//! ships, and the config surface is not big enough to be worth a crate.
//!
//! Unknown keys and unknown rule codes are errors, not silent no-ops.

use crate::diag::Severity;
use crate::rules;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const CONFIG_NAME: &str = ".make-lint.toml";

#[derive(Default, Debug)]
pub struct Config {
    /// Rules to switch off entirely.
    pub disable: Vec<String>,
    /// Forced severities, by code.
    pub severity: BTreeMap<String, Severity>,
    /// Whether `$(shell ...)` commands may run.
    pub exec: Option<bool>,
    pub allow_commands: Vec<String>,
    pub fail_level: Option<Severity>,
    pub show_notes: Option<bool>,
    pub include_dirs: Vec<PathBuf>,
    /// Where it was read from, for error messages.
    pub path: Option<PathBuf>,
}

impl Config {
    /// Look for a config beside the makefile, then in each parent directory.
    pub fn discover(start: &Path) -> Result<Config, String> {
        for dir in start.ancestors() {
            let candidate = dir.join(CONFIG_NAME);
            if candidate.is_file() {
                return Config::load(&candidate);
            }
        }
        Ok(Config::default())
    }

    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut cfg = Config::from_toml(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.path = Some(path.to_path_buf());
        Ok(cfg)
    }

    pub fn from_toml(text: &str) -> Result<Config, String> {
        let entries = parse(text)?;
        let mut cfg = Config::default();

        for (key, line, value) in entries {
            let at = |m: String| format!("line {line}: {m}");
            match key.as_str() {
                "disable" => {
                    for k in value.array().map_err(at)? {
                        let r =
                            rules::lookup(&k).ok_or_else(|| at(format!("`{k}` is not a rule")))?;
                        cfg.disable.push(r.code.to_string());
                    }
                }
                "fail-level" => {
                    let s = value.string().map_err(at)?;
                    cfg.fail_level = Some(
                        Severity::parse(&s).ok_or_else(|| at(format!("unknown level `{s}`")))?,
                    );
                }
                "show-notes" => cfg.show_notes = Some(value.boolean().map_err(at)?),
                "include-dirs" => {
                    cfg.include_dirs =
                        value.array().map_err(at)?.into_iter().map(PathBuf::from).collect();
                }
                "exec.enabled" => cfg.exec = Some(value.boolean().map_err(at)?),
                "exec.allow" => cfg.allow_commands = value.array().map_err(at)?,
                _ => match key.strip_prefix("severity.") {
                    Some(code) => {
                        let r = rules::lookup(code)
                            .ok_or_else(|| at(format!("`{code}` is not a rule")))?;
                        let s = value.string().map_err(at)?;
                        let sev = Severity::parse(&s)
                            .ok_or_else(|| at(format!("unknown level `{s}`")))?;
                        cfg.severity.insert(r.code.to_string(), sev);
                    }
                    None => return Err(at(format!("unknown key `{key}`"))),
                },
            }
        }
        Ok(cfg)
    }
}

// ---------------------------------------------------------------------------
// The subset parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Str(String),
    Bool(bool),
    Arr(Vec<String>),
}

impl Value {
    fn string(&self) -> Result<String, String> {
        match self {
            Value::Str(s) => Ok(s.clone()),
            other => Err(format!("expected a string, found {}", other.kind())),
        }
    }
    fn boolean(&self) -> Result<bool, String> {
        match self {
            Value::Bool(b) => Ok(*b),
            other => Err(format!("expected true or false, found {}", other.kind())),
        }
    }
    fn array(&self) -> Result<Vec<String>, String> {
        match self {
            Value::Arr(a) => Ok(a.clone()),
            other => Err(format!("expected an array of strings, found {}", other.kind())),
        }
    }
    fn kind(&self) -> &'static str {
        match self {
            Value::Str(_) => "a string",
            Value::Bool(_) => "a boolean",
            Value::Arr(_) => "an array",
        }
    }
}

struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn line(&self) -> usize {
        self.text[..self.pos].bytes().filter(|&b| b == b'\n').count() + 1
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    /// Whitespace, newlines and comments all separate tokens.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.pos += 1,
                Some(b'#') => {
                    while !matches!(self.peek(), None | Some(b'\n')) {
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn bare_key(&mut self) -> Result<String, String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if (c as char).is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if start == self.pos {
            return Err(format!("line {}: expected a key", self.line()));
        }
        Ok(self.text[start..self.pos].to_string())
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        self.skip_trivia();
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("line {}: expected `{}`", self.line(), c as char))
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let quote = self.peek().ok_or("unexpected end of file")?;
        if quote != b'"' && quote != b'\'' {
            return Err(format!("line {}: expected a quoted string", self.line()));
        }
        self.pos += 1;
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == quote {
                let s = self.text[start..self.pos].to_string();
                self.pos += 1;
                return Ok(s);
            }
            if c == b'\n' {
                break;
            }
            // No escapes in this subset; a backslash is a plain character.
            self.pos += 1;
        }
        Err(format!("line {}: unterminated string", self.line()))
    }

    fn value(&mut self) -> Result<Value, String> {
        self.skip_trivia();
        match self.peek() {
            Some(b'"') | Some(b'\'') => Ok(Value::Str(self.string()?)),
            Some(b'[') => {
                // Report an unterminated array where it opened, not at the end
                // of the file.
                let opened = self.line();
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_trivia();
                    if self.peek() == Some(b']') {
                        self.pos += 1;
                        return Ok(Value::Arr(items));
                    }
                    items.push(self.string()?);
                    self.skip_trivia();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {}
                        None => {
                            return Err(format!("line {opened}: unterminated array"));
                        }
                        _ => return Err(format!("line {}: expected `,` or `]`", self.line())),
                    }
                }
            }
            _ => {
                if self.text[self.pos..].starts_with("true") {
                    self.pos += 4;
                    Ok(Value::Bool(true))
                } else if self.text[self.pos..].starts_with("false") {
                    self.pos += 5;
                    Ok(Value::Bool(false))
                } else {
                    Err(format!("line {}: expected a string, a boolean, or an array", self.line()))
                }
            }
        }
    }
}

/// A key, the line it was written on, and its value.
type Entry = (String, usize, Value);

/// Flatten to dotted keys: `[severity]` + `MK006 = "note"` becomes
/// `severity.MK006`.
fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut c = Cursor { text, pos: 0 };
    let mut table = String::new();
    let mut out = Vec::new();

    loop {
        c.skip_trivia();
        if c.peek().is_none() {
            return Ok(out);
        }
        if c.peek() == Some(b'[') {
            c.pos += 1;
            table = c.bare_key()?;
            c.expect(b']')?;
            continue;
        }
        let line = c.line();
        let key = c.bare_key()?;
        c.expect(b'=')?;
        let value = c.value()?;
        let full = if table.is_empty() { key } else { format!("{table}.{key}") };
        out.push((full, line, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_config_parses() {
        let cfg = Config::from_toml(
            r#"
# what to report
disable = ["MK002", "unused-variable"]
fail-level = "error"
show-notes = true
include-dirs = ["mk", "build"]

[severity]
MK006 = "note"
MK021 = "error"

[exec]
enabled = false
allow = [
  "cksum",
  "go",
]
"#,
        )
        .unwrap();
        assert_eq!(cfg.disable, vec!["MK002", "MK002"]);
        assert_eq!(cfg.fail_level, Some(Severity::Error));
        assert_eq!(cfg.show_notes, Some(true));
        assert_eq!(cfg.include_dirs.len(), 2);
        assert_eq!(cfg.severity.get("MK006"), Some(&Severity::Note));
        assert_eq!(cfg.exec, Some(false));
        assert_eq!(cfg.allow_commands, vec!["cksum", "go"]);
    }

    #[test]
    fn an_empty_config_is_fine() {
        let cfg = Config::from_toml("# nothing here\n").unwrap();
        assert!(cfg.disable.is_empty());
        assert_eq!(cfg.exec, None);
    }

    // A key nobody reads is worse than an error: the setting silently does
    // nothing and the user believes it worked.
    #[test]
    fn unknown_keys_and_rules_are_errors() {
        assert!(Config::from_toml("disabel = [\"MK006\"]").unwrap_err().contains("unknown key"));
        assert!(Config::from_toml("disable = [\"MK999\"]").unwrap_err().contains("not a rule"));
        assert!(
            Config::from_toml("[severity]\nMK999 = \"note\"").unwrap_err().contains("not a rule")
        );
        assert!(Config::from_toml("fail-level = \"loud\"").unwrap_err().contains("unknown level"));
    }

    #[test]
    fn type_errors_name_what_was_expected() {
        let e = Config::from_toml("disable = \"MK006\"").unwrap_err();
        assert!(e.contains("expected an array"), "{e}");
        let e = Config::from_toml("show-notes = \"yes\"").unwrap_err();
        assert!(e.contains("expected true or false"), "{e}");
    }

    #[test]
    fn syntax_errors_carry_a_line_number() {
        let e = Config::from_toml("\n\ndisable = [\"a\"\n").unwrap_err();
        assert!(e.contains("line 3"), "{e}");
    }
}
