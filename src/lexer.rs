//! Lexer for La3. Turns source text into a flat token stream.
//!
//! Newlines are emitted as explicit `Newline` tokens; the parser decides where
//! a newline terminates a statement (only when the preceding text is already a
//! complete expression), which mirrors the language's "optional semicolons" rule.

use crate::diag::{Diagnostic, Phase, Pos, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    // Literals
    Int(i64),
    Float(f64),
    Str(String),
    /// A formatted string literal, kept raw (without the leading `f`); the
    /// parser splits it into literal and `{expr}` segments.
    FStr(String),
    Char(char),

    Ident(String),

    // Keywords
    Let,
    Mut,
    Const,
    Fn,
    Return,
    If,
    Else,
    Match,
    Loop,
    While,
    For,
    In,
    Break,
    Continue,
    Struct,
    Enum,
    Interface,
    Impl,
    Type,
    Mod,
    Use,
    Pub,
    Async,
    Await,
    Move,
    Spawn,
    Unsafe,
    Try,
    As,
    SelfKw,
    True,
    False,
    Nil,

    // Punctuation / operators
    Plus,
    Minus,
    Star,
    StarStar,   // **
    Slash,
    Percent,
    Amp,        // &
    AmpAmp,     // &&
    Pipe,       // |
    PipePipe,   // ||
    Caret,      // ^
    Tilde,      // ~
    Shl,        // <<
    Shr,        // >>
    Bang,       // !
    Eq,         // =
    EqEq,       // ==
    Ne,         // !=
    Lt,
    Gt,
    Le,
    Ge,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    DotDot,   // ..
    DotDotEq, // ..=
    Question,     // ?
    QuestionDot,  // ?.
    QuestionQuestion, // ??
    Arrow,    // ->
    FatArrow, // =>
    Dot,
    Comma,
    Colon,
    ColonColon, // ::
    Semicolon,
    At,         // @
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,

    Newline,
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub pos: Pos,
}

pub struct Lexer {
    chars: Vec<char>,
    i: usize,
    line: u32,
    col: u32,
}

impl Lexer {
    pub fn new(src: &str) -> Self {
        Lexer {
            chars: src.chars().collect(),
            i: 0,
            line: 1,
            col: 1,
        }
    }

    fn pos(&self) -> Pos {
        Pos::new(self.line, self.col)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn peek2(&self) -> Option<char> {
        self.chars.get(self.i + 1).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.i).copied();
        if let Some(c) = c {
            self.i += 1;
            if c == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
        }
        c
    }

    fn err(&self, pos: Pos, msg: impl Into<String>) -> Diagnostic {
        Diagnostic::new(Phase::Lex, pos, msg)
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut out = Vec::new();
        loop {
            let t = self.next_token()?;
            let is_eof = matches!(t.tok, Tok::Eof);
            out.push(t);
            if is_eof {
                break;
            }
        }
        Ok(out)
    }

    fn next_token(&mut self) -> Result<Token> {
        loop {
            let start = self.pos();
            let c = match self.peek() {
                None => return Ok(Token { tok: Tok::Eof, pos: start }),
                Some(c) => c,
            };

            // Newlines are significant; emit them.
            if c == '\n' {
                self.bump();
                return Ok(Token { tok: Tok::Newline, pos: start });
            }

            // Other whitespace is skipped.
            if c == ' ' || c == '\t' || c == '\r' {
                self.bump();
                continue;
            }

            // Line comments. The La3 spec overloads `//` for both line comments
            // (from C/Rust) and floor division (from Lua); a lexer cannot tell
            // `7 // 2` from `x // note` apart, and trailing comments are far more
            // common, so this implementation treats `//` as always a comment and
            // exposes floor division as the `idiv(a, b)` builtin instead.
            if c == '/' && self.peek2() == Some('/') {
                while let Some(ch) = self.peek() {
                    if ch == '\n' {
                        break;
                    }
                    self.bump();
                }
                continue;
            }
            if c == '/' && self.peek2() == Some('*') {
                self.bump();
                self.bump();
                let mut depth = 1;
                while depth > 0 {
                    match self.bump() {
                        None => return Err(self.err(start, "unterminated block comment")),
                        Some('*') if self.peek() == Some('/') => {
                            self.bump();
                            depth -= 1;
                        }
                        Some('/') if self.peek() == Some('*') => {
                            self.bump();
                            depth += 1;
                        }
                        _ => {}
                    }
                }
                continue;
            }

            return self.scan_token(start);
        }
    }

