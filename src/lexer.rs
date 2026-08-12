//! Lexer: turns source text into tokens, applying Haskell-style layout
//! (indentation) rules to produce explicit `{`, `}`, `;` tokens.

use crate::error::{Diagnostic, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum TokKind {
    Ident(String),
    ConId(String),
    Int(i64),
    Float(f64),
    Str(String),
    Char(char),
    // keywords
    Let,
    In,
    If,
    Then,
    Else,
    Case,
    Of,
    Data,
    Type,
    Class,
    Instance,
    Where,
    Module,
    Use,
    Clone,
    Mut,
    // punctuation
    DColon,   // ::
    Arrow,    // ->
    FatArrow, // =>
    Backslash,
    Pipe,
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Dot,
    Underscore,
    At,
    Amp,
    Eq,       // =
    Op(String),
    Eof,
}

impl TokKind {
    pub fn describe(&self) -> String {
        match self {
            TokKind::Ident(s) => format!("identifier `{s}`"),
            TokKind::ConId(s) => format!("constructor/type `{s}`"),
            TokKind::Int(n) => format!("integer literal `{n}`"),
            TokKind::Float(f) => format!("float literal `{f}`"),
            TokKind::Str(s) => format!("string literal `{s}`"),
            TokKind::Char(c) => format!("char literal `{c}`"),
            TokKind::Op(op) => format!("operator `{op}`"),
            TokKind::Eof => "end of file".to_string(),
            k => format!("`{k:?}`"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokKind,
    pub span: Span,
    pub line: usize,
    pub col: usize,
}

/// A keyword token opener for a layout block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opener {
    Where,
    Let,
    Of,
    Top,
}

#[derive(Debug)]
struct Ctx {
    indent: usize,
    opener: Opener,
    explicit: bool,
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

pub fn lex(src: &str) -> Result<Vec<Token>, Vec<Diagnostic>> {
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        line: 0,
        col: 0,
    };
    let mut raw = Vec::new();
    let mut errs = Vec::new();
    loop {
        let tok = lx.next_token();
        match tok {
            Ok(Some(t)) => raw.push(t),
            Ok(None) => break,
            Err(d) => {
                errs.push(d);
                break;
            }
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    raw.push(Token {
        kind: TokKind::Eof,
        span: Span::new(lx.pos, lx.pos),
        line: lx.line,
        col: lx.col,
    });
    Ok(apply_layout(raw))
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
    fn peek2(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let ch = self.src[self.pos..].chars().next()?;
        self.pos += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.col = 0;
        } else if ch == '\t' {
            self.col = (self.col / 8 + 1) * 8;
        } else {
            self.col += 1;
        }
        Some(ch)
    }
    fn starts_with(&self, s: &str) -> bool {
        self.src[self.pos..].starts_with(s)
    }

    fn skip_trivia(&mut self) {
        loop {
            if self.starts_with("--") {
                while let Some(c) = self.peek() {
                    if c == b'\n' {
                        break;
                    }
                    self.bump();
                }
            } else if self.starts_with("{-") {
                let start = self.pos;
                self.bump();
                self.bump();
                let mut depth = 1;
                while depth > 0 {
                    if self.starts_with("{-") {
                        self.bump();
                        self.bump();
                        depth += 1;
                    } else if self.starts_with("-}") {
                        self.bump();
                        self.bump();
                        depth -= 1;
                    } else if self.peek().is_none() {
                        // Unterminated comment; let it slide, error is acceptable.
                        break;
                    } else {
                        self.bump();
                    }
                }
                let _ = start;
            } else if matches!(self.peek(), Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')) {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self) -> Result<Option<Token>, Diagnostic> {
        self.skip_trivia();
        let lo = self.pos;
        let line = self.line;
        let col = self.col;
        let Some(c) = self.peek() else {
            return Ok(None);
        };
        let c = c as char;

        let mk = |kind: TokKind, lx: &Self| Token {
            kind,
            span: Span::new(lo, lx.pos),
            line,
            col,
        };

        // Identifiers and keywords
        if c.is_ascii_alphabetic() || c == '_' {
            if c == '_' && !matches!(self.peek2(), Some(b) if is_ident_char(b)) {
                self.bump();
                return Ok(Some(mk(TokKind::Underscore, self)));
            }
            let mut s = String::new();
            while let Some(b) = self.peek() {
                let ch = b as char;
                if is_ident_char(b) {
                    s.push(ch);
                    self.bump();
                } else {
                    break;
                }
            }
            let kind = match s.as_str() {
                "let" => TokKind::Let,
                "in" => TokKind::In,
                "if" => TokKind::If,
                "then" => TokKind::Then,
                "else" => TokKind::Else,
                "case" => TokKind::Case,
                "of" => TokKind::Of,
                "data" => TokKind::Data,
                "type" => TokKind::Type,
                "class" => TokKind::Class,
                "instance" => TokKind::Instance,
                "where" => TokKind::Where,
                "module" => TokKind::Module,
                "use" => TokKind::Use,
                "clone" => TokKind::Clone,
                "mut" => TokKind::Mut,
                _ if s.chars().next().unwrap().is_ascii_uppercase() => TokKind::ConId(s),
                _ => TokKind::Ident(s),
            };
            return Ok(Some(mk(kind, self)));
        }

        // Numbers
        if c.is_ascii_digit() {
            return Ok(Some(self.number(lo, line, col)));
        }

        // Char literals
        if c == '\'' {
            self.bump();
            let (ch, _) = self.escape_char()?;
            if self.peek() != Some(b'\'') {
                return Err(Diagnostic::error(
                    1001,
                    "unterminated character literal",
                    Some(Span::new(lo, self.pos)),
                ));
            }
            self.bump();
            return Ok(Some(Token {
                kind: TokKind::Char(ch),
                span: Span::new(lo, self.pos),
                line,
                col,
            }));
        }

        // String literals
        if c == '"' {
            self.bump();
            let mut s = String::new();
            loop {
                match self.peek() {
                    None => {
                        return Err(Diagnostic::error(
                            1002,
                            "unterminated string literal",
                            Some(Span::new(lo, self.pos)),
                        ));
                    }
                    Some(b'"') => {
                        self.bump();
                        break;
                    }
                    Some(b'\\') => {
                        let (ch, _) = self.escape_char()?;
                        s.push(ch);
                    }
                    Some(_) => {
                        let ch = self.bump().unwrap();
                        s.push(ch);
                    }
                }
            }
            return Ok(Some(Token {
                kind: TokKind::Str(s),
                span: Span::new(lo, self.pos),
                line,
                col,
            }));
        }

        // Operators and punctuation
        let two = self.src[self.pos..].chars().take(2).collect::<String>();
        let kind = match two.as_str() {
            "::" => {
                self.bump();
                self.bump();
                TokKind::DColon
            }
            "->" => {
                self.bump();
                self.bump();
                TokKind::Arrow
            }
            "=>" => {
                self.bump();
                self.bump();
                TokKind::FatArrow
            }
            _ => {
                match c {
                    '\\' => {
                        self.bump();
                        TokKind::Backslash
                    }
                    '{' => {
                        self.bump();
                        TokKind::LBrace
                    }
                    '}' => {
                        self.bump();
                        TokKind::RBrace
                    }
                    '(' => {
                        self.bump();
                        TokKind::LParen
                    }
                    ')' => {
                        self.bump();
                        TokKind::RParen
                    }
                    '[' => {
                        self.bump();
                        TokKind::LBracket
                    }
                    ']' => {
                        self.bump();
                        TokKind::RBracket
                    }
                    ';' => {
                        self.bump();
                        TokKind::Semi
                    }
                    ',' => {
                        self.bump();
                        TokKind::Comma
                    }
                    '.' => {
                        self.bump();
                        TokKind::Dot
                    }
                    '@' => {
                        self.bump();
                        TokKind::At
                    }
                    c if is_symbol_char(c) => {
                        let mut s = String::new();
                        while let Some(b) = self.peek() {
                            let ch = b as char;
                            if is_symbol_char(ch) {
                                s.push(ch);
                                self.bump();
                            } else {
                                break;
                            }
                        }
                        if s == "=" {
                            TokKind::Eq
                        } else if s == "|" {
                            TokKind::Pipe
                        } else if s == "&" {
                            TokKind::Amp
                        } else {
                            TokKind::Op(s)
                        }
                    }
                    _ => {
                        return Err(Diagnostic::error(
                            1000,
                            format!("unexpected character `{c}`"),
                            Some(Span::new(lo, lo + c.len_utf8())),
                        ));
                    }
                }
            }
        };
        Ok(Some(Token {
            kind,
            span: Span::new(lo, self.pos),
            line,
            col,
        }))
    }

    fn number(&mut self, lo: usize, line: usize, col: usize) -> Token {
        // hex
        if self.starts_with("0x") || self.starts_with("0X") {
            self.bump();
            self.bump();
            let mut val: i64 = 0;
            while let Some(b) = self.peek() {
                let d = match b {
                    b'0'..=b'9' => (b - b'0') as i64,
                    b'a'..=b'f' => (b - b'a' + 10) as i64,
                    b'A'..=b'F' => (b - b'A' + 10) as i64,
                    _ => break,
                };
                val = val * 16 + d;
                self.bump();
            }
            return Token {
                kind: TokKind::Int(val),
                span: Span::new(lo, self.pos),
                line,
                col,
            };
        }
        let mut int_part = String::new();
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                int_part.push(b as char);
                self.bump();
            } else {
                break;
            }
        }
        let mut frac_part = String::new();
        if self.peek() == Some(b'.') && self.peek2().is_some_and(|b| b.is_ascii_digit()) {
            self.bump();
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    frac_part.push(b as char);
                    self.bump();
                } else {
                    break;
                }
            }
        }
        if frac_part.is_empty() {
            let val = int_part.parse::<i64>().unwrap_or(0);
            Token {
                kind: TokKind::Int(val),
                span: Span::new(lo, self.pos),
                line,
                col,
            }
        } else {
            let text = format!("{int_part}.{frac_part}");
            let val = text.parse::<f64>().unwrap_or(0.0);
            Token {
                kind: TokKind::Float(val),
                span: Span::new(lo, self.pos),
                line,
                col,
            }
        }
    }

    fn escape_char(&mut self) -> Result<(char, bool), Diagnostic> {
        // Called after the backslash has been consumed.
        let lo = self.pos.saturating_sub(1);
        let Some(c) = self.bump() else {
            return Err(Diagnostic::error(
                1003,
                "unterminated escape sequence",
                Some(Span::new(lo, self.pos)),
            ));
        };
        match c {
            'n' => Ok(('\n', true)),
            't' => Ok(('\t', true)),
            'r' => Ok(('\r', true)),
            '0' => Ok(('\0', true)),
            '\\' => Ok(('\\', true)),
            '"' => Ok(('"', true)),
            '\'' => Ok(('\'', true)),
            other => Ok((other, true)),
        }
    }
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'\''
}

fn is_symbol_char(c: char) -> bool {
    "!#$%&*+./<=>?@\\^|-~:".contains(c)
}

/// Convert the raw token stream into one with explicit `{`, `}`, `;`,
/// following Haskell's layout algorithm.
fn apply_layout(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut stack: Vec<Ctx> = Vec::new();
    let mut pending: Option<Opener> = None;
    let mut prev_line: Option<usize> = None;

    // Establish the top-level layout context from the first token.
    if let Some(first) = tokens.first()
        && !matches!(first.kind, TokKind::LBrace | TokKind::Eof) {
            stack.push(Ctx {
                indent: first.col,
                opener: Opener::Top,
                explicit: false,
            });
        }

    // Virtual token helper. Virtual tokens borrow the span of the token that
    // triggered them so merged spans never regress to byte 0.
    let emit_virtual = |kind: TokKind, span: Span, out: &mut Vec<Token>| {
        out.push(Token {
            kind,
            span,
            line: 0,
            col: 0,
        });
    };

    for t in tokens {
        let new_line = prev_line.is_some() && t.line > prev_line.unwrap();

        if new_line {
            if pending.is_some() && !matches!(t.kind, TokKind::LBrace) {
                let op = pending.take().unwrap();
                stack.push(Ctx {
                    indent: t.col,
                    opener: op,
                    explicit: false,
                });
                emit_virtual(TokKind::LBrace, t.span, &mut out);
            } else {
                // Indentation handling relative to the topmost layout context.
                let top_is_explicit = stack.last().map(|c| c.explicit).unwrap_or(false);
                if !top_is_explicit {
                    while let Some(top) = stack.last() {
                        if !top.explicit && top.indent > t.col {
                            emit_virtual(TokKind::RBrace, t.span, &mut out);
                            stack.pop();
                        } else {
                            break;
                        }
                    }
                    if let Some(top) = stack.last()
                        && !top.explicit && top.indent == t.col {
                            emit_virtual(TokKind::Semi, t.span, &mut out);
                        }
                }
            }
        }

        // Same-line layout block opening (`where`, `let`, `of` immediately
        // followed by the first declaration/alternative on the same line).
        if pending.is_some() {
            if matches!(t.kind, TokKind::LBrace) {
                pending = None;
            } else {
                let op = pending.take().unwrap();
                stack.push(Ctx {
                    indent: t.col,
                    opener: op,
                    explicit: false,
                });
                emit_virtual(TokKind::LBrace, t.span, &mut out);
            }
        }

        // `in` terminates a `let` layout block.
        if matches!(t.kind, TokKind::In) && stack.iter().rev().any(|c| c.opener == Opener::Let) {
            loop {
                let c = stack.pop().unwrap();
                emit_virtual(TokKind::RBrace, t.span, &mut out);
                if c.opener == Opener::Let {
                    break;
                }
            }
        }

        // Real token.
        let is_lbrace = matches!(t.kind, TokKind::LBrace);
        let is_rbrace = matches!(t.kind, TokKind::RBrace);
        let is_opener_kw = matches!(
            t.kind,
            TokKind::Where | TokKind::Let | TokKind::Of
        );
        let tline = t.line;

        // Set pending BEFORE pushing explicit brace contexts so `{` after a
        // layout keyword is seen as explicit.
        if is_opener_kw {
            pending = Some(match t.kind {
                TokKind::Where => Opener::Where,
                TokKind::Let => Opener::Let,
                TokKind::Of => Opener::Of,
                _ => unreachable!(),
            });
        }

        if is_lbrace {
            stack.push(Ctx {
                indent: t.col,
                opener: Opener::Top,
                explicit: true,
            });
        }

        out.push(t);

        if is_rbrace {
            // Close everything up to and including the nearest explicit brace.
            while let Some(c) = stack.pop() {
                if c.explicit {
                    break;
                }
            }
        }

        prev_line = Some(tline);
    }

    // Close any remaining layout contexts at end of file (the implicit
    // top-level module context needs no closing brace).
    let last_span = out.last().map(|t| t.span).unwrap_or_default();
    while let Some(c) = stack.pop() {
        if !c.explicit && c.opener != Opener::Top {
            emit_virtual(TokKind::RBrace, last_span, &mut out);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokKind> {
        lex(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn lex_basic() {
        let k = kinds("add x y = x + y");
        assert_eq!(k[0], TokKind::Ident("add".into()));
        assert_eq!(k[3], TokKind::Eq);
        assert_eq!(k[5], TokKind::Op("+".into()));
    }

    #[test]
    fn lex_keywords() {
        let k = kinds("let x = 5 in x");
        assert!(k.contains(&TokKind::Let));
        assert!(k.contains(&TokKind::In));
    }

    #[test]
    fn layout_where() {
        let k = kinds("f x = y where y = x + 1");
        assert!(k.contains(&TokKind::LBrace));
        assert!(k.contains(&TokKind::RBrace));
        assert!(k.contains(&TokKind::Where));
    }

    #[test]
    fn layout_multiline() {
        let src = "f x =\n    x + 1\ng y =\n    y + 2\n";
        let k = kinds(src);
        let braces = k.iter().filter(|k| **k == TokKind::LBrace).count();
        let rbraces = k.iter().filter(|k| **k == TokKind::RBrace).count();
        let semis = k.iter().filter(|k| **k == TokKind::Semi).count();
        assert_eq!(braces, rbraces);
        assert!(semis >= 1);
    }

    #[test]
    fn numbers() {
        let k = kinds("1 2.5 0x10");
        assert_eq!(k[0], TokKind::Int(1));
        assert_eq!(k[1], TokKind::Float(2.5));
        assert_eq!(k[2], TokKind::Int(16));
    }
}
