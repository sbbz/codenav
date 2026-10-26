//! Presence conditions.
//!
//! A presence condition is a conjunction of literals — a cube — not a general
//! boolean formula. Anything that will not reduce to a cube (notably `||`)
//! collapses to TOP and is flagged approximate. A navigation index only needs
//! contradiction detection, which on sorted cubes is a linear merge-scan, so
//! none of the BDD or SAT machinery of TypeChef/SuperC is required.

use crate::intern::{SymId, Symbols};
use crate::lex::{Kind, Token, WellKnown};
use rustc_hash::FxHashMap;

/// An atom is identified by the interned text it stands for: the macro name for
/// `defined(X)`, the whole expression text for anything opaque, or `arch=<x>`.
pub type AtomId = SymId;

/// Atom id in the upper bits, polarity in bit 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lit(u32);

impl Lit {
    #[inline]
    pub fn new(atom: AtomId, positive: bool) -> Self {
        Lit((atom << 1) | positive as u32)
    }
    #[inline]
    pub fn atom(self) -> AtomId {
        self.0 >> 1
    }
    #[inline]
    pub fn positive(self) -> bool {
        self.0 & 1 != 0
    }
    /// Raw packed form, for serializing into the on-disk index.
    #[inline]
    pub fn raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub fn from_raw(v: u32) -> Self {
        Lit(v)
    }

    #[inline]
    pub fn negated(self) -> Self {
        Lit(self.0 ^ 1)
    }
}

pub type PcId = u32;

/// The empty cube: always true. Unconditional code carries this.
pub const PC_TRUE: PcId = 0;
/// A cube containing a contradiction. Code under it is unreachable in every
/// configuration, but is still indexed — it just ranks last.
pub const PC_FALSE: PcId = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Cube {
    /// Sorted by atom id, at most one polarity per atom.
    lits: Vec<Lit>,
    /// Set when information was discarded building this condition, e.g. a `||`
    /// that was widened to TOP.
    pub approx: bool,
}

impl Cube {
    pub fn lits(&self) -> &[Lit] {
        &self.lits
    }
    pub fn is_true(&self) -> bool {
        self.lits.is_empty()
    }
}

pub struct PcStore {
    cubes: Vec<Cube>,
    map: FxHashMap<Cube, PcId>,
    /// Atoms that are false by construction — see [`PcStore::for_c`].
    false_atoms: Vec<AtomId>,
}

impl Default for PcStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PcStore {
    pub fn new() -> Self {
        let mut s = Self {
            cubes: Vec::new(),
            map: FxHashMap::default(),
            false_atoms: Vec::new(),
        };
        // PC_TRUE
        s.intern_cube(Cube::default());
        // PC_FALSE: a distinguished contradictory cube, never produced by
        // interning a real cube because atom 0 is the empty string.
        let f = Cube {
            lits: vec![Lit::new(0, false), Lit::new(0, true)],
            approx: false,
        };
        s.cubes.push(f.clone());
        s.map.insert(f, PC_FALSE);
        s
    }

    pub fn get(&self, id: PcId) -> &Cube {
        &self.cubes[id as usize]
    }

    pub fn len(&self) -> usize {
        self.cubes.len()
    }

    /// Folds another store in, given that store's atom-id remapping. Returns
    /// its PcIds mapped onto this store's.
    ///
    /// Cubes must be re-sorted: they are ordered by atom id, and remapping
    /// changes those ids.
    pub fn merge_from(&mut self, other: &PcStore, sym: &[SymId]) -> Vec<PcId> {
        let mut map = Vec::with_capacity(other.len());
        for i in 0..other.len() as PcId {
            if i == PC_TRUE || i == PC_FALSE {
                map.push(i);
                continue;
            }
            let c = other.get(i);
            let mut lits: Vec<Lit> = c
                .lits()
                .iter()
                .map(|l| Lit::new(sym[l.atom() as usize], l.positive()))
                .collect();
            lits.sort_unstable_by_key(|l| l.atom());
            let approx = c.approx;
            map.push(self.intern_cube(Cube { lits, approx }));
        }
        map
    }

    /// Raw literals of every cube, ordered by PcId, for writing to the index.
    pub fn export(&self) -> Vec<Vec<u32>> {
        self.cubes
            .iter()
            .map(|c| c.lits().iter().map(|l| l.raw()).collect())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.cubes.is_empty()
    }

    fn intern_cube(&mut self, c: Cube) -> PcId {
        if let Some(&id) = self.map.get(&c) {
            return id;
        }
        let id = self.cubes.len() as PcId;
        self.cubes.push(c.clone());
        self.map.insert(c, id);
        id
    }

