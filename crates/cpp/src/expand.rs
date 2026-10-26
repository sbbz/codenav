//! Guarded macro table and expander.
//!
//! Because every `#ifdef` branch is kept, a macro name can carry several
//! definitions at once, each tagged with the presence condition under which it
//! applies. Lookup filters by compatibility with the use site's condition.

use crate::intern::{SymId, Symbols};
use crate::lex::{Kind, Token, WellKnown, F_BOL, F_WS};
use crate::pc::{PcId, PcStore, PC_TRUE};
use rustc_hash::FxHashMap;

#[derive(Debug, Clone)]
pub struct MacroDef {
    pub is_fn: bool,
    pub variadic: bool,
    pub params: Vec<SymId>,
    pub body: Vec<Token>,
    /// Byte offset of the macro name at its definition site.
    pub off: u32,
    /// Condition under which this definition is the live one.
    pub pc: PcId,
    /// `#undef` under `pc`, rather than a definition.
    pub undef: bool,
}

/// Beyond this many live definitions for one name, expansion goes opaque rather
/// than guessing. Kernel names like `pr_fmt` have dozens of per-file variants.
pub const MAX_VARIANTS: usize = 4;

#[derive(Default)]
pub struct MacroTable {
    defs: FxHashMap<SymId, Vec<MacroDef>>,
}

impl MacroTable {
    #[inline]
    pub fn variants(&self, s: SymId) -> &[MacroDef] {
        self.defs.get(&s).map(|v| &v[..]).unwrap_or(&[])
    }

    /// Definitions whose guard does not contradict `at`. An unconditional
    /// definition wins outright, matching the common case of a header that
    /// defines a name once.
    pub fn lookup(&self, s: SymId, at: PcId, pcs: &PcStore) -> Option<&MacroDef> {
        let v = self.defs.get(&s)?;
        if v.len() == 1 {
            let d = &v[0];
            return (!d.undef && !pcs.contradicts(d.pc, at)).then_some(d);
        }
        // Later definitions shadow earlier ones, so scan backwards.
        let mut found: Option<&MacroDef> = None;
        let mut compatible = 0usize;
        for d in v.iter().rev() {
            if pcs.contradicts(d.pc, at) {
                continue;
            }
            compatible += 1;
            if found.is_none() {
                found = Some(d);
            }
        }
        if compatible > MAX_VARIANTS {
            return None; // too ambiguous to expand; stays navigable as a macro
        }
        found.filter(|d| !d.undef)
    }

    pub fn insert(&mut self, name: SymId, def: MacroDef) {
        let v = self.defs.entry(name).or_default();
        // An identical guard replaces rather than accumulates, which is what
        // keeps re-including a header from growing the table.
        if let Some(slot) = v.iter_mut().find(|d| d.pc == def.pc) {
            *slot = def;
            return;
        }
        // Unconditional definitions supersede everything before them.
        if def.pc == PC_TRUE {
            v.clear();
        }
        v.push(def);
    }

    pub fn undef(&mut self, name: SymId, pc: PcId, off: u32) {
        if pc == PC_TRUE {
            self.defs.remove(&name);
            return;
        }
        self.insert(
            name,
            MacroDef {
                is_fn: false,
                variadic: false,
                params: Vec::new(),
                body: Vec::new(),
                off,
                pc,
                undef: true,
            },
        );
    }

    /// Distinct macro names.
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Definitions across all names, including guarded variants.
    pub fn total_defs(&self) -> usize {
        self.defs.values().map(|v| v.len()).sum()
    }

