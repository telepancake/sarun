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

use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use anyhow::Result;
use parking_lot::Mutex;

use crate::file::Makefile;

static CACHE: LazyLock<Mutex<MakefileCacheManager>> = LazyLock::new(|| {
    Mutex::new(MakefileCacheManager {
        cache: HashMap::new(),
        extra_file_deps: HashSet::new(),
    })
});

struct MakefileCacheManager {
    // sarun: keyed by the RESOLVED absolute path (display name joined onto the
    // requesting Evaluator's working_dir), not the relative name. Two in-process
    // makes in different directories that both `include Makefile` therefore get
    // their own (correct) entries instead of colliding, and the cache stays
    // safely shareable across instances.
    cache: HashMap<PathBuf, Option<Arc<Makefile>>>,
    extra_file_deps: HashSet<OsString>,
}

impl MakefileCacheManager {
    fn get_makefile(
        &mut self,
        display_name: &OsStr,
        open_path: &Path,
    ) -> Result<Option<Arc<Makefile>>> {
        if let Some(mk) = self.cache.get(open_path) {
            return Ok(mk.clone());
        }
        let mk = Makefile::from_file(display_name, open_path)?;
        self.cache.insert(open_path.to_path_buf(), mk.clone());
        Ok(mk)
    }
}

/// sarun: resolve a makefile/include name against a logical working directory.
/// Absolute names pass through; relative names are joined onto `base_dir` (the
/// Evaluator's working_dir). The result is used ONLY for the fs read and as the
/// cache key — the original `filename` is what gets interned for
/// Locs/$(MAKEFILE_LIST), so display strings are unchanged. With base_dir ==
/// process cwd this names the same file a bare relative open would.
fn resolve(base_dir: &Path, filename: &OsStr) -> PathBuf {
    let p = Path::new(filename);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

pub fn get_makefile(filename: &OsStr, base_dir: &Path) -> Result<Option<Arc<Makefile>>> {
    let open_path = resolve(base_dir, filename);
    CACHE.lock().get_makefile(filename, &open_path)
}

pub fn add_extra_file_dep(filename: OsString) {
    CACHE.lock().extra_file_deps.insert(filename);
}

pub fn get_all_filenames() -> HashSet<OsString> {
    let manager = CACHE.lock();
    let mut ret = HashSet::new();
    for p in manager.cache.keys() {
        ret.insert(p.as_os_str().to_os_string());
    }
    for f in &manager.extra_file_deps {
        ret.insert(f.clone());
    }
    ret
}
