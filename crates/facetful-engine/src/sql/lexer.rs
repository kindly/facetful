//! Hand-rolled lexer: every token carries its span. Keywords are reserved
//! upfront (including ones whose syntax lands later) so adding sugar never
//! breaks an existing query's identifiers. `"double quoted"` identifiers
//! escape the reserved list.

use super::span::{Diagnostic, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals & names
    Number(f64),
    Str(String),
    Ident(String),
    /// "quoted" identifier — never a keyword
    QuotedIdent(String),
    // punctuation / operators
    LParen,
    RParen,
    Comma,
    Star,
    Slash,
    Percent,
    Plus,
    Minus,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Concat, // ||
    // keywords (reserved set — some spellings are sugar, all reserved now)
    Select,
    From,
    Where,
    Group,
    Order,
    By,
    Asc,
    Desc,
    Limit,
    Offset,
    As,
    And,
    Or,
    Not,
    In,
    Is,
    Null,
    Between,
    Like,
    Case,
    When,
    Then,
    Else,
    End,
    Cast,
    Distinct,
    Eof,
}

pub const KEYWORDS: &[(&str, Tok)] = &[
    ("select", Tok::Select),
    ("from", Tok::From),
    ("where", Tok::Where),
    ("group", Tok::Group),
    ("order", Tok::Order),
    ("by", Tok::By),
    ("asc", Tok::Asc),
    ("desc", Tok::Desc),
    ("limit", Tok::Limit),
    ("offset", Tok::Offset),
    ("as", Tok::As),
    ("and", Tok::And),
    ("or", Tok::Or),
    ("not", Tok::Not),
    ("in", Tok::In),
    ("is", Tok::Is),
    ("null", Tok::Null),
    ("between", Tok::Between),
    ("like", Tok::Like),
    ("case", Tok::Case),
    ("when", Tok::When),
    ("then", Tok::Then),
    ("else", Tok::Else),
    ("end", Tok::End),
    ("cast", Tok::Cast),
    ("distinct", Tok::Distinct),
];

#[derive(Debug, Clone)]
pub struct SpannedTok {
    pub tok: Tok,
    pub span: Span,
}

pub fn lex(src: &str) -> Result<Vec<SpannedTok>, Diagnostic> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        let start = i;
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
                continue;
            }
            b'(' => push1(&mut out, Tok::LParen, &mut i),
            b')' => push1(&mut out, Tok::RParen, &mut i),
            b',' => push1(&mut out, Tok::Comma, &mut i),
            b'*' => push1(&mut out, Tok::Star, &mut i),
            b'/' => push1(&mut out, Tok::Slash, &mut i),
            b'%' => push1(&mut out, Tok::Percent, &mut i),
            b'+' => push1(&mut out, Tok::Plus, &mut i),
            b'-' => {
                // line comment `-- …`
                if b.get(i + 1) == Some(&b'-') {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                push1(&mut out, Tok::Minus, &mut i)
            }
            b'=' => push1(&mut out, Tok::Eq, &mut i),
            b'|' => {
                if b.get(i + 1) == Some(&b'|') {
                    out.push(SpannedTok { tok: Tok::Concat, span: Span::new(i, i + 2) });
                    i += 2;
                } else {
                    return Err(Diagnostic::new(
                        "unexpected '|'",
                        Span::new(i, i + 1),
                    )
                    .with_hint("string concatenation is '||' (or concat(a, b))"));
                }
            }
            b'<' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push(SpannedTok { tok: Tok::Le, span: Span::new(i, i + 2) });
                    i += 2;
                } else if b.get(i + 1) == Some(&b'>') {
                    out.push(SpannedTok { tok: Tok::Ne, span: Span::new(i, i + 2) });
                    i += 2;
                } else {
                    push1(&mut out, Tok::Lt, &mut i)
                }
            }
            b'>' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push(SpannedTok { tok: Tok::Ge, span: Span::new(i, i + 2) });
                    i += 2;
                } else {
                    push1(&mut out, Tok::Gt, &mut i)
                }
            }
            b'!' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push(SpannedTok { tok: Tok::Ne, span: Span::new(i, i + 2) });
                    i += 2;
                } else {
                    return Err(Diagnostic::new("unexpected '!'", Span::new(i, i + 1))
                        .with_hint("not-equals is '!=' or '<>'"));
                }
            }
            b'\'' => {
                // 'string' with '' as the escaped quote
                let mut s = String::new();
                i += 1;
                loop {
                    match b.get(i) {
                        None => {
                            return Err(Diagnostic::new(
                                "unterminated string literal",
                                Span::new(start, src.len()),
                            )
                            .with_hint("close it with ' (a literal quote inside is written '')"))
                        }
                        Some(b'\'') if b.get(i + 1) == Some(&b'\'') => {
                            s.push('\'');
                            i += 2;
                        }
                        Some(b'\'') => {
                            i += 1;
                            break;
                        }
                        Some(_) => {
                            // consume one UTF-8 scalar
                            let ch_len = utf8_len(b[i]);
                            s.push_str(&src[i..i + ch_len]);
                            i += ch_len;
                        }
                    }
                }
                out.push(SpannedTok { tok: Tok::Str(s), span: Span::new(start, i) });
            }
            b'"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match b.get(i) {
                        None => {
                            return Err(Diagnostic::new(
                                "unterminated quoted identifier",
                                Span::new(start, src.len()),
                            ))
                        }
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(_) => {
                            let ch_len = utf8_len(b[i]);
                            s.push_str(&src[i..i + ch_len]);
                            i += ch_len;
                        }
                    }
                }
                out.push(SpannedTok { tok: Tok::QuotedIdent(s), span: Span::new(start, i) });
            }
            b'0'..=b'9' | b'.' => {
                let mut j = i;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if b.get(j) == Some(&b'.') {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                let text = &src[i..j];
                let n: f64 = text.parse().map_err(|_| {
                    Diagnostic::new(format!("'{text}' is not a number"), Span::new(i, j))
                })?;
                out.push(SpannedTok { tok: Tok::Number(n), span: Span::new(i, j) });
                i = j;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let mut j = i;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                let word = &src[i..j];
                let lower = word.to_ascii_lowercase();
                let tok = KEYWORDS
                    .iter()
                    .find(|(k, _)| *k == lower)
                    .map(|(_, t)| t.clone())
                    .unwrap_or(Tok::Ident(lower));
                out.push(SpannedTok { tok, span: Span::new(i, j) });
                i = j;
            }
            _ => {
                let ch_len = utf8_len(c);
                return Err(Diagnostic::new(
                    format!("unexpected character '{}'", &src[i..i + ch_len]),
                    Span::new(i, i + ch_len),
                ));
            }
        }
    }
    out.push(SpannedTok { tok: Tok::Eof, span: Span::new(src.len(), src.len()) });
    Ok(out)
}

fn push1(out: &mut Vec<SpannedTok>, tok: Tok, i: &mut usize) {
    out.push(SpannedTok { tok, span: Span::new(*i, *i + 1) });
    *i += 1;
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}
