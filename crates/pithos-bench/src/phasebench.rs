use clap::Parser;
use pithos_analysis::{
    ChunkFingerprint, ChunkOrigin, ChunkingConfig, DedupInput, assign_chunk_ids, chunk_fastcdc,
    exact_dedup,
};
use pithos_telemetry::{Operation, Stage, TelemetryCollector, write_jsonl};
use serde::Serialize;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

#[derive(Debug, Parser)]
#[command(
    name = "pithos-phasebench",
    about = "Measure current Pithos Phase 3 chunking/fingerprint/exact-dedup costs"
)]
struct Args {
    #[arg(long, default_value = "tst_compact")]
    corpus: PathBuf,

    #[arg(long)]
    results: Option<PathBuf>,

    /// Safety cap for data loaded into memory for the combined analysis probe.
    #[arg(long, default_value_t = 2048)]
    max_total_mib: u64,
}

#[derive(Debug, Serialize)]
struct PhaseSummary {
    corpus: String,
    file_count: usize,
    original_bytes: u64,
    chunk_count: usize,
    canonical_chunks: u64,
    referenced_chunks: u64,
    gross_duplicate_bytes: u64,
    reference_bytes: u64,
    net_saved_bytes: u64,
    dedup_output_estimate_bytes: u64,
    dedup_savings_percent: f64,
    scan_ms: u128,
    chunking_ms: u128,
    fingerprint_ms: u128,
    exact_dedup_ms: u128,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("phase benchmark failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let results = args.results.unwrap_or_else(|| args.corpus.join("results"));
    fs::create_dir_all(&results)?;
    let files = collect_files(&args.corpus, &results)?;
    if files.is_empty() {
        return Err(io::Error::other("benchmark corpus is empty").into());
    }

    let declared_bytes = files.iter().try_fold(0_u64, |total, path| {
        Ok::<u64, std::io::Error>(total.saturating_add(fs::metadata(path)?.len()))
    })?;
    let limit_bytes = args.max_total_mib.saturating_mul(1024 * 1024);
    if declared_bytes > limit_bytes {
        return Err(io::Error::other(format!(
            "corpus is {} MiB, above --max-total-mib {}",
            declared_bytes / (1024 * 1024),
            args.max_total_mib
        ))
        .into());
    }

    let collector = TelemetryCollector::new(
        "phase3-combined",
        Operation::Benchmark,
        Some("phase3-analysis".to_owned()),
        files
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        None,
    );

    let scan_started = Instant::now();
    let mut data = Vec::with_capacity(files.len());
    let mut original_bytes = 0_u64;
    for path in &files {
        let bytes = fs::read(path)?;
        original_bytes = original_bytes.saturating_add(bytes.len() as u64);
        data.push(bytes);
    }
    let scan_elapsed = scan_started.elapsed();
    collector.record(
        Stage::Scan,
        scan_elapsed,
        Some(original_bytes),
        Some(original_bytes),
        Some(files.len() as u64),
        Some("read corpus bytes for deterministic analysis probe".to_owned()),
    );

    let config = ChunkingConfig::default();
    let chunking_started = Instant::now();
    let mut drafts = Vec::new();
    for (entry_id, bytes) in data.iter().enumerate() {
        let mut file_drafts = chunk_fastcdc(
            bytes,
            ChunkOrigin {
                entry_id: entry_id as u64,
                object_id: 0,
                base_offset: 0,
            },
            &config,
        )?;
        drafts.append(&mut file_drafts);
    }
    let chunks = assign_chunk_ids(drafts, config.max_chunks)?;
    let chunking_elapsed = chunking_started.elapsed();
    collector.record(
        Stage::Chunking,
        chunking_elapsed,
        Some(original_bytes),
        None,
        Some(chunks.len() as u64),
        Some(format!("FastCDC {} logical chunks", chunks.len())),
    );

    let fingerprint_started = Instant::now();
    let mut fingerprints = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let bytes = chunk_bytes(
            chunk.entry_id as usize,
            chunk.logical_offset,
            chunk.length,
            &data,
        )?;
        fingerprints.push(ChunkFingerprint::compute(chunk.chunk_id, bytes)?);
    }
    let fingerprint_elapsed = fingerprint_started.elapsed();
    collector.record(
        Stage::Fingerprinting,
        fingerprint_elapsed,
        Some(original_bytes),
        None,
        Some(fingerprints.len() as u64),
        Some("XXH3/BLAKE3/CRC32C/superfeatures".to_owned()),
    );

