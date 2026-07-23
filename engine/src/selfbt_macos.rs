//! Darwin stubs for the Linux SUD self-unwind diagnostic.
//!
//! SUD is a Linux syscall-user-dispatch backend. macOS runs boxes through
//! QEMU, whose debugger path is the QEMU gdb stub rather than ptrace/selfbt.

pub fn dump_signal() -> i32 {
    libc::SIGUSR2
}

pub fn sink_path(key: i32) -> String {
    format!("/tmp/sarun-stuck-{key}.bt")
}

pub fn runner_setup(_cmd: &mut std::process::Command, _key: i32) {}

pub fn install() {}
