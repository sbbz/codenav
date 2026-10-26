//! Symbol and occurrence extraction from a parsed tree.
//!
//! Every occurrence carries the id of the symbol enclosing it. That single
//! field is what makes call hierarchy a one-seek lookup in both directions, so
//! it is populated even for plain references.

use crate::files::FileId;
use crate::intern::SymId;
use crate::render::Rendered;
use tree_sitter::{Node, Tree};

pub type SymbolId = u32;
pub const NO_SYMBOL: SymbolId = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SymKind {
    Function,
    Variable,
    Struct,
    Union,
    Enum,
    EnumConstant,
    Typedef,
    Field,
    Param,
    Macro,
    Label,
    /// Python: a class.
    Class,
    /// Python: a function defined directly in a class body.
    Method,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    /// A declaration without a body.
    Decl,
    /// A definition: function body, struct body, initialized object.
    Def,
    /// A plain read or write.
    Ref,
    /// The callee of a call expression.
    Call,
}

#[derive(Debug, Clone, Copy)]
pub struct Symbol {
    pub name: SymId,
    pub kind: SymKind,
    pub is_def: bool,
    pub file: FileId,
    /// Byte offset in the original source, via the location map.
    pub off: u32,
    pub len: u32,
    pub pc: u32,
    pub enclosing: SymbolId,
}

#[derive(Debug, Clone, Copy)]
pub struct Occurrence {
    pub name: SymId,
    pub file: FileId,
    pub off: u32,
    pub len: u32,
    pub role: Role,
    pub pc: u32,
    /// Symbol containing this occurrence, or NO_SYMBOL at file scope.
    pub enclosing: SymbolId,
}

#[derive(Debug, Default)]
pub struct Extracted {
    pub symbols: Vec<Symbol>,
    pub occurrences: Vec<Occurrence>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ExtractStats {
    pub identifiers: u64,
    pub attributed: u64,
    pub in_error_nodes: u64,
    /// Subtrees abandoned at MAX_DEPTH. Non-zero means real code is being
    /// skipped, so it is worth watching rather than silently tolerating.
    pub too_deep: u64,
}

/// The walk is recursive, and expanded kernel output can nest deeply enough to
/// exhaust a worker thread's stack. This bounds it independently of how much
/// stack the caller happens to have.
///
/// Sized to be a genuine backstop rather than a working limit: at 400 it was
/// discarding real code (63k occurrences across the kernel). Frames are small,
/// so 8000 costs well under a megabyte against the 64 MB worker stacks.
const MAX_DEPTH: u32 = 8000;

/// Node kind ids resolved once, so the walk compares integers rather than
/// strings at every node.
pub struct Kinds {
    function_definition: u16,
    declaration: u16,
    parameter_declaration: u16,
    field_declaration: u16,
    type_definition: u16,
    struct_specifier: u16,
    union_specifier: u16,
    enum_specifier: u16,
    enumerator: u16,
    call_expression: u16,
    identifier: u16,
    field_identifier: u16,
    type_identifier: u16,
    statement_identifier: u16,
    preproc_def: u16,
    preproc_function_def: u16,
    pointer_declarator: u16,
    array_declarator: u16,
    function_declarator: u16,
    parenthesized_declarator: u16,
    init_declarator: u16,
    attributed_declarator: u16,
}

impl Kinds {
    pub fn new(lang: &tree_sitter::Language) -> Self {
        let k = |n: &str| lang.id_for_node_kind(n, true);
        Self {
            function_definition: k("function_definition"),
            declaration: k("declaration"),
            parameter_declaration: k("parameter_declaration"),
            field_declaration: k("field_declaration"),
            type_definition: k("type_definition"),
            struct_specifier: k("struct_specifier"),
            union_specifier: k("union_specifier"),
            enum_specifier: k("enum_specifier"),
            enumerator: k("enumerator"),
            call_expression: k("call_expression"),
            identifier: k("identifier"),
            field_identifier: k("field_identifier"),
            type_identifier: k("type_identifier"),
            statement_identifier: k("statement_identifier"),
            preproc_def: k("preproc_def"),
            preproc_function_def: k("preproc_function_def"),
            pointer_declarator: k("pointer_declarator"),
            array_declarator: k("array_declarator"),
            function_declarator: k("function_declarator"),
            parenthesized_declarator: k("parenthesized_declarator"),
            init_declarator: k("init_declarator"),
            attributed_declarator: k("attributed_declarator"),
        }
    }

