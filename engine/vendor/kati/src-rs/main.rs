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

// sarun: dropped jemalloc + gperf + the `#![deny(warnings)]` lint that turns
// every future upstream change into a compile error for us.

#![allow(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::ffi::{OsStr, OsString};
use std::io::{Write, stdout};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use anyhow::{Result, bail};
use bytes::{BufMut, Bytes, BytesMut};
use parking_lot::Mutex;

use kati::dep::{NamedDepNode, make_dep};
use kati::fileutil::clear_glob_cache;
use kati::log;
use kati::ninja::generate_ninja;
use kati::regen::needs_regen;
use kati::regen_dump::stamp_dump_main;

use kati::eval::FrameType;
use kati::expr::{Evaluable, Value};
use kati::loc::Loc;
use kati::stmt::Stmt;
use kati::var::{VarOrigin, Variable};

use kati::eval::Evaluator;
use kati::flags::FLAGS;
use kati::symtab::{Symbol, intern, join_symbols};
use kati::timeutil::ScopedTimeReporter;

fn read_bootstrap_makefile(targets: &[Symbol]) -> Result<Arc<Mutex<Vec<Stmt>>>> {
    let mut bootstrap = BytesMut::new();
    bootstrap.put_slice(b"CC?=cc\n");
    if cfg!(target_os = "macos") {
        bootstrap.put_slice(b"CXX?=c++\n");
    } else {
        bootstrap.put_slice(b"CXX?=g++\n");
    }
    bootstrap.put_slice(b"AR?=ar\n");
    // sarun: upstream pins 4.2.1 for Android-build stability; we declared
    // GNU make 4.3 (Ubuntu 22.04 LTS) as our compat target, so report that.
    bootstrap.put_slice(b"MAKE_VERSION?=4.3\n");
    // sarun: MAKELEVEL tracks recursion depth across sub-makes. Real make
    // reads it from the environment (default 0), reports it via $(MAKELEVEL),
    // and bumps it by 1 in every recipe environment so child make
    // invocations see the next level. Mirror that here.
    {
        let level = std::env::var("MAKELEVEL")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        bootstrap.put_slice(format!("MAKELEVEL:={level}\n").as_bytes());
    }
    bootstrap.put_slice(b"KATI?=ckati\n");
    bootstrap.put_slice(b"SHELL=/bin/sh\n");
    // sarun: GNU make 4.x advertises its optional features via .FEATURES;
    // makefiles gate on it (the Linux kernel bails out without `undefine`).
    // Same list the engine's embedded-make bootstrap advertises (katirun.rs)
    // — kati implements (or accepts as syntax) each token. GNU's value starts
    // with the same tokens in the same order, so $(filter …,$(.FEATURES))
    // output is corpus-comparable.
    bootstrap.put_slice(
        b".FEATURES?=target-specific order-only second-expansion else-if \
          shortest-stem undefine oneshell\n",
    );

    if !FLAGS.no_builtin_rules {
        bootstrap.put_slice(b".c.o:\n");
        bootstrap.put_slice(b"\t$(CC) $(CFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c -o $@ $<\n");
        bootstrap.put_slice(b".cc.o:\n");
        bootstrap.put_slice(b"\t$(CXX) $(CXXFLAGS) $(CPPFLAGS) $(TARGET_ARCH) -c -o $@ $<\n");
    }
    if FLAGS.generate_ninja {
        bootstrap.put_slice(format!("MAKE?=make -j{}\n", FLAGS.num_jobs.max(1)).as_bytes());
    } else {
        bootstrap.put_slice(b"MAKE?=");
        bootstrap.put_slice(FLAGS.subkati_args.join(OsStr::new(" ")).as_bytes());
        bootstrap.put_u8(b'\n');
    }
    bootstrap.put_slice(b"MAKECMDGOALS?=");
    bootstrap.put(join_symbols(targets, b" "));
    bootstrap.put_u8(b'\n');

    bootstrap.put_slice(b"CURDIR:=");
    bootstrap.put_slice(std::env::current_dir()?.as_os_str().as_bytes());
    bootstrap.put_u8(b'\n');

    kati::parser::parse_buf(
        &bootstrap.freeze(),
        Loc {
            filename: intern("*bootstrap*"),
            line: 0,
        },
    )
}

