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

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::{
    collections::HashMap,
    ffi::{CStr, CString, OsStr},
    process::{Command, ExitStatus},
    slice,
    sync::{Arc, LazyLock},
    time::SystemTime,
};

use anyhow::Result;
use bytes::{BufMut, Bytes, BytesMut};
use memchr::memchr2;

// sarun: hook for embedders (the sarun engine) to substitute their own
// recipe runner — runs the shell command IN-PROCESS via embedded brush
// instead of fork+exec'ing /bin/sh. Returns the recipe's exit code; the
// merged stdout+stderr bytes are pushed through `output_cb` as they
// arrive. When `None` (rkati standalone), `exec.rs` falls back to the
// classic `run_command()` path below. The closure form (boxed) lets
// callers capture a tokio runtime / oneshot context without making
// kati depend on brush.
pub enum RecipeRunnerDecision {
    /// Hook ran the command; here's the merged output and "success" flag.
    Ran { success: bool, output: Vec<u8> },
    /// Hook declined (e.g. SHELL not a /bin/sh-shaped shell); let
    /// `exec.rs` fall through to the classic fork+exec path.
    Passthrough,
}

pub type RecipeRunner = Box<
    dyn Fn(&[u8] /* shell */, &[u8] /* shellflag */, &[u8] /* cmd */, &mut dyn FnMut(&[u8]))
        -> RecipeRunnerDecision
        + Send
        + Sync
        + 'static,
>;

static RECIPE_RUNNER: parking_lot::Mutex<Option<RecipeRunner>> =
    parking_lot::Mutex::new(None);

/// Install an in-process recipe runner. exec.rs will consult it before
/// fork+exec'ing /bin/sh; the runner may handle the command (returning
/// Ran) or decline and fall back to the classic path (Passthrough).
/// Idempotent; last call wins.
pub fn install_recipe_runner(f: RecipeRunner) {
    *RECIPE_RUNNER.lock() = Some(f);
}

/// Run `cmd` through the installed in-process runner, if any. Returns
/// `Some((success, merged_output))` when the hook handled the command,
/// `None` when no hook is installed OR the hook declined.
pub fn run_with_installed_runner(
    shell: &[u8],
    shellflag: &[u8],
    cmd: &[u8],
) -> Option<(bool, Vec<u8>)> {
    let guard = RECIPE_RUNNER.lock();
    let runner = guard.as_ref()?;
    let mut out = Vec::new();
    match runner(shell, shellflag, cmd, &mut |b| out.extend_from_slice(b)) {
        RecipeRunnerDecision::Ran { success, output } => {
            // The runner pushed bytes through our local callback; ignore
            // the `output` field (kept for callers that prefer to build
            // their own buffer). Drop it to avoid confusing copies.
            let _ = output;
            Some((success, out))
        }
        RecipeRunnerDecision::Passthrough => None,
    }
}
use parking_lot::Mutex;

use crate::log;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectStderr {
    None,
    Stdout,
    DevNull,
}

pub fn get_timestamp(filename: &[u8]) -> Result<Option<SystemTime>> {
    let filename = <OsStr as OsStrExt>::from_bytes(filename);
    if !std::fs::exists(filename)? {
        return Ok(None);
    }
    let metadata = std::fs::metadata(filename)?;
    Ok(Some(metadata.modified()?))
}

