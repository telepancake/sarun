//! Chupa owns local mirror creation, updates, publication, browsing, and
//! reading. It has no Sarun dependency; Sarun supplies only product adapters.

pub mod gateway;
pub mod reader;
mod reader_bindings;
pub mod supervisor;
pub mod tui;

pub use reader_bindings::{
    ReaderAction, ReaderBindingContext, reader_action, reader_context_hint,
};

// Compatibility namespace used by the reader moved out of Sarun. Keeping the
// dependency pointing inward makes the ownership boundary explicit while the
// public types live at the Chupa crate root.
pub(crate) mod ui {
    pub use crate::{ReaderAction, ReaderBindingContext, reader_action, reader_context_hint};
}

#[cfg(test)]
pub(crate) mod depot {
    pub static TEST_STATE_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

#[cfg(test)]
pub(crate) mod paths {
    pub fn state_home() -> std::path::PathBuf {
        crate::supervisor::state_home()
    }
}