    pub fn multi_defined(&self) -> usize {
        self.defs.values().filter(|v| v.len() > 1).count()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

/// Where the expander gets macro definitions. Implemented both by the plain
/// per-TU `MacroTable` and by the closure-scoped global table.
pub trait MacroSource {
    /// The definition to expand at a use site under condition `at`, or None if
    /// the name is not a macro here or is too ambiguous to expand.
    fn lookup_def(&self, name: SymId, at: PcId, pcs: &PcStore) -> Option<&MacroDef>;
    /// Whether the name is a macro anywhere. Distinguishes "not a macro" from
    /// "a macro that did not apply here", which is worth counting separately.
    fn is_known(&self, name: SymId) -> bool;
}

impl MacroSource for MacroTable {
    fn lookup_def(&self, name: SymId, at: PcId, pcs: &PcStore) -> Option<&MacroDef> {
        self.lookup(name, at, pcs)
    }
    fn is_known(&self, name: SymId) -> bool {
        !self.variants(name).is_empty()
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ExpStats {
    pub expansions: u64,
    pub pastes: u64,
    pub stringizes: u64,
    /// Expansions abandoned because they hit the depth or token budget. These
    /// become `LOSSY` regions: the macro name is still navigable, but its
    /// generated symbols are not.
    pub budget_hits: u64,
    /// Identifier uses that name a macro somewhere but were not expanded here.
    /// Covers both "defined in a file this one does not include" (correct) and
    /// "too many competing meanings" (a real gap). `LookupStats` separates them.
    pub unexpanded_known: u64,
    pub tokens_out: u64,
}

const MAX_DEPTH: u32 = 64;
/// Per top-level expansion. Guards against pathological recursive macros
/// without capping legitimately large ones.
const TOKEN_BUDGET: i64 = 1 << 16;

pub struct Expander<'a, M: MacroSource, S: Symbols> {
    pub mt: &'a M,
    pub it: &'a mut S,
    pub wk: &'a WellKnown,
    pub pcs: &'a PcStore,
    pub stats: ExpStats,
    active: Vec<SymId>,
    depth: u32,
    budget: i64,
    scratch: String,
}

impl<'a, M: MacroSource, S: Symbols> Expander<'a, M, S> {
    pub fn new(mt: &'a M, it: &'a mut S, wk: &'a WellKnown, pcs: &'a PcStore) -> Self {
        Self {
            mt,
            it,
            wk,
            pcs,
            stats: ExpStats::default(),
            active: Vec::with_capacity(32),
            depth: 0,
            budget: TOKEN_BUDGET,
            scratch: String::with_capacity(256),
        }
    }

    pub fn expand_seq(&mut self, input: &[Token], out: &mut Vec<Token>) {
        self.budget = TOKEN_BUDGET;
        self.expand_into(input, out);
        self.stats.tokens_out += out.len() as u64;
    }

    fn expand_into(&mut self, input: &[Token], out: &mut Vec<Token>) {
        let mut i = 0usize;
        while i < input.len() {
            let t = input[i];

            if t.kind != Kind::Ident || self.active.contains(&t.sym) {
                out.push(t);
                i += 1;
                continue;
            }

            let Some(def) = self.mt.lookup_def(t.sym, t.pc, self.pcs) else {
                if self.mt.is_known(t.sym) {
                    self.stats.unexpanded_known += 1;
                }
                out.push(t);
                i += 1;
                continue;
            };

            if self.budget <= 0 || self.depth >= MAX_DEPTH {
                self.stats.budget_hits += 1;
                out.push(t);
                i += 1;
                continue;
            }

            if !def.is_fn {
                self.stats.expansions += 1;
                let sub = self.substitute(def, &[], t);
                self.push_active(t.sym);
                self.expand_into(&sub, out);
                self.pop_active();
                i += 1;
                continue;
            }

            // A function-like macro name not followed by `(` is just an identifier.
            match self.collect_args(input, i + 1, def) {
                Some((args, next)) => {
                    self.stats.expansions += 1;
                    let sub = self.substitute(def, &args, t);
                    self.push_active(t.sym);
                    self.expand_into(&sub, out);
                    self.pop_active();
                    i = next;
                }
                None => {
                    out.push(t);
                    i += 1;
                }
            }
        }
    }

    #[inline]
    fn push_active(&mut self, s: SymId) {
        self.active.push(s);
        self.depth += 1;
    }

    #[inline]
    fn pop_active(&mut self) {
        self.active.pop();
        self.depth -= 1;
    }

    /// Splits a function-like invocation's arguments. Returns the raw
    /// (unexpanded) argument token runs and the offset past the closing paren.
    fn collect_args(
        &self,
        input: &[Token],
        start: usize,
        def: &MacroDef,
    ) -> Option<(Vec<Vec<Token>>, usize)> {
        let mut i = start;
        if i >= input.len() || !input[i].is(Kind::Punct, self.wk.lparen) {
            return None;
        }
        i += 1;

        let nparams = def.params.len();
        let mut args: Vec<Vec<Token>> = Vec::with_capacity(nparams.max(1));
        let mut cur: Vec<Token> = Vec::new();
        let mut depth = 0i32;

        while i < input.len() {
            let t = input[i];
            if t.kind == Kind::Punct {
                if t.sym == self.wk.lparen {
                    depth += 1;
                } else if t.sym == self.wk.rparen {
                    if depth == 0 {
                        args.push(std::mem::take(&mut cur));
                        // A single empty argument means a zero-argument call.
                        if args.len() == 1 && args[0].is_empty() && nparams == 0 {
                            args.clear();
                        }
                        while args.len() < nparams {
                            args.push(Vec::new());
                        }
                        return Some((args, i + 1));
                    }
                    depth -= 1;
                } else if t.sym == self.wk.comma
                    && depth == 0
                    && !(def.variadic && args.len() + 1 >= nparams)
                {
                    // Once the variadic parameter is reached, commas belong to it.
                    args.push(std::mem::take(&mut cur));
                    i += 1;
                    continue;
                }
            }
            cur.push(t);
            i += 1;
        }
        None // unterminated invocation
    }

    /// Replaces parameters in the macro body, handling `#` and `##`.
    fn substitute(&mut self, def: &MacroDef, args: &[Vec<Token>], call: Token) -> Vec<Token> {
        let body = &def.body;
        let mut out: Vec<Token> = Vec::with_capacity(body.len() + 8);
        let mut i = 0usize;

        while i < body.len() {
            let t = body[i];

            // `#param` stringizes the raw argument.
            if t.kind == Kind::Punct && t.sym == self.wk.hash && i + 1 < body.len() {
                if let Some(pi) = self.param_index(def, body[i + 1].sym) {
                    self.stats.stringizes += 1;
                    let s = self.stringize(args.get(pi).map(|v| &v[..]).unwrap_or(&[]));
                    let sym = self.it.intern(&s);
                    out.push(Token {
                        kind: Kind::Str,
                        flags: t.flags & F_WS,
                        sym,
                        off: call.off,
                        pc: call.pc,
                    });
                    i += 2;
                    continue;
                }
            }

            // GNU `, ## __VA_ARGS__`: drop the comma when varargs are empty.
            if t.kind == Kind::Punct
                && t.sym == self.wk.comma
                && i + 2 < body.len()
                && body[i + 1].is(Kind::Punct, self.wk.hashhash)
            {
                if let Some(pi) = self.param_index(def, body[i + 2].sym) {
                    let empty = args.get(pi).map(|a| a.is_empty()).unwrap_or(true);
                    if def.variadic && pi + 1 == def.params.len() {
                        if empty {
                            i += 3;
                            continue;
                        }
                        out.push(t);
                        self.push_arg_raw(args, pi, call, &mut out);
                        i += 3;
                        continue;
                    }
                }
            }

            // `lhs ## rhs`
            if i + 1 < body.len() && body[i + 1].is(Kind::Punct, self.wk.hashhash) {
                // Operands of ## are substituted unexpanded.
                self.push_operand(def, args, t, call, &mut out);
                let mut j = i + 1;
                while j + 1 < body.len() && body[j].is(Kind::Punct, self.wk.hashhash) {
                    let rhs = body[j + 1];
                    let before = out.len();
                    self.push_operand(def, args, rhs, call, &mut out);
                    self.paste(before, &mut out, call);
                    j += 2;
                }
                i = j;
                continue;
            }

            if let Some(pi) = self.param_index(def, t.sym) {
                // Ordinary parameter use: substitute the fully expanded argument.
                let raw = args.get(pi).map(|v| &v[..]).unwrap_or(&[]);
                let mut ex = Vec::with_capacity(raw.len());
                self.expand_into(raw, &mut ex);
                let n = out.len();
                out.extend_from_slice(&ex);
                if let Some(first) = out.get_mut(n) {
                    first.flags = (first.flags & !F_BOL) | (t.flags & F_WS);
                }
                for tok in &mut out[n..] {
                    tok.off = call.off;
                    tok.pc = call.pc;
                }
                i += 1;
                continue;
            }

            let mut t = t;
            t.off = call.off;
            t.pc = call.pc;
            t.flags &= !F_BOL;
            out.push(t);
            i += 1;
        }

        self.budget -= out.len() as i64;
        out
    }

    #[inline]
    fn param_index(&self, def: &MacroDef, s: SymId) -> Option<usize> {
        if !def.is_fn {
            return None;
        }
        def.params.iter().position(|&p| p == s)
    }

    fn push_arg_raw(&self, args: &[Vec<Token>], pi: usize, call: Token, out: &mut Vec<Token>) {
        if let Some(a) = args.get(pi) {
            let n = out.len();
            out.extend_from_slice(a);
            for tok in &mut out[n..] {
                tok.off = call.off;
                tok.pc = call.pc;
                tok.flags &= !F_BOL;
            }
        }
    }

    /// Pushes one operand of `##`, unexpanded if it is a parameter.
    fn push_operand(
        &mut self,
        def: &MacroDef,
        args: &[Vec<Token>],
        t: Token,
        call: Token,
        out: &mut Vec<Token>,
    ) {
        if let Some(pi) = self.param_index(def, t.sym) {
            self.push_arg_raw(args, pi, call, out);
        } else {
            let mut t = t;
            t.off = call.off;
            t.pc = call.pc;
            t.flags &= !F_BOL;
            out.push(t);
        }
    }

    /// Concatenates the token before `at` with the one at `at`, re-lexing the
    /// result. `at` is where the right-hand operand began.
    fn paste(&mut self, at: usize, out: &mut Vec<Token>, call: Token) {
        if at == 0 || at >= out.len() {
            return; // one side was empty; nothing to paste
        }
        self.stats.pastes += 1;
        let lhs = out[at - 1];
        let rhs = out[at];

        self.scratch.clear();
        self.scratch.push_str(self.it.text(lhs.sym));
        self.scratch.push_str(self.it.text(rhs.sym));

        let joined = std::mem::take(&mut self.scratch);
        let relexed = crate::lex::lex(joined.as_bytes(), self.it);
        self.scratch = joined;
        self.scratch.clear();

        out.truncate(at - 1);
        let n = out.len();
        out.extend(relexed);
        for tok in &mut out[n..] {
            tok.off = call.off;
            tok.pc = call.pc;
            tok.flags &= !F_BOL;
        }
        if let Some(first) = out.get_mut(n) {
            first.flags |= lhs.flags & F_WS;
        }
    }

    fn stringize(&mut self, toks: &[Token]) -> String {
        let mut s = String::with_capacity(32);
        s.push('"');
        for (i, t) in toks.iter().enumerate() {
            if i > 0 && t.flags & F_WS != 0 {
                s.push(' ');
            }
            let text = self.it.text(t.sym);
            if matches!(t.kind, Kind::Str | Kind::Char) {
                for c in text.chars() {
                    if c == '"' || c == '\\' {
                        s.push('\\');
                    }
                    s.push(c);
                }
            } else {
                s.push_str(text);
            }
        }
        s.push('"');
        s
    }
}

/// Parses the token run of a `#define` line (starting at the macro name).
pub fn parse_define(line: &[Token], wk: &WellKnown) -> Option<(SymId, MacroDef)> {
    let name_tok = *line.first()?;
    if name_tok.kind != Kind::Ident {
        return None;
    }

    let mut params = Vec::new();
    let mut variadic = false;
    let mut is_fn = false;
    let mut i = 1usize;

    // Function-like only if `(` is immediately adjacent to the name.
    if line
        .get(1)
        .is_some_and(|t| t.is(Kind::Punct, wk.lparen) && t.flags & F_WS == 0)
    {
        is_fn = true;
        i = 2;
        loop {
            let t = *line.get(i)?;
            if t.is(Kind::Punct, wk.rparen) {
                i += 1;
                break;
            }
            if t.is(Kind::Punct, wk.comma) {
                i += 1;
                continue;
            }
            if t.is(Kind::Punct, wk.ellipsis) {
                variadic = true;
                params.push(wk.va_args);
                i += 1;
                continue;
            }
            if t.kind == Kind::Ident {
                // GNU named variadic: `args...`
                if line
                    .get(i + 1)
                    .is_some_and(|n| n.is(Kind::Punct, wk.ellipsis))
                {
                    variadic = true;
                    params.push(t.sym);
                    i += 2;
                    continue;
                }
                params.push(t.sym);
                i += 1;
                continue;
            }
            return None;
        }
    }

    Some((
        name_tok.sym,
        MacroDef {
            is_fn,
            variadic,
            params,
            body: line[i..].to_vec(),
            off: name_tok.off,
            pc: PC_TRUE,
            undef: false,
        },
    ))
}

/// Directive keywords, interned once per pass.
pub struct Keywords {
    pub define: SymId,
    pub undef: SymId,
    pub if_: SymId,
    pub ifdef: SymId,
    pub ifndef: SymId,
    pub elif: SymId,
    pub elifdef: SymId,
    pub elifndef: SymId,
    pub else_: SymId,
    pub endif: SymId,
    pub include: SymId,
}

impl Keywords {
    pub fn new(it: &mut impl Symbols) -> Self {
        Self {
            define: it.intern("define"),
            undef: it.intern("undef"),
            if_: it.intern("if"),
            ifdef: it.intern("ifdef"),
            ifndef: it.intern("ifndef"),
            elif: it.intern("elif"),
            elifdef: it.intern("elifdef"),
            elifndef: it.intern("elifndef"),
            else_: it.intern("else"),
            endif: it.intern("endif"),
            include: it.intern("include"),
        }
    }
}

struct CondFrame {
    /// Condition in force outside this conditional.
    outer: PcId,
    /// Condition of the branch currently open.
    current: PcId,
    /// Conjunction of the negations of branches already seen. Exact only while
    /// each branch is a single literal; widens to TOP otherwise.
    prior_negated: PcId,
}

/// Config-less single-file preprocessing: every `#define` is recorded with the
/// condition it sits under, and every conditional branch is emitted into one
/// token stream, tagged so downstream stages can tell them apart.
pub fn preprocess<S: Symbols>(
    toks: &[Token],
    mt: &mut MacroTable,
    it: &mut S,
    wk: &WellKnown,
    pcs: &mut PcStore,
    kw: &Keywords,
) -> (Vec<Token>, ExpStats) {
    use crate::pc::{CondKind, PcParser};

    let mut out = Vec::with_capacity(toks.len());
    let mut stats = ExpStats::default();
    let mut run: Vec<Token> = Vec::new();
    let mut stack: Vec<CondFrame> = Vec::new();
    let mut pc = PC_TRUE;
    let parser = PcParser::new(wk);
    let mut i = 0usize;

    while i < toks.len() {
        let t = toks[i];
        let is_directive = t.bol() && t.kind == Kind::Punct && t.sym == wk.hash;

        if !is_directive {
            let start = i;
            while i < toks.len() {
                let t = toks[i];
                if t.bol() && t.kind == Kind::Punct && t.sym == wk.hash {
                    break;
                }
                i += 1;
            }
            run.clear();
            run.extend_from_slice(&toks[start..i]);
            for t in &mut run {
                t.pc = pc;
            }
            let mut ex = Expander::new(mt, it, wk, pcs);
            ex.expand_seq(&run, &mut out);
            let s = ex.stats;
            stats.expansions += s.expansions;
            stats.pastes += s.pastes;
            stats.stringizes += s.stringizes;
            stats.budget_hits += s.budget_hits;
            stats.unexpanded_known += s.unexpanded_known;
            continue;
        }

        let start = i + 1;
        let mut j = start;
        while j < toks.len() && !toks[j].bol() {
            j += 1;
        }
        let line = &toks[start..j];

        if let Some(k) = line.first().filter(|k| k.kind == Kind::Ident) {
            let expr = &line[1..];
            let s = k.sym;

            if s == kw.define {
                if let Some((name, mut def)) = parse_define(expr, wk) {
                    def.pc = pc;
                    mt.insert(name, def);
                }
            } else if s == kw.undef {
                if let Some(n) = expr.first() {
                    mt.undef(n.sym, pc, n.off);
                }
            } else if s == kw.if_ || s == kw.ifdef || s == kw.ifndef {
                let kind = if s == kw.ifdef {
                    CondKind::Ifdef
                } else if s == kw.ifndef {
                    CondKind::Ifndef
                } else {
                    CondKind::If
                };
                let cond = parser.build(kind, expr, pcs, it);
                let neg = pcs.not(cond);
                let current = pcs.and(pc, cond);
                stack.push(CondFrame {
                    outer: pc,
                    current,
                    prior_negated: neg,
                });
                pc = current;
            } else if s == kw.elif || s == kw.elifdef || s == kw.elifndef || s == kw.else_ {
                if let Some(f) = stack.last_mut() {
                    let branch = if s == kw.else_ {
                        f.prior_negated
                    } else {
                        let kind = if s == kw.elifdef {
                            CondKind::Ifdef
                        } else if s == kw.elifndef {
                            CondKind::Ifndef
                        } else {
                            CondKind::If
                        };
                        let cond = parser.build(kind, expr, pcs, it);
                        // This branch holds when no earlier branch did and this
                        // condition does. Only afterwards does its negation join
                        // the accumulator for the branches that follow.
                        let branch = pcs.and(f.prior_negated, cond);
                        let neg = pcs.not(cond);
                        f.prior_negated = pcs.and(f.prior_negated, neg);
                        branch
                    };
                    f.current = pcs.and(f.outer, branch);
                    pc = f.current;
                }
            } else if s == kw.endif {
                pc = stack.pop().map(|f| f.outer).unwrap_or(PC_TRUE);
            }
        }
        i = j;
    }

    stats.tokens_out = out.len() as u64;
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intern::Interner;
    use crate::lex::lex;

    fn pp(src: &str) -> String {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = Keywords::new(&mut it);
        let mut pcs = PcStore::new();
        let toks = lex(src.as_bytes(), &mut it);
        let mut mt = MacroTable::default();
        let (out, _) = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
        out.iter()
            .map(|t| it.text(t.sym))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn object_like() {
        assert_eq!(pp("#define A 42\nint x = A;"), "int x = 42 ;");
    }

    #[test]
    fn function_like_with_args() {
        assert_eq!(
            pp("#define ADD(a,b) ((a)+(b))\nADD(1,2);"),
            "( ( 1 ) + ( 2 ) ) ;"
        );
    }

    #[test]
    fn nested_expansion() {
        assert_eq!(pp("#define A B\n#define B 7\nA;"), "7 ;");
    }

    #[test]
    fn self_reference_terminates() {
        assert_eq!(pp("#define A A\nA;"), "A ;");
        assert_eq!(pp("#define X (X + 1)\nX;"), "( X + 1 ) ;");
    }

    #[test]
    fn token_paste() {
        assert_eq!(pp("#define CAT(a,b) a##b\nCAT(foo,bar);"), "foobar ;");
    }

    #[test]
    fn stringize() {
        assert_eq!(pp("#define S(x) #x\nS(a+b);"), "\"a+b\" ;");
    }

    #[test]
    fn fn_like_without_parens_is_plain_identifier() {
        assert_eq!(pp("#define F(x) x\nint F;"), "int F ;");
    }

    #[test]
    fn variadic_and_gnu_comma_paste() {
        let src = "#define P(fmt, ...) f(fmt, ##__VA_ARGS__)\nP(\"a\");\nP(\"a\", 1);";
        assert_eq!(pp(src), "f ( \"a\" ) ; f ( \"a\" , 1 ) ;");
    }

    #[test]
    fn multiline_invocation_spans_lines() {
        assert_eq!(pp("#define ADD(a,b) a+b\nADD(1,\n2);"), "1 + 2 ;");
    }

    #[test]
    fn undef_removes_definition() {
        assert_eq!(pp("#define A 1\n#undef A\nA;"), "A ;");
    }

    #[test]
    fn all_branches_are_taken() {
        // The config-less model keeps both arms of every conditional.
        assert_eq!(
            pp("#ifdef X\nint a;\n#else\nint b;\n#endif"),
            "int a ; int b ;"
        );
    }

    /// Preprocesses and returns the table and store for inspection.
    fn pp_full(src: &str) -> (String, MacroTable, Interner, PcStore, ExpStats) {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = Keywords::new(&mut it);
        let mut pcs = PcStore::new();
        let toks = lex(src.as_bytes(), &mut it);
        let mut mt = MacroTable::default();
        let (out, st) = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
        let text = out
            .iter()
            .map(|t| it.text(t.sym))
            .collect::<Vec<_>>()
            .join(" ");
        (text, mt, it, pcs, st)
    }

    #[test]
    fn conditional_define_records_both_variants() {
        let src = "#ifdef CONFIG_SMP\n#define NR 8\n#else\n#define NR 1\n#endif\n";
        let (_, mt, mut it, _, _) = pp_full(src);
        let nr = it.intern("NR");
        assert_eq!(mt.variants(nr).len(), 2, "both arms must be recorded");
        assert_eq!(mt.total_defs(), 2);
    }

    #[test]
    fn use_site_condition_selects_the_matching_variant() {
        // The use of NR sits under the same condition as the first definition,
        // so the other arm must be ruled out as contradictory.
        let src = "#ifdef CONFIG_SMP\n#define NR 8\n#else\n#define NR 1\n#endif\n\
                   #ifdef CONFIG_SMP\nint a[NR];\n#endif\n";
        let (text, ..) = pp_full(src);
        assert!(text.contains("8"), "got: {text}");
        assert!(
            !text.contains("1"),
            "opposite arm must not be chosen: {text}"
        );
    }

    #[test]
    fn unconditional_define_supersedes_guarded_ones() {
        let src = "#ifdef A\n#define M 1\n#endif\n#define M 2\nM;";
        let (text, mt, mut it, _, _) = pp_full(src);
        let m = it.intern("M");
        assert_eq!(mt.variants(m).len(), 1);
        assert_eq!(text, "2 ;");
    }

    #[test]
    fn redefining_under_the_same_guard_replaces() {
        let src = "#ifdef A\n#define M 1\n#endif\n#ifdef A\n#define M 2\n#endif\n";
        let (_, mt, mut it, _, _) = pp_full(src);
        let m = it.intern("M");
        assert_eq!(mt.variants(m).len(), 1, "same guard must not accumulate");
    }

    #[test]
    fn too_many_variants_stays_unexpanded_but_navigable() {
        let mut src = String::new();
        for i in 0..MAX_VARIANTS + 2 {
            src.push_str(&format!("#ifdef C{i}\n#define M v{i}\n#endif\n"));
        }
        src.push_str("M;");
        let (text, _, _, _, st) = pp_full(&src);
        assert_eq!(text, "M ;", "ambiguous macro must be left alone");
        assert!(st.unexpanded_known > 0);
    }

    #[test]
    fn guarded_undef_only_suppresses_under_its_condition() {
        let src = "#define M 1\n#ifdef A\n#undef M\n#endif\n#ifdef A\nM;\n#endif\n";
        let (text, ..) = pp_full(src);
        assert_eq!(text, "M ;", "M is undefined under A");
    }

    #[test]
    fn elif_branches_are_mutually_exclusive() {
        let src = "#if defined(A)\n#define M 1\n#elif defined(B)\n#define M 2\n#endif\n\
                   #if defined(B)\nM;\n#endif\n";
        let (text, ..) = pp_full(src);
        assert_eq!(text, "2 ;");
    }

    #[test]
    fn kernel_style_export_symbol() {
        let src = "#define EXPORT_SYMBOL(sym) \
                   extern typeof(sym) sym; \
                   static const char __kstrtab_##sym[] = #sym\n\
                   EXPORT_SYMBOL(do_thing);";
        let got = pp(src);
        assert!(got.contains("__kstrtab_do_thing"), "got: {got}");
        assert!(got.contains("\"do_thing\""), "got: {got}");
    }
}
