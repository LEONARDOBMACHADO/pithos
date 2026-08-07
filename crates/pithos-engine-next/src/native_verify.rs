use crate::native_archive;
use pithos_core::{DecodeLimits, PithosError, Result};
use pithos_engine_legacy::{CancellationToken, VerificationReport};
use pithos_format::{FOOTER_LEN, Footer};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

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
    let mut report = native_archive::verify_with_control(archive, limits, cancellation)?;
    let mut file = File::open(archive)?;
    let length = file.metadata()?.len();
    if length < FOOTER_LEN as u64 {
        return Err(PithosError::InvalidRange);
    }
    file.seek(SeekFrom::Start(length - FOOTER_LEN as u64))?;
    let mut bytes = [0_u8; FOOTER_LEN];
    file.read_exact(&mut bytes)?;
    report.blake3_root = Footer::decode(&bytes)?.blake3_root;
    Ok(report)
}