    fn scan_token(&mut self, start: Pos) -> Result<Token> {
        let c = self.peek().unwrap();

        // Identifiers and keywords.
        if c.is_alphabetic() || c == '_' {
            // f-string prefix
            if c == 'f' && self.peek2() == Some('"') {
                self.bump(); // f
                return self.scan_string(start, true);
            }
            let mut s = String::new();
            while let Some(ch) = self.peek() {
                if ch.is_alphanumeric() || ch == '_' {
                    s.push(ch);
                    self.bump();
                } else {
                    break;
                }
            }
            return Ok(Token { tok: keyword_or_ident(s), pos: start });
        }

        // Numbers.
        if c.is_ascii_digit() {
            return self.scan_number(start);
        }

        // Strings.
        if c == '"' {
            return self.scan_string(start, false);
        }

        // Char literals.
        if c == '\'' {
            return self.scan_char(start);
        }

        // Operators and punctuation.
        self.bump();
        let two = self.peek();
        let tok = match c {
            '+' => self.pick(two, '=', Tok::PlusEq, Tok::Plus),
            '-' => match two {
                Some('>') => {
                    self.bump();
                    Tok::Arrow
                }
                Some('=') => {
                    self.bump();
                    Tok::MinusEq
                }
                _ => Tok::Minus,
            },
            '*' => match two {
                Some('*') => {
                    self.bump();
                    Tok::StarStar
                }
                Some('=') => {
                    self.bump();
                    Tok::StarEq
                }
                _ => Tok::Star,
            },
            '/' => self.pick(two, '=', Tok::SlashEq, Tok::Slash),
            '%' => self.pick(two, '=', Tok::PercentEq, Tok::Percent),
            '&' => self.pick(two, '&', Tok::AmpAmp, Tok::Amp),
            '|' => self.pick(two, '|', Tok::PipePipe, Tok::Pipe),
            '^' => Tok::Caret,
            '~' => Tok::Tilde,
            '!' => self.pick(two, '=', Tok::Ne, Tok::Bang),
            '=' => match two {
                Some('=') => {
                    self.bump();
                    Tok::EqEq
                }
                Some('>') => {
                    self.bump();
                    Tok::FatArrow
                }
                _ => Tok::Eq,
            },
            '<' => match two {
                Some('<') => {
                    self.bump();
                    Tok::Shl
                }
                Some('=') => {
                    self.bump();
                    Tok::Le
                }
                _ => Tok::Lt,
            },
            '>' => match two {
                Some('>') => {
                    self.bump();
                    Tok::Shr
                }
                Some('=') => {
                    self.bump();
                    Tok::Ge
                }
                _ => Tok::Gt,
            },
            '.' => match two {
                Some('.') => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        Tok::DotDotEq
                    } else {
                        Tok::DotDot
                    }
                }
                _ => Tok::Dot,
            },
            '?' => match two {
                Some('.') => {
                    self.bump();
                    Tok::QuestionDot
                }
                Some('?') => {
                    self.bump();
                    Tok::QuestionQuestion
                }
                _ => Tok::Question,
            },
            ':' => self.pick(two, ':', Tok::ColonColon, Tok::Colon),
            '@' => Tok::At,
            ',' => Tok::Comma,
            ';' => Tok::Semicolon,
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            '{' => Tok::LBrace,
            '}' => Tok::RBrace,
            '[' => Tok::LBracket,
            ']' => Tok::RBracket,
            other => return Err(self.err(start, format!("unexpected character '{}'", other))),
        };
        Ok(Token { tok, pos: start })
    }

    fn pick(&mut self, next: Option<char>, want: char, two: Tok, one: Tok) -> Tok {
        if next == Some(want) {
            self.bump();
            two
        } else {
            one
        }
    }

    fn scan_number(&mut self, start: Pos) -> Result<Token> {
        let mut s = String::new();
        // hex/oct/bin
        if self.peek() == Some('0') {
            if let Some(base) = self.peek2() {
                if base == 'x' || base == 'o' || base == 'b' {
                    self.bump();
                    self.bump();
                    let radix = match base {
                        'x' => 16,
                        'o' => 8,
                        _ => 2,
                    };
                    let mut digits = String::new();
                    while let Some(ch) = self.peek() {
                        if ch == '_' {
                            self.bump();
                        } else if ch.is_alphanumeric() {
                            digits.push(ch);
                            self.bump();
                        } else {
                            break;
                        }
                    }
                    let v = i64::from_str_radix(&digits, radix)
                        .map_err(|_| self.err(start, "invalid integer literal"))?;
                    self.skip_suffix();
                    return Ok(Token { tok: Tok::Int(v), pos: start });
                }
            }
        }
        let mut is_float = false;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                s.push(ch);
                self.bump();
            } else if ch == '_' {
                self.bump();
            } else if ch == '.' && self.peek2().map_or(false, |d| d.is_ascii_digit()) {
                is_float = true;
                s.push('.');
                self.bump();
            } else if ch == 'e' || ch == 'E' {
                is_float = true;
                s.push(ch);
                self.bump();
                if matches!(self.peek(), Some('+') | Some('-')) {
                    s.push(self.bump().unwrap());
                }
            } else {
                break;
            }
        }
        self.skip_suffix();
        if is_float {
            let v: f64 = s.parse().map_err(|_| self.err(start, "invalid float literal"))?;
            Ok(Token { tok: Tok::Float(v), pos: start })
        } else {
            let v: i64 = s.parse().map_err(|_| self.err(start, "invalid integer literal"))?;
            Ok(Token { tok: Tok::Int(v), pos: start })
        }
    }

    /// Skip a trailing numeric type suffix like `u8`, `i32`, `f64`, `usize`.
    fn skip_suffix(&mut self) {
        let save = (self.i, self.line, self.col);
        if matches!(self.peek(), Some('u') | Some('i') | Some('f')) {
            let mut suffix = String::new();
            while let Some(ch) = self.peek() {
                if ch.is_alphanumeric() {
                    suffix.push(ch);
                    self.bump();
                } else {
                    break;
                }
            }
            let ok = matches!(
                suffix.as_str(),
                "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" | "isize"
                    | "f32" | "f64" | "byte"
            );
            if !ok {
                // not a suffix; roll back
                self.i = save.0;
                self.line = save.1;
                self.col = save.2;
            }
        }
    }

    fn scan_string(&mut self, start: Pos, is_fstring: bool) -> Result<Token> {
        self.bump(); // opening quote
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err(start, "unterminated string literal")),
                Some('"') => break,
                Some('\\') => {
                    let e = self.bump().ok_or_else(|| self.err(start, "unterminated escape"))?;
                    s.push(unescape(e));
                }
                Some(c) => s.push(c),
            }
        }
        let tok = if is_fstring { Tok::FStr(s) } else { Tok::Str(s) };
        Ok(Token { tok, pos: start })
    }

    fn scan_char(&mut self, start: Pos) -> Result<Token> {
        self.bump(); // opening quote
        let c = match self.bump() {
            None => return Err(self.err(start, "unterminated char literal")),
            Some('\\') => {
                let e = self.bump().ok_or_else(|| self.err(start, "unterminated escape"))?;
                unescape(e)
            }
            Some('\'') => return Err(self.err(start, "empty char literal is not allowed")),
            Some(c) => c,
        };
        match self.bump() {
            Some('\'') => Ok(Token { tok: Tok::Char(c), pos: start }),
            _ => Err(self.err(start, "char literal must contain exactly one character")),
        }
    }
}

