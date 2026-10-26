//! Renders an expanded token stream back to text for tree-sitter, carrying a
//! location map from rendered byte offsets to original source offsets.
//!
//! Whitespace is inserted only where omitting it would merge two tokens, so the
//! rendered buffer stays close to the size of the token spellings.

use crate::intern::{SymId, Symbols};
use crate::lex::{Kind, Token, F_BOL, F_WS};

pub struct Rendered {
    pub text: String,
    /// Rendered byte offset of each token, parallel to `src_off`.
    starts: Vec<u32>,
    /// Original source byte offset of each token.
    src_off: Vec<u32>,
    /// Interned spelling of each token, so symbol extraction can read identity
    /// straight off the token rather than re-interning slices of the text.
    syms: Vec<SymId>,
    /// Presence condition of each token.
    pcs: Vec<u32>,
    /// Last token index resolved. Symbol extraction walks the tree in source
    /// order, so consecutive lookups are almost always adjacent; without this
    /// every one of the kernel's 99M occurrences pays a ~19-step binary search.
    hint: std::cell::Cell<usize>,
}

impl Rendered {
    /// Index of the token covering a byte offset in the rendered text.
    pub fn to_token(&self, rendered_off: u32) -> Option<usize> {
        if self.starts.is_empty() {
            return None;
        }

        // Try just after the previous answer before falling back to a search.
        let h = self.hint.get();
        if h < self.starts.len() && self.starts[h] <= rendered_off {
            let end = (h + 8).min(self.starts.len());
            for i in h..end {
                if self.starts[i] > rendered_off {
                    self.hint.set(i - 1);
                    return Some(i - 1);
                }
            }
            if end == self.starts.len() && self.starts[end - 1] <= rendered_off {
                self.hint.set(end - 1);
                return Some(end - 1);
            }
        }

        let found = match self.starts.binary_search(&rendered_off) {
            Ok(i) => Some(i),
            Err(0) => None,
            Err(i) => Some(i - 1),
        };
        if let Some(i) = found {
            self.hint.set(i);
        }
        found
    }

    /// Maps a byte offset in the rendered text back to an offset in the
    /// original source file.
    pub fn to_source(&self, rendered_off: u32) -> Option<u32> {
        self.to_token(rendered_off).map(|i| self.src_off[i])
    }

    /// Interned spelling of the token covering a rendered offset.
    pub fn sym_at(&self, rendered_off: u32) -> Option<SymId> {
        self.to_token(rendered_off).map(|i| self.syms[i])
    }

    /// Presence condition of the token covering a rendered offset.
    pub fn pc_at(&self, rendered_off: u32) -> Option<u32> {
        self.to_token(rendered_off).map(|i| self.pcs[i])
    }

