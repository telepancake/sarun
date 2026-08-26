use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

fn run_bumba(args: &[&str], cwd: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bumba"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run Bumba")
}

fn run_bumba_with_timeout(
    args: &[&str],
    cwd: &std::path::Path,
    timeout: std::time::Duration,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bumba"));
    command.args(args).current_dir(cwd);
    run_command_with_timeout(command, timeout)
}

fn run_command_with_timeout(
    mut command: Command,
    timeout: std::time::Duration,
) -> std::process::Output {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run Bumba");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).expect("read Bumba stdout");
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).expect("read Bumba stderr");
        bytes
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll Bumba") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().expect("kill timed-out Bumba");
            let _ = child.wait();
            panic!("Bumba did not complete within {timeout:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    std::process::Output {
        status,
        stdout: stdout_reader.join().expect("join stdout reader"),
        stderr: stderr_reader.join().expect("join stderr reader"),
    }
}

fn executable(path: &std::path::Path, script: &str) {
    fs::write(path, script).expect("write executable fixture");
    let mut permissions = fs::metadata(path).expect("fixture metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make fixture executable");
}

#[test]
fn standalone_cli_has_help_version_and_shell_argument_semantics() {
    let temp = tempfile::tempdir().expect("temporary project root");

    let help = run_bumba(&["--help"], temp.path());
    assert!(help.status.success(), "{}", String::from_utf8_lossy(&help.stderr));
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(help_text.contains("Usage:"), "{help_text}");
    assert!(help_text.contains("bumba -c COMMAND"), "{help_text}");

    let version = run_bumba(&["--version"], temp.path());
    assert!(version.status.success(), "{}", String::from_utf8_lossy(&version.stderr));
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        concat!("bumba ", env!("CARGO_PKG_VERSION"))
    );

    let positional = run_bumba(
        &["-c", "printf '%s|%s|%s\\n' \"$0\" \"$1\" \"$2\"", "logical-name", "one", "two"],
        temp.path(),
    );
    assert!(positional.status.success(), "{}", String::from_utf8_lossy(&positional.stderr));
    assert_eq!(
        String::from_utf8_lossy(&positional.stdout),
        "logical-name|one|two\n"
    );

    let exit_trap = run_bumba(
        &["-c", "trap \"printf 'exit-trap\\n'\" EXIT; printf 'body\\n'"],
        temp.path(),
    );
    assert!(exit_trap.status.success(), "{}", String::from_utf8_lossy(&exit_trap.stderr));
    assert_eq!(String::from_utf8_lossy(&exit_trap.stdout), "body\nexit-trap\n");

    let bad = run_bumba(&["--definitely-not-an-option"], temp.path());
    assert_eq!(bad.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("unknown option"),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );

    let missing = run_bumba(&["missing-script.sh"], temp.path());
    assert_eq!(missing.status.code(), Some(127));
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("missing-script.sh"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

#[test]
fn redirected_stdin_is_executed_as_a_script() {
    let temp = tempfile::tempdir().expect("temporary project root");
    let mut child = Command::new(env!("CARGO_BIN_EXE_bumba"))
        .current_dir(temp.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start Bumba");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"printf 'from stdin\\n'\n")
        .expect("write script");
    let output = child.wait_with_output().expect("wait for Bumba");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "from stdin\n");
}

#[test]
fn repeated_in_process_makes_keep_parallelism_and_silence_scoped() {
    let temp = tempfile::tempdir().expect("temporary project root");
    let warm = temp.path().join("warm.mk");
    let work = temp.path().join("work.mk");
    fs::write(&warm, ".PHONY: all\nall:\n\t:\n").expect("write warm makefile");
    fs::write(
        &work,
        ".PHONY: all a b c d\nall: a b c d\na b c d:\n\tsleep 0.5\n",
    )
    .expect("write parallel makefile");

    // The first make used to freeze kati's process-global FLAGS. A serial first
    // invocation therefore made the later -j4 build serial as well.
    let script = format!(
        "make -j1 -s -f '{}'; make -j4 -f '{}'",
        warm.display(),
        work.display(),
    );
    let started = std::time::Instant::now();
    let parallel = run_bumba(&["-c", &script], temp.path());
    let parallel_elapsed = started.elapsed();
    assert!(
        parallel.status.success(),
        "{}",
        String::from_utf8_lossy(&parallel.stderr)
    );
    let stdout = String::from_utf8_lossy(&parallel.stdout);
    assert_eq!(
        stdout.matches("sleep 0.5").count(),
        4,
        "the first make's -s leaked into the second invocation: {stdout:?}"
    );

    // Check the inverse: a parallel first make must not make a later -j1 build
    // concurrent. The four sleeps establish a deliberately wide timing gap.
    let script = format!(
        "make -j4 -s -f '{}'; make -j1 -s -f '{}'",
        warm.display(),
        work.display(),
    );
    let started = std::time::Instant::now();
    let serial = run_bumba(&["-c", &script], temp.path());
    let serial_elapsed = started.elapsed();
    assert!(
        serial.status.success(),
        "{}",
        String::from_utf8_lossy(&serial.stderr)
    );
    assert!(
        serial_elapsed.saturating_sub(parallel_elapsed)
            >= std::time::Duration::from_millis(700),
        "later make did not respect its own job count: parallel={parallel_elapsed:?}, serial={serial_elapsed:?}"
    );
}

#[test]
fn recursive_make_inherits_the_parent_parallelism() {
    let temp = tempfile::tempdir().expect("temporary project root");
    fs::write(
        temp.path().join("Makefile"),
        ".PHONY: all\nall:\n\t$(MAKE) -s -f child.mk\n",
    )
    .expect("write parent makefile");
    fs::write(
        temp.path().join("child.mk"),
        ".PHONY: all a b c d record\nall: a b c d record\na b c d:\n\tsleep 0.5\nrecord:\n\tprintf '%s\\n' '$(MAKEFLAGS)' > inherited-flags\n",
    )
    .expect("write child makefile");

    let output = Command::new(env!("CARGO_BIN_EXE_bumba"))
        .args(["make", "-j4", "-s"])
        .current_dir(temp.path())
        .env("BUMBA_SCHED_STATS", "1")
        .output()
        .expect("run recursive make");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let scheduler_stats = String::from_utf8_lossy(&output.stderr);
    assert!(
        scheduler_stats.lines().any(|line| line.contains("max_running=4")),
        "recursive make did not run four recipes concurrently:\n{scheduler_stats}"
    );
    let inherited = fs::read_to_string(temp.path().join("inherited-flags"))
        .expect("read recursive MAKEFLAGS");
    assert!(
        inherited.contains("--jobserver-auth=fifo:"),
        "recursive make did not inherit a real jobserver: {inherited:?}"
    );
}

#[test]
fn recursive_make_streams_more_than_pipe_capacity_through_logical_output() {
    let temp = tempfile::tempdir().expect("temporary project root");
    let levels = ["Makefile", "one.mk", "two.mk", "three.mk"];
    for (index, level) in levels.iter().enumerate() {
        let recipe = if let Some(next) = levels.get(index + 1) {
            format!(".PHONY: all\nall:\n\t$(MAKE) -s -f {next}\n")
        } else {
            ".PHONY: all\nall:\n\t@awk 'BEGIN { for (i = 0; i < 4096; ++i) print \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\" }'\n".to_string()
        };
        fs::write(temp.path().join(level), recipe).expect("write recursive makefile");
    }
    const EXPECTED_BYTES: u64 = 4096 * 65;

    let redirected = run_bumba_with_timeout(
        &["-c", "make -j4 -s > recursive.out"],
        temp.path(),
        std::time::Duration::from_secs(15),
    );
    assert!(
        redirected.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&redirected.stdout),
        String::from_utf8_lossy(&redirected.stderr),
    );
    assert!(redirected.stdout.is_empty());
    assert!(redirected.stderr.is_empty());
    assert_eq!(
        fs::metadata(temp.path().join("recursive.out"))
            .expect("recursive output metadata")
            .len(),
        EXPECTED_BYTES,
    );

    let piped = run_bumba_with_timeout(
        &["-c", "make -j4 -s | wc -c"],
        temp.path(),
        std::time::Duration::from_secs(15),
    );
    assert!(piped.status.success(), "{}", String::from_utf8_lossy(&piped.stderr));
    assert_eq!(
        String::from_utf8_lossy(&piped.stdout).trim().parse::<u64>().unwrap(),
        EXPECTED_BYTES,
    );

    let log_dir = temp.path().join("recursive-target-logs");
    let mut logged_command = Command::new(env!("CARGO_BIN_EXE_bumba"));
    logged_command
        .args(["make", "-j4", "-s"])
        .current_dir(temp.path())
        .env("BUMBA_TARGET_LOG_DIR", &log_dir);
    let logged = run_command_with_timeout(logged_command, std::time::Duration::from_secs(15));
    assert!(
        logged.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&logged.stdout),
        String::from_utf8_lossy(&logged.stderr),
    );
    assert_eq!(logged.stdout.len() as u64, EXPECTED_BYTES);
    assert!(logged.stderr.is_empty());
    let logged_bytes = fs::read_dir(&log_dir)
        .expect("read recursive target logs")
        .map(|entry| fs::read(entry.expect("target log entry").path()).expect("read target log"))
        .map(|bytes| bytes.len() as u64)
        .sum::<u64>();
    assert!(
        logged_bytes >= EXPECTED_BYTES,
        "target logs lost recursive output: expected at least {EXPECTED_BYTES}, got {logged_bytes}"
    );

    let warning = "$(warning 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef)\n";
    fs::write(
        temp.path().join("Makefile"),
        format!("{}.PHONY: all\nall:\n\t@:\n", warning.repeat(4096)),
    )
    .expect("write high-volume diagnostic makefile");
    let stderr_only_pipe = run_bumba_with_timeout(
        &["-c", "make -j4 -s 3>&1 1>/dev/null 2>&3 | wc -c"],
        temp.path(),
        std::time::Duration::from_secs(15),
    );
    assert!(
        stderr_only_pipe.status.success(),
        "{}",
        String::from_utf8_lossy(&stderr_only_pipe.stderr)
    );
    assert!(
        String::from_utf8_lossy(&stderr_only_pipe.stdout)
            .trim()
            .parse::<u64>()
            .unwrap()
            >= EXPECTED_BYTES,
        "make diagnostics did not reach the stderr-only pipeline: {}",
        String::from_utf8_lossy(&stderr_only_pipe.stdout),
    );
}