    /// Conjoins a single literal onto an existing condition.
    pub fn and_lit(&mut self, pc: PcId, lit: Lit) -> PcId {
        if pc == PC_FALSE {
            return PC_FALSE;
        }
        let cur = &self.cubes[pc as usize];
        match cur.lits.binary_search_by_key(&lit.atom(), |l| l.atom()) {
            Ok(i) => {
                if cur.lits[i] == lit {
                    pc // already implied
                } else {
                    PC_FALSE // A and not-A
                }
            }
            Err(i) => {
                let mut c = cur.clone();
                c.lits.insert(i, lit);
                self.intern_cube(c)
            }
        }
    }

    /// Conjunction of two cubes, interned in a single step.
    ///
    /// Applying literals one at a time would intern an intermediate cube per
    /// literal. Those are never referenced by anything, and worse, *which* ones
    /// appear depends on atom-id order — so the store's contents varied with
    /// interning order rather than with the code being indexed.
    pub fn and(&mut self, a: PcId, b: PcId) -> PcId {
        if a == PC_FALSE || b == PC_FALSE {
            return PC_FALSE;
        }
        if a == PC_TRUE {
            return b;
        }
        if b == PC_TRUE {
            return a;
        }

        let (x, y) = (&self.cubes[a as usize], &self.cubes[b as usize]);
        let approx = x.approx || y.approx;
        let mut lits = Vec::with_capacity(x.lits.len() + y.lits.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < x.lits.len() && j < y.lits.len() {
            let (la, lb) = (x.lits[i], y.lits[j]);
            match la.atom().cmp(&lb.atom()) {
                std::cmp::Ordering::Less => {
                    lits.push(la);
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    lits.push(lb);
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    if la.positive() != lb.positive() {
                        return PC_FALSE;
                    }
                    lits.push(la);
                    i += 1;
                    j += 1;
                }
            }
        }
        lits.extend_from_slice(&x.lits[i..]);
        lits.extend_from_slice(&y.lits[j..]);
        self.intern_cube(Cube { lits, approx })
    }

    /// Negation of a cube is a disjunction, which is not representable. A
    /// single literal negates exactly; anything larger widens to TOP.
    pub fn not(&mut self, pc: PcId) -> PcId {
        match pc {
            PC_TRUE => PC_FALSE,
            PC_FALSE => PC_TRUE,
            _ => {
                let c = &self.cubes[pc as usize];
                if c.lits.len() == 1 {
                    let l = c.lits[0].negated();
                    let approx = c.approx;
                    let cube = Cube {
                        lits: vec![l],
                        approx,
                    };
                    self.intern_cube(cube)
                } else {
                    self.intern_cube(Cube {
                        lits: Vec::new(),
                        approx: true,
                    })
                }
            }
        }
    }

    /// Affirmed literals minus negated ones. `defined(X)` scores +1 and its
    /// `#else` counterpart -1, so this ranks a guarded definition by how much
    /// it asserts rather than denies.
    ///
    /// Needed because a great many kernel headers write
    /// `#ifdef CONFIG_X / #define M ...real... / #else / #define M / #endif`.
    /// Both variants are compatible with an undecided condition, and picking by
    /// source order would always choose the empty `#else` arm.
    pub fn polarity(&self, pc: PcId) -> i32 {
        if pc == PC_FALSE {
            return i32::MIN;
        }
        self.cubes[pc as usize]
            .lits
            .iter()
            .map(|l| if l.positive() { 1 } else { -1 })
            .sum()
    }

    /// True when no configuration satisfies both conditions. This is the only
    /// logical query the index needs, and it drives result ranking.
    /// A store for reading files as C, so assembly-only arms are unreachable.
    pub fn for_c<S: crate::intern::Symbols>(it: &mut S) -> Self {
        let mut s = Self::new();
        s.false_atoms = ASSEMBLY_ATOMS.iter().map(|n| it.intern(n)).collect();
        s
    }

    /// True when the cube requires an atom that cannot hold.
    fn asserts_false_atom(&self, id: PcId) -> bool {
        if self.false_atoms.is_empty() {
            return false;
        }
        self.cubes[id as usize]
            .lits
            .iter()
            .any(|l| l.positive() && self.false_atoms.contains(&l.atom()))
    }

    pub fn contradicts(&self, a: PcId, b: PcId) -> bool {
        if a == PC_FALSE || b == PC_FALSE {
            return true;
        }
        if self.asserts_false_atom(a) || self.asserts_false_atom(b) {
            return true;
        }
        let (x, y) = (&self.cubes[a as usize], &self.cubes[b as usize]);
        let (mut i, mut j) = (0usize, 0usize);
        while i < x.lits.len() && j < y.lits.len() {
            let (la, lb) = (x.lits[i], y.lits[j]);
            match la.atom().cmp(&lb.atom()) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    if la.positive() != lb.positive() {
                        return true;
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
        false
    }
}

/// Atoms that are false whenever a file is being read as C.
///
/// `__ASSEMBLY__` is defined only when the compiler is handed a `.S` file, and
/// codenav parses everything as C. The arms it guards are GNU assembler
/// directives — `include/linux/annotate.h` defines `.macro ANNOTATE type` there
/// — so treating it as undecided lets assembler syntax into C expansions, which
/// breaks the parse and takes the rest of the file with it.
///
/// This is the one place where a conditional is *decided*. `CONFIG_*` stays
/// undecided on purpose; this is not that. `__ASSEMBLY__` has a single correct
/// value for every file codenav looks at.
pub const ASSEMBLY_ATOMS: [&str; 2] = ["__ASSEMBLY__", "__ASSEMBLER__"];

/// Builds a presence condition from the tokens of an `#if`-family directive.
///
/// Recognizes conjunctions of `defined(X)`, `!defined(X)`, and bare macro names.
/// Anything else — arithmetic, comparisons, `||` — becomes a single opaque atom
/// keyed on the expression text, or TOP when even that is not sound.
pub struct PcParser<'a> {
    pub wk: &'a WellKnown,
}

impl<'a> PcParser<'a> {
    pub fn new(wk: &'a WellKnown) -> Self {
        Self { wk }
    }

    /// `kind` is the directive keyword: ifdef, ifndef, if, elif.
    pub fn build<S: Symbols>(
        &self,
        kind: CondKind,
        expr: &[Token],
        store: &mut PcStore,
        it: &mut S,
    ) -> PcId {
        match kind {
            CondKind::Ifdef => match expr.first() {
                Some(t) if t.kind == Kind::Ident => store.and_lit(PC_TRUE, Lit::new(t.sym, true)),
                _ => PC_TRUE,
            },
            CondKind::Ifndef => match expr.first() {
                Some(t) if t.kind == Kind::Ident => store.and_lit(PC_TRUE, Lit::new(t.sym, false)),
                _ => PC_TRUE,
            },
            CondKind::If => self.parse_expr(expr, store, it),
        }
    }

    fn parse_expr<S: Symbols>(&self, expr: &[Token], store: &mut PcStore, it: &mut S) -> PcId {
        // A top-level `||` cannot be a cube. Widen the whole thing.
        if self.has_top_level_or(expr) {
            return store.intern_cube(Cube {
                lits: Vec::new(),
                approx: true,
            });
        }

        let mut pc = PC_TRUE;
        for part in self.split_top_level_and(expr) {
            let lit = self.term_to_lit(part, it);
            match lit {
                Some(l) => pc = store.and_lit(pc, l),
                None => {
                    let mut c = store.get(pc).clone();
                    c.approx = true;
                    pc = store.intern_cube(c);
                }
            }
            if pc == PC_FALSE {
                return PC_FALSE;
            }
        }
        pc
    }

    /// A `||` anywhere outside a `defined(...)` argument means the condition is
    /// a disjunction and cannot be a cube.
    fn has_top_level_or(&self, expr: &[Token]) -> bool {
        let mut depth = 0i32;
        for t in expr {
            if t.kind != Kind::Punct {
                continue;
            }
            if t.sym == self.wk.lparen {
                depth += 1;
            } else if t.sym == self.wk.rparen {
                depth -= 1;
            } else if t.sym == self.wk.oror && depth == 0 {
                return true;
            }
        }
        false
    }

    fn split_top_level_and<'t>(&self, expr: &'t [Token]) -> Vec<&'t [Token]> {
        let mut out = Vec::new();
        let mut depth = 0i32;
        let mut start = 0usize;
        for (i, t) in expr.iter().enumerate() {
            if t.kind != Kind::Punct {
                continue;
            }
            if t.sym == self.wk.lparen {
                depth += 1;
            } else if t.sym == self.wk.rparen {
                depth -= 1;
            } else if depth == 0 && t.sym == self.wk.andand {
                out.push(&expr[start..i]);
                start = i + 1;
            }
        }
        out.push(&expr[start..]);
        out.retain(|s| !s.is_empty());
        out
    }

