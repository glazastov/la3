//! Source spans and diagnostics shared across the lexer, parser, checker, and interpreter.

use std::fmt;

/// A 1-based line/column position in a source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

impl Pos {
    pub fn new(line: u32, col: u32) -> Self {
        Pos { line, col }
    }
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

/// The phase that produced a diagnostic. Used only for the printed label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Lex,
    Parse,
    Check,
    Runtime,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Lex => "lex error",
            Phase::Parse => "parse error",
            Phase::Check => "type error",
            Phase::Runtime => "runtime error",
        }
    }
}

/// A single error with a position and message.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub phase: Phase,
    pub pos: Pos,
    pub message: String,
}

impl Diagnostic {
    pub fn new(phase: Phase, pos: Pos, message: impl Into<String>) -> Self {
        Diagnostic {
            phase,
            pos,
            message: message.into(),
        }
    }

    /// Render the diagnostic against the source, with a caret line.
    pub fn render(&self, file: &str, src: &str) -> String {
        let mut out = format!(
            "{}: {} ({}:{})\n",
            self.phase.label(),
            self.message,
            file,
            self.pos
        );
        if let Some(line) = src.lines().nth(self.pos.line.saturating_sub(1) as usize) {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
            let caret_pad = self.pos.col.saturating_sub(1) as usize;
            out.push_str("  ");
            out.push_str(&" ".repeat(caret_pad));
            out.push('^');
        }
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.phase.label(), self.message, self.pos)
    }
}

pub type Result<T> = std::result::Result<T, Diagnostic>;