fn run(targets: &[Symbol], cl_vars: &Vec<Bytes>, orig_args: OsString) -> Result<i32> {
    let start_time = std::time::SystemTime::now();

    if FLAGS.generate_ninja && (FLAGS.regen || FLAGS.dump_kati_stamp) {
        let _tr = ScopedTimeReporter::new("regen_check_time");
        if !needs_regen(start_time, &orig_args) {
            eprintln!("No need to regenerate ninja file");
            return Ok(0);
        }
        if FLAGS.dump_kati_stamp {
            println!("Need to regenerate ninja file");
            return Ok(0);
        }
        clear_glob_cache();
    }

    let mut ev = Evaluator::new();
    ev.start()?;
    // sarun: GNU make's MAKEFILE_LIST has no leading space — the main
    // makefile is the very first word. Kati used to seed it with " name",
    // which leaked an extra space into any recipe that referenced
    // $(MAKEFILE_LIST) (e.g. `echo  Makefile` instead of `echo Makefile`).
    let mut makefile_list = BytesMut::new();
    makefile_list.put_slice(FLAGS.makefile.lock().clone().unwrap().as_bytes());
    ev.set_global_var(
        intern("MAKEFILE_LIST"),
        Variable::with_simple_string(
            makefile_list.freeze(),
            VarOrigin::File,
            Some(ev.current_frame()),
            ev.loc.clone(),
        ),
        false,
        None,
    )?;
    for (k, v) in std::env::vars_os() {
        let v = Bytes::from(v.as_bytes().to_vec());
        let val = Arc::new(Value::Literal(None, v.clone()));
        ev.set_global_var(
            intern(k.as_bytes().to_vec()),
            Variable::new_recursive(
                val,
                VarOrigin::Environment,
                Some(ev.current_frame()),
                None,
                v,
            ),
            false,
            None,
        )?;
    }

    let bootstrap_asts = read_bootstrap_makefile(targets)?;

    {
        let _frame = ev.enter(
            FrameType::Phase,
            Bytes::from_static(b"*bootstrap*"),
            Loc::default(),
        );
        ev.in_bootstrap();
        for stmt in bootstrap_asts.lock().iter() {
            log!("{stmt:?}");
            stmt.eval(&mut ev)?;
        }
    }

    {
        let _frame = ev.enter(
            FrameType::Phase,
            Bytes::from_static(b"*command line*"),
            Loc::default(),
        );
        ev.in_command_line();
        for l in cl_vars {
            let asts = kati::parser::parse_buf(
                l,
                Loc {
                    filename: intern("*bootstrap*"),
                    line: 0,
                },
            )?;
            let asts = asts.lock();
            assert!(asts.len() == 1);
            asts[0].eval(&mut ev)?;
        }
    }
    ev.in_toplevel_makefile();

    {
        let _eval_frame = ev.enter(
            FrameType::Phase,
            Bytes::from_static(b"*parse*"),
            Loc::default(),
        );
        let _tr = ScopedTimeReporter::new("eval time");

        let makefile = FLAGS.makefile.lock().clone().unwrap();
        let _file_frame = ev.enter(
            FrameType::Parse,
            Bytes::from(makefile.as_bytes().to_vec()),
            Loc::default(),
        );
        let Some(mk) = kati::file_cache::get_makefile(&makefile, &ev.working_dir)? else {
            bail!("makefile not found")
        };
        let stmts = mk.stmts.lock();
        for stmt in stmts.iter() {
            log!("{stmt:?}");
            stmt.eval(&mut ev)?;
        }
    }

    if let Some(filename) = &FLAGS.dump_include_graph {
        ev.dump_include_json(filename)?;
    }

    // sarun: GNU make's "remake the makefile" loop, step 1 of 2.
    // Before we do dep analysis on the user's targets, check whether
    // any required `include` was deferred during parse and is producible
    // by a rule we have. If so, fold those names into the targets list
    // for the single make_dep call below, exec the resulting nodes
    // (which build the missing includes), and then re-exec ourselves
    // with the same args — the second invocation parses with the
    // freshly-generated content visible. Guard against infinite
    // re-exec via SARUN_KATI_REMAKE_DEPTH (capped at 5).
    let mut remake_targets: Vec<Symbol> = Vec::new();
    {
        let pending = std::mem::take(&mut ev.pending_remake_includes);
        for (loc, name, required) in &pending {
            let sym = intern(name.as_bytes().to_vec());
            // Literal or PATTERN-rule producible (e.g. the kernel's
            // `%/auto.conf %/auto.conf.cmd: $(KCONFIG_CONFIG)`).
            let producible = ev.rules.iter().any(|r| {
                r.outputs.contains(&sym)
                    || r.output_patterns.iter().any(|p| {
                        kati::strutil::Pattern::new(bytes::Bytes::from(p.as_bytes().to_vec()))
                            .matches(name.as_bytes())
                    })
            });
            if producible {
                remake_targets.push(sym);
            } else if *required {
                // Missing required include with no matching rule. Emit a
                // single line matching upstream kati's `error_loc!` shape;
                // the corpus normalizer rolls real make's follow-up
                // "*** No rule to make target X" line into the same.
                let pat_str = String::from_utf8_lossy(name.as_bytes());
                eprintln!("{loc}: {pat_str}: No such file or directory");
                std::process::exit(2);
            }
            // A missing OPTIONAL include with no rule: GNU tolerates it.
        }
    }
    let remake_active = !remake_targets.is_empty();

    let nodes: Vec<NamedDepNode>;
    {
        let _frame = ev.enter(
            FrameType::Phase,
            Bytes::from_static(b"*dependency analysis*"),
            Loc::default(),
        );
        let _tr = ScopedTimeReporter::new("make dep time");
        // When remaking, we only want to build the remake targets in
        // this invocation; the user's real targets get built in the
        // re-exec'd process.
        let dep_targets = if remake_active {
            remake_targets.clone()
        } else {
            targets.to_owned()
        };
        nodes = make_dep(&mut ev, dep_targets)?;
    }

    if FLAGS.is_syntax_check_only {
        return Ok(0);
    }

    if remake_active {
        let depth: u32 = std::env::var("SARUN_KATI_REMAKE_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if depth >= 5 {
            eprintln!(
                "*** kati: remake-the-makefile loop exceeded 5 iterations (still missing: {:?})",
                remake_targets
            );
            std::process::exit(2);
        }
        // Build the remake targets.
        {
            let _frame = ev.enter(
                FrameType::Phase,
                Bytes::from_static(b"*remake*"),
                Loc::default(),
            );
            kati::exec::exec(nodes, &mut ev)?;
        }
        // Re-exec ourselves with the same args. GLOB_CACHE et al. reset
        // naturally because it's a fresh process. Use current_exe() for
        // the actual binary path (argv[0] might be "make" via FUSE shadow
        // / Command::arg0, which would PATH-resolve to a different
        // binary), and preserve the original argv[0] for the new
        // process.
        let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
        let argv0 = argv.first().cloned().unwrap_or_default();
        let exe = std::env::current_exe().unwrap_or_else(|_| argv0.clone().into());
        let mut cmd = std::process::Command::new(&exe);
        std::os::unix::process::CommandExt::arg0(&mut cmd, &argv0);
        cmd.args(argv.iter().skip(1));
        cmd.env("SARUN_KATI_REMAKE_DEPTH", (depth + 1).to_string());
        let err = std::os::unix::process::CommandExt::exec(&mut cmd);
        eprintln!("*** kati: failed to re-exec for remake: {err}");
        std::process::exit(2);
    }

    if FLAGS.generate_ninja {
        let _frame = ev.enter(
            FrameType::Phase,
            Bytes::from_static(b"*ninja generation*"),
            Loc::default(),
        );
        let _tr = ScopedTimeReporter::new("generate ninja time");
        generate_ninja(&nodes, &mut ev, orig_args.as_bytes(), start_time)?;
        ev.finish()?;
        return Ok(0);
    }

    // sarun: bump MAKELEVEL by 1 in the recipe environment so any sub-make
    // launched from a recipe sees the next level. Matches real make.
    {
        let level = std::env::var("MAKELEVEL")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        // SAFETY: single-threaded at this point in the pipeline.
        unsafe {
            std::env::set_var("MAKELEVEL", (level + 1).to_string());
        }
    }

    // sarun: `.EXPORT_ALL_VARIABLES:` makes every make-defined variable
    // visible to recipe environments. Stage them all here, before the
    // explicit-exports loop (which can still override with unexport).
    if ev.export_all_vars {
        let all = ev.get_symbol_names(|v| {
            !matches!(
                v.read().origin(),
                kati::var::VarOrigin::Default | kati::var::VarOrigin::Automatic
            )
        });
        for (sym, _name) in all {
            if !ev.exports.contains_key(&sym) {
                ev.exports.insert(sym, true);
            }
        }
    }

    // sarun: GNU make ALWAYS passes MAKEFLAGS to children through the
    // environment, including makefile-level appends (`MAKEFLAGS += --foo`) —
    // the Linux kernel's `MAKEFLAGS += --include-dir=$(abs_srctree)` only
    // works in the re-invoked sub-make via env. Export the FINAL evaluated
    // value; skip when empty so a plain run doesn't grow an empty env var.
    {
        if let Some(v) = ev.lookup_var(intern("MAKEFLAGS"))? {
            let value = v.read().eval_to_buf(&mut ev)?;
            if !value.is_empty() {
                // SAFETY: single-threaded at this point in the pipeline.
                unsafe {
                    std::env::set_var("MAKEFLAGS", OsStr::from_bytes(&value));
                }
            }
        }
    }

    for (name, export) in ev.exports.clone() {
        if export {
            let value = if let Some(v) = ev.lookup_var(name)? {
                v.read().eval_to_buf(&mut ev)?
            } else {
                Bytes::new()
            };
            log!("setenv({name}, {})", String::from_utf8_lossy(&value));
            // SAFETY: we're single threaded here. If that changes, we could pass the
            // expected environment to the children explicitly.
            unsafe {
                std::env::set_var(
                    OsStr::from_bytes(&name.as_bytes()),
                    OsStr::from_bytes(&value),
                );
            }
        } else {
            log!("unsetenv({name})");
            // SAFETY: we're single threaded here. If that changes, we could pass the
            // expected environment to the children explicitly.
            unsafe {
                std::env::remove_var(OsStr::from_bytes(&name.as_bytes()));
            }
        }
    }

    {
        let _frame = ev.enter(
            FrameType::Phase,
            Bytes::from_static(b"*execution*"),
            Loc::default(),
        );
        let _tr = ScopedTimeReporter::new("exec time");
        kati::exec::exec(nodes, &mut ev)?;
    }

    ev.finish()?;

    Ok(0)
}