pub fn run_command(
    shell: &[u8],
    shellflag: &[u8],
    cmd: &Bytes,
    redirect_stderr: RedirectStderr,
) -> Result<(ExitStatus, Vec<u8>)> {
    let mut cmd_with_shell;
    let args = if !shell.starts_with(b"/") || memchr2(b' ', b'$', shell).is_some() {
        let cmd_escaped = crate::strutil::escape_shell(cmd);
        cmd_with_shell = BytesMut::new();
        cmd_with_shell.put_slice(shell);
        cmd_with_shell.put_u8(b' ');
        cmd_with_shell.put_slice(shellflag);
        cmd_with_shell.put_slice(b" \"");
        cmd_with_shell.put_slice(&cmd_escaped);
        cmd_with_shell.put_u8(b'\"');
        &[
            <OsStr as OsStrExt>::from_bytes(b"/bin/sh"),
            <OsStr as OsStrExt>::from_bytes(b"-c"),
            <OsStr as OsStrExt>::from_bytes(&cmd_with_shell),
        ]
    } else {
        // If the shell isn't complicated, we don't need to wrap in /bin/sh
        &[
            <OsStr as OsStrExt>::from_bytes(shell),
            <OsStr as OsStrExt>::from_bytes(shellflag),
            <OsStr as OsStrExt>::from_bytes(cmd),
        ]
    };

    log!("run_command({args:?})");

    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..]);

    let (mut reader, writer) = os_pipe::pipe()?;
    match redirect_stderr {
        RedirectStderr::None => {
            cmd.stderr(std::process::Stdio::inherit());
        }
        RedirectStderr::Stdout => {
            cmd.stderr(writer.try_clone()?);
        }
        RedirectStderr::DevNull => {
            cmd.stderr(std::process::Stdio::null());
        }
    }
    cmd.stdout(writer);

    let mut handle = cmd.spawn()?;
    // Drop the cmd, otherwise the pipe will be retained.
    drop(cmd);

    let mut output = Vec::new();
    reader.read_to_end(&mut output)?;

    let res = handle.wait()?;

    Ok((res, output))
}

pub type GlobResults = Arc<Result<Vec<Bytes>, std::io::Error>>;

pub static GLOB_CACHE: LazyLock<Mutex<HashMap<Bytes, GlobResults>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn glob(pat: Bytes) -> GlobResults {
    let mut cache = GLOB_CACHE.lock();
    if let Some(entry) = cache.get(&pat) {
        return entry.clone();
    }
    let glob = Arc::new(
        if pat.contains(&b'?') || pat.contains(&b'*') || pat.contains(&b'[') || pat.contains(&b'\\')
        {
            libc_glob(&pat)
        } else if let Err(err) = std::fs::metadata(<OsStr as OsStrExt>::from_bytes(&pat)) {
            Err(err)
        } else {
            Ok(vec![pat.clone()])
        },
    );
    cache.insert(pat, glob.clone());
    glob
}

// Use libc glob over the `glob` crate, to maintain compatibility.
// The glob crate ends up normalizing the paths too much:
//   ./src/*_test.cc -> src/find_test.cc
// This breaks makefiles that do further string manipulation.
fn libc_glob(pattern: &[u8]) -> Result<Vec<Bytes>, std::io::Error> {
    let pat = CString::new(pattern).unwrap();
    let mut ret = Vec::new();
    // SAFETY: All of the types in glob_t are safe to be zero'd.
    let mut gl: libc::glob_t = unsafe { std::mem::zeroed() };
    // SAFETY: gl has been zero'd above, and pat is used as an input.
    // We'll free any allocated memory with globfree below.
    let r = unsafe { libc::glob(pat.as_ptr(), 0, None, &mut gl) };
    if r == 0 && gl.gl_pathc > 0 && !gl.gl_pathv.is_null() {
        // SAFETY: We've verified that glob succeeded, and the
        // gl_pathv is not null.
        //
        // We assume that the pointers are properly aligned.
        //
        // We can't guarantee that these came from the same allocated
        // object, but this is also only temporary, and will not be
        // used past the globfree which will deallocate any memory.
        let paths = unsafe { slice::from_raw_parts(gl.gl_pathv, gl.gl_pathc) };
        ret.reserve_exact(gl.gl_pathc);
        for ptr in paths {
            if !ptr.is_null() {
                // SAFETY: This is a non-null pointer, and we assume
                // glob created valid C strings. We're immediately
                // copying out of this string, so mutability and
                // lifetimes aren't issues.
                let s = unsafe { CStr::from_ptr(*ptr) };
                ret.push(Bytes::from(s.to_bytes().to_owned()));
            }
        }
    }
    // SAFETY: we're no longer using anything from gl, and this will
    // only free things allocated by libc::glob.
    unsafe { libc::globfree(&mut gl) };
    Ok(ret)
}

pub fn fnmatch(pattern: &CString, string: &[u8], flags: i32) -> bool {
    let string = CString::new(string).unwrap();
    // SAFETY: This is a relatively simple C func, both CStrings are inputs
    // and only need to last through the function call.
    unsafe { libc::fnmatch(pattern.as_ptr(), string.as_ptr(), flags) == 0 }
}

pub fn clear_glob_cache() {
    GLOB_CACHE.lock().clear();
}
