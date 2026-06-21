// Copyright 2017 Google Inc.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

use std::cell::RefCell;
use std::error::Error;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Matcher, MatcherIO, WalkEntry};

/// sarun: the directory a `-execdir`/`-okdir` command runs in (the entry's
/// parent), or `None` for the shell's own logical cwd — used both by a normal
/// `-exec` (the subshell already has the cwd) and the awkward `parent == ""`
/// case. Mirrors the `current_dir` choices in the process-spawning path.
fn exec_dir_cwd(exec_in_parent_dir: bool, file_info: &WalkEntry) -> Option<PathBuf> {
    if !exec_in_parent_dir {
        return None;
    }
    match file_info.path().parent() {
        None => Some(file_info.path().to_path_buf()), // root "/" → run from "/"
        Some(p) if p == Path::new("") => None,        // "foo" → avoid chdir("")
        Some(p) => Some(p.to_path_buf()),
    }
}

enum Arg {
    FileArg(Vec<OsString>),
    LiteralArg(OsString),
}

fn parse_arg(s: &str) -> Arg {
    let parts = s.split("{}").collect::<Vec<_>>();
    if parts.len() == 1 {
        Arg::LiteralArg(OsString::from(s))
    } else {
        Arg::FileArg(parts.iter().map(OsString::from).collect())
    }
}

pub struct SingleExecMatcher {
    executable: Arg,
    args: Vec<Arg>,
    exec_in_parent_dir: bool,
    interactive: bool,
}

impl SingleExecMatcher {
    pub fn new(
        executable: &str,
        args: &[&str],
        exec_in_parent_dir: bool,
    ) -> Result<Self, Box<dyn Error>> {
        Ok(Self::new_impl(executable, args, exec_in_parent_dir, false))
    }

    pub fn new_interactive(
        executable: &str,
        args: &[&str],
        exec_in_parent_dir: bool,
    ) -> Result<Self, Box<dyn Error>> {
        Ok(Self::new_impl(executable, args, exec_in_parent_dir, true))
    }

    fn new_impl(
        executable: &str,
        args: &[&str],
        exec_in_parent_dir: bool,
        interactive: bool,
    ) -> Self {
        let transformed_args = args.iter().map(|&a| parse_arg(a)).collect();

        Self {
            executable: parse_arg(executable),
            args: transformed_args,
            exec_in_parent_dir,
            interactive,
        }
    }
}