#[test]
fn standalone_make_direct_output_and_opt_in_target_logs_agree() {
    let temp = tempfile::tempdir().expect("temporary project root");
    fs::write(
        temp.path().join("Makefile"),
        ".PHONY: all\nall:\n\t@printf 'recipe-out\\n'\n\t@printf 'recipe-err\\n' >&2\n\t@/bin/ls marker\n",
    )
    .expect("write output Makefile");
    fs::write(temp.path().join("marker"), b"").expect("write external-command marker");

    let direct = run_bumba(&["make", "-s"], temp.path());
    assert!(direct.status.success(), "{}", String::from_utf8_lossy(&direct.stderr));
    assert_eq!(
        String::from_utf8_lossy(&direct.stdout),
        "recipe-out\nrecipe-err\nmarker\n",
    );
    assert!(direct.stderr.is_empty());

    let log_dir = temp.path().join("target-logs");
    let captured = Command::new(env!("CARGO_BIN_EXE_bumba"))
        .args(["make", "-s"])
        .current_dir(temp.path())
        .env("BUMBA_TARGET_LOG_DIR", &log_dir)
        .output()
        .expect("run Bumba with target logs");
    assert!(captured.status.success(), "{}", String::from_utf8_lossy(&captured.stderr));
    assert_eq!(captured.stdout, direct.stdout);
    assert!(captured.stderr.is_empty());
    let logs = fs::read_dir(&log_dir)
        .expect("read target logs")
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(logs.contains("recipe-out\n"), "{logs:?}");
    assert!(logs.contains("recipe-err\n"), "{logs:?}");
    assert!(logs.contains("marker\n"), "{logs:?}");
}