    /// Converts one conjunct to a literal, or None if it must stay opaque.
    fn term_to_lit<S: Symbols>(&self, term: &[Token], it: &mut S) -> Option<Lit> {
        let mut t = term;
        let mut positive = true;

        // Strip leading `!` and redundant parens.
        loop {
            if let Some(f) = t.first() {
                if f.kind == Kind::Punct && f.sym == self.wk.bang {
                    positive = !positive;
                    t = &t[1..];
                    continue;
                }
                if f.kind == Kind::Punct
                    && f.sym == self.wk.lparen
                    && t.last().is_some_and(|l| l.sym == self.wk.rparen)
                    && balanced(t, self.wk)
                {
                    t = &t[1..t.len() - 1];
                    continue;
                }
            }
            break;
        }

        // `defined X` / `defined(X)`
        if let Some(f) = t.first() {
            if f.kind == Kind::Ident && f.sym == self.wk.defined {
                let rest = &t[1..];
                let name = if rest.first().is_some_and(|x| x.sym == self.wk.lparen) {
                    rest.get(1)
                } else {
                    rest.first()
                };
                return name
                    .filter(|n| n.kind == Kind::Ident)
                    .map(|n| Lit::new(n.sym, positive));
            }
        }

        // A bare macro name is `!= 0`, which for navigation is the same signal
        // as being defined.
        if t.len() == 1 && t[0].kind == Kind::Ident {
            return Some(Lit::new(t[0].sym, positive));
        }

        // Anything else becomes one opaque atom keyed on its spelling, so that
        // `#if X > 3` and its `#else` are still recognized as incompatible.
        if t.is_empty() {
            return None;
        }
        let mut key = String::with_capacity(32);
        for (i, tok) in t.iter().enumerate() {
            if i > 0 {
                key.push(' ');
            }
            key.push_str(it.text(tok.sym));
        }
        Some(Lit::new(it.intern(&key), positive))
    }
}

