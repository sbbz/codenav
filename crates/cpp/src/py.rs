//! Python parsing, symbol extraction and import scanning.
//!
//! Far simpler than the C path: no preprocessor, so no macro table, no presence
//! conditions and no location map — tree-sitter positions are already source
//! positions. Every symbol therefore carries `PC_TRUE`.
//!
//! Unlike C, the import graph is not needed *during* extraction (nothing is
//! expanded), only for query-time ranking. So each file is parsed exactly once
//! and yields its symbols and its imports together.

use crate::files::FileId;
use crate::intern::Symbols;
use crate::symbols::{
    ExtractStats, Extracted, Occurrence, Role, SymKind, Symbol, SymbolId, NO_SYMBOL,
};
use tree_sitter::{Node, Tree};

/// One `import` as written, before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImport {
    /// Dotted module path as a relative path fragment, e.g. `a/b/c`. Empty for
    /// a bare `from . import x`.
    pub path: String,
    /// Leading dots: 0 absolute, 1 current package, 2 parent, and so on.
    pub level: u32,
}

/// Node kind ids resolved once so the walk compares integers, not strings.
pub struct PyKinds {
    import_statement: u16,
    import_from_statement: u16,
    dotted_name: u16,
    relative_import: u16,
    import_prefix: u16,
    aliased_import: u16,
    assignment: u16,
    class_definition: u16,
    function_definition: u16,
    default_parameter: u16,
    typed_parameter: u16,
    typed_default_parameter: u16,
    list_splat_pattern: u16,
    dictionary_splat_pattern: u16,
    call: u16,
    attribute: u16,
    identifier: u16,
    decorator: u16,
}

impl PyKinds {
    pub fn new(lang: &tree_sitter::Language) -> Self {
        let k = |n: &str| lang.id_for_node_kind(n, true);
        Self {
            import_statement: k("import_statement"),
            import_from_statement: k("import_from_statement"),
            dotted_name: k("dotted_name"),
            relative_import: k("relative_import"),
            import_prefix: k("import_prefix"),
            aliased_import: k("aliased_import"),
            assignment: k("assignment"),
            class_definition: k("class_definition"),
            function_definition: k("function_definition"),
            default_parameter: k("default_parameter"),
            typed_parameter: k("typed_parameter"),
            typed_default_parameter: k("typed_default_parameter"),
            list_splat_pattern: k("list_splat_pattern"),
            dictionary_splat_pattern: k("dictionary_splat_pattern"),
            call: k("call"),
            attribute: k("attribute"),
            identifier: k("identifier"),
            decorator: k("decorator"),
        }
    }

    fn is_param(&self, id: u16) -> bool {
        id == self.identifier
            || id == self.default_parameter
            || id == self.typed_parameter
            || id == self.typed_default_parameter
            || id == self.list_splat_pattern
            || id == self.dictionary_splat_pattern
    }
}

/// Guards against pathological nesting exhausting a worker's stack, matching
/// the C extractor. Real Python nests far shallower than this.
const MAX_DEPTH: u32 = 8000;

pub struct PyExtractor<'a, S: Symbols> {
    k: &'a PyKinds,
    src: &'a [u8],
    file: FileId,
    it: &'a mut S,
    out: Extracted,
    imports: Vec<RawImport>,
    stats: ExtractStats,
}

impl<'a, S: Symbols> PyExtractor<'a, S> {
    pub fn new(k: &'a PyKinds, src: &'a [u8], file: FileId, it: &'a mut S) -> Self {
        Self {
            k,
            src,
            file,
            it,
            out: Extracted::default(),
            imports: Vec::new(),
            stats: ExtractStats::default(),
        }
    }

    pub fn run(mut self, tree: &Tree) -> (Extracted, Vec<RawImport>, ExtractStats) {
        self.visit(tree.root_node(), NO_SYMBOL, false, 0);
        (self.out, self.imports, self.stats)
    }

