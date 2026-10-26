//! C preprocessing-token lexer.
//!
//! Produces a flat token vector with every spelling interned, which is what the
//! expander needs for `#` stringize and `##` paste. Comments become nothing,
//! line continuations are invisible, and each token records whether it began a
//! line so the directive parser can find `#` at line start.

use crate::intern::{SymId, Symbols};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Ident,
    Number,
    Str,
    Char,
    Punct,
}

pub const F_BOL: u8 = 1 << 0;
pub const F_WS: u8 = 1 << 1;

#[derive(Debug, Clone, Copy)]
pub struct Token {
    pub kind: Kind,
    pub flags: u8,
    /// Interned spelling.
    pub sym: SymId,
    /// Byte offset in the original file. Survives expansion so the location map
    /// can point back here.
    pub off: u32,
    /// The `crate::pc::PcId` under which this token exists. Every branch of
    /// every conditional is emitted into one stream, so this is what keeps them
    /// distinguishable downstream. Held as a plain u32 to keep `lex` free of a
    /// dependency on `pc`.
    pub pc: u32,
}

impl Token {
    #[inline]
    pub fn is(&self, kind: Kind, sym: SymId) -> bool {
        self.kind == kind && self.sym == sym
    }
    #[inline]
    pub fn bol(&self) -> bool {
        self.flags & F_BOL != 0
    }
}

/// Punctuator spellings, longest first within each starting character so the
/// maximal-munch scan is a simple prefix test.
const PUNCTS: &[&str] = &[
    "%:%:", "...", "<<=", ">>=", "->", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||",
    "*=", "/=", "%=", "+=", "-=", "&=", "^=", "|=", "##", "<:", ":>", "<%", "%>", "%:", "[", "]",
    "(", ")", "{", "}", ".", "&", "*", "+", "-", "~", "!", "/", "%", "<", ">", "^", "|", "?", ":",
    ";", "=", ",", "#", "@", "$", "\\",
];

pub struct WellKnown {
    pub lparen: SymId,
    pub rparen: SymId,
    pub comma: SymId,
    pub hash: SymId,
    pub hashhash: SymId,
    pub ellipsis: SymId,
    pub va_args: SymId,
    pub defined: SymId,
    pub andand: SymId,
    pub oror: SymId,
    pub bang: SymId,
}

impl WellKnown {
    pub fn new(it: &mut impl Symbols) -> Self {
        Self {
            lparen: it.intern("("),
            rparen: it.intern(")"),
            comma: it.intern(","),
            hash: it.intern("#"),
            hashhash: it.intern("##"),
            ellipsis: it.intern("..."),
            va_args: it.intern("__VA_ARGS__"),
            defined: it.intern("defined"),
            andand: it.intern("&&"),
            oror: it.intern("||"),
            bang: it.intern("!"),
        }
    }
}

#[inline]
fn ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

#[inline]
fn ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

