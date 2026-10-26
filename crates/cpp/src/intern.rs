use rustc_hash::FxHashMap;

pub type SymId = u32;

pub const SYM_INVALID: SymId = u32::MAX;

/// Anything that can hand out and resolve symbol ids.
///
/// Exists so the lexer, expander and renderer can run against either a plain
/// `Interner` (single-threaded build) or a `ThreadInterner` (a read-only shared
/// table plus per-thread overflow), without the read path taking a lock.
pub trait Symbols {
    fn intern(&mut self, s: &str) -> SymId;
    fn intern_bytes(&mut self, b: &[u8]) -> SymId;
    fn text(&self, id: SymId) -> &str;
}

impl Symbols for Interner {
    fn intern(&mut self, s: &str) -> SymId {
        Interner::intern(self, s)
    }
    fn intern_bytes(&mut self, b: &[u8]) -> SymId {
        Interner::intern_bytes(self, b)
    }
    fn text(&self, id: SymId) -> &str {
        Interner::text(self, id)
    }
}

/// A worker's view: the shared table is read-only, and anything genuinely new
/// (only `##` paste results in practice, since the pre-pass already interned
/// every identifier in the tree) lands in a private overflow above `base`.
pub struct ThreadInterner<'a> {
    global: &'a Interner,
    base: SymId,
    local: Interner,
}

impl<'a> ThreadInterner<'a> {
    pub fn new(global: &'a Interner) -> Self {
        Self {
            global,
            base: global.len() as SymId,
            local: Interner::new(),
        }
    }

    pub fn overflow(&self) -> usize {
        self.local.len()
    }
}

impl Symbols for ThreadInterner<'_> {
    fn intern(&mut self, s: &str) -> SymId {
        if let Some(id) = self.global.lookup(s) {
            return id;
        }
        self.base + self.local.intern(s)
    }

    fn intern_bytes(&mut self, b: &[u8]) -> SymId {
        match std::str::from_utf8(b) {
            Ok(s) => Symbols::intern(self, s),
            Err(_) => {
                let owned = String::from_utf8_lossy(b).into_owned();
                Symbols::intern(self, &owned)
            }
        }
    }

    fn text(&self, id: SymId) -> &str {
        if id < self.base {
            self.global.text(id)
        } else {
            self.local.text(id - self.base)
        }
    }
}

/// Per-thread string interner. Workers each own one; ids are merged into a
/// global table at the end of a pass.
pub struct Interner {
    map: FxHashMap<&'static str, SymId>,
    texts: Vec<&'static str>,
    arena: Vec<String>,
}

const CHUNK: usize = 1 << 20;

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

impl Interner {
    pub fn new() -> Self {
        let mut s = Self {
            map: FxHashMap::default(),
            texts: Vec::new(),
            arena: vec![String::with_capacity(CHUNK)],
        };
        // Keep id 0 as the empty string so SymId::default() is harmless.
        s.intern("");
        s
    }

    pub fn intern(&mut self, s: &str) -> SymId {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let stored = self.alloc(s);
        let id = self.texts.len() as SymId;
        self.texts.push(stored);
        self.map.insert(stored, id);
        id
    }

    pub fn intern_bytes(&mut self, b: &[u8]) -> SymId {
        // Source may contain non-UTF8 in comments/literals; tokens we intern are
        // identifiers, numbers, punctuation and literal spellings.
        match std::str::from_utf8(b) {
            Ok(s) => self.intern(s),
            Err(_) => {
                let owned = String::from_utf8_lossy(b).into_owned();
                self.intern(&owned)
            }
        }
    }

    /// Folds another interner in, returning `other`'s ids mapped onto this
    /// one's. Lets each worker intern independently and reconcile afterwards.
    pub fn merge_from(&mut self, other: &Interner) -> Vec<SymId> {
        let mut map = Vec::with_capacity(other.len());
        for i in 0..other.len() as SymId {
            map.push(self.intern(other.text(i)));
        }
        map
    }

    /// Read-only lookup, so worker threads can share one table without locks.
    #[inline]
    pub fn lookup(&self, s: &str) -> Option<SymId> {
        self.map.get(s).copied()
    }

    #[inline]
    pub fn text(&self, id: SymId) -> &str {
        self.texts[id as usize]
    }

    pub fn len(&self) -> usize {
        self.texts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }

    /// Bump-allocates into owned chunks that are never reallocated, so the
    /// `&'static str` handed out stays valid for the interner's lifetime.
    /// Safety rests on the arena chunks never being freed or moved before the
    /// map and texts that reference them.
    fn alloc(&mut self, s: &str) -> &'static str {
        if s.len() > CHUNK / 4 {
            let owned = s.to_string();
            let ptr: *const str = owned.as_str();
            self.arena.push(owned);
            return unsafe { &*ptr };
        }
        let cur = self.arena.last().unwrap();
        // Never let a chunk grow past its initial capacity: reallocating would
        // move the heap buffer and dangle every &str already handed out.
        if cur.len() + s.len() > cur.capacity() {
            self.arena.push(String::with_capacity(CHUNK));
        }
        let cur = self.arena.last_mut().unwrap();
        let start = cur.len();
        cur.push_str(s);
        let ptr: *const str = &cur[start..start + s.len()];
        unsafe { &*ptr }
    }
}

// The arena is owned by the interner and its chunks are never reallocated, so
// the interior `&'static str` references never escape it.
unsafe impl Send for Interner {}
