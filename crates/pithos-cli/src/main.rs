use clap::Parser;
use pithos_agent_api::{ApiProfile, PathScope, ReadRangeResult};
use pithos_cli::{
    Cli, CliProfile, Commands, DaemonClient, DaemonClientError, ExecutionMode, OutputFormat,
    default_archive_path, default_daemon_state_dir,
};
use pithos_core::{CompressionProfile, PithosError};
use pithos_engine::{
    ExtractRequest, PackRequest, UnpackRequest, extract, extract_to_writer, inspect, list, pack,
    unpack, verify,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error(transparent)]
    Engine(#[from] PithosError),
    #[error(transparent)]
    Daemon(#[from] DaemonClientError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("invalid command: {0}")]
    Invalid(&'static str),
}

const STDOUT_RANGE_BYTES: u64 = 64 * 1024 * 1024;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let output_format = cli.output_format;
    match execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            emit_error(output_format, &error);
            ExitCode::FAILURE
        }
    }
}

async fn execute(cli: Cli) -> Result<(), AppError> {
    let Cli {
        command,
        output_format,
        mode,
        daemon_state_dir,
    } = cli;
    match mode {
        ExecutionMode::Standalone => {
            if daemon_state_dir.is_some() {
                return Err(AppError::Invalid(
                    "--daemon-state-dir requires --mode daemon",
                ));
            }
            run_standalone(command, output_format)
        }
        ExecutionMode::Daemon => {
            let state_dir = daemon_state_dir.unwrap_or_else(default_daemon_state_dir);
            run_daemon(command, output_format, state_dir).await
        }
    }
}

fn run_standalone(command: Commands, output_format: OutputFormat) -> Result<(), AppError> {
    match command {
        Commands::Capabilities => emit(
            output_format,
            &json!({
                "version": env!("CARGO_PKG_VERSION"),
                "format": "PAF 0.1-draft",
                "extension": ".pits",
                "legacy_extensions": [".phs", ".pts", ".pithos"],
                "codecs": ["STORE", "Zstandard", "Brotli", "LZMA2"],
                "profiles": ["raw", "stream", "random", "balanced", "archive-max"],
            }),
            "Pithos R1 v0.1.0 (PAF 0.1-draft, .pits)\nCodecs implementados: STORE, Zstandard, Brotli, LZMA2\nPerfis: raw, stream, random, balanced, archive-max",
        )?,
        Commands::Pack {
            inputs,
            output,
            profile,
        } => {
            let output = normalize_standalone_output(
                output.unwrap_or_else(|| default_archive_path(&inputs)),
            );
            pack(PackRequest {
                inputs,
                output: output.clone(),
                profile: standalone_profile(profile),
            })?;
            emit(
                output_format,
                &json!({"status": "packed", "archive": output}),
                "Contêiner empacotado com sucesso.",
            )?;
        }
        Commands::Unpack { archive, output } => {
            unpack(UnpackRequest {
                archive,
                output_dir: output.clone(),
            })?;
            emit(
                output_format,
                &json!({"status": "unpacked", "output": output}),
                "Contêiner desempacotado com sucesso.",
            )?;
        }
        Commands::List { archive } => {
            let entries = list(&archive)?;
            let human = entries
                .iter()
                .map(|entry| format!("{:>10}  {:?}  {}", entry.size, entry.kind, entry.path))
                .collect::<Vec<_>>()
                .join("\n");
            emit(output_format, &entries, &human)?;
        }
        Commands::Inspect { archive } => {
            let report = inspect(&archive)?;
            let human = format!(
                "{}\nEntradas: {} (arquivos: {}, diretórios: {}, hardlinks: {}, symlinks: {})\nGrupos: {}\nBytes originais: {}\nBytes do arquivo: {}\nMetadados verificados: sim",
                report.format_version,
                report.entry_count,
                report.file_count,
                report.directory_count,
                report.hardlink_count,
                report.symlink_count,
                report.group_count,
                report.original_bytes,
                report.archive_bytes,
            );
            emit(output_format, &report, &human)?;
        }
        Commands::Extract {
            archive,
            entry,
            output,
            stdout,
        } => {
            if stdout {
                ensure_stdout_format(output_format)?;
                let mut writer = io::stdout().lock();
                extract_to_writer(&archive, Path::new(&entry), &mut writer)?;
            } else {
                let output = output.ok_or(AppError::Invalid("extract output"))?;
                let report = extract(ExtractRequest {
                    archive,
                    entry: entry.into(),
                    output_dir: output,
                })?;
                let human = format!(
                    "Entrada extraída: {} ({} bytes)",
                    report.path, report.bytes_written
                );
                emit(output_format, &report, &human)?;
            }
        }
        Commands::Verify { archive } => {
            let report = verify(&archive)?;
            let human = format!(
                "Integridade verificada: {} entradas, {} grupos, raiz BLAKE3 {}",
                report.entry_count,
                report.group_count,
                hex(&report.blake3_root),
            );
            emit(output_format, &report, &human)?;
        }
    }
    Ok(())
}