pub fn lex<S: Symbols>(src: &[u8], it: &mut S) -> Vec<Token> {
    let mut out = Vec::with_capacity(src.len() / 6);
    let n = src.len();
    let mut i = 0usize;
    let mut flags = F_BOL;

    // Scratch buffer for spellings that contain a line continuation and so are
    // not contiguous in the source.
    let mut spell = Vec::new();

    while i < n {
        let b = src[i];

        if b == b'\\' && i + 1 < n && (src[i + 1] == b'\n' || src[i + 1] == b'\r') {
            i += if src[i + 1] == b'\r' && i + 2 < n && src[i + 2] == b'\n' {
                3
            } else {
                2
            };
            continue;
        }
        if b == b'\n' {
            flags = F_BOL;
            i += 1;
            continue;
        }
        if b == b' ' || b == b'\t' || b == b'\r' || b == 0x0b || b == 0x0c {
            flags |= F_WS;
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < n && src[i + 1] == b'/' {
            while i < n && src[i] != b'\n' {
                if src[i] == b'\\' && i + 1 < n && src[i + 1] == b'\n' {
                    i += 1;
                }
                i += 1;
            }
            flags |= F_WS;
            continue;
        }
        if b == b'/' && i + 1 < n && src[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(src[i] == b'*' && src[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            flags |= F_WS;
            continue;
        }

        let off = i as u32;

        // String and character literals, including the u8/u/U/L prefixes.
        let lit_quote = if b == b'"' || b == b'\'' {
            Some(b)
        } else if (b == b'u' || b == b'U' || b == b'L') && i + 1 < n {
            let mut j = i + 1;
            if b == b'u' && j < n && src[j] == b'8' {
                j += 1;
            }
            if j < n && (src[j] == b'"' || src[j] == b'\'') {
                Some(src[j])
            } else {
                None
            }
        } else {
            None
        };

        if let Some(q) = lit_quote {
            spell.clear();
            while i < n && src[i] != q {
                spell.push(src[i]);
                i += 1;
            }
            if i < n {
                spell.push(src[i]);
                i += 1;
            }
            loop {
                if i >= n {
                    break;
                }
                let c = src[i];
                if c == b'\\' && i + 1 < n {
                    if src[i + 1] == b'\n' {
                        i += 2;
                        continue;
                    }
                    spell.push(c);
                    spell.push(src[i + 1]);
                    i += 2;
                    continue;
                }
                spell.push(c);
                i += 1;
                if c == q {
                    break;
                }
                if c == b'\n' {
                    break; // unterminated literal; don't swallow the file
                }
            }
            out.push(Token {
                kind: if q == b'"' { Kind::Str } else { Kind::Char },
                flags,
                sym: it.intern_bytes(&spell),
                off,
                pc: 0,
            });
            flags = 0;
            continue;
        }

        if ident_start(b) {
            let (sym, next) = scan_run(src, i, it, &mut spell, ident_cont);
            out.push(Token {
                kind: Kind::Ident,
                flags,
                sym,
                off,
                pc: 0,
            });
            i = next;
            flags = 0;
            continue;
        }

        // pp-number: digit, or '.' followed by a digit.
        if b.is_ascii_digit() || (b == b'.' && i + 1 < n && src[i + 1].is_ascii_digit()) {
            spell.clear();
            spell.push(src[i]);
            i += 1;
            while i < n {
                let c = src[i];
                if c == b'\\' && i + 1 < n && src[i + 1] == b'\n' {
                    i += 2;
                    continue;
                }
                // Exponent signs are part of the pp-number.
                if (c == b'+' || c == b'-')
                    && matches!(
                        spell.last().copied(),
                        Some(b'e') | Some(b'E') | Some(b'p') | Some(b'P')
                    )
                {
                    spell.push(c);
                    i += 1;
                    continue;
                }
                if ident_cont(c) || c == b'.' {
                    spell.push(c);
                    i += 1;
                    continue;
                }
                break;
            }
            out.push(Token {
                kind: Kind::Number,
                flags,
                sym: it.intern_bytes(&spell),
                off,
                pc: 0,
            });
            flags = 0;
            continue;
        }

        // Punctuator, maximal munch.
        let rest = &src[i..];
        let mut matched = None;
        for p in PUNCTS {
            let pb = p.as_bytes();
            if pb[0] == b && rest.len() >= pb.len() && &rest[..pb.len()] == pb {
                matched = Some(*p);
                break;
            }
        }
        let p = matched.unwrap_or("@");
        let len = if matched.is_some() { p.len() } else { 1 };
        out.push(Token {
            kind: Kind::Punct,
            flags,
            sym: it.intern(p),
            off,
            pc: 0,
        });
        i += len;
        flags = 0;
    }

    out
}

/// Scans an identifier run, using the scratch buffer only if a line
/// continuation splits it (rare), so the common case avoids a copy.
#[inline]
fn scan_run<S: Symbols>(
    src: &[u8],
    start: usize,
    it: &mut S,
    spell: &mut Vec<u8>,
    cont: fn(u8) -> bool,
) -> (SymId, usize) {
    let n = src.len();
    let mut i = start;
    while i < n && cont(src[i]) {
        i += 1;
    }
    if i < n && src[i] == b'\\' && i + 1 < n && src[i + 1] == b'\n' {
        spell.clear();
        spell.extend_from_slice(&src[start..i]);
        while i < n {
            if src[i] == b'\\' && i + 1 < n && src[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            if !cont(src[i]) {
                break;
            }
            spell.push(src[i]);
            i += 1;
        }
        return (it.intern_bytes(spell), i);
    }
    (it.intern_bytes(&src[start..i]), i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intern::Interner;

    fn toks(s: &str) -> (Vec<Token>, Interner) {
        let mut it = Interner::new();
        let t = lex(s.as_bytes(), &mut it);
        (t, it)
    }

    #[test]
    fn basic_tokens() {
        let (t, it) = toks("int x = 1;");
        let spelled: Vec<&str> = t.iter().map(|k| it.text(k.sym)).collect();
        assert_eq!(spelled, ["int", "x", "=", "1", ";"]);
    }

    #[test]
    fn comments_vanish_and_leave_whitespace() {
        let (t, it) = toks("a/*c*/b // z\nc");
        let spelled: Vec<&str> = t.iter().map(|k| it.text(k.sym)).collect();
        assert_eq!(spelled, ["a", "b", "c"]);
        assert!(t[2].bol());
    }

    #[test]
    fn line_continuation_joins_identifier() {
        let (t, it) = toks("foo\\\nbar");
        assert_eq!(t.len(), 1);
        assert_eq!(it.text(t[0].sym), "foobar");
    }

    #[test]
    fn maximal_munch_punctuators() {
        let (t, it) = toks("a>>=b ... c##d");
        let spelled: Vec<&str> = t.iter().map(|k| it.text(k.sym)).collect();
        assert_eq!(spelled, ["a", ">>=", "b", "...", "c", "##", "d"]);
    }

    #[test]
    fn strings_with_escaped_quotes_and_braces() {
        let (t, it) = toks(r#"char *p = "a\"{b}";"#);
        let spelled: Vec<&str> = t.iter().map(|k| it.text(k.sym)).collect();
        assert_eq!(spelled, ["char", "*", "p", "=", r#""a\"{b}""#, ";"]);
    }

    #[test]
    fn pp_numbers_absorb_exponent_signs() {
        let (t, it) = toks("1.5e-10 0x1p+3 3UL");
        let spelled: Vec<&str> = t.iter().map(|k| it.text(k.sym)).collect();
        assert_eq!(spelled, ["1.5e-10", "0x1p+3", "3UL"]);
    }

    #[test]
    fn bol_flag_marks_directive_lines() {
        let (t, it) = toks("x\n#define A 1\ny");
        let hash = t.iter().find(|k| it.text(k.sym) == "#").unwrap();
        assert!(hash.bol());
    }

    #[test]
    fn wide_and_prefixed_literals() {
        let (t, it) = toks(r#"L"w" u8"x" U'c'"#);
        let spelled: Vec<&str> = t.iter().map(|k| it.text(k.sym)).collect();
        assert_eq!(spelled, [r#"L"w""#, r#"u8"x""#, "U'c'"]);
    }
}
