fn driver_invocation(args: &[String]) -> Option<i32> {
    let executable = std::env::args().next().unwrap_or_default();
    let basename = std::path::Path::new(&executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let run = |name: &str, tail: &[String]| match name {
        "gitdepot" => Some(gitdepot::cli_main(tail)),
        "wikimak" => Some(wikimak_wikipedia::cli_main(tail)),
        "ietfmak" => Some(ietf_mirror::cli_main(tail)),
        _ => None,
    };
    run(basename, args).or_else(|| args.first().and_then(|name| run(name, &args[1..])))
}

fn usage() {
    eprintln!(
        "usage: chupa [gui]\n       chupa list\n       chupa add KIND SOURCE DESTINATION [INTERVAL_SECONDS]\n       chupa run ID | pending | pause ID | resume ID | cancel ID\n       chupa read wiki|ietf PATH [TITLE]\n       chupa gitdepot|wikimak|ietfmak ..."
    );
}

fn id(args: &[String]) -> Result<i64, String> {
    args.first()
        .ok_or_else(|| "missing mirror job id".to_string())?
        .parse()
        .map_err(|_| "mirror job id must be an integer".to_string())
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(code) = driver_invocation(&args) {
        std::process::exit(code);
    }
    let result: Result<(), String> = match args.first().map(String::as_str) {
        None | Some("gui") => {
            chupa::supervisor::scheduler_thread();
            let gateway = std::env::current_exe()
                .map_err(|error| format!("resolve Chupa executable: {error}"))
                .and_then(|path| chupa::gateway::Gateway::start(path.to_string_lossy().into_owned()));
            let gateway = match gateway {
                Ok(gateway) => Some(gateway),
                Err(error) => {
                    eprintln!("chupa: archive gateway unavailable: {error}");
                    None
                }
            };
            let result = chupa::tui::run();
            chupa::supervisor::stop_all();
            if let Some(gateway) = gateway {
                gateway.shutdown();
            }
            result
        }
        Some("list") => chupa::supervisor::jobs_list().map(|jobs| {
            for job in jobs {
                println!("{}\t{}\t{}\t{}\t{}", job.id, job.kind, job.state, job.src, job.dest);
            }
        }),
        Some("add") if args.len() >= 4 => {
            let interval = args.get(4).and_then(|value| value.parse().ok()).unwrap_or(86_400);
            chupa::supervisor::job_add(&args[1], &args[2], &args[3], interval)
                .map(|id| println!("{id}"))
        }
        Some("run") => id(&args[1..]).and_then(chupa::supervisor::job_run),
        Some("pending") => chupa::supervisor::run_pending().map(|ids| println!("{}", ids.len())),
        Some("pause") => id(&args[1..]).and_then(|id| chupa::supervisor::job_set_paused(id, true)),
        Some("resume") => id(&args[1..]).and_then(|id| chupa::supervisor::job_set_paused(id, false)),
        Some("cancel") => id(&args[1..]).and_then(chupa::supervisor::job_cancel),
        Some("read") if args.len() >= 3 => {
            let path = std::path::PathBuf::from(&args[2]);
            let reader = match args[1].as_str() {
                "wiki" => chupa::reader::Reader::open_wiki(path, args.get(3).cloned()),
                "ietf" => chupa::reader::Reader::open_ietf(path, args.get(3).cloned()),
                _ => Err(anyhow::anyhow!("reader kind must be wiki or ietf")),
            };
            reader
                .map_err(|error| error.to_string())
                .and_then(chupa::tui::run_reader)
        }
        _ => {
            usage();
            Err("invalid command".into())
        }
    };
    if let Err(error) = result {
        eprintln!("chupa: {error}");
        std::process::exit(1);
    }
}