fn normalize_standalone_output(path: PathBuf) -> PathBuf {
    if path
        .parent()
        .is_some_and(|parent| parent.as_os_str().is_empty())
    {
        PathBuf::from(".").join(path)
    } else {
        path
    }
}

async fn run_daemon(
    command: Commands,
    output_format: OutputFormat,
    state_dir: PathBuf,
) -> Result<(), AppError> {
    match command {
        Commands::Capabilities => {
            let root = std::fs::canonicalize(std::env::current_dir()?)?;
            let scope = PathScope {
                read_roots: vec![root],
                write_roots: Vec::new(),
            };
            let client = DaemonClient::connect(state_dir, scope).await?;
            let capabilities = client.public_capabilities()?;
            let human = capabilities_human(&capabilities)?;
            emit(output_format, &capabilities, &human)?;
        }
        Commands::Pack {
            inputs,
            output,
            profile,
        } => {
            let requested_output = output.unwrap_or_else(|| default_archive_path(&inputs));
            let prepared_inputs = inputs
                .iter()
                .map(|path| prepare_read_path(path))
                .collect::<Result<Vec<_>, _>>()?;
            let (output, write_root) = prepare_write_path(&requested_output)?;
            let scope = path_scope(&prepared_inputs, &[write_root]);
            let inputs = prepared_inputs
                .iter()
                .map(|prepared| prepared.path.clone())
                .collect();
            let mut client = DaemonClient::connect(state_dir, scope.clone()).await?;
            let result = client
                .pack(inputs, output, scope, daemon_profile(profile))
                .await?;
            let _ = result_field(&result, "archive")?;
            emit(
                output_format,
                &json!({"status": "packed", "archive": requested_output}),
                "Contêiner empacotado com sucesso.",
            )?;
        }
        Commands::Unpack { archive, output } => {
            let requested_output = output.clone();
            let archive = prepare_read_path(&archive)?;
            let (output, write_root) = prepare_write_path(&output)?;
            let scope = path_scope(std::slice::from_ref(&archive), &[write_root]);
            let mut client = DaemonClient::connect(state_dir, scope.clone()).await?;
            let result = client.unpack(archive.path, output, scope).await?;
            let _ = result_field(&result, "output")?;
            emit(
                output_format,
                &json!({"status": "unpacked", "output": requested_output}),
                "Contêiner desempacotado com sucesso.",
            )?;
        }
        Commands::List { archive } => {
            let archive = prepare_read_path(&archive)?;
            let scope = path_scope(std::slice::from_ref(&archive), &[]);
            let mut client = DaemonClient::connect(state_dir, scope.clone()).await?;
            let result = client.list(archive.path, scope).await?;
            let human = list_human(&result)?;
            emit(output_format, &result, &human)?;
        }
        Commands::Inspect { archive } => {
            let archive = prepare_read_path(&archive)?;
            let scope = path_scope(std::slice::from_ref(&archive), &[]);
            let mut client = DaemonClient::connect(state_dir, scope.clone()).await?;
            let result = client.inspect(archive.path, scope).await?;
            let human = inspect_human(&result)?;
            emit(output_format, &result, &human)?;
        }
        Commands::Extract {
            archive,
            entry,
            output,
            stdout,
        } => {
            let archive = prepare_read_path(&archive)?;
            if stdout {
                ensure_stdout_format(output_format)?;
                stream_daemon_entry(state_dir, archive, PathBuf::from(entry)).await?;
            } else {
                let output = output.ok_or(AppError::Invalid("extract output"))?;
                let (output, write_root) = prepare_write_path(&output)?;
                let scope = path_scope(std::slice::from_ref(&archive), &[write_root]);
                let mut client = DaemonClient::connect(state_dir, scope.clone()).await?;
                let report = client
                    .extract(archive.path, entry.into(), output, scope)
                    .await?;
                let human = format!(
                    "Entrada extraída: {} ({} bytes)",
                    string_field(&report, "path")?,
                    u64_field(&report, "bytes_written")?
                );
                emit(output_format, &report, &human)?;
            }
        }
        Commands::Verify { archive } => {
            let archive = prepare_read_path(&archive)?;
            let scope = path_scope(std::slice::from_ref(&archive), &[]);
            let mut client = DaemonClient::connect(state_dir, scope.clone()).await?;
            let result = client.verify(archive.path, scope).await?;
            let human = format!(
                "Integridade verificada: {} entradas, {} grupos, raiz BLAKE3 {}",
                u64_field(&result, "entry_count")?,
                u64_field(&result, "group_count")?,
                json_hash(result_field(&result, "blake3_root")?)?,
            );
            emit(output_format, &result, &human)?;
        }
    }
    Ok(())
}

