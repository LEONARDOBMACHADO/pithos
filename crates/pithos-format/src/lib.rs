//! Pithos Archive Format (PAF 0.1-draft) primitives.

use pithos_core::{PithosError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

pub const GLOBAL_MAGIC: &[u8; 8] = b"UDOC\r\n\x1A\n";
pub const GROUP_MAGIC: &[u8; 4] = b"UGRP";
pub const FOOTER_MAGIC: &[u8; 4] = b"UFTR";

pub const HEADER_LEN: usize = 96;
pub const FOOTER_LEN: usize = 64;
pub const SECTION_ENTRY_LEN: usize = 32;
pub const GROUP_RECORD_LEN: usize = 48;
pub const REQUIRED_RAW_SECTIONS: u32 = 6;

pub const MAJOR_VERSION: u16 = 0;
pub const MINOR_VERSION: u16 = 1;

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SectionType {
    CodecRegistry = 1,
    EntryTable = 2,
    ObjectGraph = 3,
    BaseDictionaryArea = 4,
    GroupTable = 5,
    PayloadArea = 6,
    RestoreMap = 7,
    CentralIndex = 8,
    IntegrityTree = 9,
    OptionalTelemetry = 10,
}

impl SectionType {
    pub fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::CodecRegistry),
            2 => Some(Self::EntryTable),
            3 => Some(Self::ObjectGraph),
            4 => Some(Self::BaseDictionaryArea),
            5 => Some(Self::GroupTable),
            6 => Some(Self::PayloadArea),
            7 => Some(Self::RestoreMap),
            8 => Some(Self::CentralIndex),
            9 => Some(Self::IntegrityTree),
            10 => Some(Self::OptionalTelemetry),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalHeader {
    pub major: u16,
    pub minor: u16,
    pub flags: u64,
    pub archive_id: [u8; 16],
    pub original_total_size: u64,
    pub entry_count: u64,
    pub logical_chunk_count: u64,
    pub group_count: u64,
    pub section_directory_offset: u64,
    pub footer_offset: u64,
    pub section_count: u32,
}

impl GlobalHeader {
    pub fn new(archive_id: [u8; 16]) -> Self {
        Self {
            major: MAJOR_VERSION,
            minor: MINOR_VERSION,
            flags: 0,
            archive_id,
            original_total_size: 0,
            entry_count: 0,
            logical_chunk_count: 0,
            group_count: 0,
            section_directory_offset: HEADER_LEN as u64,
            footer_offset: 0,
            section_count: REQUIRED_RAW_SECTIONS,
        }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buffer = [0_u8; HEADER_LEN];
        buffer[0..8].copy_from_slice(GLOBAL_MAGIC);
        buffer[8..10].copy_from_slice(&self.major.to_le_bytes());
        buffer[10..12].copy_from_slice(&self.minor.to_le_bytes());
        buffer[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        buffer[16..24].copy_from_slice(&self.flags.to_le_bytes());
        buffer[24..40].copy_from_slice(&self.archive_id);
        buffer[40..48].copy_from_slice(&self.original_total_size.to_le_bytes());
        buffer[48..56].copy_from_slice(&self.entry_count.to_le_bytes());
        buffer[56..64].copy_from_slice(&self.logical_chunk_count.to_le_bytes());
        buffer[64..72].copy_from_slice(&self.group_count.to_le_bytes());
        buffer[72..80].copy_from_slice(&self.section_directory_offset.to_le_bytes());
        buffer[80..88].copy_from_slice(&self.footer_offset.to_le_bytes());
        buffer[92..96].copy_from_slice(&self.section_count.to_le_bytes());
        let crc = header_crc(&buffer);
        buffer[88..92].copy_from_slice(&crc.to_le_bytes());
        buffer
    }

    pub fn decode(buffer: &[u8; HEADER_LEN]) -> Result<Self> {
        if &buffer[0..8] != GLOBAL_MAGIC {
            return Err(PithosError::InvalidMagic);
        }
        if read_u32(buffer, 88) != header_crc(buffer) {
            return Err(PithosError::ChecksumMismatch);
        }
        let major = read_u16(buffer, 8);
        let minor = read_u16(buffer, 10);
        if read_u32(buffer, 12) as usize != HEADER_LEN
            || major != MAJOR_VERSION
            || minor > MINOR_VERSION
        {
            return Err(PithosError::UnsupportedContainerVersion);
        }
        let mut archive_id = [0_u8; 16];
        archive_id.copy_from_slice(&buffer[24..40]);
        Ok(Self {
            major,
            minor,
            flags: read_u64(buffer, 16),
            archive_id,
            original_total_size: read_u64(buffer, 40),
            entry_count: read_u64(buffer, 48),
            logical_chunk_count: read_u64(buffer, 56),
            group_count: read_u64(buffer, 64),
            section_directory_offset: read_u64(buffer, 72),
            footer_offset: read_u64(buffer, 80),
            section_count: read_u32(buffer, 92),
        })
    }
}

fn header_crc(buffer: &[u8; HEADER_LEN]) -> u32 {
    let first = crc32c::crc32c(&buffer[..88]);
    crc32c::crc32c_append(first, &buffer[92..96])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionDirectoryRecord {
    pub section_type: u16,
    pub section_version: u16,
    pub flags: u32,
    pub offset: u64,
    pub length: u64,
    pub crc32c: u32,
    pub reserved: u32,
}

impl SectionDirectoryRecord {
    pub fn encode(&self) -> [u8; SECTION_ENTRY_LEN] {
        let mut buffer = [0_u8; SECTION_ENTRY_LEN];
        buffer[0..2].copy_from_slice(&self.section_type.to_le_bytes());
        buffer[2..4].copy_from_slice(&self.section_version.to_le_bytes());
        buffer[4..8].copy_from_slice(&self.flags.to_le_bytes());
        buffer[8..16].copy_from_slice(&self.offset.to_le_bytes());
        buffer[16..24].copy_from_slice(&self.length.to_le_bytes());
        buffer[24..28].copy_from_slice(&self.crc32c.to_le_bytes());
        buffer[28..32].copy_from_slice(&self.reserved.to_le_bytes());
        buffer
    }

    pub fn decode(buffer: &[u8; SECTION_ENTRY_LEN]) -> Self {
        Self {
            section_type: read_u16(buffer, 0),
            section_version: read_u16(buffer, 2),
            flags: read_u32(buffer, 4),
            offset: read_u64(buffer, 8),
            length: read_u64(buffer, 16),
            crc32c: read_u32(buffer, 24),
            reserved: read_u32(buffer, 28),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRecord {
    pub version: u16,
    pub flags: u16,
    pub group_id: u64,
    pub codec_chain_id: u32,
    pub chunk_count: u32,
    pub uncompressed_len: u64,
    pub compressed_len: u64,
    pub descriptor_len: u32,
    pub payload_crc32c: u32,
}

impl GroupRecord {
    pub fn encode(&self) -> [u8; GROUP_RECORD_LEN] {
        let mut buffer = [0_u8; GROUP_RECORD_LEN];
        buffer[0..4].copy_from_slice(GROUP_MAGIC);
        buffer[4..6].copy_from_slice(&self.version.to_le_bytes());
        buffer[6..8].copy_from_slice(&self.flags.to_le_bytes());
        buffer[8..16].copy_from_slice(&self.group_id.to_le_bytes());
        buffer[16..20].copy_from_slice(&self.codec_chain_id.to_le_bytes());
        buffer[20..24].copy_from_slice(&self.chunk_count.to_le_bytes());
        buffer[24..32].copy_from_slice(&self.uncompressed_len.to_le_bytes());
        buffer[32..40].copy_from_slice(&self.compressed_len.to_le_bytes());
        buffer[40..44].copy_from_slice(&self.descriptor_len.to_le_bytes());
        buffer[44..48].copy_from_slice(&self.payload_crc32c.to_le_bytes());
        buffer
    }

    pub fn decode(buffer: &[u8; GROUP_RECORD_LEN]) -> Result<Self> {
        if &buffer[0..4] != GROUP_MAGIC {
            return Err(PithosError::InvalidMagic);
        }
        Ok(Self {
            version: read_u16(buffer, 4),
            flags: read_u16(buffer, 6),
            group_id: read_u64(buffer, 8),
            codec_chain_id: read_u32(buffer, 16),
            chunk_count: read_u32(buffer, 20),
            uncompressed_len: read_u64(buffer, 24),
            compressed_len: read_u64(buffer, 32),
            descriptor_len: read_u32(buffer, 40),
            payload_crc32c: read_u32(buffer, 44),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathEncoding {
    Utf8,
    UnixBytes,
    WindowsUtf16Le,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathComponent {
    pub encoding: PathEncoding,
    pub bytes: Vec<u8>,
}

impl PathComponent {
    fn from_os_str(value: &std::ffi::OsStr) -> Self {
        if let Some(utf8) = value.to_str() {
            return Self {
                encoding: PathEncoding::Utf8,
                bytes: utf8.as_bytes().to_vec(),
            };
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self {
                encoding: PathEncoding::UnixBytes,
                bytes: value.as_bytes().to_vec(),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let bytes = value
                .encode_wide()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            Self {
                encoding: PathEncoding::WindowsUtf16Le,
                bytes,
            }
        }
    }

    fn to_os_string(&self) -> Result<std::ffi::OsString> {
        let value = match self.encoding {
            PathEncoding::Utf8 => {
                let text = std::str::from_utf8(&self.bytes)
                    .map_err(|_| PithosError::InvalidPathEncoding)?;
                if text.is_empty() || text.contains(['/', '\\']) || text == "." || text == ".." {
                    return Err(PithosError::UnsafePath);
                }
                std::ffi::OsString::from(text)
            }
            PathEncoding::UnixBytes => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    if self.bytes.is_empty()
                        || self.bytes.contains(&b'/')
                        || self.bytes.contains(&0)
                    {
                        return Err(PithosError::UnsafePath);
                    }
                    std::ffi::OsString::from_vec(self.bytes.clone())
                }
                #[cfg(not(unix))]
                {
                    return Err(PithosError::InvalidPathEncoding);
                }
            }
            PathEncoding::WindowsUtf16Le => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt;
                    if !self.bytes.len().is_multiple_of(2) {
                        return Err(PithosError::InvalidPathEncoding);
                    }
                    let wide = self
                        .bytes
                        .chunks_exact(2)
                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                        .collect::<Vec<_>>();
                    if wide.is_empty()
                        || wide.iter().any(|value| {
                            *value == 0 || *value == b'/' as u16 || *value == b'\\' as u16
                        })
                    {
                        return Err(PithosError::UnsafePath);
                    }
                    std::ffi::OsString::from_wide(&wide)
                }
                #[cfg(not(windows))]
                {
                    return Err(PithosError::InvalidPathEncoding);
                }
            }
        };
        validate_normal_component(&value)?;
        Ok(value)
    }
}

fn validate_normal_component(value: &std::ffi::OsStr) -> Result<()> {
    let mut components = Path::new(value).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(PithosError::UnsafePath);
    }
    #[cfg(windows)]
    if let Some(text) = value.to_str() {
        let trimmed = text.trim_end_matches(['.', ' ']);
        let stem = trimmed.split('.').next().unwrap_or_default();
        let upper = stem.to_ascii_uppercase();
        let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || upper.strip_prefix("COM").is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
            || upper.strip_prefix("LPT").is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            });
        if reserved || trimmed.len() != text.len() || text.contains(':') {
            return Err(PithosError::UnsafePath);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivePath {
    pub components: Vec<PathComponent>,
}

impl ArchivePath {
    pub fn from_relative(path: &Path) -> Result<Self> {
        let mut components = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(value) => components.push(PathComponent::from_os_str(value)),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(PithosError::UnsafePath);
                }
            }
        }
        if components.is_empty() {
            return Err(PithosError::UnsafePath);
        }
        Ok(Self { components })
    }

    pub fn to_path_buf(&self) -> Result<PathBuf> {
        if self.components.is_empty() {
            return Err(PithosError::UnsafePath);
        }
        let mut path = PathBuf::new();
        for component in &self.components {
            path.push(component.to_os_string()?);
        }
        Ok(path)
    }

    pub fn sort_key(&self) -> Vec<u8> {
        let mut key = Vec::new();
        for component in &self.components {
            key.push(component.encoding as u8);
            key.extend_from_slice(&(component.bytes.len() as u64).to_le_bytes());
            key.extend_from_slice(&component.bytes);
        }
        key
    }

    pub fn encoded_len(&self) -> u64 {
        self.components
            .iter()
            .map(|component| component.bytes.len() as u64)
            .sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkComponent {
    Parent,
    Normal(PathComponent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkTarget {
    pub components: Vec<LinkComponent>,
}

impl LinkTarget {
    pub fn from_relative(path: &Path) -> Result<Self> {
        let mut components = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(value) => {
                    components.push(LinkComponent::Normal(PathComponent::from_os_str(value)));
                }
                Component::ParentDir => components.push(LinkComponent::Parent),
                Component::CurDir => {}
                Component::RootDir | Component::Prefix(_) => {
                    return Err(PithosError::UnsafeSymlink);
                }
            }
        }
        if components.is_empty() {
            return Err(PithosError::UnsafeSymlink);
        }
        Ok(Self { components })
    }

    pub fn to_path_buf(&self) -> Result<PathBuf> {
        let mut path = PathBuf::new();
        for component in &self.components {
            match component {
                LinkComponent::Parent => path.push(".."),
                LinkComponent::Normal(value) => path.push(value.to_os_string()?),
            }
        }
        Ok(path)
    }

    pub fn resolves_within(&self, link_path: &ArchivePath) -> bool {
        let mut depth = link_path.components.len().saturating_sub(1);
        for component in &self.components {
            match component {
                LinkComponent::Parent => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                }
                LinkComponent::Normal(_) => depth += 1,
            }
        }
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    File {
        group_id: u64,
    },
    Directory,
    Hardlink {
        target_entry_id: u64,
    },
    Symlink {
        target: LinkTarget,
        target_is_dir: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRecord {
    pub entry_id: u64,
    pub path: ArchivePath,
    pub size: u64,
    pub modified_ns: i64,
    pub mode: u32,
    pub blake3: [u8; 32],
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupTableRecord {
    pub group: GroupRecord,
    pub payload_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreMapRecord {
    pub entry_id: u64,
    pub original_offset: u64,
    pub length: u64,
    pub group_id: u64,
    pub group_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CentralIndexRecord {
    pub path: ArchivePath,
    pub entry_id: u64,
    pub group_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityRecord {
    pub entry_id: u64,
    pub blake3: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    pub archive_length: u64,
    pub blake3_root: [u8; 32],
    pub directory_crc32c: u32,
    pub version: u32,
}

impl Footer {
    pub fn encode(&self) -> [u8; FOOTER_LEN] {
        let mut buffer = [0_u8; FOOTER_LEN];
        buffer[0..4].copy_from_slice(FOOTER_MAGIC);
        buffer[4..12].copy_from_slice(&self.archive_length.to_le_bytes());
        buffer[12..44].copy_from_slice(&self.blake3_root);
        buffer[44..48].copy_from_slice(&self.directory_crc32c.to_le_bytes());
        buffer[48..52].copy_from_slice(&self.version.to_le_bytes());
        let crc = footer_crc(&buffer);
        buffer[56..60].copy_from_slice(&crc.to_le_bytes());
        buffer
    }

    pub fn decode(buffer: &[u8; FOOTER_LEN]) -> Result<Self> {
        if &buffer[0..4] != FOOTER_MAGIC {
            return Err(PithosError::InvalidMagic);
        }
        if read_u32(buffer, 56) != footer_crc(buffer) {
            return Err(PithosError::ChecksumMismatch);
        }
        let mut root = [0_u8; 32];
        root.copy_from_slice(&buffer[12..44]);
        Ok(Self {
            archive_length: read_u64(buffer, 4),
            blake3_root: root,
            directory_crc32c: read_u32(buffer, 44),
            version: read_u32(buffer, 48),
        })
    }
}

fn footer_crc(buffer: &[u8; FOOTER_LEN]) -> u32 {
    let first = crc32c::crc32c(&buffer[..56]);
    crc32c::crc32c_append(first, &buffer[60..64])
}

fn read_u16(buffer: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buffer[offset], buffer[offset + 1]])
}

fn read_u32(buffer: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buffer[offset],
        buffer[offset + 1],
        buffer[offset + 2],
        buffer[offset + 3],
    ])
}

fn read_u64(buffer: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        buffer[offset],
        buffer[offset + 1],
        buffer[offset + 2],
        buffer[offset + 3],
        buffer[offset + 4],
        buffer[offset + 5],
        buffer[offset + 6],
        buffer[offset + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn header_roundtrip_covers_section_count_crc_and_version() {
        let mut header = GlobalHeader::new([7; 16]);
        header.entry_count = 42;
        let bytes = header.encode();
        assert_eq!(GlobalHeader::decode(&bytes).unwrap(), header);

        let mut corrupt = bytes;
        corrupt[92] ^= 1;
        assert!(matches!(
            GlobalHeader::decode(&corrupt),
            Err(PithosError::ChecksumMismatch)
        ));
    }

    #[test]
    fn footer_crc_covers_reserved_tail() {
        let footer = Footer {
            archive_length: 64,
            blake3_root: [9; 32],
            directory_crc32c: 7,
            version: 1,
        };
        let mut bytes = footer.encode();
        bytes[63] ^= 1;
        assert!(matches!(
            Footer::decode(&bytes),
            Err(PithosError::ChecksumMismatch)
        ));
    }

    #[test]
    fn archive_path_rejects_parent_and_absolute_components() {
        assert!(ArchivePath::from_relative(Path::new("../escape")).is_err());
        assert!(ArchivePath::from_relative(Path::new("/absolute")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn archive_path_rejects_windows_prefixes_and_device_names() {
        let prefixed = ArchivePath {
            components: vec![PathComponent {
                encoding: PathEncoding::Utf8,
                bytes: b"C:escape".to_vec(),
            }],
        };
        let device = ArchivePath {
            components: vec![PathComponent {
                encoding: PathEncoding::Utf8,
                bytes: b"CON.txt".to_vec(),
            }],
        };
        assert!(prefixed.to_path_buf().is_err());
        assert!(device.to_path_buf().is_err());
    }

    #[test]
    fn link_target_cannot_resolve_above_archive_root() {
        let link = ArchivePath::from_relative(Path::new("nested/link")).unwrap();
        let safe = LinkTarget::from_relative(Path::new("../target")).unwrap();
        let unsafe_target = LinkTarget::from_relative(Path::new("../../target")).unwrap();
        assert!(safe.resolves_within(&link));
        assert!(!unsafe_target.resolves_within(&link));
    }

    #[test]
    fn codec_registry_roundtrips_stable_chain_records() {
        let registry = CodecRegistry {
            records: vec![
                CodecRegistryRecord {
                    chain_id: 1,
                    codec_id: 1,
                    codec_version: 1,
                    level: 9,
                    flags: CODEC_FLAG_REQUIRED,
                },
                CodecRegistryRecord {
                    chain_id: 2,
                    codec_id: 2,
                    codec_version: 1,
                    level: 7,
                    flags: CODEC_FLAG_REQUIRED,
                },
            ],
        };
        let encoded = registry.encode().unwrap();
        assert_eq!(CodecRegistry::decode(&encoded, 8).unwrap(), registry);
    }

    #[test]
    fn codec_registry_rejects_unknown_required_codec() {
        let registry = CodecRegistry {
            records: vec![CodecRegistryRecord {
                chain_id: 1,
                codec_id: 99,
                codec_version: 1,
                level: 0,
                flags: CODEC_FLAG_REQUIRED,
            }],
        };
        let encoded = registry.encode().unwrap();
        assert!(matches!(
            CodecRegistry::decode(&encoded, 8),
            Err(PithosError::UnsupportedCodec)
        ));
    }

    #[test]
    fn codec_registry_rejects_duplicate_chains_and_declared_size_abuse() {
        let duplicate = CodecRegistry {
            records: vec![
                CodecRegistryRecord::store(1),
                CodecRegistryRecord::store(1),
            ],
        };
        assert!(duplicate.encode().is_err());

        let mut oversized = 9_u32.to_le_bytes().to_vec();
        oversized.resize(4 + 9 * CODEC_REGISTRY_RECORD_LEN, 0);
        assert!(matches!(
            CodecRegistry::decode(&oversized, 8),
            Err(PithosError::ResourceLimit(_))
        ));
        assert!(CodecRegistry::decode(&[1, 0, 0, 0], 8).is_err());
    }

    proptest! {
        #[test]
        fn fixed_record_decoders_never_panic(
            header in any::<[u8; HEADER_LEN]>(),
            group in any::<[u8; GROUP_RECORD_LEN]>(),
            footer in any::<[u8; FOOTER_LEN]>(),
        ) {
            let _ = GlobalHeader::decode(&header);
            let _ = GroupRecord::decode(&group);
            let _ = Footer::decode(&footer);
        }
    }
}