fn find_first_makefile() {
    let mut makefile = FLAGS.makefile.lock();
    if makefile.is_some() {
        return;
    }
    if std::fs::exists("GNUMakefile").unwrap_or(false) {
        *makefile = Some(OsString::from("GNUMakefile"));
    } else if !cfg!(target_os = "macos") && std::fs::exists("makefile").unwrap_or(false) {
        *makefile = Some(OsString::from("makefile"));
    } else if std::fs::exists("Makefile").unwrap_or(false) {
        *makefile = Some(OsString::from("Makefile"));
    }
}

fn handle_realpath(args: Vec<String>) {
    for arg in args {
        if let Ok(path) = std::fs::canonicalize(&arg) {
            let _ = stdout().write_all(path.as_os_str().as_bytes());
            println!();
        }
    }
}

fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn)
        .format(|buf, record| {
            if let (Some(file), Some(line)) = (record.file(), record.line()) {
                writeln!(buf, "*kati*: {file}:{line}: {}", record.args())
            } else {
                writeln!(buf, "*kati*: {}", record.args())
            }
        })
        .parse_env("KATI_LOG")
        .init();

    if std::env::args().len() >= 2 {
        let arg = std::env::args().nth(1).unwrap();
        if arg == "--realpath" {
            handle_realpath(std::env::args().skip(2).collect());
            return;
        } else if arg == "--dump_stamp_tool" {
            if let Err(err) = stamp_dump_main() {
                eprintln!("{err}");
                std::process::exit(1);
            }
            return;
        }
    }

    if let Some(working_dir) = &FLAGS.working_dir
        && let Err(e) = std::env::set_current_dir(working_dir)
    {
        eprintln!("*** {}: {}", working_dir.to_string_lossy(), e);
        std::process::exit(1);
    }
    let orig_args = std::env::args_os()
        .collect::<Vec<OsString>>()
        .join(OsStr::new(" "));
    find_first_makefile();
    if FLAGS.makefile.lock().is_none() {
        eprintln!("*** No targets specified and no makefile found.");
        std::process::exit(1);
    }
    let ret = match run(&FLAGS.targets, &FLAGS.cl_vars, orig_args) {
        Ok(ret) => ret,
        Err(err) => {
            // sarun: a recipe failure already printed `*** [target] Error N`
            // (exec.rs now propagates BuildFailed instead of std::process::exit
            // so an in-process builtin survives); preserve its exit code here
            // for the standalone rkati without re-printing.
            if let Some(bf) = err.downcast_ref::<kati::exec::BuildFailed>() {
                bf.0
            } else {
                for cause in err.chain() {
                    eprintln!("{cause}");
                }
                1
            }
        }
    };
    kati::stats::report_all_stats();
    std::process::exit(ret);
}