async fn stream_daemon_entry(
    state_dir: PathBuf,
    archive: PreparedReadPath,
    entry: PathBuf,
) -> Result<(), AppError> {
    let scope = path_scope(std::slice::from_ref(&archive), &[]);
    let mut client = DaemonClient::connect(state_dir.clone(), scope.clone()).await?;
    let entries = client.list(archive.path.clone(), scope.clone()).await?;
    let selector = archive_entry_selector(&entry)?;
    let selected = entries
        .as_array()
        .ok_or(AppError::Invalid("daemon returned an invalid list result"))?
        .iter()
        .find(|candidate| candidate.get("path").and_then(Value::as_str) == Some(&selector))
        .ok_or(AppError::Invalid("entry not found"))?;
    if !matches!(string_field(selected, "kind")?, "file" | "hardlink") {
        return Err(AppError::Invalid("--stdout requires a file entry"));
    }
    let entry_size = u64_field(selected, "size")?;
    let mut offset = 0_u64;
    let mut writer = io::stdout().lock();
    loop {
        let length = if entry_size == 0 {
            0
        } else {
            (entry_size - offset).min(STDOUT_RANGE_BYTES)
        };
        let transfer = client
            .read_range(
                archive.path.clone(),
                entry.clone(),
                offset,
                length,
                scope.clone(),
            )
            .await?;
        if transfer.offset != offset || transfer.length != length {
            return Err(AppError::Invalid("daemon returned a mismatched range"));
        }
        let mut file = verified_transfer(&state_dir, &transfer)?;
        io::copy(&mut file, &mut writer)?;
        if entry_size == 0 || offset + length == entry_size {
            break;
        }
        offset += length;
    }
    writer.flush()?;
    Ok(())
}

fn archive_entry_selector(path: &Path) -> Result<String, AppError> {
    let parts = path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(part) => part
                .to_str()
                .filter(|part| !part.is_empty())
                .ok_or(AppError::Invalid("entry path is not portable")),
            _ => Err(AppError::Invalid("entry path must be relative")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parts.is_empty() {
        return Err(AppError::Invalid("entry path is empty"));
    }
    Ok(parts.join("/"))
}

fn verified_transfer(
    state_dir: &Path,
    transfer: &ReadRangeResult,
) -> Result<std::fs::File, AppError> {
    let transfer_metadata = std::fs::symlink_metadata(&transfer.path)?;
    if !transfer_metadata.is_file() || transfer_metadata.file_type().is_symlink() {
        return Err(AppError::Invalid("daemon returned an unsafe transfer"));
    }
    let state_dir = std::fs::canonicalize(state_dir)?;
    let transfers_dir = std::fs::canonicalize(state_dir.join("transfers"))?;
    let transfer_path = std::fs::canonicalize(&transfer.path)?;
    if transfer_path.parent() != Some(transfers_dir.as_path())
        || !transfer_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("range-") && name.ends_with(".bin"))
    {
        return Err(AppError::Invalid("daemon returned an unsafe transfer"));
    }
    let mut file = std::fs::File::open(transfer_path)?;
    if file.metadata()?.len() != transfer.length {
        return Err(AppError::Invalid("daemon returned a truncated transfer"));
    }
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher)?;
    if hasher.finalize().to_hex().as_str() != transfer.blake3 {
        return Err(AppError::Invalid("daemon returned a corrupt transfer"));
    }
    file.rewind()?;
    Ok(file)
}

