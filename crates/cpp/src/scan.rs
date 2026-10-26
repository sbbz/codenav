//! Directive-only scanner: the Phase 0 measurement.
//!
//! Walks raw bytes tracking comment/string state and delimiter depth, and
//! classifies every preprocessor conditional group by how hostile it is to the
//! "concatenate all branches into one token stream" strategy.

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub files: u64,
    pub bytes: u64,

    pub groups: u64,
    pub include_guards: u64,

    /// A branch of the group changed `{}`/`()`/`[]` depth. Concatenating
    /// branches would produce unparseable text.
    pub unbalanced_delta: u64,
    /// The `#if` sits inside an unclosed `(` or `[` — argument lists,
    /// subscripts. The nastiest case; needs true forking.
    pub inside_parens: u64,
    /// The `#if` interrupts a declaration already in progress at brace depth 0.
    /// Informational: concatenation still parses, it just duplicates.
    pub mid_declaration: u64,
    /// unbalanced_delta OR inside_parens — the cases concatenation cannot serve.
    pub problematic: u64,

    pub defines: u64,
    pub includes: u64,
    pub max_nesting: u32,
    pub unterminated: u64,
}

impl Stats {
    pub fn merge(&mut self, o: &Stats) {
        self.files += o.files;
        self.bytes += o.bytes;
        self.groups += o.groups;
        self.include_guards += o.include_guards;
        self.unbalanced_delta += o.unbalanced_delta;
        self.inside_parens += o.inside_parens;
        self.mid_declaration += o.mid_declaration;
        self.problematic += o.problematic;
        self.defines += o.defines;
        self.includes += o.includes;
        self.max_nesting = self.max_nesting.max(o.max_nesting);
        self.unterminated += o.unterminated;
    }

    /// Conditionals excluding include guards — guards are always benign and
    /// including them in the denominator flatters the result.
    pub fn real_groups(&self) -> u64 {
        self.groups.saturating_sub(self.include_guards)
    }

    pub fn problematic_pct(&self) -> f64 {
        let d = self.real_groups();
        if d == 0 {
            0.0
        } else {
            self.problematic as f64 * 100.0 / d as f64
        }
    }
}

#[derive(Clone, Copy)]
struct Depths {
    paren: i32,
    brace: i32,
    bracket: i32,
}

struct Frame {
    /// Depths at the `#if`. Each branch is measured against this, since in real
    /// C every branch starts from the same state.
    start: Depths,
    branch_start: Depths,
    unbalanced: bool,
    in_parens: bool,
    mid_decl: bool,
}

