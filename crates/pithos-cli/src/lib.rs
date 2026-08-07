//! CLI Commands and Argument Types

use clap::{Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

mod daemon_client;

pub use daemon_client::{DaemonClient, DaemonClientError, default_daemon_state_dir};

pub const PITHOS_EXTENSION: &str = "pits";
pub const LEGACY_PITHOS_EXTENSION: &str = "pithos";
pub const LEGACY_PHS_EXTENSION: &str = "phs";
pub const LEGACY_PTS_EXTENSION: &str = "pts";

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliProfile {
    Raw,
    Stream,
    Random,
    Balanced,
    ArchiveMax,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Human,
    Json,
}

#[derive(ValueEnum, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    #[default]
    Standalone,
    Daemon,
}

#[derive(Parser, Debug)]
#[command(
    name = "pithos",
    version,
    about = "Pithos R1 - Universal Compression Engine"
)]
pub struct Cli {
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    pub output_format: OutputFormat,

    /// Select direct in-process execution or the local pithosd Agent API.
    #[arg(long, global = true, value_enum, default_value_t = ExecutionMode::Standalone)]
    pub mode: ExecutionMode,

    /// Override the private pithosd state directory used to derive its local IPC endpoint.
    #[arg(long, global = true)]
    pub daemon_state_dir: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Empacotar arquivos em um contêiner .pits
    Pack {
        #[arg(required = true)]
        inputs: Vec<std::path::PathBuf>,

        /// Output archive. Defaults to <input-name>.pits for one input or files.pits for many.
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,

        #[arg(long, value_enum, default_value_t = CliProfile::Raw)]
        profile: CliProfile,
    },
    /// Extrair conteúdo de um contêiner .pits (arquivos legados continuam aceitos)
    Unpack {
        archive: std::path::PathBuf,

        #[arg(short, long)]
        output: std::path::PathBuf,
    },
    /// Listar conteúdo de um contêiner sem descompactar os dados
    List { archive: std::path::PathBuf },
    /// Inspecionar estrutura e métricas do contêiner
    Inspect { archive: std::path::PathBuf },
    /// Extrair uma entrada específica
    Extract {
        archive: std::path::PathBuf,
        entry: String,

        #[arg(
            short,
            long,
            required_unless_present = "stdout",
            conflicts_with = "stdout"
        )]
        output: Option<std::path::PathBuf>,

        #[arg(long, conflicts_with = "output")]
        stdout: bool,
    },
    /// Verificar integridade e checksums do contêiner
    Verify { archive: std::path::PathBuf },
    /// Exibir capacidades e versões do Pithos
    Capabilities,
}

/// Computes the canonical default archive path without consulting filesystem state.
///
/// One input preserves its full file/directory name and appends `.pits`:
/// `report.pdf -> report.pdf.pits`, `Project -> Project.pits`.
/// Multiple inputs intentionally use the predictable `files.pits` name.
///
/// A bare relative filename is explicitly rooted at `.` so the engine receives
/// a valid parent directory on Windows instead of an empty parent path.
pub fn default_archive_path(inputs: &[PathBuf]) -> PathBuf {
    if inputs.len() == 1 {
        let input = &inputs[0];
        if let Some(file_name) = input.file_name() {
            let mut output_name: OsString = file_name.to_os_string();
            output_name.push(".pits");
            let output = input.with_file_name(output_name);
            if output
                .parent()
                .is_some_and(|parent| parent.as_os_str().is_empty())
            {
                return PathBuf::from(".").join(output);
            }
            return output;
        }
    }
    PathBuf::from(".").join("files.pits")
}

pub fn is_pithos_archive_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case(PITHOS_EXTENSION)
                || extension.eq_ignore_ascii_case(LEGACY_PITHOS_EXTENSION)
                || extension.eq_ignore_ascii_case(LEGACY_PHS_EXTENSION)
                || extension.eq_ignore_ascii_case(LEGACY_PTS_EXTENSION)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_profile_is_explicit_and_defaults_to_raw() {
        let cli = Cli::try_parse_from(["pithos", "pack", "input"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Pack {
                profile: CliProfile::Raw,
                output: None,
                ..
            }
        ));
    }

    #[test]
    fn every_phase_two_profile_is_accepted_with_stable_cli_spelling() {
        for (spelling, expected) in [
            ("raw", CliProfile::Raw),
            ("stream", CliProfile::Stream),
            ("random", CliProfile::Random),
            ("balanced", CliProfile::Balanced),
            ("archive-max", CliProfile::ArchiveMax),
        ] {
            let cli =
                Cli::try_parse_from(["pithos", "pack", "input", "--profile", spelling]).unwrap();
            assert!(matches!(
                cli.command,
                Commands::Pack { profile, .. } if profile == expected
            ));
        }
        assert!(Cli::try_parse_from(["pithos", "pack", "input", "--profile", "unknown",]).is_err());
    }

    #[test]
    fn default_pits_naming_is_predictable() {
        assert_eq!(
            default_archive_path(&[PathBuf::from("report.pdf")]),
            PathBuf::from(".").join("report.pdf.pits")
        );
        assert_eq!(
            default_archive_path(&[PathBuf::from("folder")]),
            PathBuf::from(".").join("folder.pits")
        );
        assert_eq!(
            default_archive_path(&[PathBuf::from("a.bin"), PathBuf::from("b.bin")]),
            PathBuf::from(".").join("files.pits")
        );
        assert!(is_pithos_archive_path(Path::new("new.pits")));
        assert!(is_pithos_archive_path(Path::new("legacy.phs")));
        assert!(is_pithos_archive_path(Path::new("provisional.pts")));
        assert!(is_pithos_archive_path(Path::new("legacy.pithos")));
        assert!(!is_pithos_archive_path(Path::new("archive.zip")));
    }

    #[test]
    fn output_format_is_global_and_extract_requires_one_destination() {
        let cli =
            Cli::try_parse_from(["pithos", "--output-format", "json", "list", "archive.pits"])
                .unwrap();
        assert!(matches!(cli.output_format, OutputFormat::Json));

        assert!(Cli::try_parse_from(["pithos", "extract", "archive.pits", "entry.txt"]).is_err());
        assert!(
            Cli::try_parse_from([
                "pithos",
                "extract",
                "archive.pits",
                "entry.txt",
                "--stdout",
                "--output",
                "destination",
            ])
            .is_err()
        );
    }

    #[test]
    fn standalone_is_the_default_and_daemon_endpoint_is_configurable() {
        let standalone = Cli::try_parse_from(["pithos", "capabilities"]).unwrap();
        assert_eq!(standalone.mode, ExecutionMode::Standalone);
        assert!(standalone.daemon_state_dir.is_none());

        let daemon = Cli::try_parse_from([
            "pithos",
            "--mode",
            "daemon",
            "--daemon-state-dir",
            "custom-state",
            "capabilities",
        ])
        .unwrap();
        assert_eq!(daemon.mode, ExecutionMode::Daemon);
        assert_eq!(
            daemon.daemon_state_dir.as_deref(),
            Some(std::path::Path::new("custom-state"))
        );

        let explicit =
            Cli::try_parse_from(["pithos", "--mode", "standalone", "capabilities"]).unwrap();
        assert_eq!(explicit.mode, ExecutionMode::Standalone);
    }
}
