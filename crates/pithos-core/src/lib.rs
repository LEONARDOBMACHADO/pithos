//! Pithos Core Types and Error Definitions

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Erros tipados normativos do Pithos.
#[derive(Error, Debug)]
pub enum PithosError {
    #[error("Magic binário inválido")]
    InvalidMagic,

    #[error("Versão de contêiner não suportada")]
    UnsupportedContainerVersion,

    #[error("Codec ou transformação não suportada")]
    UnsupportedCodec,

    #[error("Range inválido ou fora dos limites do arquivo")]
    InvalidRange,

    #[error("Estouro aritmético (overflow)")]
    IntegerOverflow,

    #[error("Seção sobreposta no diretório do contêiner")]
    OverlappingSections,

    #[error("Checksum CRC32C divergente")]
    ChecksumMismatch,

    #[error("Hash BLAKE3 divergente")]
    HashMismatch,

    #[error("Dependência cíclica detectada")]
    DependencyCycle,

    #[error("Profundidade máxima de dependências excedida")]
    DependencyDepthExceeded,

    #[error("Path inseguro ou tentativa de escape")]
    UnsafePath,

    #[error("Codificação de path não suportada nesta plataforma")]
    InvalidPathEncoding,

    #[error("Metadados do contêiner inválidos: {0}")]
    InvalidMetadata(&'static str),

    #[error("Seção obrigatória ausente: {0}")]
    MissingSection(&'static str),

    #[error("Seção duplicada no diretório")]
    DuplicateSection,

    #[error("Tipo de arquivo não suportado")]
    UnsupportedFileType,

    #[error("Symlink inseguro ou fora da raiz de entrada")]
    UnsafeSymlink,

    #[error("Destino já existe")]
    OutputExists,

    #[error("Limite de recursos excedido: {0}")]
    ResourceLimit(&'static str),

    #[error("Limite de memória excedido")]
    MemoryLimit,

    #[error("Limite de espaço temporário excedido")]
    TemporarySpaceLimit,

    #[error("Arquivo de entrada foi alterado durante o processamento")]
    InputChanged,

    #[error("Operação cancelada pelo usuário ou agente")]
    Cancelled,

    #[error("Candidato excedeu o custo total do incumbent")]
    CandidateExceededIncumbent,

    #[error("Erro de I/O: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, PithosError>;

/// Perfis oficiais de compactação.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CompressionProfile {
    Raw,
    Stream,
    Random,
    #[default]
    Balanced,
    ArchiveMax,
}

/// Estados formais do processo de empacotamento.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackState {
    Created,
    Scanning,
    StructuralAnalysis,
    RecompressionAnalysis,
    Chunking,
    Fingerprinting,
    Indexing,
    Clustering,
    Planning,
    Encoding,
    Writing,
    Verifying,
    Committed,
    Failed,
    Cancelled,
}

/// Estados de um trabalho assíncrono mantido pelo daemon (pithosd).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Queued,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

/// Motivos normativos de rejeição de candidatos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    NotEligible,
    TooSmall,
    LowRepetition,
    LowSimilarity,
    TooManyExceptions,
    EstimatedNoGain,
    ExactNoGain,
    MetadataOverhead,
    BaseCost,
    DictionaryCost,
    TimeBudget,
    MemoryBudget,
    DependencyLimit,
    ReconstructionMismatch,
    HashMismatch,
    CandidateDominated,
    Cancelled,
}

/// Limites de segurança durante decodificação.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodeLimits {
    pub max_entries: u64,
    pub max_groups: u64,
    pub max_chunks: u64,
    pub max_original_bytes: u64,
    pub max_group_output: u64,
    pub max_dependency_depth: u8,
    pub max_rule_depth: u8,
    pub max_rules: u64,
    pub max_expansion_ratio: u64,
    pub max_metadata_bytes: u64,
    pub max_path_bytes: u64,
    pub max_path_components: u64,
    pub max_sections: u32,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_entries: 10_000_000,
            max_groups: 1_000_000,
            max_chunks: 50_000_000,
            max_original_bytes: 1_000_000_000_000, // 1 TB
            max_group_output: 1_073_741_824,       // 1 GB
            max_dependency_depth: 32,
            max_rule_depth: 64,
            max_rules: 1_000_000,
            // Highly repetitive legitimate inputs can exceed 1000:1. The hard
            // max_group_output remains the primary allocation/output bound, so
            // this ratio can be permissive without allowing unbounded decode.
            max_expansion_ratio: 65_536,
            max_metadata_bytes: 256 * 1024 * 1024,
            max_path_bytes: 32 * 1024,
            max_path_components: 1024,
            max_sections: 64,
        }
    }
}