    pub fn tokens(&self) -> usize {
        self.starts.len()
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// True when `a` immediately followed by `b` would lex as something other than
/// the two tokens themselves.
fn would_merge(a: &str, ak: Kind, b: &str, bk: Kind) -> bool {
    let Some(last) = a.as_bytes().last().copied() else {
        return false;
    };
    let Some(first) = b.as_bytes().first().copied() else {
        return false;
    };

    let word_end = last.is_ascii_alphanumeric() || last == b'_' || last == b'$' || last >= 0x80;
    let word_start =
        first.is_ascii_alphanumeric() || first == b'_' || first == b'$' || first >= 0x80;
    if word_end && word_start {
        return true;
    }

    // `L` + `"x"` would lex as a wide string literal.
    if ak == Kind::Ident && matches!(bk, Kind::Str | Kind::Char) {
        return true;
    }

    // A pp-number absorbs `+`/`-` after an exponent marker, and absorbs a
    // following `.` or identifier character.
    if ak == Kind::Number {
        if matches!(last, b'e' | b'E' | b'p' | b'P') && (first == b'+' || first == b'-') {
            return true;
        }
        if first == b'.' {
            return true;
        }
    }
    // `.` + digit lexes as a pp-number.
    if first.is_ascii_digit() && last == b'.' {
        return true;
    }

    // Two punctuators whose junction forms a longer punctuator.
    if ak == Kind::Punct && bk == Kind::Punct {
        return matches!(
            (last, first),
            (b'+', b'+')
                | (b'-', b'-')
                | (b'-', b'>')
                | (b'<', b'<')
                | (b'>', b'>')
                | (b'&', b'&')
                | (b'|', b'|')
                | (b'#', b'#')
                | (b'<', b':')
                | (b'<', b'%')
                | (b'%', b':')
                | (b'%', b'>')
                | (b':', b'>')
                | (b'.', b'.')
                | (b'=', b'=')
                | (b'!', b'=')
                | (b'<', b'=')
                | (b'>', b'=')
                | (b'*', b'=')
                | (b'/', b'=')
                | (b'%', b'=')
                | (b'+', b'=')
                | (b'-', b'=')
                | (b'&', b'=')
                | (b'^', b'=')
                | (b'|', b'=')
                | (b'/', b'*')
                | (b'/', b'/')
        );
    }

    false
}

/// Token count of an `__attribute__((...))` run starting at `i`.
///
/// `None` when this is not an attribute, or when its parens never close — an
/// unbalanced run is emitted verbatim rather than swallowing the rest of the
/// file.
fn attribute_run<S: Symbols>(toks: &[Token], i: usize, it: &S) -> Option<usize> {
    let s = it.text(toks[i].sym);
    if s.len() != 13 && s.len() != 11 {
        return None;
    }
    if s != "__attribute__" && s != "__attribute" {
        return None;
    }
    if it.text(toks.get(i + 1)?.sym) != "(" {
        return None;
    }

    let mut depth = 0usize;
    for (n, t) in toks.iter().enumerate().skip(i + 1) {
        match it.text(t.sym) {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some(n + 1 - i);
                }
            }
            _ => {}
        }
    }
    None
}

