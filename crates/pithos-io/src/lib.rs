//! Pithos I/O & Path Handling Utilities

use pithos_core::{PithosError, Result};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// Sanitiza e valida caminhos de arquivos para evitar Path Traversal.
pub fn sanitize_relative_path(path: &Path) -> Result<PathBuf> {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(c) => clean.push(c),
            std::path::Component::ParentDir => return Err(PithosError::UnsafePath),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(PithosError::UnsafePath);
            }
            std::path::Component::CurDir => {}
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(PithosError::UnsafePath);
    }
    Ok(clean)
}

/// Cria um spool temporário no mesmo diretório de destino quando possível.
pub fn create_atomic_spool(dest_dir: &Path) -> Result<NamedTempFile> {
    std::fs::create_dir_all(dest_dir)?;
    NamedTempFile::new_in(dest_dir).map_err(PithosError::Io)
}

/// Realiza o commit atômico substituindo o destino de forma segura.
pub fn atomic_commit(temp_file: NamedTempFile, target_path: &Path) -> Result<()> {
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    temp_file.as_file().sync_all()?;
    let file = temp_file.persist_noclobber(target_path).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            PithosError::OutputExists
        } else {
            PithosError::Io(error.error)
        }
    })?;
    file.sync_all()?;
    sync_parent(target_path)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}
