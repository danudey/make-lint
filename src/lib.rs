//! A static linter for GNU Makefiles.
//!
//! Phase 1 covers parsing and syntax-only checks. The parser deliberately keeps
//! both sides of every conditional and never expands a value, so the tree it
//! produces is a faithful record of the source rather than of one evaluation.

pub mod ast;
pub mod builtins;
pub mod checks;
pub mod diag;
pub mod expr;
pub mod lexer;
pub mod parser;
pub mod render;
pub mod span;
pub mod workspace;

/// Filenames GNU make looks for, in order.
pub const DEFAULT_MAKEFILES: &[&str] = &["GNUmakefile", "makefile", "Makefile"];