impl Matcher for SingleExecMatcher {
    fn matches(&self, file_info: &WalkEntry, matcher_io: &mut MatcherIO) -> bool {
        let path_to_file = if self.exec_in_parent_dir {
            if let Some(f) = file_info.path().file_name() {
                Path::new(".").join(f)
            } else {
                Path::new(".").join(file_info.path())
            }
        } else {
            // {} substitutes the DISPLAY (relative) path the user expects; the
            // child runs in the embedder's logical cwd (set below) so it still
            // resolves. -execdir keeps the "./basename" + real-parent-cwd form.
            file_info.display_path().to_path_buf()
        };

        let resolved_executable = match self.executable {
            Arg::LiteralArg(ref a) => a.clone(),
            Arg::FileArg(ref parts) => parts.join(path_to_file.as_os_str()),
        };

        if self.interactive {
            // GNU find prints a fixed, abbreviated prompt of the form
            // "< executable ... pathname > ? ".  It does not render the
            // substituted argument list, and always shows the full path of
            // the entry being processed (even for -okdir, whose command runs
            // with the "./basename" form).
            let prompt = format!(
                "< {} ... {} > ? ",
                resolved_executable.to_string_lossy(),
                file_info.path().to_string_lossy()
            );

            if !matcher_io.confirm(&prompt) {
                return false;
            }
        }

        // The fully-substituted argv (program + each arg, with {} replaced).
        let mut argv: Vec<OsString> = vec![resolved_executable.clone()];
        for arg in &self.args {
            argv.push(match *arg {
                Arg::LiteralArg(ref a) => a.clone(),
                Arg::FileArg(ref parts) => parts.join(path_to_file.as_os_str()),
            });
        }

        // sarun: run the command through the embedder's shell (builtin /
        // function / external, snooped) instead of spawning a process. -execdir
        // runs in the entry's parent dir; a normal -exec runs in the shell's own
        // logical cwd (cwd == None — the subshell already has it).
        if matcher_io.deps.exec_via_shell() {
            let cwd = exec_dir_cwd(self.exec_in_parent_dir, file_info);
            return matcher_io.deps.run(&argv, cwd.as_deref()) == 0;
        }

        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        if self.exec_in_parent_dir {
            match file_info.path().parent() {
                None => {
                    // Root paths like "/" have no parent.  Run them from the root to match GNU find.
                    command.current_dir(file_info.path());
                }
                Some(parent) if parent == Path::new("") => {
                    // Paths like "foo" have a parent of "".  Avoid chdir("").
                }
                Some(parent) => {
                    command.current_dir(parent);
                }
            }
        } else if let Some(d) = matcher_io.deps.cwd() {
            // sarun: a normal -exec child runs in the embedder's logical cwd, so
            // the relative {} (display path) resolves there without the engine
            // ever changing its own process cwd.
            command.current_dir(d);
        }
        // Route the child's stdout/stderr to the embedder's logical streams when
        // provided (the in-process builtin dups the shell's logical fds), so
        // `find … -exec cmd \; > file` / `| next` honor the box's redirects and
        // pipes. Default (standalone) inherits the process fds, as upstream does.
        if let Some(s) = matcher_io.deps.child_stdout() {
            command.stdout(s);
        }
        if let Some(s) = matcher_io.deps.child_stderr() {
            command.stderr(s);
        }
        match command.status() {
            Ok(status) => status.success(),
            Err(e) => {
                writeln!(
                    &mut *matcher_io.deps.get_error_output().borrow_mut(),
                    "Failed to run {}: {}",
                    resolved_executable.to_string_lossy(),
                    e
                )
                .unwrap();
                false
            }
        }
    }

    fn has_side_effects(&self) -> bool {
        true
    }
}

pub struct MultiExecMatcher {
    executable: String,
    args: Vec<OsString>,
    exec_in_parent_dir: bool,
    /// Command to build while matching (the process-spawning / standalone path).
    command: RefCell<Option<argmax::Command>>,
    /// sarun: accumulated path arguments for the shell-exec path. In-process
    /// `run_argv` has no `execve` arg-length limit, so we just collect every
    /// matched path and flush them in one command per batch (a directory for
    /// `-execdir`, or the whole run for `-exec`).
    pending: RefCell<Vec<OsString>>,
}

impl MultiExecMatcher {
    pub fn new(
        executable: &str,
        args: &[&str],
        exec_in_parent_dir: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let transformed_args = args.iter().map(OsString::from).collect();

        Ok(Self {
            executable: executable.to_string(),
            args: transformed_args,
            exec_in_parent_dir,
            command: RefCell::new(None),
            pending: RefCell::new(Vec::new()),
        })
    }

    /// sarun: flush the accumulated `pending` paths as ONE command through the
    /// embedder's shell, in `cwd` (the entry's parent for `-execdir`, else the
    /// logical cwd). A non-zero exit sets find's exit code, matching the
    /// process-spawning path.
    fn dispatch_via_shell(&self, cwd: Option<&Path>, matcher_io: &mut MatcherIO) {
        let pending = std::mem::take(&mut *self.pending.borrow_mut());
        if pending.is_empty() {
            return;
        }
        let mut argv: Vec<OsString> = Vec::with_capacity(1 + self.args.len() + pending.len());
        argv.push(OsString::from(&self.executable));
        argv.extend(self.args.iter().cloned());
        argv.extend(pending);
        if matcher_io.deps.run(&argv, cwd) != 0 {
            matcher_io.set_exit_code(1);
        }
    }

    fn new_command(&self) -> argmax::Command {
        let mut command = argmax::Command::new(&self.executable);
        command.try_args(&self.args).unwrap();
        command
    }