    fn text(&self, n: Node) -> &'a [u8] {
        &self.src[n.start_byte()..n.end_byte()]
    }

    fn push_symbol(&mut self, n: Node, kind: SymKind, enclosing: SymbolId) -> SymbolId {
        let sym = self
            .it
            .intern_bytes(&self.src[n.start_byte()..n.end_byte()]);
        let id = self.out.symbols.len() as SymbolId;
        let (off, len) = (
            n.start_byte() as u32,
            (n.end_byte() - n.start_byte()) as u32,
        );
        self.out.symbols.push(Symbol {
            name: sym,
            kind,
            is_def: true,
            file: self.file,
            off,
            len,
            pc: 0,
            enclosing,
        });
        self.out.occurrences.push(Occurrence {
            name: sym,
            file: self.file,
            off,
            len,
            role: Role::Def,
            pc: 0,
            enclosing,
        });
        self.stats.attributed += 1;
        id
    }

    fn push_occurrence(&mut self, n: Node, role: Role, enclosing: SymbolId) {
        let sym = self
            .it
            .intern_bytes(&self.src[n.start_byte()..n.end_byte()]);
        self.out.occurrences.push(Occurrence {
            name: sym,
            file: self.file,
            off: n.start_byte() as u32,
            len: (n.end_byte() - n.start_byte()) as u32,
            role,
            pc: 0,
            enclosing,
        });
        self.stats.attributed += 1;
    }

    /// `a.b.c` -> `a/b/c`, so the existing path-suffix resolver can be reused.
    fn dotted_to_path(&self, n: Node) -> String {
        String::from_utf8_lossy(self.text(n)).replace('.', "/")
    }

    fn record_import(&mut self, n: Node) {
        let id = n.kind_id();
        let mut cur = n.walk();

        if id == self.k.import_statement {
            for c in n.named_children(&mut cur) {
                let target = if c.kind_id() == self.k.aliased_import {
                    c.named_child(0)
                } else {
                    Some(c)
                };
                if let Some(t) = target.filter(|t| t.kind_id() == self.k.dotted_name) {
                    self.imports.push(RawImport {
                        path: self.dotted_to_path(t),
                        level: 0,
                    });
                }
            }
            return;
        }

        // `from X import ...` — only the module field matters for the graph.
        if let Some(m) = n.child_by_field_name("module_name") {
            if m.kind_id() == self.k.relative_import {
                let mut level = 0u32;
                let mut path = String::new();
                let mut c2 = m.walk();
                for part in m.named_children(&mut c2) {
                    if part.kind_id() == self.k.import_prefix {
                        level = self.text(part).iter().filter(|&&b| b == b'.').count() as u32;
                    } else if part.kind_id() == self.k.dotted_name {
                        path = self.dotted_to_path(part);
                    }
                }
                self.imports.push(RawImport { path, level });
            } else if m.kind_id() == self.k.dotted_name {
                self.imports.push(RawImport {
                    path: self.dotted_to_path(m),
                    level: 0,
                });
            }
        }
    }

    fn visit(&mut self, n: Node, enclosing: SymbolId, in_class: bool, depth: u32) {
        if depth >= MAX_DEPTH {
            self.stats.too_deep += 1;
            return;
        }
        let depth = depth + 1;
        let id = n.kind_id();
        let mut cur = n.walk();

        if id == self.k.import_statement || id == self.k.import_from_statement {
            self.record_import(n);
            // Imported names are still references worth indexing.
            let mut c2 = n.walk();
            for c in n.named_children(&mut c2) {
                self.visit(c, enclosing, in_class, depth);
            }
            return;
        }

        if id == self.k.function_definition {
            // A function defined directly in a class body is a method.
            let kind = if in_class {
                SymKind::Method
            } else {
                SymKind::Function
            };
            let inner = match n.child_by_field_name("name") {
                Some(name) => self.push_symbol(name, kind, enclosing),
                None => enclosing,
            };
            if let Some(params) = n.child_by_field_name("parameters") {
                let mut c2 = params.walk();
                for p in params.named_children(&mut c2) {
                    if let Some(name) = self.param_name(p) {
                        self.push_symbol(name, SymKind::Param, inner);
                    }
                    // Defaults and annotations are ordinary expressions.
                    if p.kind_id() != self.k.identifier {
                        self.visit(p, inner, false, depth);
                    }
                }
            }
            if let Some(body) = n.child_by_field_name("body") {
                self.visit(body, inner, false, depth);
            }
            // Decorators and return annotations belong to the enclosing scope.
            for c in n.named_children(&mut cur) {
                if c.kind_id() == self.k.decorator {
                    self.visit(c, enclosing, in_class, depth);
                }
            }
            return;
        }

        if id == self.k.class_definition {
            let inner = match n.child_by_field_name("name") {
                Some(name) => self.push_symbol(name, SymKind::Class, enclosing),
                None => enclosing,
            };
            // Base classes are references in the enclosing scope.
            if let Some(bases) = n.child_by_field_name("superclasses") {
                self.visit(bases, enclosing, in_class, depth);
            }
            if let Some(body) = n.child_by_field_name("body") {
                self.visit(body, inner, true, depth);
            }
            return;
        }

        if id == self.k.assignment {
            if let Some(lhs) = n.child_by_field_name("left") {
                if lhs.kind_id() == self.k.identifier {
                    // Class-body assignments are fields; elsewhere, variables.
                    let kind = if in_class {
                        SymKind::Field
                    } else {
                        SymKind::Variable
                    };
                    self.push_symbol(lhs, kind, enclosing);
                } else if lhs.kind_id() == self.k.attribute {
                    // `self.x = ...` declares an attribute on the class.
                    if let Some(attr) = lhs.child_by_field_name("attribute") {
                        self.push_symbol(attr, SymKind::Field, enclosing);
                    }
                    if let Some(obj) = lhs.child_by_field_name("object") {
                        self.visit(obj, enclosing, in_class, depth);
                    }
                } else {
                    self.visit(lhs, enclosing, in_class, depth);
                }
            }
            if let Some(rhs) = n.child_by_field_name("right") {
                self.visit(rhs, enclosing, in_class, depth);
            }
            return;
        }

        if id == self.k.call {
            if let Some(f) = n.child_by_field_name("function") {
                if f.kind_id() == self.k.identifier {
                    self.push_occurrence(f, Role::Call, enclosing);
                } else if f.kind_id() == self.k.attribute {
                    // `obj.method()` — the method name is the callee.
                    if let Some(attr) = f.child_by_field_name("attribute") {
                        self.push_occurrence(attr, Role::Call, enclosing);
                    }
                    if let Some(obj) = f.child_by_field_name("object") {
                        self.visit(obj, enclosing, in_class, depth);
                    }
                } else {
                    self.visit(f, enclosing, in_class, depth);
                }
            }
            if let Some(args) = n.child_by_field_name("arguments") {
                self.visit(args, enclosing, in_class, depth);
            }
            return;
        }

        if id == self.k.attribute {
            if let Some(attr) = n.child_by_field_name("attribute") {
                self.push_occurrence(attr, Role::Ref, enclosing);
            }
            if let Some(obj) = n.child_by_field_name("object") {
                self.visit(obj, enclosing, in_class, depth);
            }
            return;
        }

        if id == self.k.identifier {
            self.stats.identifiers += 1;
            if n.parent().is_some_and(|p| p.has_error()) {
                self.stats.in_error_nodes += 1;
            }
            self.push_occurrence(n, Role::Ref, enclosing);
            return;
        }

        for c in n.named_children(&mut cur) {
            self.visit(c, enclosing, in_class, depth);
        }
    }

    /// The identifier a parameter binds, looking through defaults, annotations
    /// and splats. Generic over the node's lifetime so the tree's borrow is
    /// carried through rather than transmuted away.
    fn param_name<'n>(&self, p: Node<'n>) -> Option<Node<'n>> {
        let id = p.kind_id();
        if !self.k.is_param(id) {
            return None;
        }
        if id == self.k.identifier {
            return Some(p);
        }
        p.child_by_field_name("name")
            .or_else(|| p.named_child(0))
            .filter(|c| c.kind_id() == self.k.identifier)
    }
}