    let dedup_started = Instant::now();
    let mut dedup_inputs = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.iter().enumerate() {
        let bytes = chunk_bytes(
            chunk.entry_id as usize,
            chunk.logical_offset,
            chunk.length,
            &data,
        )?;
        dedup_inputs.push(DedupInput {
            chunk,
            fingerprint: &fingerprints[index],
            data: bytes,
        });
    }
    let plan = exact_dedup(&dedup_inputs)?;
    let dedup_elapsed = dedup_started.elapsed();
    let dedup_output = original_bytes.saturating_sub(plan.net_saved_bytes);
    let savings_percent = if original_bytes == 0 {
        0.0
    } else {
        plan.net_saved_bytes as f64 / original_bytes as f64 * 100.0
    };
    collector.record(
        Stage::ExactDedup,
        dedup_elapsed,
        Some(original_bytes),
        Some(dedup_output),
        Some(plan.referenced_chunks),
        Some(format!(
            "canonical={} referenced={} gross_duplicate_bytes={} reference_bytes={} net_saved_bytes={}",
            plan.canonical_chunks,
            plan.referenced_chunks,
            plan.gross_duplicate_bytes,
            plan.reference_bytes,
            plan.net_saved_bytes
        )),
    );

    let summary = PhaseSummary {
        corpus: args.corpus.to_string_lossy().into_owned(),
        file_count: files.len(),
        original_bytes,
        chunk_count: chunks.len(),
        canonical_chunks: plan.canonical_chunks,
        referenced_chunks: plan.referenced_chunks,
        gross_duplicate_bytes: plan.gross_duplicate_bytes,
        reference_bytes: plan.reference_bytes,
        net_saved_bytes: plan.net_saved_bytes,
        dedup_output_estimate_bytes: dedup_output,
        dedup_savings_percent: savings_percent,
        scan_ms: scan_elapsed.as_millis(),
        chunking_ms: chunking_elapsed.as_millis(),
        fingerprint_ms: fingerprint_elapsed.as_millis(),
        exact_dedup_ms: dedup_elapsed.as_millis(),
    };

    let run = collector.finish(Some(original_bytes), Some(dedup_output));
    let mut jsonl = BufWriter::new(File::create(results.join("phase-analysis.jsonl"))?);
    write_jsonl(&mut jsonl, &run)?;
    jsonl.flush()?;
    fs::write(
        results.join("phase-analysis-summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;

    println!("Phase 3 analysis benchmark");
    println!("files: {}", summary.file_count);
    println!("input: {} bytes", summary.original_bytes);
    println!("chunks: {}", summary.chunk_count);
    println!("dedup references: {}", summary.referenced_chunks);
    println!(
        "dedup potential: {} bytes ({:.3}%)",
        summary.net_saved_bytes, summary.dedup_savings_percent
    );
    println!(
        "time ms: scan={} chunking={} fingerprint={} exact_dedup={}",
        summary.scan_ms, summary.chunking_ms, summary.fingerprint_ms, summary.exact_dedup_ms
    );
    println!("results: {}", results.display());
    Ok(())
}

fn chunk_bytes(
    entry_id: usize,
    logical_offset: u64,
    length: u32,
    data: &[Vec<u8>],
) -> Result<&[u8], Box<dyn std::error::Error>> {
    let entry = data
        .get(entry_id)
        .ok_or_else(|| io::Error::other("chunk entry_id outside corpus"))?;
    let start = usize::try_from(logical_offset)?;
    let end = start
        .checked_add(length as usize)
        .ok_or_else(|| io::Error::other("chunk range overflow"))?;
    entry
        .get(start..end)
        .ok_or_else(|| io::Error::other("chunk range outside input").into())
}

fn collect_files(root: &Path, excluded: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let root = fs::canonicalize(root)?;
    let excluded = fs::canonicalize(excluded).unwrap_or_else(|_| excluded.to_path_buf());
    let mut files = Vec::new();
    collect_recursive(&root, &excluded, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_recursive(
    current: &Path,
    excluded: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), std::io::Error> {
    if current.starts_with(excluded) {
        return Ok(());
    }
    let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_recursive(&path, excluded, files)?;
        } else if metadata.is_file() {
            files.push(path);
        }
    }
    Ok(())
}
