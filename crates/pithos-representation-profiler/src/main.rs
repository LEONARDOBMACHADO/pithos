use pithos_native_v12 as v12;
use pithos_native_v14 as v14;
use pithos_native_v15 as v15;
use pithos_native_v16 as v16;
use pithos_native_v17 as v17;
use pithos_native_v18 as v18;
use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let corpus = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: pithos-representation-profiler <corpus-dir> [output.csv] [versions]")?;
    let output = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("representation-profile.csv"));
    let versions = args.next().unwrap_or_else(|| "12,14,15,16,17".to_string());
    let selected = versions
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>();

    if !corpus.is_dir() {
        return Err(format!("corpus directory not found: {}", corpus.display()).into());
    }

    let mut files = Vec::new();
    collect_files(&corpus, &corpus, &mut files)?;
    files.sort_by(|left, right| {
        relative_key(&corpus, left).cmp(&relative_key(&corpus, right))
    });
    if files.is_empty() {
        return Err("corpus is empty".into());
    }

    let mut input = Vec::new();
    let mut member_lengths = Vec::with_capacity(files.len());
    for path in &files {
        let bytes = fs::read(path)?;
        member_lengths.push(bytes.len() as u64);
        input.extend_from_slice(&bytes);
    }

    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut writer = BufWriter::new(File::create(&output)?);
    writeln!(
        writer,
        "version,input_files,input_bytes,payload_bytes,encode_ms,decode_ms,chunk_count,canonical_chunks,gross_duplicate_bytes,representation_bytes,stats_encoded_bytes,roundtrip"
    )?;

    eprintln!(
        "PITHOS_REP_PROFILE\tstage=corpus\tfiles={}\tbytes={}\tversions={}",
        files.len(),
        input.len(),
        versions
    );

    macro_rules! run_version {
        ($version:literal, $module:ident) => {
            if selected.contains($version) {
                let encode_started = Instant::now();
                let (payload, stats) = $module::encode_exact_dedup(&input, &member_lengths, 15)?;
                let encode_ms = encode_started.elapsed().as_secs_f64() * 1000.0;

                let decode_started = Instant::now();
                let decoded = $module::decode_exact_dedup(&payload, input.len() as u64)?;
                let decode_ms = decode_started.elapsed().as_secs_f64() * 1000.0;
                let roundtrip = decoded == input;
                if !roundtrip {
                    return Err(format!("native v{} round-trip mismatch", $version).into());
                }

                writeln!(
                    writer,
                    "{},{},{},{},{:.3},{:.3},{},{},{},{},{},{}",
                    $version,
                    files.len(),
                    input.len(),
                    payload.len(),
                    encode_ms,
                    decode_ms,
                    stats.chunk_count,
                    stats.canonical_chunks,
                    stats.gross_duplicate_bytes,
                    stats.representation_bytes,
                    stats.encoded_bytes,
                    roundtrip
                )?;
                writer.flush()?;
                eprintln!(
                    "PITHOS_REP_PROFILE\tstage=native\tversion={}\tpayload_bytes={}\tencode_ms={:.3}\tdecode_ms={:.3}\tchunks={}\tcanonical={}\tduplicate_bytes={}\trepresentation_bytes={}\troundtrip={}",
                    $version,
                    payload.len(),
                    encode_ms,
                    decode_ms,
                    stats.chunk_count,
                    stats.canonical_chunks,
                    stats.gross_duplicate_bytes,
                    stats.representation_bytes,
                    roundtrip
                );
                drop(decoded);
                drop(payload);
            }
        };
    }

    run_version!("12", v12);
    run_version!("14", v14);
    run_version!("15", v15);
    run_version!("16", v16);
    run_version!("17", v17);
    run_version!("18", v18);

    writer.flush()?;
    eprintln!(
        "PITHOS_REP_PROFILE\tstage=complete\toutput={}",
        output.display()
    );
    Ok(())
}

fn collect_files(root: &Path, directory: &Path, output: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name == "results" || name == ".git" || name == "target" {
                continue;
            }
            collect_files(root, &path, output)?;
        } else if file_type.is_file() {
            if path.strip_prefix(root).is_ok() {
                output.push(path);
            }
        }
    }
    Ok(())
}

fn relative_key(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
