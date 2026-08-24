//! Sarun adapter for Bumba's embedded Ninja implementation.

pub use bumba::ninja::is_ninja_invocation;

pub fn n2_main(argv: &[String]) -> i32 {
    crate::bumba_adapter::install();
    bumba::ninja::n2_main_with_executor(argv, crate::brush::n2_executor)
}

pub fn ninja_builtin(
    argv: &[String],
    base_cwd: &std::path::Path,
    out: impl std::io::Write,
    err: impl std::io::Write,
) -> i32 {
    crate::bumba_adapter::install();
    bumba::ninja::ninja_builtin_with_executor(
        argv,
        base_cwd,
        out,
        err,
        crate::brush::n2_executor,
    )
}
