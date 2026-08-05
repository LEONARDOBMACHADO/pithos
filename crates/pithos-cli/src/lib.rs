//! CLI Commands and Argument Types

use clap::{Parser, Subcommand, ValueEnum};

mod daemon_client;

pub use daemon_client::{DaemonClient, DaemonClientError, default_daemon_state_dir};

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliProfile {
    Raw,
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
    /// Empacotar arquivos em um contêiner .pithos
    Pack {
        #[arg(required = true)]
        inputs: Vec<std::path::PathBuf>,

        #[arg(short, long)]
        output: std::path::PathBuf,

        #[arg(long, value_enum, default_value_t = CliProfile::Raw)]
        profile: CliProfile,
    },
    /// Extrair conteúdo de um contêiner .pithos
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_profile_is_explicit_and_defaults_to_raw() {
        let cli = Cli::try_parse_from(["pithos", "pack", "input", "-o", "archive.pithos"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Pack {
                profile: CliProfile::Raw,
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
            let cli = Cli::try_parse_from([
                "pithos",
                "pack",
                "input",
                "-o",
                "archive.pithos",
                "--profile",
                spelling,
            ])
            .unwrap();
            assert!(matches!(
                cli.command,
                Commands::Pack { profile, .. } if profile == expected
            ));
        }
        assert!(Cli::try_parse_from([
            "pithos",
            "pack",
            "input",
            "-o",
            "archive.pithos",
            "--profile",
            "unknown",
        ])
        .is_err());
    }

    #[test]
    fn output_format_is_global_and_extract_requires_one_destination() {
        let cli = Cli::try_parse_from([
            "pithos",
            "--output-format",
            "json",
            "list",
            "archive.pithos",
        ])
        .unwrap();
        assert!(matches!(cli.output_format, OutputFormat::Json));

        assert!(Cli::try_parse_from(["pithos", "extract", "archive.pithos", "entry.txt"]).is_err());
        assert!(
            Cli::try_parse_from([
                "pithos",
                "extract",
                "archive.pithos",
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
