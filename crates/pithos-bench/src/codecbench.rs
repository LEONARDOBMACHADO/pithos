use clap::Parser;
use pithos_codecs::{BrotliCodec, Codec, CodecConfig, CodecId, Lzma2Codec, StoreCodec, ZstdCodec};
use serde::Serialize;
use std::fs::{self, File};
use std::io::{BufWriter, Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

#[derive(Debug, Parser)]
#[command(
    name = "pithos-codecbench",
    about = "Measure Pithos codec ratio, encode/decode time and memory bound per corpus file"
)]
struct Args {
    #[arg(long, default_value = "tst_compact")]
    corpus: PathBuf,

    #[arg(long)]
    results: Option<PathBuf>,

    /// Skip files larger than this threshold for this direct in-memory codec probe.
    #[arg(long, default_value_t = 150)]
    max_file_mib: u64,
}

#[derive(Debug, Serialize)]
struct CodecRecord {
    relative_path: String,
    extension: String,
    codec: String,
    level: i32,
    input_bytes: u64,
    output_bytes: u64,
    compression_ratio: f64,
    savings_percent: f64,
    encode_ms: u128,
    decode_ms: u128,
    throughput_encode_mib_s: f64,
    throughput_decode_mib_s: f64,
    memory_bound_bytes: u64,
    roundtrip_ok: bool,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("codec benchmark failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let results = args.results.unwrap_or_else(|| args.corpus.join("results"));
    fs::create_dir_all(&results)?;
    let files = collect_files(&args.corpus, &results)?;
    if files.is_empty() {
        return Err(std::io::Error::other("benchmark corpus is empty").into());
    }

    let max_bytes = args.max_file_mib.saturating_mul(1024 * 1024);
    let jsonl_path = results.join("codec-benchmark.jsonl");
    let csv_path = results.join("codec-benchmark.csv");
    let mut jsonl = BufWriter::new(File::create(jsonl_path)?);
    let mut csv = BufWriter::new(File::create(csv_path)?);
    writeln!(
        csv,
        "relative_path,extension,codec,level,input_bytes,output_bytes,compression_ratio,savings_percent,encode_ms,decode_ms,throughput_encode_mib_s,throughput_decode_mib_s,memory_bound_bytes,roundtrip_ok"
    )?;

    let codecs: [(&str, &dyn Codec); 4] = [
        ("store", &StoreCodec),
        ("zstd", &ZstdCodec),
        ("brotli", &BrotliCodec),
        ("lzma2", &Lzma2Codec),
    ];

    let corpus_root = fs::canonicalize(&args.corpus)?;
    let mut records = 0_usize;
    for path in files {
        let size = fs::metadata(&path)?.len();
        if size > max_bytes {
            continue;
        }
        let input = fs::read(&path)?;
        let relative = path
            .strip_prefix(&corpus_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        for (name, codec) in codecs {
            let config = CodecConfig::deterministic_default(codec.id());
            let memory_bound = codec.memory_bound(input.len() as u64, &config)?;

            let encode_started = Instant::now();
            let mut encoded = Vec::new();
            let stats = codec.encode(&input, &config, &mut encoded)?;
            let encode_elapsed = encode_started.elapsed();

            let decode_started = Instant::now();
            let mut decoded = Vec::with_capacity(input.len());
            codec.decode(
                &mut Cursor::new(encoded.as_slice()),
                input.len() as u64,
                &mut decoded,
            )?;
            let decode_elapsed = decode_started.elapsed();
            let roundtrip_ok = decoded == input;
            if !roundtrip_ok {
                return Err(std::io::Error::other(format!(
                    "codec round-trip mismatch: {name} on {relative}"
                ))
                .into());
            }

            let ratio = ratio(stats.input_bytes, stats.output_bytes);
            let record = CodecRecord {
                relative_path: relative.clone(),
                extension: extension.clone(),
                codec: name.to_owned(),
                level: config.level,
                input_bytes: stats.input_bytes,
                output_bytes: stats.output_bytes,
                compression_ratio: ratio,
                savings_percent: (1.0 - ratio) * 100.0,
                encode_ms: encode_elapsed.as_millis(),
                decode_ms: decode_elapsed.as_millis(),
                throughput_encode_mib_s: throughput_mib_s(stats.input_bytes, encode_elapsed.as_secs_f64()),
                throughput_decode_mib_s: throughput_mib_s(stats.input_bytes, decode_elapsed.as_secs_f64()),
                memory_bound_bytes: memory_bound,
                roundtrip_ok,
            };
            serde_json::to_writer(&mut jsonl, &record)?;
            jsonl.write_all(b"\n")?;
            write_csv_record(&mut csv, &record)?;
            records += 1;
        }
    }

    jsonl.flush()?;
    csv.flush()?;
    println!("codec benchmark complete: {records} records; results: {}", results.display());
    Ok(())
}

fn codec_id_name(id: CodecId) -> &'static str {
    match id {
        CodecId::Store => "store",
        CodecId::Zstd => "zstd",
        CodecId::Brotli => "brotli",
        CodecId::Lzma2 => "lzma2",
    }
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

fn ratio(input: u64, output: u64) -> f64 {
    if input == 0 {
        1.0
    } else {
        output as f64 / input as f64
    }
}

fn throughput_mib_s(bytes: u64, seconds: f64) -> f64 {
    if seconds <= 0.0 {
        0.0
    } else {
        (bytes as f64 / (1024.0 * 1024.0)) / seconds
    }
}

fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn write_csv_record<W: Write>(writer: &mut W, record: &CodecRecord) -> std::io::Result<()> {
    writeln!(
        writer,
        "{},{},{},{},{},{},{:.6},{:.4},{},{},{:.3},{:.3},{},{}",
        csv_field(&record.relative_path),
        csv_field(&record.extension),
        csv_field(&record.codec),
        record.level,
        record.input_bytes,
        record.output_bytes,
        record.compression_ratio,
        record.savings_percent,
        record.encode_ms,
        record.decode_ms,
        record.throughput_encode_mib_s,
        record.throughput_decode_mib_s,
        record.memory_bound_bytes,
        record.roundtrip_ok,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_names_remain_stable() {
        assert_eq!(codec_id_name(CodecId::Store), "store");
        assert_eq!(codec_id_name(CodecId::Zstd), "zstd");
        assert_eq!(codec_id_name(CodecId::Brotli), "brotli");
        assert_eq!(codec_id_name(CodecId::Lzma2), "lzma2");
    }

    #[test]
    fn ratio_for_empty_input_is_neutral() {
        assert_eq!(ratio(0, 0), 1.0);
    }
}