pub fn render<S: Symbols>(toks: &[Token], it: &S) -> Rendered {
    let mut text = String::with_capacity(toks.len() * 6);
    let mut starts = Vec::with_capacity(toks.len());
    let mut src_off = Vec::with_capacity(toks.len());
    let mut syms = Vec::with_capacity(toks.len());
    let mut pcs = Vec::with_capacity(toks.len());

    let mut prev: Option<(&str, Kind)> = None;

    let mut i = 0usize;
    while i < toks.len() {
        let t = &toks[i];
        let s = it.text(t.sym);

        // `T * __attribute__((...)) name(...)` does not parse in tree-sitter-c:
        // an attribute between the `*` and the name breaks the declarator, and
        // the whole function is lost. The kernel writes this constantly, via
        // `__must_check` and friends — it is why `ERR_PTR` and `ERR_CAST` had no
        // definition while their non-pointer neighbours in the same header did.
        //
        // Attributes carry nothing a reader navigates to, so they are dropped
        // rather than worked around. An unbalanced one is left alone.
        if let Some(n) = attribute_run(toks, i, it) {
            i += n;
            continue;
        }
        // `asm __inline volatile(...)` — the kernel's `asm_inline`. Another
        // construct tree-sitter-c rejects, and another pure compiler hint.
        if matches!(s, "__inline" | "__inline__")
            && matches!(prev, Some(("asm" | "__asm__" | "__asm", _)))
        {
            i += 1;
            continue;
        }
        // `__builtin_offsetof(struct T, m)` takes a *type* as its first
        // argument, which tree-sitter-c cannot parse in expression position —
        // and the error takes the whole enclosing function with it. Dropping
        // just the `struct`/`union`/`enum` keyword leaves an ordinary call,
        // which parses, and keeps both the type name and the member navigable.
        //
        // This is not a niche construct: `offsetof` expands to it, and
        // `container_of` expands to `offsetof`, so it appears in a large share
        // of kernel functions.
        if matches!(s, "struct" | "union" | "enum")
            && i >= 2
            && it.text(toks[i - 1].sym) == "("
            && it.text(toks[i - 2].sym) == "__builtin_offsetof"
        {
            i += 1;
            continue;
        }
        i += 1;

        if t.flags & F_BOL != 0 && !text.is_empty() {
            text.push('\n');
        } else if let Some((ps, pk)) = prev {
            if t.flags & F_WS != 0 || would_merge(ps, pk, s, t.kind) {
                text.push(' ');
            }
        }

        starts.push(text.len() as u32);
        src_off.push(t.off);
        syms.push(t.sym);
        pcs.push(t.pc);
        text.push_str(s);
        prev = Some((s, t.kind));
    }

    Rendered {
        text,
        starts,
        src_off,
        syms,
        pcs,
        hint: std::cell::Cell::new(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expand::{preprocess, MacroTable};
    use crate::intern::Interner;
    use crate::lex::{lex, WellKnown};

    fn round_trip(src: &str) -> String {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = crate::expand::Keywords::new(&mut it);
        let mut pcs = crate::pc::PcStore::new();
        let toks = lex(src.as_bytes(), &mut it);
        let mut mt = MacroTable::default();
        let (out, _) = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
        render(&out, &it).text
    }

    /// Rendering then re-lexing must produce the same token spellings.
    fn assert_stable(src: &str) {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = crate::expand::Keywords::new(&mut it);
        let mut pcs = crate::pc::PcStore::new();
        let toks = lex(src.as_bytes(), &mut it);
        let mut mt = MacroTable::default();
        let (out, _) = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
        let want: Vec<String> = out.iter().map(|t| it.text(t.sym).to_string()).collect();

        let r = render(&out, &it);
        let mut it2 = Interner::new();
        let relexed = lex(r.text.as_bytes(), &mut it2);
        let got: Vec<String> = relexed
            .iter()
            .map(|t| it2.text(t.sym).to_string())
            .collect();

        assert_eq!(want, got, "rendered: {:?}", r.text);
    }

    #[test]
    fn attributes_are_dropped() {
        // tree-sitter-c cannot parse an attribute between `*` and the name, and
        // loses the entire function. Attributes navigate to nothing, so they go.
        assert_eq!(
            round_trip("#define A __attribute__((x))\nvoid * A f(long e);"),
            "void * f(long e);"
        );
        assert_eq!(round_trip("int __attribute__((aligned(8))) v;"), "int v;");
        assert_eq!(
            round_trip("void __attribute__((a)) __attribute__((b)) g();"),
            "void g();"
        );
    }

    #[test]
    fn asm_inline_loses_its_hint() {
        // `asm __inline volatile(...)` is the kernel's asm_inline; tree-sitter-c
        // rejects the `__inline` and the error takes the rest of the file.
        assert_eq!(
            round_trip("void f(void){ asm __inline volatile(\"nop\"); }"),
            "void f(void){ asm volatile(\"nop\"); }"
        );
        // Only directly after asm — `__inline` is a legal function specifier.
        assert_eq!(round_trip("__inline int f(void);"), "__inline int f(void);");
    }

    #[test]
    fn offsetof_loses_only_its_type_keyword() {
        // The member and the type name both stay — they are what a reader
        // navigates to. Only the keyword, which is what breaks the parse, goes.
        // The space it leaves behind is inherited from the dropped token and
        // costs nothing: whitespace is not significant to the parser.
        assert_eq!(
            round_trip("int x = __builtin_offsetof(struct T, m);"),
            "int x = __builtin_offsetof( T, m);"
        );
        assert_eq!(
            round_trip("int x = __builtin_offsetof(union U, m);"),
            "int x = __builtin_offsetof( U, m);"
        );
        // A `struct` keyword anywhere else is untouched.
        assert_eq!(round_trip("struct T v;"), "struct T v;");
        assert_eq!(round_trip("int f(struct T *p);"), "int f(struct T *p);");
        assert_eq!(
            round_trip("int x = sizeof(struct T);"),
            "int x = sizeof(struct T);"
        );
    }

    #[test]
    fn an_unbalanced_attribute_is_left_alone() {
        // Better to emit it verbatim than to swallow the rest of the file.
        let out = round_trip("void __attribute__((a f();");
        assert!(out.contains("__attribute__"), "got {out:?}");
    }

    #[test]
    fn a_bare_attribute_identifier_survives() {
        // Only `__attribute__` *followed by a paren* is a run to skip.
        assert_eq!(round_trip("int __attribute__;"), "int __attribute__;");
    }

    #[test]
    fn no_spurious_whitespace() {
        assert_eq!(round_trip("int x=1;"), "int x=1;");
    }

    #[test]
    fn separates_adjacent_identifiers() {
        assert_eq!(round_trip("#define A int\nA x;"), "int x;");
    }

    #[test]
    fn separates_punctuators_that_would_merge() {
        assert_stable("a+ +b");
        assert_stable("a- -b");
        assert_stable("a/ *b");
        assert_stable("x< <y");
    }

    #[test]
    fn separates_number_exponent_from_sign() {
        assert_stable("1e +2");
        assert_stable("x = 1.0 .5;");
    }

    #[test]
    fn separates_ident_from_string_literal() {
        assert_stable("L \"abc\"");
    }

    #[test]
    fn macro_expansion_stays_relexable() {
        assert_stable("#define NEG(x) -x\nint y = 1 - NEG(2);");
        assert_stable("#define CAT(a,b) a##b\nCAT(foo,bar) CAT(1,2);");
    }

    #[test]
    fn location_map_points_back_to_source() {
        let src = "#define A 42\nint value = A;";
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = crate::expand::Keywords::new(&mut it);
        let mut pcs = crate::pc::PcStore::new();
        let toks = lex(src.as_bytes(), &mut it);
        let mut mt = MacroTable::default();
        let (out, _) = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
        let r = render(&out, &it);

        // `42` in the output came from the macro body, so it must map back to
        // the invocation site, not to the #define line.
        let pos = r.text.find("42").unwrap() as u32;
        let mapped = r.to_source(pos).unwrap() as usize;
        assert_eq!(&src[mapped..mapped + 1], "A");

        // `value` maps to itself.
        let pos = r.text.find("value").unwrap() as u32;
        let mapped = r.to_source(pos).unwrap() as usize;
        assert!(src[mapped..].starts_with("value"));
    }

    /// The hint path must agree with a plain binary search for every offset,
    /// including out-of-order queries that defeat the hint. A divergence here
    /// silently mis-attributes symbols rather than failing.
    #[test]
    fn token_lookup_hint_agrees_with_binary_search() {
        let mut src = String::new();
        for i in 0..4000 {
            src.push_str(&format!("int var_{i} = {i};\n"));
        }
        let mut it = Interner::new();
        let toks = lex(src.as_bytes(), &mut it);
        let r = render(&toks, &it);

        let reference = |off: u32| -> Option<usize> {
            match r.starts.binary_search(&off) {
                Ok(i) => Some(i),
                Err(0) => None,
                Err(i) => Some(i - 1),
            }
        };

        // Forward, which the hint handles.
        for off in (0..r.text.len() as u32).step_by(7) {
            assert_eq!(r.to_token(off), reference(off), "forward at {off}");
        }
        // Backward and scattered, which it must not get wrong either.
        for off in (0..r.text.len() as u32).step_by(13).rev() {
            assert_eq!(r.to_token(off), reference(off), "backward at {off}");
        }
        for off in [0u32, 1, 5, 999, 12345, r.text.len() as u32 - 1] {
            assert_eq!(r.to_token(off), reference(off), "scattered at {off}");
        }
    }

    #[test]
    fn offsets_inside_a_token_map_to_that_token() {
        let src = "int abcdef;";
        let mut it = Interner::new();
        let toks = lex(src.as_bytes(), &mut it);
        let r = render(&toks, &it);
        let start = r.text.find("abcdef").unwrap() as u32;
        assert_eq!(r.to_source(start + 3), r.to_source(start));
    }
}
