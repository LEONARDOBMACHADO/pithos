use crate::native_archive;
use pithos_core::{DecodeLimits, PithosError, Result};
use pithos_engine_legacy::{CancellationToken, UnpackRequest, VerificationReport};
use pithos_format::{EntryKind, EntryRecord, FOOTER_LEN, Footer, RestoreMapRecord};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub(crate) fn verify_with_control(
    archive: &Path,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<VerificationReport> {
    checkpoint(cancellation)?;
    let catalog = native_archive::read_catalog(archive, limits, cancellation)?;
    let restore_by_entry = catalog
        .restore_map
        .iter()
        .map(|restore| (restore.entry_id, restore))
        .collect::<HashMap<_, _>>();

    catalog.groups.par_iter().try_for_each(|group| -> Result<()> {
        checkpoint(cancellation)?;
        let mut file = File::open(archive)?;
        let decoded = native_archive::decode_group(
            &mut file,
            group,
            &catalog.registry,
            limits,
            cancellation,
        )?;
        for entry in catalog.entries.iter().filter(|entry| {
            matches!(entry.kind, EntryKind::File { group_id } if group_id == group.group.group_id)
        }) {
            let restore = restore_by_entry
                .get(&entry.entry_id)
                .copied()
                .ok_or(PithosError::InvalidMetadata("restore entry"))?;
            let bytes = restored_slice(&decoded, restore, 0, restore.length)?;
            if blake3::hash(bytes).as_bytes() != &entry.blake3 {
                return Err(PithosError::HashMismatch);
            }
        }
        Ok(())
    })?;

    let root = read_footer_root(archive)?;
    Ok(report(&catalog.entries, catalog.header.original_total_size, catalog.groups.len(), fs::metadata(archive)?.len(), root))
}

pub(crate) fn unpack_with_control_and_temp_limit(
    request: UnpackRequest,
    limits: &DecodeLimits,
    max_temp_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<()> {
    checkpoint(cancellation)?;
    if path_entry_exists(&request.output_dir)? {
        return Err(PithosError::OutputExists);
    }
    let catalog = native_archive::read_catalog(&request.archive, limits, cancellation)?;
    if catalog.header.original_total_size > max_temp_bytes {
        return Err(PithosError::ResourceLimit("unpack temporary bytes"));
    }

    let output_parent = request
        .output_dir
        .parent()
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(output_parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".pithos-unpack-par-")
        .tempdir_in(output_parent)?;

    for entry in catalog
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Directory))
    {
        checkpoint(cancellation)?;
        fs::create_dir_all(staging.path().join(entry.path.to_path_buf()?))?;
    }

    let restore_by_entry = catalog
        .restore_map
        .iter()
        .map(|restore| (restore.entry_id, restore))
        .collect::<HashMap<_, _>>();
    let archive_path = &request.archive;
    let staging_path = staging.path();

    catalog.groups.par_iter().try_for_each(|group| -> Result<()> {
        checkpoint(cancellation)?;
        let mut archive = File::open(archive_path)?;
        let decoded = native_archive::decode_group(
            &mut archive,
            group,
            &catalog.registry,
            limits,
            cancellation,
        )?;
        for entry in catalog.entries.iter().filter(|entry| {
            matches!(entry.kind, EntryKind::File { group_id } if group_id == group.group.group_id)
        }) {
            let restore = restore_by_entry
                .get(&entry.entry_id)
                .copied()
                .ok_or(PithosError::InvalidMetadata("restore entry"))?;
            let bytes = restored_slice(&decoded, restore, 0, restore.length)?;
            if blake3::hash(bytes).as_bytes() != &entry.blake3 {
                return Err(PithosError::HashMismatch);
            }
            let destination = staging_path.join(entry.path.to_path_buf()?);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            output.write_all(bytes)?;
            output.sync_all()?;
            apply_file_mode(&destination, entry.mode)?;
        }
        Ok(())
    })?;

    for entry in catalog
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Hardlink { .. }))
    {
        let EntryKind::Hardlink { target_entry_id } = entry.kind else {
            unreachable!();
        };
        let target = catalog
            .entries
            .get(target_entry_id as usize)
            .ok_or(PithosError::InvalidMetadata("hardlink target"))?;
        let destination = staging.path().join(entry.path.to_path_buf()?);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::hard_link(staging.path().join(target.path.to_path_buf()?), destination)?;
    }

    for entry in catalog
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Symlink { .. }))
    {
        let EntryKind::Symlink {
            ref target,
            target_is_dir,
        } = entry.kind
        else {
            unreachable!();
        };
        if !target.resolves_within(&entry.path) {
            return Err(PithosError::UnsafeSymlink);
        }
        let destination = staging.path().join(entry.path.to_path_buf()?);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        create_symlink(&target.to_path_buf()?, &destination, target_is_dir)?;
    }

    checkpoint(cancellation)?;
    fs::rename(staging.path(), &request.output_dir)?;
    sync_parent(&request.output_dir)?;
    Ok(())
}

fn report(
    entries: &[EntryRecord],
    original_bytes: u64,
    group_count: usize,
    archive_bytes: u64,
    blake3_root: [u8; 32],
) -> VerificationReport {
    let mut file_count = 0_u64;
    let mut directory_count = 0_u64;
    let mut hardlink_count = 0_u64;
    let mut symlink_count = 0_u64;
    for entry in entries {
        match entry.kind {
            EntryKind::File { .. } => file_count += 1,
            EntryKind::Directory => directory_count += 1,
            EntryKind::Hardlink { .. } => hardlink_count += 1,
            EntryKind::Symlink { .. } => symlink_count += 1,
        }
    }
    VerificationReport {
        archive_bytes,
        original_bytes,
        entry_count: entries.len() as u64,
        file_count,
        directory_count,
        hardlink_count,
        symlink_count,
        group_count: group_count as u64,
        blake3_root,
    }
}

fn read_footer_root(archive: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(archive)?;
    let length = file.metadata()?.len();
    if length < FOOTER_LEN as u64 {
        return Err(PithosError::InvalidRange);
    }
    file.seek(SeekFrom::Start(length - FOOTER_LEN as u64))?;
    let mut bytes = [0_u8; FOOTER_LEN];
    file.read_exact(&mut bytes)?;
    Ok(Footer::decode(&bytes)?.blake3_root)
}

fn restored_slice<'a>(
    decoded: &'a [u8],
    restore: &RestoreMapRecord,
    offset: u64,
    length: u64,
) -> Result<&'a [u8]> {
    let start = restore
        .group_offset
        .checked_add(offset)
        .ok_or(PithosError::IntegerOverflow)?;
    let end = start
        .checked_add(length)
        .ok_or(PithosError::IntegerOverflow)?;
    let start = usize::try_from(start).map_err(|_| PithosError::IntegerOverflow)?;
    let end = usize::try_from(end).map_err(|_| PithosError::IntegerOverflow)?;
    decoded.get(start..end).ok_or(PithosError::InvalidRange)
}

fn checkpoint(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        Err(PithosError::Cancelled)
    } else {
        Ok(())
    }
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn apply_file_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_file_mode(path: &Path, mode: u32) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path, _target_is_dir: bool) -> Result<()> {
    std::os::unix::fs::symlink(target, destination)?;
    Ok(())
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path, target_is_dir: bool) -> Result<()> {
    if target_is_dir {
        std::os::windows::fs::symlink_dir(target, destination)?;
    } else {
        std::os::windows::fs::symlink_file(target, destination)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}
