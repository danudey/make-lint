//! A static linter for GNU Makefiles.
//!
//! The parser keeps both sides of every conditional and never expands a value,
//! so the tree is a record of the source rather than of one evaluation. The
//! evaluator then builds make's variable table on top of it, resolving only the
//! `$(shell ...)` commands that can be proven read-only. Every check that needs
//! a value stops at an `Unknown`.

pub mod ast;
pub mod builtins;
pub mod checks;
pub mod config;
pub mod diag;
pub mod eval;
pub mod expr;
pub mod fix;
pub mod funcs;
pub mod lexer;
pub mod parser;
pub mod render;
pub mod rules;
pub mod shell;
pub mod span;
pub mod suppress;
pub mod value;
pub mod workspace;

/// Filenames GNU make looks for, in order.
pub const DEFAULT_MAKEFILES: &[&str] = &["GNUmakefile", "makefile", "Makefile"];