fn unescape(e: char) -> char {
    match e {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        '\\' => '\\',
        '\'' => '\'',
        '"' => '"',
        other => other,
    }
}

fn keyword_or_ident(s: String) -> Tok {
    match s.as_str() {
        "let" => Tok::Let,
        "mut" => Tok::Mut,
        "const" => Tok::Const,
        "fn" => Tok::Fn,
        "return" => Tok::Return,
        "if" => Tok::If,
        "else" => Tok::Else,
        "match" => Tok::Match,
        "loop" => Tok::Loop,
        "while" => Tok::While,
        "for" => Tok::For,
        "in" => Tok::In,
        "break" => Tok::Break,
        "continue" => Tok::Continue,
        "struct" => Tok::Struct,
        "enum" => Tok::Enum,
        "interface" => Tok::Interface,
        "impl" => Tok::Impl,
        "type" => Tok::Type,
        "mod" => Tok::Mod,
        "use" => Tok::Use,
        "pub" => Tok::Pub,
        "async" => Tok::Async,
        "await" => Tok::Await,
        "move" => Tok::Move,
        "spawn" => Tok::Spawn,
        "unsafe" => Tok::Unsafe,
        "try" => Tok::Try,
        "as" => Tok::As,
        "self" => Tok::SelfKw,
        "true" => Tok::True,
        "false" => Tok::False,
        "nil" => Tok::Nil,
        _ => Tok::Ident(s),
    }
}
