pub mod expand;
pub mod files;
pub mod global;
pub mod graph;
pub mod incr;
pub mod incremental;
pub mod intern;
pub mod lex;
pub mod pc;
pub mod progress;
pub mod py;
pub mod render;
pub mod scan;
pub mod serve;
pub mod symbols;
pub mod tu;
pub mod watch;

/// The index directory, at the root of the indexed tree.
pub const INDEX_DIR: &str = ".codenav";
