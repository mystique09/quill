//! Diagnostics with source spans and rustc-style rendering.

use std::fmt;

/// Byte range into a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub lo: usize,
    pub hi: usize,
}

impl Span {
    pub fn new(lo: usize, hi: usize) -> Self {
        Span { lo, hi }
    }

    pub fn merge(self, other: Span) -> Span {
        Span {
            lo: self.lo.min(other.lo),
            hi: self.hi.max(other.hi),
        }
    }
}

/// An error code, like `E0001`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode(pub u16);

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "E{:04}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: ErrorCode,
    pub message: String,
    pub span: Option<Span>,
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn error(code: u16, message: impl Into<String>, span: Option<Span>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code: ErrorCode(code),
            message: message.into(),
            span,
            notes: Vec::new(),
        }
    }

    pub fn warning(code: u16, message: impl Into<String>, span: Option<Span>) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            code: ErrorCode(code),
            message: message.into(),
            span,
            notes: Vec::new(),
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// Compute the (1-based) line and column for a byte offset.
pub fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in source.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn render_line(source: &str, line: usize) -> String {
    source.lines().nth(line - 1).unwrap_or("").to_string()
}

/// Render a diagnostic the way rustc does: caret, code, notes.
pub fn render(source: &str, diag: &Diagnostic) -> String {
    let sev = match diag.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let mut out = String::new();
    let (line, col) = match diag.span {
        Some(sp) => line_col(source, sp.lo),
        None => (0, 0),
    };
    out.push_str(&format!(
        "{}[{}]: {}\n",
        sev, diag.code, diag.message
    ));
    if let Some(sp) = diag.span {
        let (_, end_col) = line_col(source, sp.hi);
        let text = render_line(source, line);
        let gutter = line.to_string();
        let pad = " ".repeat(gutter.len());
        out.push_str(&format!(" {} {} |\n", pad, ""));
        out.push_str(&format!(" {} {} | {}\n", pad, gutter, text));
        let width = if sp.lo == sp.hi {
            1
        } else {
            end_col.saturating_sub(col).max(1)
        };
        let carets = "^".repeat(width);
        out.push_str(&format!(
            " {} {} | {}{}\n",
            pad,
            "",
            " ".repeat(col - 1),
            carets
        ));
    }
    for note in &diag.notes {
        out.push_str(&format!("  = note: {}\n", note));
    }
    out
}
