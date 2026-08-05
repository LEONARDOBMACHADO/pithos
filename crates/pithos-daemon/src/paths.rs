use pithos_agent_api::{JsonRpcError, PathScope, PublicErrorKind};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PathAuthorizer {
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
}

impl PathAuthorizer {
    pub fn new(scope: &PathScope) -> Result<Self, JsonRpcError> {
        if scope.read_roots.is_empty() && scope.write_roots.is_empty() {
            return Err(permission_denied());
        }
        Ok(Self {
            read_roots: canonical_roots(&scope.read_roots)?,
            write_roots: canonical_roots(&scope.write_roots)?,
        })
    }

    pub fn authorize_read(&self, path: &Path) -> Result<PathBuf, JsonRpcError> {
        reject_unsafe_syntax(path)?;
        let canonical = fs::canonicalize(path).map_err(|_| permission_denied())?;
        if !self
            .read_roots
            .iter()
            .any(|root| canonical.starts_with(root))
        {
            return Err(permission_denied());
        }
        reject_symlink_components(&canonical, &self.read_roots)?;
        Ok(canonical)
    }

    pub fn authorize_write(&self, path: &Path) -> Result<PathBuf, JsonRpcError> {
        reject_unsafe_syntax(path)?;
        let parent = path.parent().ok_or_else(permission_denied)?;
        let canonical_parent = canonicalize_nearest_parent(parent)?;
        if !self
            .write_roots
            .iter()
            .any(|root| canonical_parent.starts_with(root))
        {
            return Err(permission_denied());
        }
        reject_symlink_components(&canonical_parent, &self.write_roots)?;
        let name = path.file_name().ok_or_else(permission_denied)?;
        Ok(canonical_parent.join(name))
    }

    pub fn scope(&self) -> PathScope {
        PathScope {
            read_roots: self.read_roots.clone(),
            write_roots: self.write_roots.clone(),
        }
    }

    pub fn grant(&self, requested: &PathScope) -> Result<Self, JsonRpcError> {
        let requested = Self::new(requested)?;
        if requested.read_roots.iter().any(|root| {
            !self
                .read_roots
                .iter()
                .any(|allowed| root.starts_with(allowed))
        }) || requested.write_roots.iter().any(|root| {
            !self
                .write_roots
                .iter()
                .any(|allowed| root.starts_with(allowed))
        }) {
            return Err(permission_denied());
        }
        Ok(requested)
    }

    pub fn revalidate_read(path: &Path) -> Result<(), JsonRpcError> {
        reject_unsafe_syntax(path)?;
        let metadata = fs::symlink_metadata(path).map_err(|_| permission_denied())?;
        if metadata.file_type().is_symlink() {
            return Err(permission_denied());
        }
        let current = fs::canonicalize(path).map_err(|_| permission_denied())?;
        if current != path {
            return Err(permission_denied());
        }
        Ok(())
    }

    pub fn revalidate_write(path: &Path) -> Result<(), JsonRpcError> {
        reject_unsafe_syntax(path)?;
        let parent = path.parent().ok_or_else(permission_denied)?;
        let current_parent = canonicalize_nearest_parent(parent)?;
        if current_parent != parent {
            return Err(permission_denied());
        }
        if let Ok(metadata) = fs::symlink_metadata(path)
            && metadata.file_type().is_symlink()
        {
            return Err(permission_denied());
        }
        Ok(())
    }
}

fn canonical_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>, JsonRpcError> {
    if roots.len() > 64 {
        return Err(JsonRpcError::resource_limit("too many path roots"));
    }
    roots
        .iter()
        .map(|root| {
            reject_unsafe_syntax(root)?;
            let metadata = fs::symlink_metadata(root).map_err(|_| permission_denied())?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(permission_denied());
            }
            fs::canonicalize(root).map_err(|_| permission_denied())
        })
        .collect()
}

fn reject_unsafe_syntax(path: &Path) -> Result<(), JsonRpcError> {
    if path.as_os_str().is_empty() || path.as_os_str().to_string_lossy().len() > 32 * 1024 {
        return Err(permission_denied());
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(JsonRpcError::domain(
                PublicErrorKind::UnsafePath,
                "unsafe path",
            ));
        }
    }
    Ok(())
}

fn canonicalize_nearest_parent(path: &Path) -> Result<PathBuf, JsonRpcError> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(permission_denied());
                }
                let mut canonical = fs::canonicalize(current).map_err(|_| permission_denied())?;
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let name = current.file_name().ok_or_else(permission_denied)?;
                missing.push(name.to_os_string());
                current = current.parent().ok_or_else(permission_denied)?;
            }
            Err(_) => return Err(permission_denied()),
        }
    }
}

fn reject_symlink_components(path: &Path, roots: &[PathBuf]) -> Result<(), JsonRpcError> {
    let root = roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .ok_or_else(permission_denied)?;
    let mut current = root.clone();
    let relative = path.strip_prefix(root).map_err(|_| permission_denied())?;
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(permission_denied()),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(_) => return Err(permission_denied()),
        }
    }
    Ok(())
}

fn permission_denied() -> JsonRpcError {
    JsonRpcError::domain(
        PublicErrorKind::PermissionDenied,
        "path is outside the allowed scope",
    )
}