#[test]
fn up_to_date_included_makefiles_are_checked_silently() {
    let temp = tempfile::tempdir().expect("temporary project root");
    fs::write(temp.path().join("deps.mk"), "# already generated\n")
        .expect("write included makefile");
    fs::write(
        temp.path().join("Makefile"),
        ".DEFAULT_GOAL := all\ninclude deps.mk\n\ndeps.mk:\n\n.PHONY: all\nall:\n\t@:\n",
    )
    .expect("write including makefile");

    let output = run_bumba(&["make"], temp.path());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.stdout.is_empty(),
        "internal makefile-remake root leaked a goal diagnostic: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stderr.is_empty());
}

#[cfg(target_os = "linux")]
fn read_terminal_until(
    master: &mut std::fs::File,
    replies: &mut std::fs::File,
    transcript: &mut Vec<u8>,
    expected: &[u8],
) {
    use std::os::fd::AsRawFd as _;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let mut pollfd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, 100) };
        assert!(ready >= 0, "poll terminal: {}", std::io::Error::last_os_error());
        if ready == 0 {
            continue;
        }
        let mut buffer = [0u8; 4096];
        let count = master.read(&mut buffer).expect("read terminal output");
        if count == 0 {
            break;
        }
        let chunk = &buffer[..count];
        transcript.extend_from_slice(chunk);
        if chunk.windows(7).any(|window| window == b"\x1b]11;?\x1b") {
            replies
                .write_all(b"\x1b]11;rgb:0000/0000/0000\x1b\\")
                .expect("answer terminal background query");
        }
        let cursor_queries = chunk
            .windows(4)
            .filter(|window| *window == b"\x1b[6n")
            .count();
        for _ in 0..cursor_queries {
            replies
                .write_all(b"\x1b[1;1R")
                .expect("answer terminal cursor query");
        }
        if transcript.windows(expected.len()).any(|window| window == expected) {
            return;
        }
    }
    panic!(
        "terminal output did not contain {:?}: {:?}",
        String::from_utf8_lossy(expected),
        String::from_utf8_lossy(transcript)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_invocation_shows_a_prompt_and_runs_commands() {
    use std::os::fd::{FromRawFd as _, OwnedFd};
    use std::os::unix::process::CommandExt as _;

    let mut master_fd = -1;
    let mut slave_fd = -1;
    let opened = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };
    let mut output = std::fs::File::from(master);
    let mut input = output.try_clone().expect("clone terminal master");
    let child_stdin = std::fs::File::from(slave);
    let child_stdout = child_stdin.try_clone().expect("clone terminal slave");
    let child_stderr = child_stdin.try_clone().expect("clone terminal slave");

    let mut command = Command::new(env!("CARGO_BIN_EXE_bumba"));
    command
        .env("TERM", "xterm-256color")
        .stdin(child_stdin)
        .stdout(child_stdout)
        .stderr(child_stderr);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("start interactive Bumba");
    let mut transcript = Vec::new();
    let prompt: &[u8] = if unsafe { libc::geteuid() } == 0 {
        b"bumba# "
    } else {
        b"bumba$ "
    };
    read_terminal_until(&mut output, &mut input, &mut transcript, prompt);
    transcript.clear();
    input
        .write_all(b"printf 'interactive-ok\\n'\n")
        .expect("write interactive command");
    read_terminal_until(&mut output, &mut input, &mut transcript, b"interactive-ok\r\n");
    transcript.clear();
    read_terminal_until(&mut output, &mut input, &mut transcript, prompt);
    input.write_all(b"exit\n").expect("exit interactive shell");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("poll interactive Bumba") {
            assert!(status.success(), "interactive Bumba exited with {status}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "interactive Bumba did not exit"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn absolute_sdk_paths_and_nested_shell_exec_stay_in_process() {
    let temp = tempfile::tempdir().expect("temporary project root");
    let sdk = temp.path().join("sdk/bin");
    fs::create_dir_all(&sdk).expect("create fake SDK");
    let marker = temp.path().join("external-ran");
    let fake_body = format!("#!/bin/sh\nprintf external >> {}\nexit 99\n", marker.display());
    executable(&sdk.join("cat"), &fake_body);
    executable(&sdk.join("bash"), &fake_body);
    executable(&sdk.join("uname"), &fake_body);
    fs::write(temp.path().join("input.txt"), "from builtin\n").expect("write input");

    let driver = temp.path().join("driver.sh");
    let script = format!(
        "PATH={}:$PATH\n\
         export PATH\n\
         if test -z \"${{BUMBA_REEXEC:-}}\"; then\n\
         BUMBA_REEXEC=1\n\
         export BUMBA_REEXEC\n\
         CONFIG_SHELL=$(command -v bash)\n\
         exec \"$CONFIG_SHELL\" \"$0\" reentered\n\
         fi\n\
         CAT=$(command -v cat)\n\
         \"$CAT\" input.txt\n\
         UNAME=$(command -v uname)\n\
         \"$UNAME\" --definitely-invalid 2>uname.err || :\n\
         printf \"arg=%s\\n\" \"$1\"\n",
        sdk.display(),
    );
    fs::write(&driver, script).expect("write driver");

    let output = run_bumba(&[driver.to_str().expect("UTF-8 fixture path")], temp.path());
    assert!(
        output.status.success(),
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "from builtin\narg=reentered\n");
    assert!(
        output.stderr.is_empty(),
        "redirected builtin error escaped logical stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        fs::read_to_string(temp.path().join("uname.err"))
            .expect("read redirected diagnostic")
            .contains("unexpected argument")
    );
    assert!(!marker.exists(), "an absolute SDK executable escaped Bumba");
}

#[test]
fn mixed_coreutils_rebind_localization_without_cross_call_state() {
    let temp = tempfile::tempdir().expect("temporary project root");
    let output = run_bumba(
        &[
            "-c",
            "cat missing-one 2>&1 || :; rm missing-two 2>&1 || :; cat missing-three 2>&1 || :",
        ],
        temp.path(),
    );

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cat: missing-one:"), "{stdout}");
    assert!(stdout.contains("rm: cannot remove"), "{stdout}");
    assert!(stdout.contains("cat: missing-three:"), "{stdout}");
    assert!(!stdout.contains("cat-error-"), "raw cat Fluent key: {stdout}");
    assert!(!stdout.contains("rm-error-"), "raw rm Fluent key: {stdout}");
}

#[test]
fn mixed_pipeline_transports_keep_external_descriptors_native() {
    let temp = tempfile::tempdir().expect("temporary project root");
    let producer = temp.path().join("external-producer");
    let filter = temp.path().join("external-filter");
    executable(&producer, "#!/bin/sh\nprintf abc\n");
    executable(&filter, "#!/bin/sh\ncat\n");

    // The first pipeline has a kernel edge followed by a userspace edge. The
    // second has an external command between two leaf builtins, so both edges
    // must remain native. In either case no userspace endpoint may leak into
    // the descriptor table passed to exec.
    let script = format!(
        "'{}' | cat | wc -c; printf abc | '{}' | wc -c",
        producer.display(),
        filter.display(),
    );
    let output = run_bumba(&["-c", &script], temp.path());
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "3\n3\n");
}

#[test]
fn shell_make_and_ninja_build_real_projects() {
    let temp = tempfile::tempdir().expect("temporary project root");

    let input = temp.path().join("words.txt");
    fs::write(&input, b"pear\napple\npear\n").expect("write builtin input");
    let shell = run_bumba(
        &["-c", "cat words.txt | sort | uniq | wc -l"],
        temp.path(),
    );
    assert!(shell.status.success(), "{}", String::from_utf8_lossy(&shell.stderr));
    assert_eq!(String::from_utf8_lossy(&shell.stdout).trim(), "2");

    // Every edge here is a descriptor-free leaf builtin. The payload exceeds
    // the userspace pipe capacity, exercising bounded backpressure; `head`
    // then closes its input early and the producer must observe BrokenPipe
    // rather than hang or keep an unbounded buffer alive.
    fs::write(temp.path().join("large.bin"), vec![b'x'; 70_000])
        .expect("write userspace-pipe payload");
    let userspace_pipes = run_bumba(
        &[
            "-c",
            "cat large.bin | cat | wc -c; cat large.bin | head -c 1 | wc -c",
        ],
        temp.path(),
    );
    assert!(
        userspace_pipes.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&userspace_pipes.stdout),
        String::from_utf8_lossy(&userspace_pipes.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&userspace_pipes.stdout), "70000\n1\n");
    assert!(userspace_pipes.stderr.is_empty());

    fs::create_dir(temp.path().join("tree")).expect("create find tree");
    fs::write(temp.path().join("tree/needle.txt"), b"needle\n").expect("write find input");
    let composed = run_bumba(
        &[
            "-c",
            "env TOKEN=visible printenv TOKEN; find tree -type f -print | xargs basename",
        ],
        temp.path(),
    );
    assert!(
        composed.status.success(),
        "{}",
        String::from_utf8_lossy(&composed.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&composed.stdout),
        "visible\nneedle.txt\n"
    );

    let make_dir = temp.path().join("make-project");
    fs::create_dir(&make_dir).expect("create Make project");
    fs::write(
        make_dir.join("Makefile"),
        "app: main.o message.o\n\t$(CC) main.o message.o -o app\n\n%.o: %.c\n\t$(CC) -c $< -o $@\n",
    )
    .expect("write Makefile");
    fs::write(
        make_dir.join("main.c"),
        "#include <stdio.h>\nconst char *message(void);\nint main(void) { puts(message()); return 0; }\n",
    )
    .expect("write Make main");
    fs::write(
        make_dir.join("message.c"),
        "const char *message(void) { return \"made by bumba\"; }\n",
    )
    .expect("write Make message");
    let make = run_bumba(&["make", "-j2"], &make_dir);
    assert!(make.status.success(), "{}", String::from_utf8_lossy(&make.stderr));
    let made = Command::new(make_dir.join("app"))
        .output()
        .expect("run Make output");
    assert_eq!(String::from_utf8_lossy(&made.stdout).trim(), "made by bumba");

    let ninja_dir = temp.path().join("ninja-project");
    fs::create_dir(&ninja_dir).expect("create Ninja project");
    fs::write(
        ninja_dir.join("build.ninja"),
        "rule cc\n  command = cc -c $in -o $out\nrule link\n  command = cc $in -o $out\nbuild main.o: cc main.c\nbuild message.o: cc message.c\nbuild app: link main.o message.o\ndefault app\n",
    )
    .expect("write Ninja graph");
    fs::copy(make_dir.join("main.c"), ninja_dir.join("main.c")).expect("copy Ninja main");
    fs::copy(make_dir.join("message.c"), ninja_dir.join("message.c"))
        .expect("copy Ninja message");
    let ninja = run_bumba(&["ninja", "-j2"], &ninja_dir);
    assert!(
        ninja.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&ninja.stdout),
        String::from_utf8_lossy(&ninja.stderr)
    );
    let ninja_app = Command::new(ninja_dir.join("app"))
        .output()
        .expect("run Ninja output");
    assert_eq!(
        String::from_utf8_lossy(&ninja_app.stdout).trim(),
        "made by bumba"
    );
}