    fn is_declarator_wrapper(&self, id: u16) -> bool {
        id == self.pointer_declarator
            || id == self.array_declarator
            || id == self.parenthesized_declarator
            || id == self.init_declarator
            || id == self.attributed_declarator
    }

    fn is_name_node(&self, id: u16) -> bool {
        id == self.identifier
            || id == self.field_identifier
            || id == self.type_identifier
            || id == self.statement_identifier
    }
}

pub struct Extractor<'a> {
    k: &'a Kinds,
    r: &'a Rendered,
    file: FileId,
    out: Extracted,
    pub stats: ExtractStats,
}

impl<'a> Extractor<'a> {
    pub fn new(k: &'a Kinds, r: &'a Rendered, file: FileId) -> Self {
        Self {
            k,
            r,
            file,
            out: Extracted::default(),
            stats: ExtractStats::default(),
        }
    }

    pub fn run(mut self, tree: &Tree) -> (Extracted, ExtractStats) {
        self.visit(tree.root_node(), NO_SYMBOL, false, 0);
        (self.out, self.stats)
    }

    /// Descends a declarator chain to the identifier it ultimately names, and
    /// reports whether a function_declarator was passed through.
    fn declarator_name<'n>(&self, mut n: Node<'n>) -> Option<(Node<'n>, bool)> {
        let mut is_fn = false;
        loop {
            let id = n.kind_id();
            if self.k.is_name_node(id) {
                return Some((n, is_fn));
            }
            if id == self.k.function_declarator {
                is_fn = true;
            }
            let next = n.child_by_field_name("declarator").or_else(|| {
                n.named_child(0)
                    .filter(|_| self.k.is_declarator_wrapper(id))
            });
            n = next?;
        }
    }

    fn push_symbol(
        &mut self,
        name_node: Node,
        kind: SymKind,
        is_def: bool,
        enclosing: SymbolId,
    ) -> SymbolId {
        let start = name_node.start_byte() as u32;
        let Some(sym) = self.r.sym_at(start) else {
            return NO_SYMBOL;
        };
        let off = self.r.to_source(start).unwrap_or(0);
        let id = self.out.symbols.len() as SymbolId;
        self.out.symbols.push(Symbol {
            name: sym,
            kind,
            is_def,
            file: self.file,
            off,
            len: (name_node.end_byte() - name_node.start_byte()) as u32,
            pc: self.r.pc_at(start).unwrap_or(0),
            enclosing,
        });
        self.out.occurrences.push(Occurrence {
            name: sym,
            file: self.file,
            off,
            len: (name_node.end_byte() - name_node.start_byte()) as u32,
            role: if is_def { Role::Def } else { Role::Decl },
            pc: self.r.pc_at(start).unwrap_or(0),
            enclosing,
        });
        self.stats.attributed += 1;
        id
    }

    fn push_occurrence(&mut self, n: Node, role: Role, enclosing: SymbolId) {
        let start = n.start_byte() as u32;
        let Some(sym) = self.r.sym_at(start) else {
            return;
        };
        self.out.occurrences.push(Occurrence {
            name: sym,
            file: self.file,
            off: self.r.to_source(start).unwrap_or(0),
            len: (n.end_byte() - n.start_byte()) as u32,
            role,
            pc: self.r.pc_at(start).unwrap_or(0),
            enclosing,
        });
        self.stats.attributed += 1;
    }

    fn visit(&mut self, n: Node, enclosing: SymbolId, in_error: bool, depth: u32) {
        if depth >= MAX_DEPTH {
            self.stats.too_deep += 1;
            return;
        }
        let depth = depth + 1;
        let id = n.kind_id();
        let in_error = in_error || n.is_error();
        let mut cur = n.walk();

        // A function definition becomes the enclosing symbol for its body,
        // which is what gives call edges their caller.
        if id == self.k.function_definition {
            let inner = match n
                .child_by_field_name("declarator")
                .and_then(|d| self.declarator_name(d))
            {
                Some((name, _)) => self.push_symbol(name, SymKind::Function, true, enclosing),
                None => enclosing,
            };
            for c in n.named_children(&mut cur) {
                self.visit(c, inner, in_error, depth);
            }
            return;
        }

        if id == self.k.declaration {
            // A declaration may introduce several names.
            let mut declared = Vec::new();
            for c in n.children_by_field_name("declarator", &mut cur) {
                if let Some((name, is_fn)) = self.declarator_name(c) {
                    let has_init = c.child_by_field_name("value").is_some();
                    let kind = if is_fn {
                        SymKind::Function
                    } else {
                        SymKind::Variable
                    };
                    // A function declaration is never a definition here; an
                    // object with an initializer is.
                    let sid = self.push_symbol(name, kind, !is_fn && has_init, enclosing);
                    declared.push((c, sid));
                }
            }
            let mut c2 = n.walk();
            for c in n.named_children(&mut c2) {
                let owner = declared
                    .iter()
                    .find(|(d, _)| d.id() == c.id())
                    .map(|(_, s)| *s);
                match owner {
                    // Skip the name itself; visit the rest (array sizes,
                    // initializers) so their references are still recorded.
                    Some(_) => self.visit_skipping_name(c, enclosing, in_error, depth),
                    None => self.visit(c, enclosing, in_error, depth),
                }
            }
            return;
        }

        if id == self.k.type_definition {
            for c in n.children_by_field_name("declarator", &mut cur) {
                if let Some((name, _)) = self.declarator_name(c) {
                    self.push_symbol(name, SymKind::Typedef, true, enclosing);
                }
            }
            let mut c2 = n.walk();
            for c in n.named_children(&mut c2) {
                self.visit(c, enclosing, in_error, depth);
            }
            return;
        }

        if id == self.k.parameter_declaration || id == self.k.field_declaration {
            let kind = if id == self.k.parameter_declaration {
                SymKind::Param
            } else {
                SymKind::Field
            };
            let mut named = false;
            for c in n.children_by_field_name("declarator", &mut cur) {
                if let Some((name, _)) = self.declarator_name(c) {
                    self.push_symbol(name, kind, true, enclosing);
                    named = true;
                }
            }
            let mut c2 = n.walk();
            for c in n.named_children(&mut c2) {
                if named && self.k.is_name_node(c.kind_id()) {
                    continue;
                }
                self.visit(c, enclosing, in_error, depth);
            }
            return;
        }

        if id == self.k.struct_specifier
            || id == self.k.union_specifier
            || id == self.k.enum_specifier
        {
            let kind = if id == self.k.struct_specifier {
                SymKind::Struct
            } else if id == self.k.union_specifier {
                SymKind::Union
            } else {
                SymKind::Enum
            };
            let has_body = n.child_by_field_name("body").is_some();
            let inner = match n.child_by_field_name("name") {
                // Only a body makes this a symbol. A bare `struct foo *p` is a
                // use of the tag, not a declaration of it, and recording it as a
                // symbol would bury the real definition among thousands of uses.
                Some(name) if has_body => self.push_symbol(name, kind, true, enclosing),
                Some(name) => {
                    self.push_occurrence(name, Role::Ref, enclosing);
                    enclosing
                }
                None => enclosing,
            };
            if let Some(body) = n.child_by_field_name("body") {
                let mut c2 = body.walk();
                for c in body.named_children(&mut c2) {
                    self.visit(c, inner, in_error, depth);
                }
            }
            return;
        }

        if id == self.k.enumerator {
            if let Some(name) = n.child_by_field_name("name") {
                self.push_symbol(name, SymKind::EnumConstant, true, enclosing);
            }
            if let Some(v) = n.child_by_field_name("value") {
                self.visit(v, enclosing, in_error, depth);
            }
            return;
        }

        if id == self.k.preproc_def || id == self.k.preproc_function_def {
            if let Some(name) = n.child_by_field_name("name") {
                self.push_symbol(name, SymKind::Macro, true, enclosing);
            }
            return;
        }

        if id == self.k.call_expression {
            if let Some(f) = n.child_by_field_name("function") {
                if self.k.is_name_node(f.kind_id()) {
                    self.push_occurrence(f, Role::Call, enclosing);
                } else {
                    self.visit(f, enclosing, in_error, depth);
                }
            }
            if let Some(args) = n.child_by_field_name("arguments") {
                let mut c2 = args.walk();
                for c in args.named_children(&mut c2) {
                    self.visit(c, enclosing, in_error, depth);
                }
            }
            return;
        }

        if self.k.is_name_node(id) {
            self.stats.identifiers += 1;
            if in_error {
                self.stats.in_error_nodes += 1;
            }
            self.push_occurrence(n, Role::Ref, enclosing);
            return;
        }

        for c in n.named_children(&mut cur) {
            self.visit(c, enclosing, in_error, depth);
        }
    }

    /// Visits a declarator's subtree without re-recording the name it declares.
    fn visit_skipping_name(&mut self, n: Node, enclosing: SymbolId, in_error: bool, depth: u32) {
        let skip = self.declarator_name(n).map(|(x, _)| x.id());
        let mut cur = n.walk();
        for c in n.named_children(&mut cur) {
            if Some(c.id()) == skip {
                continue;
            }
            self.visit(c, enclosing, in_error, depth);
        }
    }
}

