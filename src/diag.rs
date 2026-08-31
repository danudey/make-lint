//! Diagnostics: codes, severities, and labelled spans.

use crate::span::Span;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    Note,
    Warning,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Note => "note",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Severity> {
        match s {
            "note" => Some(Severity::Note),
            "warning" | "warn" => Some(Severity::Warning),
            "error" => Some(Severity::Error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Label {
    pub span: Span,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub primary: Span,
    pub secondary: Vec<Label>,
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn new(
        code: &'static str,
        severity: Severity,
        primary: Span,
        message: impl Into<String>,
    ) -> Self {
        Diagnostic {
            code,
            severity,
            message: message.into(),
            primary,
            secondary: Vec::new(),
            help: None,
        }
    }

    pub fn error(code: &'static str, primary: Span, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Error, primary, message)
    }

    pub fn warn(code: &'static str, primary: Span, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Warning, primary, message)
    }

    pub fn note(code: &'static str, primary: Span, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Note, primary, message)
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.secondary.push(Label { span, message: message.into() });
        self
    }
}
