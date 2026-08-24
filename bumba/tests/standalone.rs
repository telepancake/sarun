use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

fn run_bumba(args: &[&str], cwd: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bumba"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run Bumba")
}

fn executable(path: &std::path::Path, script: &str) {
    fs::write(path, script).expect("write executable fixture");
    let mut permissions = fs::metadata(path).expect("fixture metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make fixture executable");
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
    assert!(output.stderr.is_empty(), "redirected builtin error escaped logical stderr");
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