    fn run_command(&self, command: &mut argmax::Command, matcher_io: &mut MatcherIO) {
        // sarun: a normal `-exec … +` child runs in the embedder's logical cwd
        // so the relative {} (display) paths resolve. -execdir already set its
        // own current_dir (the real parent) before calling us, so only do this
        // for the non-execdir form.
        if !self.exec_in_parent_dir {
            if let Some(d) = matcher_io.deps.cwd() {
                command.current_dir(d);
            }
        }
        // Same child-stdio routing as the single-exec path: dup the embedder's
        // logical streams into the batched `-exec … +` child (argmax::Command
        // forwards stdout/stderr to the inner std Command).
        if let Some(s) = matcher_io.deps.child_stdout() {
            command.stdout(s);
        }
        if let Some(s) = matcher_io.deps.child_stderr() {
            command.stderr(s);
        }
        match command.status() {
            Ok(status) => {
                if !status.success() {
                    matcher_io.set_exit_code(1);
                }
            }
            Err(e) => {
                writeln!(&mut *matcher_io.deps.get_error_output().borrow_mut(), "Failed to run {}: {}", self.executable, e).unwrap();
                matcher_io.set_exit_code(1);
            }
        }
    }
}

impl Matcher for MultiExecMatcher {
    fn matches(&self, file_info: &WalkEntry, matcher_io: &mut MatcherIO) -> bool {
        let path_to_file = if self.exec_in_parent_dir {
            if let Some(f) = file_info.path().file_name() {
                Path::new(".").join(f)
            } else {
                Path::new(".").join(file_info.path())
            }
        } else {
            file_info.display_path().to_path_buf()
        };

        // sarun: shell-exec path — just accumulate; flushed per batch in
        // finished()/finished_dir(). No `execve` arg limit applies in-process.
        if matcher_io.deps.exec_via_shell() {
            self.pending.borrow_mut().push(path_to_file.into_os_string());
            return true;
        }

        let mut command = self.command.borrow_mut();
        let command = command.get_or_insert_with(|| self.new_command());

        // Build command, or dispatch it before when it is long enough.
        if command.try_arg(&path_to_file).is_err() {
            if self.exec_in_parent_dir {
                match file_info.path().parent() {
                    None => {
                        // Root paths like "/" have no parent.  Run them from the root to match GNU find.
                        command.current_dir(file_info.path());
                    }
                    Some(parent) if parent == Path::new("") => {
                        // Paths like "foo" have a parent of "".  Avoid chdir("").
                    }
                    Some(parent) => {
                        command.current_dir(parent);
                    }
                }
            }
            self.run_command(command, matcher_io);

            // Reset command status.
            *command = self.new_command();
            if let Err(e) = command.try_arg(&path_to_file) {
                writeln!(
                    &mut *matcher_io.deps.get_error_output().borrow_mut(),
                    "Cannot fit a single argument {}: {}",
                    &path_to_file.to_string_lossy(),
                    e
                )
                .unwrap();
                matcher_io.set_exit_code(1);
            }
        }
        true
    }

    fn finished_dir(&self, dir: &Path, matcher_io: &mut MatcherIO) {
        // Dispatch command for -execdir (one batch per directory).
        if self.exec_in_parent_dir {
            if matcher_io.deps.exec_via_shell() {
                self.dispatch_via_shell(Some(dir), matcher_io);
                return;
            }
            let mut command = self.command.borrow_mut();
            if let Some(mut command) = command.take() {
                command.current_dir(Path::new(".").join(dir));
                self.run_command(&mut command, matcher_io);
            }
        }
    }

    fn finished(&self, matcher_io: &mut MatcherIO) {
        // Dispatch command for -exec (one batch for the whole run).
        if !self.exec_in_parent_dir {
            if matcher_io.deps.exec_via_shell() {
                self.dispatch_via_shell(None, matcher_io);
                return;
            }
            let mut command = self.command.borrow_mut();
            if let Some(mut command) = command.take() {
                self.run_command(&mut command, matcher_io);
            }
        }
    }

    fn has_side_effects(&self) -> bool {
        true
    }
}

#[cfg(test)]
/// No tests here, because we need to call out to an external executable. See
/// `tests/exec_unit_tests.rs` instead.
mod tests {}
