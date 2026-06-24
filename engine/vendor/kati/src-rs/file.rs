/*
Copyright 2025 Google LLC

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::Path, sync::Arc};

use anyhow::Result;
use bytes::Bytes;
use parking_lot::Mutex;

use crate::{
    parser::parse_file,
    stmt::Stmt,
    symtab::{Symbol, intern},
};

pub struct Makefile {
    pub filename: Symbol,
    pub stmts: Arc<Mutex<Vec<Stmt>>>,
}

impl Makefile {
    /// sarun: `display_name` is recorded for Locs / `$(MAKEFILE_LIST)` verbatim
    /// (usually the relative name the makefile referenced); `open_path` is where
    /// the bytes are actually read — the display name resolved against the
    /// Evaluator's logical `working_dir`. Splitting the two lets an in-process
    /// make read a makefile relative to a logical cwd without touching the
    /// process cwd, while keeping path strings in output unchanged. With
    /// `working_dir == process cwd` the two name the same file.
    pub fn from_file(display_name: &OsStr, open_path: &Path) -> Result<Option<Arc<Makefile>>> {
        if !std::fs::exists(open_path)? {
            return Ok(None);
        }

        let buf = Bytes::from(std::fs::read(open_path)?);

        let filename = intern(display_name.as_bytes().to_vec());
        let stmts = parse_file(&buf, filename)?;

        Ok(Some(Arc::new(Makefile { filename, stmts })))
    }
}