/// Builds caller -> callee edges from extracted occurrences.
pub fn call_edges(e: &Extracted) -> Vec<(SymbolId, SymId)> {
    e.occurrences
        .iter()
        .filter(|o| o.role == Role::Call && o.enclosing != NO_SYMBOL)
        .map(|o| (o.enclosing, o.name))
        .collect()
}

/// True when the tree is unusable rather than merely imperfect.
pub fn is_degenerate(tree: &Tree) -> bool {
    let root = tree.root_node();
    root.named_child_count() == 0 && root.byte_range().len() > 64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expand::{preprocess, Keywords, MacroTable};
    use crate::intern::Interner;
    use crate::lex::{lex, WellKnown};
    use crate::pc::PcStore;
    use crate::render::render;
    use tree_sitter::Parser;

    struct Out {
        e: Extracted,
        it: Interner,
    }

    impl Out {
        fn defs(&self, kind: SymKind) -> Vec<&str> {
            self.e
                .symbols
                .iter()
                .filter(|s| s.kind == kind)
                .map(|s| self.it.text(s.name))
                .collect()
        }
        fn sym(&self, name: &str) -> Option<&Symbol> {
            self.e.symbols.iter().find(|s| self.it.text(s.name) == name)
        }
        fn calls(&self) -> Vec<(&str, &str)> {
            call_edges(&self.e)
                .into_iter()
                .map(|(caller, callee)| {
                    (
                        self.it.text(self.e.symbols[caller as usize].name),
                        self.it.text(callee),
                    )
                })
                .collect()
        }
    }

    fn extract(src: &str) -> Out {
        let mut it = Interner::new();
        let wk = WellKnown::new(&mut it);
        let kw = Keywords::new(&mut it);
        let mut pcs = PcStore::new();
        let toks = lex(src.as_bytes(), &mut it);
        let mut mt = MacroTable::default();
        let (out, _) = preprocess(&toks, &mut mt, &mut it, &wk, &mut pcs, &kw);
        let r = render(&out, &it);

        let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
        let mut p = Parser::new();
        p.set_language(&lang).unwrap();
        let tree = p.parse(r.text.as_bytes(), None).unwrap();
        let kinds = Kinds::new(&lang);
        let (e, _) = Extractor::new(&kinds, &r, 0).run(&tree);
        Out { e, it }
    }

    #[test]
    fn function_definition_is_a_definition() {
        let o = extract("int add(int a, int b) { return a + b; }");
        assert_eq!(o.defs(SymKind::Function), ["add"]);
        assert!(o.sym("add").unwrap().is_def);
        assert_eq!(o.defs(SymKind::Param), ["a", "b"]);
    }

    #[test]
    fn function_declaration_is_not_a_definition() {
        let o = extract("int add(int a, int b);");
        assert!(!o.sym("add").unwrap().is_def);
    }

    #[test]
    fn struct_and_fields_are_extracted() {
        let o = extract("struct point { int x; int y; };");
        assert_eq!(o.defs(SymKind::Struct), ["point"]);
        assert_eq!(o.defs(SymKind::Field), ["x", "y"]);
    }

    #[test]
    fn fields_are_enclosed_by_their_struct() {
        let o = extract("struct point { int x; };");
        let s = o.sym("point").unwrap();
        let sid = o.e.symbols.iter().position(|q| q.name == s.name).unwrap();
        let f = o.sym("x").unwrap();
        assert_eq!(f.enclosing as usize, sid);
    }

    #[test]
    fn struct_mention_is_a_reference_not_a_symbol() {
        // `struct point *p` uses the tag; only the braced form declares it.
        let o = extract("struct point *p; struct point { int x; };");
        assert_eq!(
            o.defs(SymKind::Struct),
            ["point"],
            "exactly one struct symbol"
        );
        let refs =
            o.e.occurrences
                .iter()
                .filter(|x| o.it.text(x.name) == "point" && x.role == Role::Ref)
                .count();
        assert_eq!(refs, 1, "the mention must be recorded as a reference");
    }

    #[test]
    fn typedef_and_enum_constants() {
        let o = extract("typedef enum color { RED, GREEN } color_t;");
        assert_eq!(o.defs(SymKind::Typedef), ["color_t"]);
        assert_eq!(o.defs(SymKind::EnumConstant), ["RED", "GREEN"]);
    }

    #[test]
    fn call_edges_carry_the_enclosing_function() {
        let o = extract("void helper(void); void caller(void) { helper(); }");
        assert_eq!(o.calls(), [("caller", "helper")]);
    }

    #[test]
    fn nested_calls_all_attribute_to_the_same_caller() {
        let o = extract("void f(void) { a(); if (b()) { c(); } }");
        let calls = o.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|(caller, _)| *caller == "f"));
    }

    #[test]
    fn pointer_and_array_declarators_resolve_to_the_name() {
        let o = extract("char *name; int table[10]; int (*fp)(void);");
        let vars = o.defs(SymKind::Variable);
        assert!(vars.contains(&"name"), "{vars:?}");
        assert!(vars.contains(&"table"), "{vars:?}");
    }

    #[test]
    fn references_inside_a_function_are_attributed_to_it() {
        let o = extract("int g; void f(void) { g = 1; }");
        let f = o.sym("f").unwrap();
        let fid = o.e.symbols.iter().position(|q| q.name == f.name).unwrap() as SymbolId;
        let refs: Vec<_> =
            o.e.occurrences
                .iter()
                .filter(|x| o.it.text(x.name) == "g" && x.role == Role::Ref)
                .collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].enclosing, fid);
    }

    #[test]
    fn macro_generated_declaration_is_extracted() {
        // The kernel's EXPORT_SYMBOL shape: the symbol only exists after
        // expansion, which is exactly what ctags cannot see.
        let o = extract(
            "#define DEFINE_THING(n) int thing_##n; void thing_##n##_init(void) {}\n\
             DEFINE_THING(foo)",
        );
        let vars = o.defs(SymKind::Variable);
        let fns = o.defs(SymKind::Function);
        assert!(vars.contains(&"thing_foo"), "vars: {vars:?}");
        assert!(fns.contains(&"thing_foo_init"), "fns: {fns:?}");
    }

    #[test]
    fn macro_generated_symbol_maps_back_to_the_invocation() {
        let src = "#define DEF(n) int var_##n;\nDEF(abc)";
        let o = extract(src);
        let v = o.sym("var_abc").unwrap();
        // The offset must point at the DEF(abc) use, not into the #define line.
        assert!(
            v.off as usize >= src.find("DEF(abc)").unwrap(),
            "offset {} should be at the invocation",
            v.off
        );
    }

    #[test]
    fn both_conditional_branches_yield_symbols() {
        let o = extract("#ifdef A\nint from_a;\n#else\nint from_b;\n#endif");
        let vars = o.defs(SymKind::Variable);
        assert!(vars.contains(&"from_a"), "{vars:?}");
        assert!(vars.contains(&"from_b"), "{vars:?}");
    }
}
