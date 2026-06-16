pub mod canon;
mod db;
mod densemap;
mod depfile;
mod eval;
pub mod graph; // sarun: pub so the engine can walk the build graph for build_edges
mod hash;
pub mod load;
pub mod parse;
pub mod process; // sarun: pub so the engine can install the in-process executor hook
#[cfg(unix)]
mod process_posix;
// sarun: process_win dropped — this fork is unix-only.
mod progress;
mod progress_dumb;
mod progress_fancy;
pub mod run;
pub mod scanner;
pub mod signal; // sarun: pub so the engine can suppress n2's SIGINT handler
mod smallmap;
mod task;
mod terminal;
mod trace;
mod work;

// sarun: the upstream jemalloc #[global_allocator] block is dropped — the
// vendored fork is lib-only with the jemalloc feature removed, so the host
// binary owns the allocator.