fn balanced(t: &[Token], wk: &WellKnown) -> bool {
    let mut depth = 0i32;
    for (i, tok) in t.iter().enumerate() {
        if tok.kind != Kind::Punct {
            continue;
        }
        if tok.sym == wk.lparen {
            depth += 1;
        } else if tok.sym == wk.rparen {
            depth -= 1;
            if depth == 0 && i != t.len() - 1 {
                return false;
            }
        }
    }
    depth == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CondKind {
    If,
    Ifdef,
    Ifndef,
}

#[cfg(test)]
mod tests {

    #[test]
    fn assembly_only_branches_are_unreachable_from_c() {
        use crate::intern::Interner;
        let mut it = Interner::new();
        let mut pcs = PcStore::for_c(&mut it);

        let asm_atom = it.intern("__ASSEMBLY__");
        let under_asm = pcs.and_lit(PC_TRUE, Lit::new(asm_atom, true));
        let not_under_asm = pcs.and_lit(PC_TRUE, Lit::new(asm_atom, false));

        // A definition guarded by `#ifdef __ASSEMBLY__` can never apply: the
        // arm holds `.macro` directives, not C.
        assert!(pcs.contradicts(under_asm, PC_TRUE));
        // Its `#ifndef` sibling is the one that applies.
        assert!(!pcs.contradicts(not_under_asm, PC_TRUE));
    }

    #[test]
    fn a_plain_store_leaves_assembly_undecided() {
        use crate::intern::Interner;
        let mut it = Interner::new();
        let mut pcs = PcStore::new();
        let a = it.intern("__ASSEMBLY__");
        let under = pcs.and_lit(PC_TRUE, Lit::new(a, true));
        assert!(!pcs.contradicts(under, PC_TRUE), "only for_c decides it");
    }
    use super::*;
    use crate::intern::Interner;
    use crate::lex::lex;

    fn setup() -> (Interner, WellKnown, PcStore) {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        (it, wk, PcStore::new())
    }

    /// Merging a store into an empty one must be the identity on cube
    /// *contents*. If it is not, every downstream contradiction test silently
    /// changes meaning.
    #[test]
    fn merge_into_empty_store_preserves_all_cubes() {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let p = PcParser::new(&wk);
        let mut src = PcStore::new();

        // An #elif chain, which is where the real divergence showed up.
        let exprs = [
            "defined(A)",
            "defined(B)",
            "X == 32",
            "X == 64",
            "defined(A) && defined(B)",
        ];
        let mut prior = PC_TRUE;
        for e in exprs {
            let toks = lex(e.as_bytes(), &mut it);
            let cond = p.build(CondKind::If, &toks, &mut src, &mut it);
            let branch = src.and(prior, cond);
            let _ = branch;
            let neg = src.not(cond);
            prior = src.and(prior, neg);
        }

        let before: std::collections::BTreeSet<Vec<u32>> = src.export().into_iter().collect();

        let mut dst = PcStore::new();
        let ident: Vec<SymId> = (0..it.len() as SymId).collect();
        dst.merge_from(&src, &ident);
        let after: std::collections::BTreeSet<Vec<u32>> = dst.export().into_iter().collect();

        let missing: Vec<_> = before.difference(&after).collect();
        assert!(
            missing.is_empty(),
            "merge lost {} cubes, e.g. {:?}",
            missing.len(),
            missing.first()
        );
    }

    #[test]
    fn true_and_false_are_distinguished() {
        let (_, _, s) = setup();
        assert!(s.get(PC_TRUE).is_true());
        assert!(s.contradicts(PC_FALSE, PC_TRUE));
        assert!(!s.contradicts(PC_TRUE, PC_TRUE));
    }

    #[test]
    fn conjoining_opposite_literals_is_false() {
        let (mut it, _, mut s) = setup();
        let a = it.intern("CONFIG_SMP");
        let p = s.and_lit(PC_TRUE, Lit::new(a, true));
        assert_eq!(s.and_lit(p, Lit::new(a, false)), PC_FALSE);
    }

    #[test]
    fn cubes_are_interned_to_the_same_id() {
        let (mut it, _, mut s) = setup();
        let (a, b) = (it.intern("A"), it.intern("B"));
        let xa = s.and_lit(PC_TRUE, Lit::new(a, true));
        let x = s.and_lit(xa, Lit::new(b, true));
        let yb = s.and_lit(PC_TRUE, Lit::new(b, true));
        let y = s.and_lit(yb, Lit::new(a, true));
        assert_eq!(x, y, "literal order must not matter");
    }

    #[test]
    fn contradiction_detects_arch_exclusivity() {
        let (mut it, _, mut s) = setup();
        let x86 = it.intern("arch=x86");
        let arm = it.intern("arch=arm64");
        let a = s.and_lit(PC_TRUE, Lit::new(x86, true));
        let b = s.and_lit(PC_TRUE, Lit::new(arm, true));
        // Different atoms, so not detected as contradictory on their own.
        assert!(!s.contradicts(a, b));
        // But an explicit negation is.
        let c = s.and_lit(PC_TRUE, Lit::new(x86, false));
        assert!(s.contradicts(a, c));
    }

    fn build(src: &str, kind: CondKind) -> (PcId, PcStore, Interner) {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let mut s = PcStore::new();
        let toks = lex(src.as_bytes(), &mut it);
        let p = PcParser::new(&wk);
        let id = p.build(kind, &toks, &mut s, &mut it);
        (id, s, it)
    }

    #[test]
    fn ifdef_and_ifndef_are_opposite() {
        let (a, s, mut it) = build("CONFIG_X", CondKind::Ifdef);
        let sym = it.intern("CONFIG_X");
        assert_eq!(s.get(a).lits(), [Lit::new(sym, true)]);

        let (b, s2, mut it2) = build("CONFIG_X", CondKind::Ifndef);
        let sym2 = it2.intern("CONFIG_X");
        assert_eq!(s2.get(b).lits(), [Lit::new(sym2, false)]);
    }

    #[test]
    fn defined_forms_are_equivalent() {
        let (a, s, _) = build("defined(CONFIG_X)", CondKind::If);
        let (b, s2, _) = build("defined CONFIG_X", CondKind::If);
        assert_eq!(s.get(a).lits().len(), 1);
        assert_eq!(s2.get(b).lits().len(), 1);
        assert!(s.get(a).lits()[0].positive());
    }

    #[test]
    fn negated_defined() {
        let (a, s, _) = build("!defined(CONFIG_X)", CondKind::If);
        assert_eq!(s.get(a).lits().len(), 1);
        assert!(!s.get(a).lits()[0].positive());
    }

    #[test]
    fn opaque_expression_becomes_one_atom() {
        let (a, s, _) = build("LINUX_VERSION > 5", CondKind::If);
        assert_eq!(s.get(a).lits().len(), 1, "should be a single opaque atom");
    }

    #[test]
    fn opaque_expression_and_its_negation_contradict() {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let mut s = PcStore::new();
        let p = PcParser::new(&wk);

        let t1 = lex(b"VER > 5", &mut it);
        let a = p.build(CondKind::If, &t1, &mut s, &mut it);
        let b = s.not(a);
        assert!(s.contradicts(a, b));
    }
}
