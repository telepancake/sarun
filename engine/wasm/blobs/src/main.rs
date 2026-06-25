//! Multiplexed coreutils wasm blob (busybox-style dispatch on argv[0]).
//!
//! The engine runs this module in-process under wasmi, passing argv with
//! argv[0] = the util name (its basename) and wiring WASI fd 0/1/2 to the box's
//! pipeline. Each arm replicates uucore's `bin!` finalize: set up the util's
//! localization bundle, call its plain `uumain`, exit with the returned code.
//!
//! A fresh wasm instance per invocation means each util gets its own linear
//! memory and its own `uucore` localization state — the cross-util OnceLock
//! poisoning that the native path works around with thread-per-util + the
//! thread-local uucore patch simply cannot happen here.

use std::ffi::OsString;

/// Run one util: install its Fluent bundle, then call `uumain(args)`.
/// `args` includes argv[0]; uumain reads the program name from it.
fn run(util: &str, args: Vec<OsString>) -> i32 {
    // Mirror uucore::bin!: localization must be set before uumain runs or the
    // util prints raw Fluent keys instead of messages. `get_canonical_util_name`
    // expects the crate name (`uu_<name>`) — it strips the `uu_` prefix — exactly
    // as `bin!` passes `stringify!(uu_<name>)`.
    let crate_name = format!("uu_{util}");
    let canonical = uucore::get_canonical_util_name(&crate_name);
    if let Err(err) = uucore::locale::setup_localization(canonical) {
        eprintln!("coreutils: could not init localization for {util}: {err:?}");
        return 99;
    }
    let code = match util {
        "head" => uu_head::uumain(args.into_iter()),
        "tail" => uu_tail::uumain(args.into_iter()),
        "nl" => uu_nl::uumain(args.into_iter()),
        "cut" => uu_cut::uumain(args.into_iter()),
        "tr" => uu_tr::uumain(args.into_iter()),
        "uniq" => uu_uniq::uumain(args.into_iter()),
        "sort" => uu_sort::uumain(args.into_iter()),
        "basename" => uu_basename::uumain(args.into_iter()),
        "dirname" => uu_dirname::uumain(args.into_iter()),
        "seq" => uu_seq::uumain(args.into_iter()),
        other => {
            eprintln!("coreutils: unknown applet {other:?}");
            127
        }
    };
    use std::io::Write;
    let _ = std::io::stdout().flush();
    code
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let prog = args
        .first()
        .map(|a| {
            std::path::Path::new(a)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| a.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    // `coreutils <applet> [args...]` (argv[0] not a known applet) also works, so
    // the host can run the blob with a generic argv[0] and pass the applet name.
    let code = if is_applet(&prog) {
        run(&prog, args)
    } else if let Some(applet) = args.get(1).map(|s| s.to_string_lossy().into_owned()) {
        if is_applet(&applet) {
            run(&applet, args[1..].to_vec())
        } else {
            eprintln!("coreutils: unknown applet {applet:?}");
            127
        }
    } else {
        eprintln!("coreutils: no applet given (argv[0]={prog:?})");
        127
    };
    std::process::exit(code);
}

fn is_applet(name: &str) -> bool {
    matches!(
        name,
        "head" | "tail" | "nl" | "cut"
            | "tr"
            | "uniq"
            | "sort"
            | "basename"
            | "dirname"
            | "seq"
    )
}
