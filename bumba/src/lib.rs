//! Bumba is a reusable, single-process build shell.
//!
//! Brush supplies shell semantics, rkati and n2 supply Make and Ninja graph
//! execution, and selected uutils run against Brush's logical I/O without
//! spawning utility processes. Hosts customize observation and filesystem
//! access through public interfaces; Bumba has no Sarun dependency.

mod builtin_exec;
pub mod coreutils;
pub mod event;
pub mod exec_wrappers;
pub mod find;
mod interpose;
pub mod jobserver;
pub mod make;
pub mod ninja;
pub mod shell;
mod xargs;

pub use event::{
    BuildEdge, ContextProvider, Event, EventContext, EventSink, set_context_provider,
    set_event_sink,
};
pub use shell::{RecipeExecutor, RecipeStderr, ShellOptions, run, run_recipe, set_recipe_executor};

pub use kati::filesystem::{
    DirEntry, FileKind, FileSystemProvider, Metadata, NativeFileSystem,
};
