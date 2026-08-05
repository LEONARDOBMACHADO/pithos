use parking_lot::Mutex;
use pithos_agent_api::{JsonRpcError, PathScope, PublicErrorKind, SessionId};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

const SESSION_SCHEMA_VERSION: u16 = 2;
const MAX_SESSION_STORE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SESSIONS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    pub session_id: SessionId,
    pub resume_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumedSession {
    pub scope: PathScope,
    pub resume_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSession {
    resume_token_hash: [u8; 32],
    scope: PathScope,
    expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistentSessions {
    schema_version: u16,
    sessions: BTreeMap<String, StoredSession>,
}

impl Default for PersistentSessions {
    fn default() -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            sessions: BTreeMap::new(),
        }
    }
}

/// Durable, local-only session identities. Capability tokens remain connection-bound;
/// only a one-way hash of the separate resume token is persisted.
pub struct SessionRegistry {
    path: PathBuf,
    state: Mutex<PersistentSessions>,
    mutation: Mutex<()>,
}

impl SessionRegistry {
    pub fn open(path: PathBuf) -> Result<Self, JsonRpcError> {
        let state = load_state(&path)?;
        if state.schema_version != SESSION_SCHEMA_VERSION {
            return Err(internal("unsupported session store schema"));
        }
        Ok(Self {
            path,
            state: Mutex::new(state),
            mutation: Mutex::new(()),
        })
    }

    pub fn create(
        &self,
        scope: PathScope,
        now_unix_ms: u64,
        expires_at_unix_ms: u64,
    ) -> Result<CreatedSession, JsonRpcError> {
        validate_expiry(now_unix_ms, expires_at_unix_ms)?;
        let _mutation = self.mutation.lock();
        let mut next = self.state.lock().clone();
        next.sessions
            .retain(|_, session| session.expires_at_unix_ms > now_unix_ms);
        if next.sessions.len() >= MAX_SESSIONS {
            return Err(JsonRpcError::resource_limit("session registry is full"));
        }
        let (session_id, resume_token) = loop {
            let session_id = SessionId::new(format!("session-{}", random_hex(16)))?;
            if !next.sessions.contains_key(session_id.as_str()) {
                break (session_id, random_hex(32));
            }
        };
        next.sessions.insert(
            session_id.as_str().to_owned(),
            StoredSession {
                resume_token_hash: *blake3::hash(resume_token.as_bytes()).as_bytes(),
                scope,
                expires_at_unix_ms,
            },
        );
        self.commit(next)?;
        Ok(CreatedSession {
            session_id,
            resume_token,
        })
    }

    pub fn resume(
        &self,
        session_id: &SessionId,
        resume_token: &str,
        now_unix_ms: u64,
        expires_at_unix_ms: u64,
    ) -> Result<ResumedSession, JsonRpcError> {
        validate_expiry(now_unix_ms, expires_at_unix_ms)?;
        if resume_token.len() != 64 || !resume_token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(permission_denied());
        }
        let _mutation = self.mutation.lock();
        let mut next = self.state.lock().clone();
        let stored = next
            .sessions
            .get_mut(session_id.as_str())
            .ok_or_else(permission_denied)?;
        if stored.expires_at_unix_ms <= now_unix_ms {
            next.sessions.remove(session_id.as_str());
            self.commit(next)?;
            return Err(permission_denied());
        }
        let supplied = blake3::hash(resume_token.as_bytes());
        if !constant_time_equal(&stored.resume_token_hash, supplied.as_bytes()) {
            return Err(permission_denied());
        }
        let scope = stored.scope.clone();
        let rotated_resume_token = random_hex(32);
        stored.resume_token_hash = *blake3::hash(rotated_resume_token.as_bytes()).as_bytes();
        stored.expires_at_unix_ms = expires_at_unix_ms;
        self.commit(next)?;
        Ok(ResumedSession {
            scope,
            resume_token: rotated_resume_token,
        })
    }

    fn commit(&self, next: PersistentSessions) -> Result<(), JsonRpcError> {
        let bytes = serde_json::to_vec(&next).map_err(|_| internal("cannot encode sessions"))?;
        if bytes.len() as u64 > MAX_SESSION_STORE_BYTES {
            return Err(JsonRpcError::resource_limit("session registry is full"));
        }
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|_| internal("cannot create session directory"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|_| internal("cannot create session transaction"))?;
        temporary
            .write_all(&bytes)
            .and_then(|_| temporary.flush())
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|_| internal("cannot sync session transaction"))?;
        temporary
            .persist(&self.path)
            .map_err(|_| internal("cannot publish session registry"))?;

        // Once rename/persist succeeds, disk contains `next`. Keep live state aligned even
        // when the following directory durability barrier reports an error.
        *self.state.lock() = next;
        sync_parent(parent)
    }
}

fn load_state(path: &Path) -> Result<PersistentSessions, JsonRpcError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(PersistentSessions::default());
        }
        Err(_) => return Err(internal("cannot open session registry")),
    };
    let length = file
        .metadata()
        .map_err(|_| internal("cannot inspect session registry"))?
        .len();
    if length > MAX_SESSION_STORE_BYTES {
        return Err(JsonRpcError::resource_limit(
            "session registry is too large",
        ));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes)
        .map_err(|_| internal("cannot read session registry"))?;
    let state: PersistentSessions =
        serde_json::from_slice(&bytes).map_err(|_| internal("session registry is corrupt"))?;
    if state.sessions.len() > MAX_SESSIONS {
        return Err(JsonRpcError::resource_limit(
            "session registry is too large",
        ));
    }
    Ok(state)
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<(), JsonRpcError> {
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|_| internal("cannot sync session directory"))
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<(), JsonRpcError> {
    Ok(())
}

fn random_hex(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    rand::rng().fill(value.as_mut_slice());
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
}

fn validate_expiry(now_unix_ms: u64, expires_at_unix_ms: u64) -> Result<(), JsonRpcError> {
    if expires_at_unix_ms <= now_unix_ms {
        return Err(JsonRpcError::invalid_params("invalid session expiration"));
    }
    Ok(())
}

fn permission_denied() -> JsonRpcError {
    JsonRpcError::domain(PublicErrorKind::PermissionDenied, "permission denied")
}

fn internal(message: &'static str) -> JsonRpcError {
    JsonRpcError::domain(PublicErrorKind::Internal, message)
}
