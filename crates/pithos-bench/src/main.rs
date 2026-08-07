use clap::Parser;
use pithos_bench::{BenchmarkConfig, run_suite};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "pithos-bench", about = "Pithos comparative compression benchmark")]
struct Args {
    /// Corpus directory. Files below this directory are benchmark inputs.
    #[arg(long, default_value = "tst_compact")]
    corpus: PathBuf,

    /// Results directory. Defaults to <corpus>/results.
    #[arg(long)]
    results: Option<PathBuf>,

    /// Skip individual-file cases and run only the combined corpus case.
    #[arg(long)]
    combined_only: bool,

    /// Skip the combined corpus case and run only individual files.
    #[arg(long)]
    individual_only: bool,

    /// Do not invoke 7-Zip, WinRAR or WinZip even when their CLIs are installed.
    #[arg(long)]
    pithos_only: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    if args.combined_only && args.individual_only {
        eprintln!("--combined-only and --individual-only cannot be used together");
        return ExitCode::FAILURE;
    }

    let results = args
        .results
        .unwrap_or_else(|| args.corpus.join("results"));
    let mut config = BenchmarkConfig::standard(args.corpus, results.clone());
    config.include_individual = !args.combined_only;
    config.include_combined = !args.individual_only;
    config.include_external = !args.pithos_only;

    match run_suite(&config) {
        Ok(records) => {
            let successful = records.iter().filter(|record| record.status == "ok").count();
            println!(
                "benchmark complete: {} records ({} ok); results: {}",
                records.len(),
                successful,
                results.display()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("benchmark failed: {error}");
            ExitCode::FAILURE
        }
    }
}
