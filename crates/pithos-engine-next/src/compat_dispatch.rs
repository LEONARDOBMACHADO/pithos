use crate::{native_archive, native_verify};
use pithos_core::{DecodeLimits, Result};
use pithos_engine_legacy::{
    ArchiveEntryKind, ArchiveEntrySummary, ArchiveInspection, CancellationToken, UnpackRequest,
    VerificationReport,
};
use pithos_format::{EntryKind, GlobalHeader, HEADER_LEN, REQUIRED_RAW_SECTIONS};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

fn requires_native_reader(archive: &Path) -> bool {
    let Ok(mut file) = File::open(archive) else {
        return false;
    };
    let mut bytes = [0_u8; HEADER_LEN];
    if file.read_exact(&mut bytes).is_err() {
        return false;
    }
    let Ok(header) = GlobalHeader::decode(&bytes) else {
        return false;
    };
    header.section_count > REQUIRED_RAW_SECTIONS
}

pub fn verify(archive: &Path) -> Result<VerificationReport> {
    verify_with_limits(archive, &DecodeLimits::default())
}
pub fn verify_with_limits(archive: &Path, limits: &DecodeLimits) -> Result<VerificationReport> {
    verify_with_control(archive, limits, &CancellationToken::new())
}
pub fn verify_with_control(
    archive: &Path,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<VerificationReport> {
    if requires_native_reader(archive) {
        native_verify::verify_with_control(archive, limits, cancellation)
    } else {
        pithos_engine_legacy::verify_with_control(archive, limits, cancellation)
    }
}
pub fn list(archive: &Path) -> Result<Vec<ArchiveEntrySummary>> {
    list_with_control(archive, &DecodeLimits::default(), &CancellationToken::new())
}
pub fn list_with_control(
    archive: &Path,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<Vec<ArchiveEntrySummary>> {
    if !requires_native_reader(archive) {
        return pithos_engine_legacy::list_with_control(archive, limits, cancellation);
    }
    let catalog = native_archive::read_catalog(archive, limits, cancellation)?;
    catalog.entries.iter().map(entry_summary).collect()
}
pub fn inspect(archive: &Path) -> Result<ArchiveInspection> {
    inspect_with_control(archive, &DecodeLimits::default(), &CancellationToken::new())
}
pub fn inspect_with_control(
    archive: &Path,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<ArchiveInspection> {
    if !requires_native_reader(archive) {
        return pithos_engine_legacy::inspect_with_control(archive, limits, cancellation);
    }
    let catalog = native_archive::read_catalog(archive, limits, cancellation)?;
    let (mut file_count, mut directory_count, mut hardlink_count, mut symlink_count) =
        (0_u64, 0_u64, 0_u64, 0_u64);
    for entry in &catalog.entries {
        match entry.kind {
            EntryKind::File { .. } => file_count += 1,
            EntryKind::Directory => directory_count += 1,
            EntryKind::Hardlink { .. } => hardlink_count += 1,
            EntryKind::Symlink { .. } => symlink_count += 1,
        }
    }
    Ok(ArchiveInspection {
        archive_bytes: fs::metadata(archive)?.len(),
        original_bytes: catalog.header.original_total_size,
        entry_count: catalog.entries.len() as u64,
        file_count,
        directory_count,
        hardlink_count,
        symlink_count,
        group_count: catalog.groups.len() as u64,
        format_version: "PAF 0.1-draft".to_owned(),
        metadata_verified: true,
    })
}
pub fn unpack(request: UnpackRequest) -> Result<()> {
    unpack_with_control(request, &DecodeLimits::default(), &CancellationToken::new())
}
pub fn unpack_with_control(
    request: UnpackRequest,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<()> {
    unpack_with_control_and_temp_limit(request, limits, limits.max_original_bytes, cancellation)
}
pub fn unpack_with_control_and_temp_limit(
    request: UnpackRequest,
    limits: &DecodeLimits,
    max_temp_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<()> {
    if requires_native_reader(&request.archive) {
        native_archive::unpack_with_control_and_temp_limit(
            request,
            limits,
            max_temp_bytes,
            cancellation,
        )
    } else {
        pithos_engine_legacy::unpack_with_control_and_temp_limit(
            request,
            limits,
            max_temp_bytes,
            cancellation,
        )
    }
}
fn entry_summary(entry: &pithos_format::EntryRecord) -> Result<ArchiveEntrySummary> {
    let kind = match entry.kind {
        EntryKind::File { .. } => ArchiveEntryKind::File,
        EntryKind::Directory => ArchiveEntryKind::Directory,
        EntryKind::Hardlink { .. } => ArchiveEntryKind::Hardlink,
        EntryKind::Symlink { .. } => ArchiveEntryKind::Symlink,
    };
    Ok(ArchiveEntrySummary {
        entry_id: entry.entry_id,
        path: entry.path.to_path_buf()?.to_string_lossy().into_owned(),
        kind,
        size: entry.size,
        modified_ns: entry.modified_ns,
    })
}
