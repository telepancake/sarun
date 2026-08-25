use std::env;
use std::fs::File;
use std::io;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use wikimak_mediawiki::{Bz2Options, new_bz2_reader};

fn run(path: &Path, workers: usize) -> io::Result<()> {
    let compressed_bytes = path.metadata()?.len();
    let source = File::open(path)?;
    let mut decoder = new_bz2_reader(source, Bz2Options { workers });
    let started = Instant::now();
    let decoded_bytes = io::copy(&mut decoder, &mut io::sink())?;
    let elapsed = started.elapsed();
    let seconds = elapsed.as_secs_f64();
    println!(
        "workers={workers} compressed_bytes={compressed_bytes} decoded_bytes={decoded_bytes} \
         elapsed_seconds={seconds:.6} compressed_bytes_per_second={:.3} \
         decoded_bytes_per_second={:.3}",
        compressed_bytes as f64 / seconds,
        decoded_bytes as f64 / seconds,
    );
    Ok(())
}

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: bz2_throughput <source.bz2> <workers>");
        return ExitCode::from(2);
    };
    let Some(workers) = args.next() else {
        eprintln!("usage: bz2_throughput <source.bz2> <workers>");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("usage: bz2_throughput <source.bz2> <workers>");
        return ExitCode::from(2);
    }
    let workers = match workers.to_string_lossy().parse::<usize>() {
        Ok(workers) if workers > 0 => workers,
        _ => {
            eprintln!("workers must be a positive integer");
            return ExitCode::from(2);
        }
    };
    match run(Path::new(&path), workers) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {error}", Path::new(&path).display());
            ExitCode::FAILURE
        }
    }
}