/// Candidate file paths a module could live in, most specific first.
///
/// `a.b.c` is either `a/b/c.py` or the package `a/b/c/__init__.py`.
pub fn module_candidates(path: &str) -> Vec<String> {
    if path.is_empty() {
        return vec!["__init__.py".to_string()];
    }
    vec![format!("{path}.py"), format!("{path}/__init__.py")]
}

/// Resolves a Python import to a file.
///
/// Absolute imports go through the same inferred roots as C includes — the
/// resolver only ever matched on path suffixes, and `a.b.c` rewritten to
/// `a/b/c` is exactly that. Relative imports walk up from the importing file's
/// own package instead.
pub fn resolve_import(
    reg: &crate::files::FileRegistry,
    resolver: &crate::files::Resolver,
    from_dir: &std::path::Path,
    imp: &RawImport,
) -> Option<FileId> {
    if imp.level > 0 {
        // `from .` is the current package, `from ..` its parent, and so on.
        let mut base = from_dir.to_path_buf();
        for _ in 1..imp.level {
            base = base.parent()?.to_path_buf();
        }
        for cand in module_candidates(&imp.path) {
            let p = crate::files::normalize(&base.join(&cand));
            if let Some(id) = reg.get(&p) {
                return Some(id);
            }
        }
        return None;
    }

    for cand in module_candidates(&imp.path) {
        if let Some(id) = resolver.resolve(reg, from_dir, &cand, true) {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intern::Interner;
    use tree_sitter::Parser;

    struct Out {
        e: Extracted,
        imports: Vec<RawImport>,
        it: Interner,
    }

    impl Out {
        fn of(&self, kind: SymKind) -> Vec<&str> {
            self.e
                .symbols
                .iter()
                .filter(|s| s.kind == kind)
                .map(|s| self.it.text(s.name))
                .collect()
        }
        fn calls(&self) -> Vec<(&str, &str)> {
            self.e
                .occurrences
                .iter()
                .filter(|o| o.role == Role::Call && o.enclosing != NO_SYMBOL)
                .map(|o| {
                    (
                        self.it.text(self.e.symbols[o.enclosing as usize].name),
                        self.it.text(o.name),
                    )
                })
                .collect()
        }
    }

    fn extract(src: &str) -> Out {
        let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        let mut p = Parser::new();
        p.set_language(&lang).unwrap();
        let tree = p.parse(src.as_bytes(), None).unwrap();
        let kinds = PyKinds::new(&lang);
        let mut it = Interner::new();
        let (e, imports, _) = PyExtractor::new(&kinds, src.as_bytes(), 0, &mut it).run(&tree);
        Out { e, imports, it }
    }

    #[test]
    fn functions_and_classes() {
        let o = extract("def make(n):\n    return n\n\nclass Widget:\n    pass\n");
        assert_eq!(o.of(SymKind::Function), ["make"]);
        assert_eq!(o.of(SymKind::Class), ["Widget"]);
        assert_eq!(o.of(SymKind::Param), ["n"]);
    }

    #[test]
    fn methods_are_distinguished_from_functions() {
        let o = extract("class W:\n    def render(self):\n        pass\n\ndef free():\n    pass\n");
        assert_eq!(o.of(SymKind::Method), ["render"]);
        assert_eq!(o.of(SymKind::Function), ["free"]);
    }

    #[test]
    fn methods_are_enclosed_by_their_class() {
        let o = extract("class W:\n    def render(self):\n        pass\n");
        let class_idx =
            o.e.symbols
                .iter()
                .position(|s| s.kind == SymKind::Class)
                .unwrap();
        let m =
            o.e.symbols
                .iter()
                .find(|s| s.kind == SymKind::Method)
                .unwrap();
        assert_eq!(m.enclosing as usize, class_idx);
    }

    #[test]
    fn self_assignment_declares_a_field() {
        let o = extract("class W:\n    def __init__(self, size):\n        self.size = size\n");
        assert!(
            o.of(SymKind::Field).contains(&"size"),
            "{:?}",
            o.of(SymKind::Field)
        );
    }

    #[test]
    fn class_body_assignment_is_a_field() {
        let o = extract("class W:\n    kind = 'w'\n");
        assert_eq!(o.of(SymKind::Field), ["kind"]);
    }

    #[test]
    fn module_level_assignment_is_a_variable() {
        let o = extract("CONST = 42\n");
        assert_eq!(o.of(SymKind::Variable), ["CONST"]);
    }

    #[test]
    fn calls_carry_the_enclosing_function() {
        let o = extract("def outer():\n    helper()\n");
        assert_eq!(o.calls(), [("outer", "helper")]);
    }

    #[test]
    fn method_calls_record_the_method_name() {
        let o = extract("def go(w):\n    return w.render(None)\n");
        assert_eq!(o.calls(), [("go", "render")]);
    }

    #[test]
    fn absolute_imports_become_paths() {
        let o = extract("import os\nimport a.b.c\n");
        assert_eq!(
            o.imports,
            vec![
                RawImport {
                    path: "os".into(),
                    level: 0
                },
                RawImport {
                    path: "a/b/c".into(),
                    level: 0
                },
            ]
        );
    }

    #[test]
    fn from_imports_record_the_module_not_the_names() {
        let o = extract("from pkg.sub import helper as h\n");
        assert_eq!(
            o.imports,
            vec![RawImport {
                path: "pkg/sub".into(),
                level: 0
            }]
        );
    }

    #[test]
    fn relative_imports_record_their_level() {
        let o = extract("from . import sibling\nfrom ..pkg import thing\n");
        assert_eq!(
            o.imports,
            vec![
                RawImport {
                    path: "".into(),
                    level: 1
                },
                RawImport {
                    path: "pkg".into(),
                    level: 2
                },
            ]
        );
    }

    #[test]
    fn module_candidates_cover_module_and_package() {
        assert_eq!(module_candidates("a/b"), ["a/b.py", "a/b/__init__.py"]);
    }

    fn reg_with(paths: &[&str]) -> crate::files::FileRegistry {
        let mut r = crate::files::FileRegistry::default();
        for p in paths {
            r.add(std::path::PathBuf::from(p));
        }
        r
    }

    #[test]
    fn absolute_import_resolves_through_an_inferred_root() {
        let reg = reg_with(&["proj/pkg/sub.py", "proj/app/main.py"]);
        let mut t = rustc_hash::FxHashMap::default();
        t.insert("pkg/sub.py".to_string(), 1u32);
        let r = crate::files::Resolver::infer(&reg, &t);
        let got = resolve_import(
            &reg,
            &r,
            std::path::Path::new("proj/app"),
            &RawImport {
                path: "pkg/sub".into(),
                level: 0,
            },
        );
        assert_eq!(got, reg.get(std::path::Path::new("proj/pkg/sub.py")));
    }

    #[test]
    fn package_import_resolves_to_dunder_init() {
        let reg = reg_with(&["proj/pkg/__init__.py", "proj/app/main.py"]);
        let mut t = rustc_hash::FxHashMap::default();
        t.insert("pkg/__init__.py".to_string(), 1u32);
        let r = crate::files::Resolver::infer(&reg, &t);
        let got = resolve_import(
            &reg,
            &r,
            std::path::Path::new("proj/app"),
            &RawImport {
                path: "pkg".into(),
                level: 0,
            },
        );
        assert_eq!(got, reg.get(std::path::Path::new("proj/pkg/__init__.py")));
    }

    #[test]
    fn single_dot_import_stays_in_the_same_package() {
        let reg = reg_with(&["proj/pkg/mod.py", "proj/pkg/sibling.py"]);
        let r = crate::files::Resolver::new(vec![]);
        let got = resolve_import(
            &reg,
            &r,
            std::path::Path::new("proj/pkg"),
            &RawImport {
                path: "sibling".into(),
                level: 1,
            },
        );
        assert_eq!(got, reg.get(std::path::Path::new("proj/pkg/sibling.py")));
    }

    #[test]
    fn double_dot_import_walks_up_a_package() {
        let reg = reg_with(&["proj/pkg/sub/mod.py", "proj/pkg/other.py"]);
        let r = crate::files::Resolver::new(vec![]);
        let got = resolve_import(
            &reg,
            &r,
            std::path::Path::new("proj/pkg/sub"),
            &RawImport {
                path: "other".into(),
                level: 2,
            },
        );
        assert_eq!(got, reg.get(std::path::Path::new("proj/pkg/other.py")));
    }

    #[test]
    fn unresolvable_import_is_none_not_a_guess() {
        let reg = reg_with(&["proj/app/main.py"]);
        let r = crate::files::Resolver::new(vec![]);
        assert_eq!(
            resolve_import(
                &reg,
                &r,
                std::path::Path::new("proj/app"),
                &RawImport {
                    path: "numpy".into(),
                    level: 0
                },
            ),
            None
        );
    }

    #[test]
    fn decorators_are_recorded_as_references() {
        let o = extract("@decorator\ndef f():\n    pass\n");
        let refs: Vec<&str> =
            o.e.occurrences
                .iter()
                .filter(|x| x.role == Role::Ref)
                .map(|x| o.it.text(x.name))
                .collect();
        assert!(refs.contains(&"decorator"), "{refs:?}");
    }
}
