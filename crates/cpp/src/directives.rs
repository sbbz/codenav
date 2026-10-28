//! A file's preprocessor directives, cached in the index.
//!
//! To re-expand one file, an update needs the macros its whole include closure
//! defines — about 1,000 headers on the kernel. Re-lexing all of them was most
//! of the cost of a one-file update. A full build already lexed every file, so
//! it stores just the tokens of each directive line, and an update reads those
//! back instead of the files.
//!
//! Tokens keep their original offsets, so [`crate::global::collect_defines`]
//! sees exactly what it would see lexing the whole file: it only ever looks at
//! directive lines.

use crate::intern::Symbols;
use crate::lex::{Kind, Token, WellKnown};

/// Encodes the directive lines of `toks`: each token as kind, flags, offset
/// and spelling.
pub fn encode<S: Symbols>(toks: &[Token], wk: &WellKnown, it: &S) -> Vec<u8> {
    let mut out = Vec::new();
    let mut in_directive = false;
    for t in toks {
        if t.bol() {
            in_directive = t.kind == Kind::Punct && t.sym == wk.hash;
        }
        if !in_directive {
            continue;
        }
        let text = it.text(t.sym).as_bytes();
        out.push(t.kind as u8);
        out.push(t.flags);
        out.extend_from_slice(&t.off.to_le_bytes());
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text);
    }
    out
}

/// Decodes what [`encode`] wrote, interning spellings into `it`. `None` if the
/// bytes are malformed, in which case the caller reads the file instead.
pub fn decode<S: Symbols>(mut b: &[u8], it: &mut S) -> Option<Vec<Token>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        if b.len() < 10 {
            return None;
        }
        let kind = match b[0] {
            0 => Kind::Ident,
            1 => Kind::Number,
            2 => Kind::Str,
            3 => Kind::Char,
            4 => Kind::Punct,
            _ => return None,
        };
        let flags = b[1];
        let off = u32::from_le_bytes(b[2..6].try_into().ok()?);
        let len = u32::from_le_bytes(b[6..10].try_into().ok()?) as usize;
        let text = b.get(10..10 + len)?;
        let sym = it.intern_bytes(text);
        out.push(Token {
            kind,
            flags,
            sym,
            off,
            pc: 0,
        });
        b = &b[10 + len..];
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expand::Keywords;
    use crate::global::{collect_defines, GlobalMacros};
    use crate::intern::Interner;
    use crate::lex::lex;
    use crate::pc::PcStore;

    const SRC: &str = "#include <linux/types.h>\n\
        /* a comment that /* spans\n   lines */ int before;\n\
        #ifdef CONFIG_X\n\
        #define LIMIT(x) \\\n    ((x) + OFFSET)\n\
        #else\n\
        #define LIMIT(x) (x)\n\
        #endif\n\
        int body(void) { return LIMIT(1); }\n\
        #define OFFSET 4 /* trailing */\n\
        #undef GONE\n";

    fn defines(toks: &[Token], it: &mut Interner, wk: &WellKnown, kw: &Keywords) -> u64 {
        let mut pcs = PcStore::new();
        let mut gm = GlobalMacros::default();
        collect_defines(0, toks, wk, kw, it, &mut pcs, &mut gm);
        gm.export_signatures()[&0]
    }

    #[test]
    fn the_cache_yields_the_same_macros_as_the_file() {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = Keywords::new(&mut it);
        let toks = lex(SRC.as_bytes(), &mut it);
        let cached = decode(&encode(&toks, &wk, &it), &mut it).unwrap();

        assert_eq!(
            defines(&toks, &mut it, &wk, &kw),
            defines(&cached, &mut it, &wk, &kw)
        );
        // Only directive lines are kept, at their original offsets.
        assert!(cached.len() < toks.len());
        let first = cached.iter().find(|t| it.text(t.sym) == "LIMIT").unwrap();
        assert_eq!(first.off as usize, SRC.find("LIMIT").unwrap());
    }

    #[test]
    fn malformed_bytes_are_refused_rather_than_misread() {
        let mut it = Interner::new();
        assert!(decode(&[0, 0, 1], &mut it).is_none());
        assert!(decode(&[9, 0, 0, 0, 0, 0, 0, 0, 0, 0], &mut it).is_none());
    }
}