pub fn scan(buf: &[u8]) -> Stats {
    let mut st = Stats {
        files: 1,
        bytes: buf.len() as u64,
        ..Default::default()
    };

    let mut d = Depths {
        paren: 0,
        brace: 0,
        bracket: 0,
    };
    let mut stack: Vec<Frame> = Vec::new();

    // Guard detection state.
    let mut first_group = true;
    let mut guard_name: Option<&[u8]> = None;
    let mut guard_armed = false;
    let mut guard_endif_at: Option<usize> = None;

    let mut code_since_boundary = false;
    let mut at_line_start = true;
    let mut i = 0usize;
    let n = buf.len();

    while i < n {
        let b = buf[i];

        // Line continuation: does not end the logical line.
        if b == b'\\' && i + 1 < n && (buf[i + 1] == b'\n' || buf[i + 1] == b'\r') {
            i += if buf[i + 1] == b'\r' && i + 2 < n && buf[i + 2] == b'\n' {
                3
            } else {
                2
            };
            continue;
        }

        if b == b'\n' {
            at_line_start = true;
            i += 1;
            continue;
        }
        if b == b' ' || b == b'\t' || b == b'\r' {
            i += 1;
            continue;
        }

        // Comments. A comment does not disturb line-start-ness: `/*c*/ #if` is legal.
        if b == b'/' && i + 1 < n {
            if buf[i + 1] == b'/' {
                while i < n && buf[i] != b'\n' {
                    if buf[i] == b'\\' && i + 1 < n && buf[i + 1] == b'\n' {
                        i += 1;
                    }
                    i += 1;
                }
                continue;
            }
            if buf[i + 1] == b'*' {
                i += 2;
                while i + 1 < n && !(buf[i] == b'*' && buf[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(n);
                continue;
            }
        }

        if at_line_start && b == b'#' {
            let (kind, arg, next) = parse_directive(buf, i);
            match kind {
                Dir::If => {
                    let in_parens = d.paren > 0 || d.bracket > 0;
                    let mid_decl = d.paren == 0 && code_since_boundary;
                    stack.push(Frame {
                        start: d,
                        branch_start: d,
                        unbalanced: false,
                        in_parens,
                        mid_decl,
                    });
                    st.max_nesting = st.max_nesting.max(stack.len() as u32);

                    if first_group && stack.len() == 1 {
                        first_group = false;
                        if let Some(name) = arg {
                            // `#ifndef NAME` as the first conditional in the file.
                            guard_name = Some(name);
                            guard_armed = false;
                        }
                    }
                }
                Dir::ElseLike => {
                    if let Some(f) = stack.last_mut() {
                        if d.paren != f.branch_start.paren
                            || d.brace != f.branch_start.brace
                            || d.bracket != f.branch_start.bracket
                        {
                            f.unbalanced = true;
                        }
                        // Every branch starts from the group's entry state.
                        let s = f.start;
                        f.branch_start = s;
                        d = s;
                    }
                    // An `#else` inside the guard group means it isn't a guard.
                    if stack.len() == 1 {
                        guard_name = None;
                    }
                }
                Dir::Endif => {
                    if let Some(f) = stack.pop() {
                        let unbalanced = f.unbalanced
                            || d.paren != f.branch_start.paren
                            || d.brace != f.branch_start.brace
                            || d.bracket != f.branch_start.bracket;
                        d = f.start;

                        st.groups += 1;
                        if unbalanced {
                            st.unbalanced_delta += 1;
                        }
                        if f.in_parens {
                            st.inside_parens += 1;
                        }
                        if f.mid_decl {
                            st.mid_declaration += 1;
                        }
                        if unbalanced || f.in_parens {
                            st.problematic += 1;
                        }
                        if stack.is_empty() && guard_armed && guard_name.is_some() {
                            guard_endif_at = Some(i);
                        }
                    }
                }
                Dir::Define => {
                    st.defines += 1;
                    // `#ifndef N` / `#define N` back to back arms the guard.
                    if let (Some(g), 1) = (guard_name, stack.len()) {
                        if arg == Some(g) && !guard_armed {
                            guard_armed = true;
                        }
                    }
                }
                Dir::Include => st.includes += 1,
                Dir::PragmaOnce => {
                    st.include_guards += 1;
                    guard_name = None;
                }
                Dir::Other => {}
            }
            i = next;
            at_line_start = true;
            continue;
        }

        at_line_start = false;

        match b {
            b'"' | b'\'' => {
                let q = b;
                i += 1;
                while i < n && buf[i] != q {
                    if buf[i] == b'\\' {
                        i += 1;
                    }
                    if i < n && buf[i] == b'\n' {
                        break; // unterminated; don't run off the file
                    }
                    i += 1;
                }
                i += 1;
                code_since_boundary = true;
                continue;
            }
            b'(' => d.paren += 1,
            b')' => d.paren -= 1,
            b'{' => d.brace += 1,
            b'}' => d.brace -= 1,
            b'[' => d.bracket += 1,
            b']' => d.bracket -= 1,
            _ => {}
        }

        match b {
            b';' | b'{' | b'}' => code_since_boundary = false,
            b',' if d.paren == 0 => code_since_boundary = false,
            _ => code_since_boundary = true,
        }

        i += 1;
    }

    st.unterminated += stack.len() as u64;

    // A guard only counts if its `#endif` is the last thing in the file.
    if let Some(pos) = guard_endif_at {
        if trailing_is_blank(buf, pos) {
            st.include_guards += 1;
        }
    }

    st
}

fn trailing_is_blank(buf: &[u8], endif_at: usize) -> bool {
    let mut i = endif_at;
    let n = buf.len();
    while i < n && buf[i] != b'\n' {
        i += 1;
    }
    while i < n {
        match buf[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if i + 1 < n && buf[i + 1] == b'/' => {
                while i < n && buf[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && buf[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(buf[i] == b'*' && buf[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            _ => return false,
        }
    }
    true
}

enum Dir {
    If,
    ElseLike,
    Endif,
    Define,
    Include,
    PragmaOnce,
    Other,
}

/// Returns the directive kind, its first identifier operand, and the offset
/// just past the end of the logical line.
fn parse_directive(buf: &[u8], hash: usize) -> (Dir, Option<&[u8]>, usize) {
    let n = buf.len();
    let mut i = hash + 1;
    while i < n && (buf[i] == b' ' || buf[i] == b'\t') {
        i += 1;
    }
    let ks = i;
    while i < n && buf[i].is_ascii_alphabetic() {
        i += 1;
    }
    let keyword = &buf[ks..i];

    let end = logical_line_end(buf, i);

    let kind = match keyword {
        b"if" | b"ifdef" | b"ifndef" => Dir::If,
        b"else" | b"elif" | b"elifdef" | b"elifndef" => Dir::ElseLike,
        b"endif" => Dir::Endif,
        b"define" => Dir::Define,
        b"include" | b"include_next" | b"import" => Dir::Include,
        b"pragma" => {
            let rest = &buf[i..end.min(n)];
            if rest.windows(4).any(|w| w.eq_ignore_ascii_case(b"once")) {
                Dir::PragmaOnce
            } else {
                Dir::Other
            }
        }
        _ => Dir::Other,
    };

    // Only `#ifndef`/`#define` operands matter, and only as bare identifiers.
    let arg = if matches!(keyword, b"ifndef" | b"define") {
        first_ident(&buf[i..end.min(n)])
    } else {
        None
    };

    (kind, arg, end)
}

fn first_ident(s: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    while i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
        i += 1;
    }
    let start = i;
    while i < s.len() && (s[i].is_ascii_alphanumeric() || s[i] == b'_') {
        i += 1;
    }
    if i > start {
        Some(&s[start..i])
    } else {
        None
    }
}

fn logical_line_end(buf: &[u8], mut i: usize) -> usize {
    let n = buf.len();
    while i < n {
        if buf[i] == b'\\' && i + 1 < n {
            if buf[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            if buf[i + 1] == b'\r' && i + 2 < n && buf[i + 2] == b'\n' {
                i += 3;
                continue;
            }
        }
        if buf[i] == b'\n' {
            return i;
        }
        i += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_toplevel_conditional_is_clean() {
        let s = scan(b"#ifdef A\nint a;\n#else\nint b;\n#endif\n");
        assert_eq!(s.groups, 1);
        assert_eq!(s.problematic, 0);
    }

    #[test]
    fn include_guard_is_recognized_and_excluded() {
        let s = scan(b"#ifndef H\n#define H\nint x;\n#endif\n");
        assert_eq!(s.groups, 1);
        assert_eq!(s.include_guards, 1);
        assert_eq!(s.real_groups(), 0);
    }

    #[test]
    fn conditional_inside_arg_list_is_problematic() {
        let s = scan(b"void f(int a,\n#ifdef A\n int b,\n#endif\n int c);\n");
        assert_eq!(s.inside_parens, 1);
        assert_eq!(s.problematic, 1);
    }

    #[test]
    fn unbalanced_brace_across_branches_is_problematic() {
        let s = scan(b"#ifdef A\nvoid f() {\n#else\nvoid f() { int x;\n#endif\n}\n");
        assert_eq!(s.unbalanced_delta, 1);
        assert_eq!(s.problematic, 1);
    }

    #[test]
    fn braces_in_strings_and_comments_do_not_count() {
        let s = scan(b"#ifdef A\nchar *p = \"{{{\"; /* ((( */\n#endif\n");
        assert_eq!(s.unbalanced_delta, 0);
        assert_eq!(s.problematic, 0);
    }

    #[test]
    fn statements_inside_function_body_are_not_mid_declaration() {
        let s = scan(b"void f(void) {\n int x;\n#ifdef A\n x = 1;\n#endif\n}\n");
        assert_eq!(s.mid_declaration, 0);
        assert_eq!(s.problematic, 0);
    }

    #[test]
    fn line_continuation_does_not_end_directive() {
        let s = scan(b"#define M(a) \\\n  do { a; } while (0)\n#ifdef A\nint z;\n#endif\n");
        assert_eq!(s.defines, 1);
        assert_eq!(s.groups, 1);
        assert_eq!(s.problematic, 0);
    }
}
