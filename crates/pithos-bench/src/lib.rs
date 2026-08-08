//! Comparative benchmark harness for Pithos and optional external compressors.

use pithos_core::CompressionProfile;
use pithos_engine::{PackRequest, UnpackRequest, pack, unpack, verify};
use pithos_telemetry::{Operation, Stage, TelemetryCollector, write_jsonl};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BenchError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Pithos(#[from] pithos_core::PithosError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("benchmark corpus is empty")]
    EmptyCorpus,
}

#[derive(Debug, Clone)]
pub struct BenchmarkConfig {
    pub corpus_dir: PathBuf,
    pub results_dir: PathBuf,
    pub profiles: Vec<CompressionProfile>,
    pub include_individual: bool,
    pub include_combined: bool,
    pub include_external: bool,
}

impl BenchmarkConfig {
    pub fn standard(corpus_dir: PathBuf, results_dir: PathBuf) -> Self {
        Self {
            corpus_dir,
            results_dir,
            profiles: vec![CompressionProfile::Balanced, CompressionProfile::ArchiveMax],
            include_individual: true,
            include_combined: true,
            include_external: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkRecord {
    pub case: String,
    pub compressor: String,
    pub profile: String,
    pub input_count: usize,
    pub original_bytes: u64,
    pub archive_bytes: Option<u64>,
    pub compression_ratio: Option<f64>,
    pub savings_percent: Option<f64>,
    pub compress_ms: Option<u128>,
    pub verify_ms: Option<u128>,
    pub decompress_ms: Option<u128>,
    pub status: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
struct BenchmarkCase {
    name: String,
    inputs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum ExternalKind {
    SevenZip,
    WinRar,
    WinZip,
}

pub fn run_suite(config: &BenchmarkConfig) -> Result<Vec<BenchmarkRecord>, BenchError> {
    fs::create_dir_all(&config.results_dir)?;
    let files = collect_files(&config.corpus_dir, &config.results_dir)?;
    if files.is_empty() {
        return Err(BenchError::EmptyCorpus);
    }

    let mut cases = Vec::new();
    if config.include_individual {
        for (index, path) in files.iter().enumerate() {
            let display = path
                .strip_prefix(&config.corpus_dir)
                .unwrap_or(path)
                .to_string_lossy();
            cases.push(BenchmarkCase {
                name: format!("single-{index:04}-{}", sanitize_name(&display)),
                inputs: vec![path.clone()],
            });
        }
    }
    if config.include_combined {
        cases.push(BenchmarkCase {
            name: "combined-all".to_owned(),
            inputs: files.clone(),
        });
    }

    let jsonl_path = config.results_dir.join("benchmark.jsonl");
    let csv_path = config.results_dir.join("benchmark.csv");
    let telemetry_path = config.results_dir.join("pithos-telemetry.jsonl");
    let mut jsonl = BufWriter::new(File::create(jsonl_path)?);
    let mut telemetry = BufWriter::new(File::create(telemetry_path)?);
    let mut records = Vec::new();

    for case in &cases {
        for profile in &config.profiles {
            let (record, run_telemetry) = run_pithos_case(case, *profile, &config.results_dir)?;
            write_jsonl(&mut jsonl, &record)?;
            write_jsonl(&mut telemetry, &run_telemetry)?;
            records.push(record);
        }

        if config.include_external {
            for kind in [
                ExternalKind::SevenZip,
                ExternalKind::WinRar,
                ExternalKind::WinZip,
            ] {
                if external_available(kind) {
                    let record = run_external_case(case, kind, &config.results_dir)?;
                    write_jsonl(&mut jsonl, &record)?;
                    records.push(record);
                }
            }
        }
    }

    jsonl.flush()?;
    telemetry.flush()?;
    write_csv(&csv_path, &records)?;
    Ok(records)
}

fn run_pithos_case(
    case: &BenchmarkCase,
    profile: CompressionProfile,
    results_dir: &Path,
) -> Result<(BenchmarkRecord, pithos_telemetry::RunTelemetry), BenchError> {
    let profile_name = profile_name(profile).to_owned();
    let case_dir = results_dir
        .join("work")
        .join(&case.name)
        .join(format!("pithos-{profile_name}"));
    reset_dir(&case_dir)?;
    let archive = case_dir.join(default_case_archive_name(&case.inputs));
    let unpack_dir = case_dir.join("unpacked");
    let original_bytes = total_file_bytes(&case.inputs)?;
    let collector = TelemetryCollector::new(
        format!("{}-pithos-{profile_name}", case.name),
        Operation::Benchmark,
        Some(profile_name.clone()),
        case.inputs
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        Some(archive.to_string_lossy().into_owned()),
    );

    if representation_trace_enabled() {
        eprintln!(
            "PITHOS_REP_TRACE\tstage=benchmark_case\tcase={}\tprofile={}\tinput_bytes={}\tinputs={}",
            case.name,
            profile_name,
            original_bytes,
            case.inputs.len()
        );
    }

    let pack_started = Instant::now();
    let pack_result = pack(PackRequest {
        inputs: case.inputs.clone(),
        output: archive.clone(),
        profile,
    });
    let pack_elapsed = pack_started.elapsed();

    if let Err(error) = pack_result {
        collector.record(
            Stage::PackTotal,
            pack_elapsed,
            Some(original_bytes),
            None,
            Some(case.inputs.len() as u64),
            Some(error.to_string()),
        );
        let telemetry = collector.finish(Some(original_bytes), None);
        return Ok((
            BenchmarkRecord {
                case: case.name.clone(),
                compressor: "pithos".to_owned(),
                profile: profile_name,
                input_count: case.inputs.len(),
                original_bytes,
                archive_bytes: None,
                compression_ratio: None,
                savings_percent: None,
                compress_ms: Some(pack_elapsed.as_millis()),
                verify_ms: None,
                decompress_ms: None,
                status: "failed".to_owned(),
                detail: Some(error.to_string()),
            },
            telemetry,
        ));
    }

    let archive_bytes = fs::metadata(&archive)?.len();
    collector.record(
        Stage::PackTotal,
        pack_elapsed,
        Some(original_bytes),
        Some(archive_bytes),
        Some(case.inputs.len() as u64),
        None,
    );

    let verify_started = Instant::now();
    let verify_result = verify(&archive);
    let verify_elapsed = verify_started.elapsed();
    collector.record(
        Stage::Verify,
        verify_elapsed,
        Some(archive_bytes),
        Some(archive_bytes),
        None,
        verify_result.as_ref().err().map(ToString::to_string),
    );

    let unpack_started = Instant::now();
    let unpack_result = if verify_result.is_ok() {
        unpack(UnpackRequest {
            archive: archive.clone(),
            output_dir: unpack_dir,
        })
    } else {
        Err(pithos_core::PithosError::InvalidMetadata(
            "benchmark verify failed",
        ))
    };
    let unpack_elapsed = unpack_started.elapsed();
    collector.record(
        Stage::UnpackTotal,
        unpack_elapsed,
        Some(archive_bytes),
        Some(original_bytes),
        Some(case.inputs.len() as u64),
        unpack_result.as_ref().err().map(ToString::to_string),
    );

    let detail = verify_result
        .err()
        .or_else(|| unpack_result.err())
        .map(|error| error.to_string());
    let status = if detail.is_none() { "ok" } else { "failed" };
    let ratio = ratio(original_bytes, archive_bytes);
    let telemetry = collector.finish(Some(original_bytes), Some(archive_bytes));
    Ok((
        BenchmarkRecord {
            case: case.name.clone(),
            compressor: "pithos".to_owned(),
            profile: profile_name,
            input_count: case.inputs.len(),
            original_bytes,
            archive_bytes: Some(archive_bytes),
            compression_ratio: Some(ratio),
            savings_percent: Some((1.0 - ratio) * 100.0),
            compress_ms: Some(pack_elapsed.as_millis()),
            verify_ms: Some(verify_elapsed.as_millis()),
            decompress_ms: Some(unpack_elapsed.as_millis()),
            status: status.to_owned(),
            detail,
        },
        telemetry,
    ))
}

fn run_external_case(
    case: &BenchmarkCase,
    kind: ExternalKind,
    results_dir: &Path,
) -> Result<BenchmarkRecord, BenchError> {
    let (label, extension) = match kind {
        ExternalKind::SevenZip => ("7zip", "7z"),
        ExternalKind::WinRar => ("winrar", "rar"),
        ExternalKind::WinZip => ("winzip", "zip"),
    };
    let case_dir = results_dir.join("work").join(&case.name).join(label);
    reset_dir(&case_dir)?;
    let archive = case_dir.join(format!("files.{extension}"));
    let unpack_dir = case_dir.join("unpacked");
    fs::create_dir_all(&unpack_dir)?;
    let original_bytes = total_file_bytes(&case.inputs)?;

    let compress_started = Instant::now();
    let compress_status = run_external_compress(kind, &archive, &case.inputs);
    let compress_elapsed = compress_started.elapsed();
    if let Err(detail) = compress_status {
        return Ok(BenchmarkRecord {
            case: case.name.clone(),
            compressor: label.to_owned(),
            profile: external_profile_name(kind).to_owned(),
            input_count: case.inputs.len(),
            original_bytes,
            archive_bytes: None,
            compression_ratio: None,
            savings_percent: None,
            compress_ms: Some(compress_elapsed.as_millis()),
            verify_ms: None,
            decompress_ms: None,
            status: "failed".to_owned(),
            detail: Some(detail),
        });
    }

    let archive_bytes = fs::metadata(&archive)?.len();
    let decompress_started = Instant::now();
    let decompress_status = run_external_decompress(kind, &archive, &unpack_dir);
    let decompress_elapsed = decompress_started.elapsed();
    let detail = decompress_status.err();
    let ratio = ratio(original_bytes, archive_bytes);
    Ok(BenchmarkRecord {
        case: case.name.clone(),
        compressor: label.to_owned(),
        profile: external_profile_name(kind).to_owned(),
        input_count: case.inputs.len(),
        original_bytes,
        archive_bytes: Some(archive_bytes),
        compression_ratio: Some(ratio),
        savings_percent: Some((1.0 - ratio) * 100.0),
        compress_ms: Some(compress_elapsed.as_millis()),
        verify_ms: None,
        decompress_ms: Some(decompress_elapsed.as_millis()),
        status: if detail.is_none() { "ok" } else { "failed" }.to_owned(),
        detail,
    })
}

fn run_external_compress(
    kind: ExternalKind,
    archive: &Path,
    inputs: &[PathBuf],
) -> Result<(), String> {
    let mut command = match kind {
        ExternalKind::SevenZip => {
            let mut cmd = Command::new(seven_zip_command());
            cmd.arg("a")
                .arg("-t7z")
                .arg("-mx=9")
                .arg("-m0=lzma2")
                .arg(archive);
            cmd
        }
        ExternalKind::WinRar => {
            let mut cmd = Command::new("WinRAR");
            cmd.arg("a")
                .arg("-ma5")
                .arg("-m5")
                .arg("-s")
                .arg("-ep1")
                .arg(archive);
            cmd
        }
        ExternalKind::WinZip => {
            let mut cmd = Command::new("wzzip");
            cmd.arg("-ex").arg(archive);
            cmd
        }
    };
    command.args(inputs);
    run_status(command)
}

fn run_external_decompress(
    kind: ExternalKind,
    archive: &Path,
    output: &Path,
) -> Result<(), String> {
    let command = match kind {
        ExternalKind::SevenZip => {
            let mut cmd = Command::new(seven_zip_command());
            cmd.arg("x")
                .arg("-y")
                .arg(format!("-o{}", output.display()))
                .arg(archive);
            cmd
        }
        ExternalKind::WinRar => {
            let mut cmd = Command::new("WinRAR");
            cmd.arg("x").arg("-y").arg(archive).arg(output);
            cmd
        }
        ExternalKind::WinZip => {
            let mut cmd = Command::new("wzunzip");
            cmd.arg(archive).arg(output);
            cmd
        }
    };
    run_status(command)
}

fn run_status(mut command: Command) -> Result<(), String> {
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exit status {}", output.status)
    };
    Err(detail)
}

fn external_available(kind: ExternalKind) -> bool {
    match kind {
        ExternalKind::SevenZip => command_exists("7z") || command_exists("7zz"),
        ExternalKind::WinRar => command_exists("WinRAR"),
        ExternalKind::WinZip => command_exists("wzzip") && command_exists("wzunzip"),
    }
}

fn seven_zip_command() -> &'static str {
    if command_exists("7z") { "7z" } else { "7zz" }
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn collect_files(root: &Path, excluded_root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let root = fs::canonicalize(root)?;
    let excluded = fs::canonicalize(excluded_root).unwrap_or_else(|_| excluded_root.to_path_buf());
    let mut files = Vec::new();
    collect_files_recursive(&root, &excluded, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_files_recursive(
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
            collect_files_recursive(&path, excluded, files)?;
        } else if metadata.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn total_file_bytes(inputs: &[PathBuf]) -> Result<u64, std::io::Error> {
    inputs.iter().try_fold(0_u64, |total, path| {
        Ok(total.saturating_add(fs::metadata(path)?.len()))
    })
}

pub fn default_case_archive_name(inputs: &[PathBuf]) -> OsString {
    if inputs.len() == 1
        && let Some(name) = inputs[0].file_name()
    {
        let mut output = name.to_os_string();
        output.push(".pits");
        return output;
    }
    OsString::from("files.pits")
}

fn reset_dir(path: &Path) -> Result<(), std::io::Error> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)
}

fn ratio(original: u64, archive: u64) -> f64 {
    if original == 0 {
        1.0
    } else {
        archive as f64 / original as f64
    }
}

fn profile_name(profile: CompressionProfile) -> &'static str {
    match profile {
        CompressionProfile::Raw => "raw",
        CompressionProfile::Stream => "stream",
        CompressionProfile::Random => "random",
        CompressionProfile::Balanced => "balanced",
        CompressionProfile::ArchiveMax => "archive-max",
    }
}

fn external_profile_name(kind: ExternalKind) -> &'static str {
    match kind {
        ExternalKind::SevenZip => "7z-lzma2-mx9",
        ExternalKind::WinRar => "rar5-m5-solid",
        ExternalKind::WinZip => "zip-best",
    }
}

fn representation_trace_enabled() -> bool {
    std::env::var("PITHOS_REP_TRACE").ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn sanitize_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "file".to_owned()
    } else {
        out
    }
}

fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn write_csv(path: &Path, records: &[BenchmarkRecord]) -> Result<(), std::io::Error> {
    let mut writer = BufWriter::new(File::create(path)?);
    writeln!(
        writer,
        "case,compressor,profile,input_count,original_bytes,archive_bytes,compression_ratio,savings_percent,compress_ms,verify_ms,decompress_ms,status,detail"
    )?;
    for record in records {
        writeln!(
            writer,
            "{},{},{},{},{},{},{},{},{},{},{},{},{}",
            csv_field(&record.case),
            csv_field(&record.compressor),
            csv_field(&record.profile),
            record.input_count,
            record.original_bytes,
            record
                .archive_bytes
                .map(|value| value.to_string())
                .unwrap_or_default(),
            record
                .compression_ratio
                .map(|value| format!("{value:.6}"))
                .unwrap_or_default(),
            record
                .savings_percent
                .map(|value| format!("{value:.4}"))
                .unwrap_or_default(),
            record
                .compress_ms
                .map(|value| value.to_string())
                .unwrap_or_default(),
            record
                .verify_ms
                .map(|value| value.to_string())
                .unwrap_or_default(),
            record
                .decompress_ms
                .map(|value| value.to_string())
                .unwrap_or_default(),
            csv_field(&record.status),
            csv_field(record.detail.as_deref().unwrap_or("")),
        )?;
    }
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_name_preserves_single_input_name_and_uses_files_for_many() {
        assert_eq!(
            default_case_archive_name(&[PathBuf::from("report.pdf")]),
            OsString::from("report.pdf.pits")
        );
        assert_eq!(
            default_case_archive_name(&[PathBuf::from("a"), PathBuf::from("b")]),
            OsString::from("files.pits")
        );
    }

    #[test]
    fn sanitizer_is_stable() {
        assert_eq!(sanitize_name("a/b c.pdf"), "a_b_c.pdf");
    }

    #[test]
    fn csv_fields_escape_delimiters_quotes_and_newlines() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_field("a\nb"), "\"a\nb\"");
    }
}