#[derive(Debug)]
struct PreparedReadPath {
    path: PathBuf,
    root: PathBuf,
}

fn prepare_read_path(path: &Path) -> Result<PreparedReadPath, AppError> {
    let path = std::fs::canonicalize(path)?;
    let metadata = std::fs::metadata(&path)?;
    let root = if metadata.is_dir() {
        path.clone()
    } else {
        path.parent()
            .ok_or(AppError::Invalid("read path has no parent"))?
            .to_path_buf()
    };
    Ok(PreparedReadPath { path, root })
}

fn prepare_write_path(path: &Path) -> Result<(PathBuf, PathBuf), AppError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let name = absolute
        .file_name()
        .ok_or(AppError::Invalid("output path has no final component"))?
        .to_os_string();
    let parent = absolute
        .parent()
        .ok_or(AppError::Invalid("output path has no parent"))?;
    let (parent, root) = resolve_write_directory(parent)?;
    Ok((parent.join(name), root))
}

fn resolve_write_directory(path: &Path) -> Result<(PathBuf, PathBuf), AppError> {
    let mut cursor = path;
    let mut missing = Vec::<OsString>::new();
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(AppError::Invalid("output parent is not a directory"));
                }
                let root = std::fs::canonicalize(cursor)?;
                let mut resolved = root.clone();
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok((resolved, root));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = cursor
                    .file_name()
                    .ok_or(AppError::Invalid("cannot resolve output parent"))?;
                missing.push(name.to_os_string());
                cursor = cursor
                    .parent()
                    .ok_or(AppError::Invalid("cannot resolve output parent"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn path_scope(reads: &[PreparedReadPath], write_roots: &[PathBuf]) -> PathScope {
    PathScope {
        read_roots: deduplicate(reads.iter().map(|prepared| prepared.root.clone())),
        write_roots: deduplicate(write_roots.iter().cloned()),
    }
}

fn deduplicate(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn capabilities_human(value: &Value) -> Result<String, AppError> {
    let formats = string_array_field(value, "format_versions")?.join(", ");
    let codecs = string_array_field(value, "supported_codecs")?.join(", ");
    let methods = string_array_field(value, "supported_methods")?.join(", ");
    Ok(format!(
        "{} v{} ({formats})\nProtocolo Agent API: {}\nCodecs implementados: {codecs}\nMétodos: {methods}",
        string_field(value, "product")?,
        string_field(value, "version")?,
        u64_field(value, "protocol_version")?,
    ))
}

fn list_human(value: &Value) -> Result<String, AppError> {
    let entries = value
        .as_array()
        .ok_or(AppError::Invalid("daemon returned an invalid list result"))?;
    entries
        .iter()
        .map(|entry| {
            let kind = match string_field(entry, "kind")? {
                "file" => "File",
                "directory" => "Directory",
                "hardlink" => "Hardlink",
                "symlink" => "Symlink",
                _ => return Err(AppError::Invalid("daemon returned an invalid entry kind")),
            };
            Ok(format!(
                "{:>10}  {}  {}",
                u64_field(entry, "size")?,
                kind,
                string_field(entry, "path")?
            ))
        })
        .collect::<Result<Vec<_>, AppError>>()
        .map(|lines| lines.join("\n"))
}

fn inspect_human(value: &Value) -> Result<String, AppError> {
    Ok(format!(
        "{}\nEntradas: {} (arquivos: {}, diretórios: {}, hardlinks: {}, symlinks: {})\nGrupos: {}\nBytes originais: {}\nBytes do arquivo: {}\nMetadados verificados: {}",
        string_field(value, "format_version")?,
        u64_field(value, "entry_count")?,
        u64_field(value, "file_count")?,
        u64_field(value, "directory_count")?,
        u64_field(value, "hardlink_count")?,
        u64_field(value, "symlink_count")?,
        u64_field(value, "group_count")?,
        u64_field(value, "original_bytes")?,
        u64_field(value, "archive_bytes")?,
        if bool_field(value, "metadata_verified")? {
            "sim"
        } else {
            "não"
        },
    ))
}

fn result_field<'a>(value: &'a Value, field: &'static str) -> Result<&'a Value, AppError> {
    value.get(field).ok_or(AppError::Invalid(
        "daemon response is missing a required field",
    ))
}

fn string_field<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, AppError> {
    result_field(value, field)?
        .as_str()
        .ok_or(AppError::Invalid(
            "daemon response field has the wrong type",
        ))
}

fn u64_field(value: &Value, field: &'static str) -> Result<u64, AppError> {
    result_field(value, field)?
        .as_u64()
        .ok_or(AppError::Invalid(
            "daemon response field has the wrong type",
        ))
}

fn bool_field(value: &Value, field: &'static str) -> Result<bool, AppError> {
    result_field(value, field)?
        .as_bool()
        .ok_or(AppError::Invalid(
            "daemon response field has the wrong type",
        ))
}

fn string_array_field<'a>(value: &'a Value, field: &'static str) -> Result<Vec<&'a str>, AppError> {
    result_field(value, field)?
        .as_array()
        .ok_or(AppError::Invalid(
            "daemon response field has the wrong type",
        ))?
        .iter()
        .map(|item| {
            item.as_str().ok_or(AppError::Invalid(
                "daemon response field has the wrong type",
            ))
        })
        .collect()
}

fn json_hash(value: &Value) -> Result<String, AppError> {
    let bytes = value
        .as_array()
        .ok_or(AppError::Invalid("daemon returned an invalid hash"))?;
    if bytes.len() != 32 {
        return Err(AppError::Invalid("daemon returned an invalid hash"));
    }
    bytes
        .iter()
        .map(|byte| {
            byte.as_u64()
                .and_then(|byte| u8::try_from(byte).ok())
                .map(|byte| format!("{byte:02x}"))
                .ok_or(AppError::Invalid("daemon returned an invalid hash"))
        })
        .collect()
}

fn ensure_stdout_format(format: OutputFormat) -> Result<(), AppError> {
    if format == OutputFormat::Json {
        return Err(AppError::Invalid(
            "--stdout cannot be combined with --output-format json",
        ));
    }
    Ok(())
}

fn standalone_profile(profile: CliProfile) -> CompressionProfile {
    match profile {
        CliProfile::Raw => CompressionProfile::Raw,
        CliProfile::Stream => CompressionProfile::Stream,
        CliProfile::Random => CompressionProfile::Random,
        CliProfile::Balanced => CompressionProfile::Balanced,
        CliProfile::ArchiveMax => CompressionProfile::ArchiveMax,
    }
}

fn daemon_profile(profile: CliProfile) -> ApiProfile {
    match profile {
        CliProfile::Raw => ApiProfile::Raw,
        CliProfile::Stream => ApiProfile::Stream,
        CliProfile::Random => ApiProfile::Random,
        CliProfile::Balanced => ApiProfile::Balanced,
        CliProfile::ArchiveMax => ApiProfile::ArchiveMax,
    }
}

fn emit<T: Serialize>(format: OutputFormat, value: &T, human: &str) -> Result<(), AppError> {
    match format {
        OutputFormat::Human => println!("{human}"),
        OutputFormat::Json => println!("{}", serde_json::to_string(value)?),
    }
    Ok(())
}

fn emit_error(format: OutputFormat, error: &AppError) {
    match format {
        OutputFormat::Human => eprintln!("Erro: {error}"),
        OutputFormat::Json => {
            let value = match error {
                AppError::Daemon(daemon) => match daemon.public_error() {
                    Some((kind, message)) => json!({"error": {"kind": kind, "message": message}}),
                    None => {
                        json!({"error": {"kind": "daemon_unavailable", "message": daemon.to_string()}})
                    }
                },
                _ => json!({"error": {"kind": "command_failed", "message": error.to_string()}}),
            };
            eprintln!("{value}");
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
