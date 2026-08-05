use parking_lot::Mutex;
use pithos_agent_api::{
    ApiJobState, ApiProfile, JobAccepted, JobId, JobLimits, JobPriority, JobSnapshot, JsonRpcError,
    ProgressEvent, PublicErrorKind, RpcMethod, SessionId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const STORE_SCHEMA_VERSION: u16 = 1;
const MAX_STORE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
const MAX_EVENTS_PER_JOB: usize = 4096;
const MAX_RETAINED_JOBS: usize = 4096;
const PACK_PUBLICATION_MARKER_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy)]
struct StorePolicy {
    max_store_bytes: u64,
    max_events_per_job: usize,
    max_retained_jobs: usize,
}

impl Default for StorePolicy {
    fn default() -> Self {
        Self {
            max_store_bytes: MAX_STORE_BYTES,
            max_events_per_job: MAX_EVENTS_PER_JOB,
            max_retained_jobs: MAX_RETAINED_JOBS,
        }
    }
}

impl StorePolicy {
    fn validate(self) -> Result<Self, JsonRpcError> {
        if self.max_store_bytes == 0 || self.max_events_per_job == 0 || self.max_retained_jobs == 0
        {
            return Err(internal("invalid job store policy"));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredOperation {
    Pack {
        inputs: Vec<PathBuf>,
        output: PathBuf,
        profile: ApiProfile,
    },
    Unpack {
        archive: PathBuf,
        output: PathBuf,
    },
    List {
        archive: PathBuf,
    },
    Inspect {
        archive: PathBuf,
    },
    Extract {
        archive: PathBuf,
        entry: PathBuf,
        output: PathBuf,
    },
    ReadRange {
        archive: PathBuf,
        entry: PathBuf,
        offset: u64,
        length: u64,
    },
    Verify {
        archive: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct JobSubmission {
    pub owner: SessionId,
    pub method: RpcMethod,
    pub priority: JobPriority,
    pub idempotency_key: String,
    pub params_hash: [u8; 32],
    pub limits: JobLimits,
    pub operation: StoredOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredJob {
    job_id: JobId,
    owner: SessionId,
    method: RpcMethod,
    #[serde(default)]
    priority: JobPriority,
    idempotency_key: String,
    params_hash: [u8; 32],
    limits: JobLimits,
    operation: StoredOperation,
    state: ApiJobState,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    result: Option<Value>,
    error: Option<JsonRpcError>,
    events: Vec<ProgressEvent>,
    next_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_publication: Option<PackPublicationMarker>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackPublicationMarker {
    marker_version: u8,
    job_id: JobId,
    params_hash: [u8; 32],
    output: PathBuf,
    output_was_absent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    published_archive: Option<ArchiveIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArchiveIdentity {
    length: u64,
    blake3: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistentState {
    schema_version: u16,
    next_job: u64,
    jobs: BTreeMap<String, StoredJob>,
    idempotency: BTreeMap<String, String>,
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            schema_version: STORE_SCHEMA_VERSION,
            next_job: 1,
            jobs: BTreeMap::new(),
            idempotency: BTreeMap::new(),
        }
    }
}

pub struct JobRegistry {
    path: PathBuf,
    state: Mutex<PersistentState>,
    mutation: Mutex<()>,
    policy: StorePolicy,
    closed: AtomicBool,
    healthy: AtomicBool,
}

impl JobRegistry {
    pub fn open(path: PathBuf) -> Result<Self, JsonRpcError> {
        Self::open_with_policy(path, StorePolicy::default())
    }

    pub(crate) fn open_with_event_limit(
        path: PathBuf,
        max_events_per_job: usize,
    ) -> Result<Self, JsonRpcError> {
        Self::open_with_policy(
            path,
            StorePolicy {
                max_events_per_job,
                ..StorePolicy::default()
            },
        )
    }

    fn open_with_policy(path: PathBuf, policy: StorePolicy) -> Result<Self, JsonRpcError> {
        let policy = policy.validate()?;
        let mut state = load_state_with_policy(&path, policy)?;
        validate_state(&state, policy, false)?;
        let mut changed = trim_retained_events(&mut state, policy);
        for job in state.jobs.values_mut() {
            if !job.state.is_terminal() {
                let recovered_archive = recover_pack_archive(job);
                if let Some((archive, identity)) = recovered_archive {
                    if let Some(marker) = job.pack_publication.as_mut() {
                        marker.published_archive = Some(identity);
                    }
                    job.state = ApiJobState::Completed;
                    job.result = Some(serde_json::json!({"archive": archive}));
                    job.error = None;
                    append_event(job, "recovered_completed", policy.max_events_per_job)?;
                } else {
                    job.state = ApiJobState::Failed;
                    job.result = None;
                    job.error = Some(JsonRpcError::domain(
                        PublicErrorKind::Internal,
                        "job interrupted by daemon restart",
                    ));
                    append_event(job, "daemon_restarted", policy.max_events_per_job)?;
                }
                changed = true;
            }
        }
        changed |= trim_retained_events(&mut state, policy);
        validate_state(&state, policy, true)?;
        if changed {
            persist_state_with_policy(&path, &state, policy)?;
        }
        Ok(Self {
            path,
            state: Mutex::new(state),
            mutation: Mutex::new(()),
            policy,
            closed: AtomicBool::new(false),
            healthy: AtomicBool::new(true),
        })
    }

    pub(crate) fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn submit(&self, submission: JobSubmission) -> Result<JobAccepted, JsonRpcError> {
        self.submit_with_limit(submission, usize::MAX)
    }

    pub fn submit_with_limit(
        &self,
        submission: JobSubmission,
        max_nonterminal_for_owner: usize,
    ) -> Result<JobAccepted, JsonRpcError> {
        let _mutation = self.mutation.lock();
        if self.closed.load(Ordering::Acquire) {
            return Err(JsonRpcError::domain(
                PublicErrorKind::JobConflict,
                "job registry is closed",
            ));
        }
        if !self.is_healthy() {
            return Err(internal("job registry storage is unhealthy"));
        }
        validate_idempotency_key(&submission.idempotency_key)?;
        let mut next = self.state.lock().clone();
        let key = idempotency_map_key(&submission.owner, &submission.idempotency_key);
        if let Some(existing_id) = next.idempotency.get(&key) {
            let existing = next
                .jobs
                .get(existing_id)
                .ok_or_else(|| internal("idempotency index is corrupt"))?;
            if existing.params_hash != submission.params_hash
                || existing.method != submission.method
                || existing.priority != submission.priority
            {
                return Err(JsonRpcError::domain(
                    PublicErrorKind::JobConflict,
                    "idempotency key has different parameters",
                ));
            }
            return Ok(JobAccepted {
                job_id: existing.job_id.clone(),
                state: existing.state,
                idempotent_replay: true,
            });
        }
        if next.jobs.len() >= self.policy.max_retained_jobs {
            return Err(JsonRpcError::resource_limit(
                "job retention capacity exceeded",
            ));
        }
        if next
            .jobs
            .values()
            .filter(|job| job.owner == submission.owner && !job.state.is_terminal())
            .count()
            >= max_nonterminal_for_owner
        {
            return Err(JsonRpcError::resource_limit("session job quota exceeded"));
        }
        let job_id = JobId::new(format!("job-{:016x}", next.next_job))?;
        next.next_job = next
            .next_job
            .checked_add(1)
            .ok_or_else(|| JsonRpcError::resource_limit("job id space exhausted"))?;
        let now = unix_ms();
        let mut job = StoredJob {
            job_id: job_id.clone(),
            owner: submission.owner,
            method: submission.method,
            priority: submission.priority,
            idempotency_key: submission.idempotency_key,
            params_hash: submission.params_hash,
            limits: submission.limits,
            operation: submission.operation,
            state: ApiJobState::Queued,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            result: None,
            error: None,
            events: Vec::new(),
            next_sequence: 1,
            pack_publication: None,
        };
        append_event(&mut job, "queued", self.policy.max_events_per_job)?;
        next.idempotency.insert(key, job_id.as_str().to_owned());
        next.jobs.insert(job_id.as_str().to_owned(), job);
        self.commit(next)?;
        Ok(JobAccepted {
            job_id,
            state: ApiJobState::Queued,
            idempotent_replay: false,
        })
    }

    pub fn transition(
        &self,
        owner: &SessionId,
        job_id: &JobId,
        state: ApiJobState,
        phase: &str,
    ) -> Result<(), JsonRpcError> {
        self.update_owned(owner, job_id, |job| {
            if !valid_transition(job.state, state) {
                return Err(JsonRpcError::domain(
                    PublicErrorKind::JobConflict,
                    "invalid job state transition",
                ));
            }
            if state == ApiJobState::Running {
                job.pack_publication = prepare_pack_publication_marker(job)?;
            }
            job.state = state;
            append_event(job, phase, self.policy.max_events_per_job)?;
            Ok(())
        })
    }

    pub fn request_cancel(
        &self,
        owner: &SessionId,
        job_id: &JobId,
    ) -> Result<ApiJobState, JsonRpcError> {
        let mut resulting_state = ApiJobState::Cancelled;
        self.update_owned(owner, job_id, |job| {
            match job.state {
                ApiJobState::Queued => {
                    job.state = ApiJobState::Cancelled;
                    job.error = Some(JsonRpcError::domain(
                        PublicErrorKind::Cancelled,
                        "job cancelled",
                    ));
                    append_event(job, "cancelled", self.policy.max_events_per_job)?;
                }
                ApiJobState::Running => {
                    job.state = ApiJobState::Cancelling;
                    append_event(job, "cancelling", self.policy.max_events_per_job)?;
                }
                ApiJobState::Cancelling | ApiJobState::Cancelled => {}
                ApiJobState::Completed | ApiJobState::Failed => {
                    return Err(JsonRpcError::domain(
                        PublicErrorKind::JobConflict,
                        "terminal job cannot be cancelled",
                    ));
                }
            }
            resulting_state = job.state;
            Ok(())
        })?;
        Ok(resulting_state)
    }

    pub fn finish_cancelled(&self, owner: &SessionId, job_id: &JobId) -> Result<(), JsonRpcError> {
        self.update_owned(owner, job_id, |job| {
            if job.state != ApiJobState::Cancelling {
                return Err(JsonRpcError::domain(
                    PublicErrorKind::JobConflict,
                    "job is not cancelling",
                ));
            }
            job.state = ApiJobState::Cancelled;
            job.error = Some(JsonRpcError::domain(
                PublicErrorKind::Cancelled,
                "job cancelled",
            ));
            append_event(job, "cancelled", self.policy.max_events_per_job)?;
            Ok(())
        })
    }

    pub fn complete(
        &self,
        owner: &SessionId,
        job_id: &JobId,
        result: Value,
    ) -> Result<(), JsonRpcError> {
        self.update_owned(owner, job_id, |job| {
            if !matches!(
                job.state,
                ApiJobState::Queued | ApiJobState::Running | ApiJobState::Cancelling
            ) {
                return Err(JsonRpcError::domain(
                    PublicErrorKind::JobConflict,
                    "job cannot complete from current state",
                ));
            }
            require_recorded_pack_publication(job)?;
            job.state = ApiJobState::Completed;
            job.result = Some(result);
            job.error = None;
            append_event(job, "completed", self.policy.max_events_per_job)?;
            Ok(())
        })
    }

    /// Persists the exact identity of a verified Pack output while the job is
    /// still nonterminal. Recovery treats this record as the commit marker and
    /// never infers ownership merely from a valid file appearing at `output`.
    pub(crate) fn record_pack_publication(
        &self,
        owner: &SessionId,
        job_id: &JobId,
    ) -> Result<(), JsonRpcError> {
        self.record_pack_publication_with(owner, job_id, verified_archive_identity)
    }

    fn record_pack_publication_with(
        &self,
        owner: &SessionId,
        job_id: &JobId,
        identify: impl FnOnce(&Path) -> Result<ArchiveIdentity, JsonRpcError>,
    ) -> Result<(), JsonRpcError> {
        // Verification can scan the whole archive. Do it outside the mutation
        // lock, then re-check all job invariants inside the durable transaction.
        let output = {
            let state = self.state.lock();
            let job = owned_job(&state, owner, job_id)?;
            if !matches!(job.state, ApiJobState::Running | ApiJobState::Cancelling) {
                return Err(JsonRpcError::domain(
                    PublicErrorKind::JobConflict,
                    "pack publication cannot be recorded from current state",
                ));
            }
            match &job.operation {
                StoredOperation::Pack { output, .. } => output.clone(),
                _ => {
                    return Err(JsonRpcError::domain(
                        PublicErrorKind::JobConflict,
                        "job is not a pack operation",
                    ));
                }
            }
        };
        let identity = identify(&output)?;
        self.update_owned(owner, job_id, |job| {
            if !matches!(job.state, ApiJobState::Running | ApiJobState::Cancelling) {
                return Err(JsonRpcError::domain(
                    PublicErrorKind::JobConflict,
                    "pack publication cannot be recorded from current state",
                ));
            }
            if !matches!(&job.operation, StoredOperation::Pack { output: current, .. } if current == &output)
            {
                return Err(JsonRpcError::domain(
                    PublicErrorKind::JobConflict,
                    "pack operation changed while publication was verified",
                ));
            }
            let marker = job
                .pack_publication
                .as_mut()
                .filter(|marker| marker.output_was_absent)
                .ok_or_else(|| internal("pack publication marker is missing"))?;
            if let Some(expected) = &marker.published_archive {
                if expected == &identity {
                    return Ok(());
                }
                return Err(JsonRpcError::domain(
                    PublicErrorKind::IntegrityMismatch,
                    "pack output identity changed",
                ));
            }
            marker.published_archive = Some(identity);
            append_event(job, "pack_published", self.policy.max_events_per_job)?;
            Ok(())
        })
    }

    /// Applies a deadline decision under the same mutation lock as submission,
    /// scheduling transitions, completion, and shutdown.
    ///
    /// The boolean tells the caller whether an in-process worker may exist and
    /// should receive cancellation after the durable state transition.
    pub(crate) fn expire_deadline(
        &self,
        owner: &SessionId,
        job_id: &JobId,
    ) -> Result<bool, JsonRpcError> {
        let _mutation = self.mutation.lock();
        if !self.is_healthy() {
            return Err(internal("job registry storage is unhealthy"));
        }
        let mut next = self.state.lock().clone();
        let job = next
            .jobs
            .get_mut(job_id.as_str())
            .filter(|job| &job.owner == owner)
            .ok_or_else(job_not_found)?;
        let (cancel_worker, changed) = match job.state {
            ApiJobState::Queued => {
                job.state = ApiJobState::Failed;
                job.result = None;
                job.error = Some(JsonRpcError::resource_limit("job deadline exceeded"));
                append_event(job, "deadline_exceeded", self.policy.max_events_per_job)?;
                (false, true)
            }
            ApiJobState::Running => {
                job.state = ApiJobState::Cancelling;
                append_event(job, "deadline_exceeded", self.policy.max_events_per_job)?;
                (true, true)
            }
            ApiJobState::Cancelling => (true, false),
            ApiJobState::Completed | ApiJobState::Failed | ApiJobState::Cancelled => (false, false),
        };
        if changed {
            self.commit(next)?;
        }
        Ok(cancel_worker)
    }

    pub fn fail(
        &self,
        owner: &SessionId,
        job_id: &JobId,
        error: JsonRpcError,
    ) -> Result<(), JsonRpcError> {
        self.update_owned(owner, job_id, |job| {
            if job.state.is_terminal() {
                return Err(JsonRpcError::domain(
                    PublicErrorKind::JobConflict,
                    "terminal job is immutable",
                ));
            }
            job.state = if error.kind == PublicErrorKind::Cancelled {
                ApiJobState::Cancelled
            } else {
                ApiJobState::Failed
            };
            job.result = None;
            job.error = Some(error);
            append_event(job, "failed", self.policy.max_events_per_job)?;
            Ok(())
        })
    }

    pub fn snapshot(&self, owner: &SessionId, job_id: &JobId) -> Result<JobSnapshot, JsonRpcError> {
        let state = self.state.lock();
        let job = owned_job(&state, owner, job_id)?;
        Ok(snapshot(job))
    }

    pub fn events(
        &self,
        owner: &SessionId,
        job_id: &JobId,
        after_sequence: u64,
    ) -> Result<Vec<ProgressEvent>, JsonRpcError> {
        let state = self.state.lock();
        let job = owned_job(&state, owner, job_id)?;
        Ok(job
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .cloned()
            .collect())
    }

    pub(crate) fn operation(
        &self,
        owner: &SessionId,
        job_id: &JobId,
    ) -> Result<(StoredOperation, JobLimits, JobPriority), JsonRpcError> {
        let state = self.state.lock();
        let job = owned_job(&state, owner, job_id)?;
        Ok((job.operation.clone(), job.limits.clone(), job.priority))
    }

    pub(crate) fn nonterminal_jobs(&self) -> Vec<(SessionId, JobId)> {
        self.state
            .lock()
            .jobs
            .values()
            .filter(|job| !job.state.is_terminal())
            .map(|job| (job.owner.clone(), job.job_id.clone()))
            .collect()
    }

    /// Atomically closes admission, cancels queued jobs, and requests
    /// cancellation of running jobs at the serialization point. The returned
    /// identities let the service cancel matching in-process workers and drain
    /// jobs left in `Cancelling` after the durable barrier.
    pub(crate) fn close_and_cancel_all(&self) -> Result<Vec<(SessionId, JobId)>, JsonRpcError> {
        let _mutation = self.mutation.lock();
        self.closed.store(true, Ordering::Release);
        let mut next = self.state.lock().clone();
        let mut affected = Vec::new();
        let mut changed = false;
        for job in next.jobs.values_mut() {
            if job.state.is_terminal() {
                continue;
            }
            affected.push((job.owner.clone(), job.job_id.clone()));
            match job.state {
                ApiJobState::Queued => {
                    job.state = ApiJobState::Cancelled;
                    job.result = None;
                    job.error = Some(JsonRpcError::domain(
                        PublicErrorKind::Cancelled,
                        "job cancelled during daemon shutdown",
                    ));
                    append_event(job, "daemon_shutdown", self.policy.max_events_per_job)?;
                    changed = true;
                }
                ApiJobState::Running => {
                    job.state = ApiJobState::Cancelling;
                    append_event(job, "daemon_shutdown", self.policy.max_events_per_job)?;
                    changed = true;
                }
                ApiJobState::Cancelling => {}
                ApiJobState::Completed | ApiJobState::Failed | ApiJobState::Cancelled => {
                    unreachable!("terminal jobs were filtered above")
                }
            }
        }
        if changed {
            self.commit(next)?;
        }
        Ok(affected)
    }

    fn update_owned(
        &self,
        owner: &SessionId,
        job_id: &JobId,
        update: impl FnOnce(&mut StoredJob) -> Result<(), JsonRpcError>,
    ) -> Result<(), JsonRpcError> {
        let _mutation = self.mutation.lock();
        if !self.is_healthy() {
            return Err(internal("job registry storage is unhealthy"));
        }
        let mut next = self.state.lock().clone();
        let job = next
            .jobs
            .get_mut(job_id.as_str())
            .filter(|job| &job.owner == owner)
            .ok_or_else(job_not_found)?;
        update(job)?;
        self.commit(next)
    }

    fn commit(&self, mut state: PersistentState) -> Result<(), JsonRpcError> {
        trim_retained_events(&mut state, self.policy);
        self.commit_with_sync(state, sync_parent)
    }

    fn commit_with_sync(
        &self,
        state: PersistentState,
        sync: impl FnOnce(&Path) -> Result<(), JsonRpcError>,
    ) -> Result<(), JsonRpcError> {
        let result = (|| {
            validate_state(&state, self.policy, true)?;
            let bytes = encode_state(&state, self.policy.max_store_bytes)?;
            let parent = publish_state(&self.path, &bytes)?;

            // The rename has already made `state` authoritative on disk. Publish
            // the same state in memory before reporting any directory-sync
            // failure so a live registry never continues from a stale snapshot.
            *self.state.lock() = state;
            sync(parent)
        })();
        if result.is_err() {
            self.healthy.store(false, Ordering::Release);
        }
        result
    }
}

#[cfg(test)]
fn load_state(path: &Path) -> Result<PersistentState, JsonRpcError> {
    load_state_with_policy(path, StorePolicy::default())
}

fn load_state_with_policy(
    path: &Path,
    policy: StorePolicy,
) -> Result<PersistentState, JsonRpcError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(PersistentState::default()),
        Err(_) => return Err(internal("cannot open job store")),
    };
    let length = file
        .metadata()
        .map_err(|_| internal("cannot inspect job store"))?
        .len();
    if length > policy.max_store_bytes {
        return Err(JsonRpcError::resource_limit("job store too large"));
    }
    let capacity =
        usize::try_from(length).map_err(|_| JsonRpcError::resource_limit("job store too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.read_to_end(&mut bytes)
        .map_err(|_| internal("cannot read job store"))?;
    serde_json::from_slice(&bytes).map_err(|_| internal("job store is corrupt"))
}

fn persist_state_with_policy(
    path: &Path,
    state: &PersistentState,
    policy: StorePolicy,
) -> Result<(), JsonRpcError> {
    validate_state(state, policy, true)?;
    let bytes = encode_state(state, policy.max_store_bytes)?;
    let parent = publish_state(path, &bytes)?;
    sync_parent(parent)
}

fn encode_state(state: &PersistentState, maximum: u64) -> Result<Vec<u8>, JsonRpcError> {
    let bytes = serde_json::to_vec(state).map_err(|_| internal("cannot encode job store"))?;
    if bytes.len() as u64 > maximum {
        return Err(JsonRpcError::resource_limit(
            "job store size limit exceeded",
        ));
    }
    Ok(bytes)
}

fn publish_state<'a>(path: &'a Path, bytes: &[u8]) -> Result<&'a Path, JsonRpcError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|_| internal("cannot create state directory"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| internal("cannot create job store transaction"))?;
    temporary
        .write_all(bytes)
        .map_err(|_| internal("cannot write job store"))?;
    temporary
        .flush()
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|_| internal("cannot sync job store"))?;
    temporary
        .persist(path)
        .map_err(|_| internal("cannot commit job store"))?;
    Ok(parent)
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<(), JsonRpcError> {
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|_| internal("cannot sync state directory"))
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<(), JsonRpcError> {
    Ok(())
}

fn owned_job<'a>(
    state: &'a PersistentState,
    owner: &SessionId,
    job_id: &JobId,
) -> Result<&'a StoredJob, JsonRpcError> {
    state
        .jobs
        .get(job_id.as_str())
        .filter(|job| &job.owner == owner)
        .ok_or_else(job_not_found)
}

fn snapshot(job: &StoredJob) -> JobSnapshot {
    JobSnapshot {
        job_id: job.job_id.clone(),
        state: job.state,
        operation: job.method,
        created_at_unix_ms: job.created_at_unix_ms,
        updated_at_unix_ms: job.updated_at_unix_ms,
        result: job.result.clone(),
        error: job.error.clone(),
    }
}

fn prepare_pack_publication_marker(
    job: &StoredJob,
) -> Result<Option<PackPublicationMarker>, JsonRpcError> {
    let StoredOperation::Pack { output, .. } = &job.operation else {
        return Ok(None);
    };
    let output_was_absent = match fs::symlink_metadata(output) {
        Ok(_) => false,
        Err(error) if error.kind() == ErrorKind::NotFound => true,
        Err(_) => return Err(internal("cannot inspect pack output before execution")),
    };
    Ok(Some(PackPublicationMarker {
        marker_version: PACK_PUBLICATION_MARKER_VERSION,
        job_id: job.job_id.clone(),
        params_hash: job.params_hash,
        output: output.clone(),
        output_was_absent,
        published_archive: None,
    }))
}

fn recover_pack_archive(job: &StoredJob) -> Option<(PathBuf, ArchiveIdentity)> {
    let StoredOperation::Pack { output, .. } = &job.operation else {
        return None;
    };
    let marker = job.pack_publication.as_ref()?;
    let expected = marker.published_archive.as_ref()?;
    if !marker.output_was_absent || !output.is_file() {
        return None;
    }
    let identity = verified_archive_identity(output).ok()?;
    if &identity != expected {
        return None;
    }
    Some((output.clone(), identity))
}

fn require_recorded_pack_publication(job: &StoredJob) -> Result<(), JsonRpcError> {
    let StoredOperation::Pack { .. } = &job.operation else {
        return Ok(());
    };
    job.pack_publication
        .as_ref()
        .filter(|marker| marker.output_was_absent && marker.published_archive.is_some())
        .map(|_| ())
        .ok_or_else(|| internal("pack publication identity is not recorded"))
}

fn verified_archive_identity(path: &Path) -> Result<ArchiveIdentity, JsonRpcError> {
    let report =
        pithos_engine::verify(path).map_err(|_| internal("pack output verification failed"))?;
    Ok(ArchiveIdentity {
        length: report.archive_bytes,
        blake3: report.blake3_root,
    })
}

fn append_event(
    job: &mut StoredJob,
    phase: &str,
    maximum_events: usize,
) -> Result<(), JsonRpcError> {
    let sequence = job.next_sequence;
    job.next_sequence = sequence
        .checked_add(1)
        .ok_or_else(|| JsonRpcError::resource_limit("job event sequence exhausted"))?;
    job.updated_at_unix_ms = unix_ms();
    job.events.push(ProgressEvent {
        job_id: job.job_id.clone(),
        sequence,
        state: job.state,
        phase: phase.to_owned(),
        completed_units: u64::from(job.state.is_terminal()),
        total_units: Some(1),
        occurred_at_unix_ms: job.updated_at_unix_ms,
    });
    trim_event_history(job, maximum_events);
    Ok(())
}

fn trim_event_history(job: &mut StoredJob, maximum_events: usize) -> bool {
    let overflow = job.events.len().saturating_sub(maximum_events);
    if overflow == 0 {
        return false;
    }
    job.events.drain(..overflow);
    true
}

fn trim_retained_events(state: &mut PersistentState, policy: StorePolicy) -> bool {
    let mut changed = false;
    for job in state.jobs.values_mut() {
        changed |= trim_event_history(job, policy.max_events_per_job);
    }
    changed
}

fn validate_state(
    state: &PersistentState,
    policy: StorePolicy,
    enforce_retention: bool,
) -> Result<(), JsonRpcError> {
    if state.schema_version != STORE_SCHEMA_VERSION {
        return Err(internal("unsupported job store schema"));
    }
    if enforce_retention && state.jobs.len() > policy.max_retained_jobs {
        return Err(JsonRpcError::resource_limit(
            "job store retention capacity exceeded",
        ));
    }
    if state.next_job == 0 || state.idempotency.len() != state.jobs.len() {
        return Err(internal("job store counters or indices are corrupt"));
    }

    let mut maximum_job_number = 0_u64;
    for (map_key, job) in &state.jobs {
        if map_key != job.job_id.as_str()
            || !valid_identifier(job.owner.as_str(), "session-")
            || job.method != operation_method(&job.operation)
            || job.limits.max_threads == 0
            || job.limits.max_memory == 0
            || job.limits.max_output == 0
        {
            return Err(internal("job store schema invariants are corrupt"));
        }
        validate_idempotency_key(&job.idempotency_key)
            .map_err(|_| internal("job store idempotency key is corrupt"))?;
        let job_number = parse_job_number(&job.job_id)?;
        maximum_job_number = maximum_job_number.max(job_number);

        let index_key = idempotency_map_key(&job.owner, &job.idempotency_key);
        if state.idempotency.get(&index_key).map(String::as_str) != Some(job.job_id.as_str()) {
            return Err(internal("job store idempotency index is corrupt"));
        }
        validate_job_events(job, policy, enforce_retention)?;
        validate_job_result(job)?;
        validate_pack_publication_marker(job)?;
    }
    if !state.jobs.is_empty() && state.next_job <= maximum_job_number {
        return Err(internal("job store next-job counter is corrupt"));
    }
    for (index_key, job_id) in &state.idempotency {
        let job = state
            .jobs
            .get(job_id)
            .ok_or_else(|| internal("job store idempotency index is corrupt"))?;
        if index_key != &idempotency_map_key(&job.owner, &job.idempotency_key) {
            return Err(internal("job store idempotency index is corrupt"));
        }
    }
    Ok(())
}

fn validate_pack_publication_marker(job: &StoredJob) -> Result<(), JsonRpcError> {
    let Some(marker) = &job.pack_publication else {
        return Ok(());
    };
    let StoredOperation::Pack { output, .. } = &job.operation else {
        return Err(internal("non-pack job has a publication marker"));
    };
    if marker.marker_version != PACK_PUBLICATION_MARKER_VERSION
        || marker.job_id != job.job_id
        || marker.params_hash != job.params_hash
        || &marker.output != output
        || job.state == ApiJobState::Queued
        || (marker.published_archive.is_some() && !marker.output_was_absent)
    {
        return Err(internal("pack publication marker is corrupt"));
    }
    Ok(())
}

fn validate_job_events(
    job: &StoredJob,
    policy: StorePolicy,
    enforce_retention: bool,
) -> Result<(), JsonRpcError> {
    if job.events.is_empty() || (enforce_retention && job.events.len() > policy.max_events_per_job)
    {
        return Err(internal("job store event history is corrupt"));
    }
    let mut previous = None;
    for event in &job.events {
        if event.job_id != job.job_id
            || event.sequence == 0
            || previous.is_some_and(|sequence| event.sequence <= sequence)
        {
            return Err(internal("job store event sequence is corrupt"));
        }
        previous = Some(event.sequence);
    }
    let last = job
        .events
        .last()
        .ok_or_else(|| internal("job store event history is corrupt"))?;
    let expected_next = last
        .sequence
        .checked_add(1)
        .ok_or_else(|| internal("job store event sequence is exhausted"))?;
    if job.next_sequence != expected_next || last.state != job.state {
        return Err(internal("job store event counter is corrupt"));
    }
    Ok(())
}

fn validate_job_result(job: &StoredJob) -> Result<(), JsonRpcError> {
    let valid = match job.state {
        ApiJobState::Completed => job.result.is_some() && job.error.is_none(),
        ApiJobState::Failed => job.result.is_none() && job.error.is_some(),
        ApiJobState::Cancelled => job.result.is_none(),
        ApiJobState::Queued | ApiJobState::Running | ApiJobState::Cancelling => {
            job.result.is_none() && job.error.is_none()
        }
    };
    if valid {
        Ok(())
    } else {
        Err(internal("job store result invariant is corrupt"))
    }
}

fn parse_job_number(job_id: &JobId) -> Result<u64, JsonRpcError> {
    let suffix = job_id
        .as_str()
        .strip_prefix("job-")
        .filter(|suffix| suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| internal("job store job id is corrupt"))?;
    u64::from_str_radix(suffix, 16).map_err(|_| internal("job store job id is corrupt"))
}

fn valid_identifier(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len() + 8
        && value.len() <= 96
        && value.starts_with(prefix)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn operation_method(operation: &StoredOperation) -> RpcMethod {
    match operation {
        StoredOperation::Pack { .. } => RpcMethod::Pack,
        StoredOperation::Unpack { .. } => RpcMethod::Unpack,
        StoredOperation::List { .. } => RpcMethod::List,
        StoredOperation::Inspect { .. } => RpcMethod::Inspect,
        StoredOperation::Extract { .. } => RpcMethod::Extract,
        StoredOperation::ReadRange { .. } => RpcMethod::ReadRange,
        StoredOperation::Verify { .. } => RpcMethod::Verify,
    }
}

fn valid_transition(from: ApiJobState, to: ApiJobState) -> bool {
    matches!(
        (from, to),
        (ApiJobState::Queued, ApiJobState::Running)
            | (ApiJobState::Queued, ApiJobState::Cancelled)
            | (ApiJobState::Running, ApiJobState::Cancelling)
            | (ApiJobState::Running, ApiJobState::Completed)
            | (ApiJobState::Running, ApiJobState::Failed)
            | (ApiJobState::Cancelling, ApiJobState::Cancelled)
            | (ApiJobState::Cancelling, ApiJobState::Failed)
    )
}

fn idempotency_map_key(owner: &SessionId, key: &str) -> String {
    format!("{}:{key}", owner.as_str())
}

fn validate_idempotency_key(key: &str) -> Result<(), JsonRpcError> {
    if key.is_empty()
        || key.len() > MAX_IDEMPOTENCY_KEY_BYTES
        || !key.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(JsonRpcError::invalid_params("invalid idempotency key"));
    }
    Ok(())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn job_not_found() -> JsonRpcError {
    JsonRpcError::domain(PublicErrorKind::JobNotFound, "job not found")
}

fn internal(message: &'static str) -> JsonRpcError {
    JsonRpcError::domain(PublicErrorKind::Internal, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pithos_core::CompressionProfile;
    use pithos_engine::{PackRequest, pack};

    fn policy(max_store_bytes: u64, max_events: usize, max_retained_jobs: usize) -> StorePolicy {
        StorePolicy {
            max_store_bytes,
            max_events_per_job: max_events,
            max_retained_jobs,
        }
    }

    fn write_unchecked_state(path: &Path, state: &PersistentState) {
        let bytes = serde_json::to_vec(state).expect("encode deliberately corrupted state");
        fs::write(path, bytes).expect("write deliberately corrupted state");
    }

    fn owner(number: u64) -> SessionId {
        SessionId::new(format!("session-{number:016x}")).expect("valid test session")
    }

    fn submission(
        owner: SessionId,
        key: &str,
        hash_byte: u8,
        operation: StoredOperation,
    ) -> JobSubmission {
        JobSubmission {
            owner,
            method: match &operation {
                StoredOperation::Pack { .. } => RpcMethod::Pack,
                StoredOperation::Verify { .. } => RpcMethod::Verify,
                _ => RpcMethod::Inspect,
            },
            priority: pithos_agent_api::JobPriority::VerifyRequested,
            idempotency_key: key.to_owned(),
            params_hash: [hash_byte; 32],
            limits: JobLimits::default(),
            operation,
        }
    }

    #[test]
    fn recovery_reconciles_an_archive_published_after_pack_started_as_completed() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).expect("source directory");
        fs::write(
            source.join("payload.txt"),
            b"published before registry commit",
        )
        .expect("source payload");
        let archive = temp.path().join("published.pithos");
        let store_path = temp.path().join("jobs.json");
        let job_owner = owner(1);
        let registry = JobRegistry::open(store_path.clone()).expect("open registry");
        let accepted = registry
            .submit(submission(
                job_owner.clone(),
                "recover-pack",
                1,
                StoredOperation::Pack {
                    inputs: Vec::new(),
                    output: archive.clone(),
                    profile: ApiProfile::Raw,
                },
            ))
            .expect("submit pack job");
        registry
            .transition(
                &job_owner,
                &accepted.job_id,
                ApiJobState::Running,
                "running",
            )
            .expect("mark job running");
        assert!(!archive.exists(), "pack output begins absent");
        pack(PackRequest {
            inputs: vec![source],
            output: archive.clone(),
            profile: CompressionProfile::Raw,
        })
        .expect("valid archive published after the job started");
        registry
            .record_pack_publication(&job_owner, &accepted.job_id)
            .expect("durably record the published archive identity");
        drop(registry);

        let recovered = JobRegistry::open(store_path).expect("recover registry");
        let (_, _, priority) = recovered
            .operation(&job_owner, &accepted.job_id)
            .expect("recovered operation");
        assert_eq!(priority, pithos_agent_api::JobPriority::VerifyRequested);
        let snapshot = recovered
            .snapshot(&job_owner, &accepted.job_id)
            .expect("recovered snapshot");
        assert_eq!(snapshot.state, ApiJobState::Completed);
        assert_eq!(
            snapshot.result.expect("completion result")["archive"],
            serde_json::to_value(&archive).expect("archive path JSON")
        );
        assert!(snapshot.error.is_none());
    }

    #[test]
    fn recovery_fails_closed_if_pack_published_before_its_identity_was_recorded() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).expect("source directory");
        fs::write(
            source.join("payload.txt"),
            b"published without commit marker",
        )
        .expect("source payload");
        let archive = temp.path().join("uncommitted.pithos");
        let store_path = temp.path().join("jobs.json");
        let job_owner = owner(13);
        let registry = JobRegistry::open(store_path.clone()).expect("open registry");
        let accepted = registry
            .submit(submission(
                job_owner.clone(),
                "uncommitted-pack",
                13,
                StoredOperation::Pack {
                    inputs: Vec::new(),
                    output: archive.clone(),
                    profile: ApiProfile::Raw,
                },
            ))
            .expect("submit pack job");
        registry
            .transition(
                &job_owner,
                &accepted.job_id,
                ApiJobState::Running,
                "running",
            )
            .expect("mark job running");
        pack(PackRequest {
            inputs: vec![source],
            output: archive,
            profile: CompressionProfile::Raw,
        })
        .expect("publish archive before simulated crash");
        drop(registry);

        let recovered = JobRegistry::open(store_path).expect("recover registry");
        assert_eq!(
            recovered
                .snapshot(&job_owner, &accepted.job_id)
                .expect("recovered job")
                .state,
            ApiJobState::Failed
        );
    }

    #[test]
    fn recovery_rejects_a_different_valid_archive_at_the_recorded_output_path() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let original_source = temp.path().join("original-source");
        let replacement_source = temp.path().join("replacement-source");
        fs::create_dir_all(&original_source).expect("original source directory");
        fs::create_dir_all(&replacement_source).expect("replacement source directory");
        fs::write(original_source.join("payload.txt"), b"original archive")
            .expect("original payload");
        fs::write(
            replacement_source.join("payload.txt"),
            b"different replacement archive",
        )
        .expect("replacement payload");
        let archive = temp.path().join("identity-bound.pithos");
        let store_path = temp.path().join("jobs.json");
        let job_owner = owner(14);
        let registry = JobRegistry::open(store_path.clone()).expect("open registry");
        let accepted = registry
            .submit(submission(
                job_owner.clone(),
                "identity-bound-pack",
                14,
                StoredOperation::Pack {
                    inputs: Vec::new(),
                    output: archive.clone(),
                    profile: ApiProfile::Raw,
                },
            ))
            .expect("submit pack job");
        registry
            .transition(
                &job_owner,
                &accepted.job_id,
                ApiJobState::Running,
                "running",
            )
            .expect("mark job running");
        pack(PackRequest {
            inputs: vec![original_source],
            output: archive.clone(),
            profile: CompressionProfile::Raw,
        })
        .expect("publish original archive");
        registry
            .record_pack_publication(&job_owner, &accepted.job_id)
            .expect("record original archive identity");

        fs::remove_file(&archive).expect("remove original archive in isolated test directory");
        pack(PackRequest {
            inputs: vec![replacement_source],
            output: archive,
            profile: CompressionProfile::Raw,
        })
        .expect("publish a different valid archive at the same path");
        drop(registry);

        let recovered = JobRegistry::open(store_path).expect("recover registry");
        assert_eq!(
            recovered
                .snapshot(&job_owner, &accepted.job_id)
                .expect("recovered job")
                .state,
            ApiJobState::Failed
        );
    }

    #[test]
    fn pack_identity_calculation_does_not_hold_the_registry_mutation_lock() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;

        let temp = tempfile::tempdir().expect("temporary test directory");
        let registry =
            Arc::new(JobRegistry::open(temp.path().join("jobs.json")).expect("open test registry"));
        let job_owner = owner(16);
        let accepted = registry
            .submit(submission(
                job_owner.clone(),
                "slow-identity",
                16,
                StoredOperation::Pack {
                    inputs: Vec::new(),
                    output: temp.path().join("slow-identity.pithos"),
                    profile: ApiProfile::Raw,
                },
            ))
            .expect("submit pack job");
        registry
            .transition(
                &job_owner,
                &accepted.job_id,
                ApiJobState::Running,
                "running",
            )
            .expect("mark pack running");

        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let worker_registry = Arc::clone(&registry);
        let worker_owner = job_owner.clone();
        let worker_job_id = accepted.job_id.clone();
        let worker = std::thread::spawn(move || {
            worker_registry.record_pack_publication_with(&worker_owner, &worker_job_id, |_| {
                started_tx.send(()).expect("announce identity calculation");
                release_rx.recv().expect("release identity calculation");
                Ok(ArchiveIdentity {
                    length: 123,
                    blake3: [16; 32],
                })
            })
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("identity calculation started");

        let mutation_was_free = registry.mutation.try_lock().is_some();
        let close_result = mutation_was_free.then(|| registry.close_and_cancel_all());
        release_tx
            .send(())
            .expect("release blocked identity calculation");
        let record_result = worker.join().expect("identity worker");

        assert!(
            mutation_was_free,
            "archive verification must not hold the registry mutation lock"
        );
        let affected = close_result
            .expect("close ran while verifier was blocked")
            .expect("close completed while verifier was blocked");
        assert!(affected.contains(&(job_owner.clone(), accepted.job_id.clone())));
        record_result.expect("publication can commit from Cancelling after close");
        assert_eq!(
            registry
                .snapshot(&job_owner, &accepted.job_id)
                .expect("post-close snapshot")
                .state,
            ApiJobState::Cancelling
        );
        registry
            .finish_cancelled(&job_owner, &accepted.job_id)
            .expect("finish test cancellation");
    }

    #[test]
    fn recovery_does_not_claim_a_preexisting_valid_archive_for_a_pack_job() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).expect("source directory");
        fs::write(source.join("payload.txt"), b"unrelated archive").expect("source payload");
        let archive = temp.path().join("preexisting.pithos");
        pack(PackRequest {
            inputs: vec![source],
            output: archive.clone(),
            profile: CompressionProfile::Raw,
        })
        .expect("preexisting valid archive");

        let store_path = temp.path().join("jobs.json");
        let job_owner = owner(9);
        let registry = JobRegistry::open(store_path.clone()).expect("open registry");
        let accepted = registry
            .submit(submission(
                job_owner.clone(),
                "preexisting-pack",
                9,
                StoredOperation::Pack {
                    inputs: Vec::new(),
                    output: archive,
                    profile: ApiProfile::Raw,
                },
            ))
            .expect("submit pack job");
        registry
            .transition(
                &job_owner,
                &accepted.job_id,
                ApiJobState::Running,
                "running",
            )
            .expect("mark job running");
        drop(registry);

        let recovered = JobRegistry::open(store_path).expect("recover registry");
        let snapshot = recovered
            .snapshot(&job_owner, &accepted.job_id)
            .expect("recovered snapshot");
        assert_eq!(snapshot.state, ApiJobState::Failed);
        assert!(snapshot.result.is_none());
        assert_eq!(
            snapshot.error.expect("recovery failure").kind,
            PublicErrorKind::Internal
        );
    }

    #[test]
    fn recovery_fails_a_nonterminal_pack_without_a_verified_output() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let store_path = temp.path().join("jobs.json");
        let missing_archive = temp.path().join("missing.pithos");
        fs::write(&missing_archive, b"not a valid Pithos archive")
            .expect("invalid recovery candidate");
        let job_owner = owner(2);
        let registry = JobRegistry::open(store_path.clone()).expect("open registry");
        let accepted = registry
            .submit(submission(
                job_owner.clone(),
                "missing-pack",
                2,
                StoredOperation::Pack {
                    inputs: Vec::new(),
                    output: missing_archive,
                    profile: ApiProfile::Raw,
                },
            ))
            .expect("submit pack job");
        registry
            .transition(
                &job_owner,
                &accepted.job_id,
                ApiJobState::Running,
                "running",
            )
            .expect("mark job running");
        drop(registry);

        let recovered = JobRegistry::open(store_path).expect("recover registry");
        let snapshot = recovered
            .snapshot(&job_owner, &accepted.job_id)
            .expect("recovered snapshot");
        assert_eq!(snapshot.state, ApiJobState::Failed);
        assert!(snapshot.result.is_none());
        assert_eq!(
            snapshot.error.expect("recovery failure").kind,
            PublicErrorKind::Internal
        );
    }

    #[test]
    fn open_rejects_a_counter_that_can_reuse_an_existing_job_id() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let store_path = temp.path().join("jobs.json");
        let job_owner = owner(3);
        let registry = JobRegistry::open(store_path.clone()).expect("open registry");
        registry
            .submit(submission(
                job_owner,
                "counter-job",
                3,
                StoredOperation::Verify {
                    archive: temp.path().join("archive.pithos"),
                },
            ))
            .expect("submit job");
        drop(registry);

        let mut state = load_state(&store_path).expect("load test state");
        state.next_job = 1;
        write_unchecked_state(&store_path, &state);

        assert!(JobRegistry::open(store_path).is_err());
    }

    #[test]
    fn commit_keeps_memory_equal_to_the_published_file_when_parent_sync_fails() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let store_path = temp.path().join("jobs.json");
        let registry = JobRegistry::open(store_path.clone()).expect("open registry");
        let mut published = registry.state.lock().clone();
        published.next_job = 41;

        let error = registry
            .commit_with_sync(published, |_| Err(internal("forced parent sync failure")))
            .expect_err("parent sync must be reported");
        assert_eq!(error.kind, PublicErrorKind::Internal);
        assert_eq!(registry.state.lock().next_job, 41);
        assert_eq!(
            load_state(&store_path)
                .expect("published state remains readable")
                .next_job,
            41
        );
        assert!(
            !registry.is_healthy(),
            "a durability failure must make the live registry fail closed"
        );
    }

    #[test]
    fn oversized_serialized_state_is_rejected_before_publication() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let store_path = temp.path().join("jobs.json");
        let store_policy = policy(4096, 16, 16);
        let job_owner = owner(4);
        let registry = JobRegistry::open_with_policy(store_path.clone(), store_policy)
            .expect("open bounded registry");
        let accepted = registry
            .submit(submission(
                job_owner.clone(),
                "bounded-result",
                4,
                StoredOperation::Verify {
                    archive: temp.path().join("archive.pithos"),
                },
            ))
            .expect("submit bounded job");
        registry
            .transition(
                &job_owner,
                &accepted.job_id,
                ApiJobState::Running,
                "running",
            )
            .expect("mark job running");
        let before = fs::read(&store_path).expect("state before oversized result");

        let error = registry
            .complete(
                &job_owner,
                &accepted.job_id,
                serde_json::json!({"payload": "x".repeat(16 * 1024)}),
            )
            .expect_err("oversized state must be rejected");
        assert_eq!(error.kind, PublicErrorKind::ResourceLimit);
        assert_eq!(
            fs::read(&store_path).expect("state after rejected result"),
            before
        );
        assert!(fs::metadata(&store_path).expect("bounded store").len() <= 4096);
        assert_eq!(
            registry
                .snapshot(&job_owner, &accepted.job_id)
                .expect("unchanged in-memory snapshot")
                .state,
            ApiJobState::Running
        );
        assert!(
            !registry.is_healthy(),
            "an unpublishable mutation must make storage unhealthy"
        );
    }

    #[test]
    fn event_retention_is_bounded_and_preserves_monotonic_terminal_tail() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let store_path = temp.path().join("jobs.json");
        let store_policy = policy(64 * 1024, 3, 16);
        let job_owner = owner(5);
        let registry = JobRegistry::open_with_policy(store_path.clone(), store_policy)
            .expect("open bounded registry");
        let accepted = registry
            .submit(submission(
                job_owner.clone(),
                "event-tail",
                5,
                StoredOperation::Verify {
                    archive: temp.path().join("archive.pithos"),
                },
            ))
            .expect("submit job");
        registry
            .transition(
                &job_owner,
                &accepted.job_id,
                ApiJobState::Running,
                "running",
            )
            .expect("mark job running");
        registry
            .request_cancel(&job_owner, &accepted.job_id)
            .expect("request cancellation");
        registry
            .finish_cancelled(&job_owner, &accepted.job_id)
            .expect("finish cancellation");

        let events = registry
            .events(&job_owner, &accepted.job_id, 0)
            .expect("retained events");
        assert_eq!(events.len(), 3);
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert_eq!(
            events.last().expect("terminal event").state,
            ApiJobState::Cancelled
        );
        drop(registry);

        let reopened = JobRegistry::open_with_policy(store_path, store_policy)
            .expect("reopen bounded registry");
        assert_eq!(
            reopened
                .events(&job_owner, &accepted.job_id, 0)
                .expect("persisted retained events")
                .len(),
            3
        );
    }

    #[test]
    fn retention_capacity_never_discards_idempotency_results() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let store_path = temp.path().join("jobs.json");
        let store_policy = policy(128 * 1024, 8, 2);
        let job_owner = owner(6);
        let registry = JobRegistry::open_with_policy(store_path.clone(), store_policy)
            .expect("open bounded registry");
        let mut completed = Vec::new();
        for number in 0_u8..2 {
            let key = format!("terminal-{number}");
            let accepted = registry
                .submit(submission(
                    job_owner.clone(),
                    &key,
                    number,
                    StoredOperation::Verify {
                        archive: temp.path().join(format!("archive-{number}.pithos")),
                    },
                ))
                .expect("submit terminal job");
            registry
                .transition(
                    &job_owner,
                    &accepted.job_id,
                    ApiJobState::Running,
                    "running",
                )
                .expect("mark job running");
            registry
                .complete(
                    &job_owner,
                    &accepted.job_id,
                    serde_json::json!({"number": number}),
                )
                .expect("complete job");
            completed.push((key, accepted.job_id));
        }

        assert!(registry.snapshot(&job_owner, &completed[0].1).is_ok());
        assert!(registry.snapshot(&job_owner, &completed[1].1).is_ok());

        let replay = registry
            .submit(submission(
                job_owner.clone(),
                &completed[0].0,
                0,
                StoredOperation::Verify {
                    archive: temp.path().join("archive-0.pithos"),
                },
            ))
            .expect("idempotency replay at capacity");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.job_id, completed[0].1);

        let conflict = registry
            .submit(submission(
                job_owner.clone(),
                &completed[0].0,
                99,
                StoredOperation::Verify {
                    archive: temp.path().join("different.pithos"),
                },
            ))
            .expect_err("same key with different parameters must conflict forever");
        assert_eq!(conflict.kind, PublicErrorKind::JobConflict);

        let full = registry
            .submit(submission(
                job_owner.clone(),
                "terminal-2",
                2,
                StoredOperation::Verify {
                    archive: temp.path().join("archive-2.pithos"),
                },
            ))
            .expect_err("new unique work must not evict durable idempotency history");
        assert_eq!(full.kind, PublicErrorKind::ResourceLimit);

        drop(registry);
        let reopened = JobRegistry::open_with_policy(store_path, store_policy)
            .expect("reopen bounded registry");
        let replay = reopened
            .submit(submission(
                job_owner,
                &completed[0].0,
                0,
                StoredOperation::Verify {
                    archive: temp.path().join("archive-0.pithos"),
                },
            ))
            .expect("idempotency replay survives restart at capacity");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.job_id, completed[0].1);
    }

    #[test]
    fn retention_capacity_is_global_and_never_evicts_another_session() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let store_path = temp.path().join("jobs.json");
        let store_policy = policy(128 * 1024, 8, 2);
        let quiet_owner = owner(10);
        let noisy_owner = owner(11);
        let registry =
            JobRegistry::open_with_policy(store_path, store_policy).expect("open bounded registry");

        let quiet = registry
            .submit(submission(
                quiet_owner.clone(),
                "quiet-terminal",
                10,
                StoredOperation::Verify {
                    archive: temp.path().join("quiet.pithos"),
                },
            ))
            .expect("submit quiet job");
        registry
            .complete(
                &quiet_owner,
                &quiet.job_id,
                serde_json::json!({"owner": "quiet"}),
            )
            .expect("complete quiet job");

        let noisy = registry
            .submit(submission(
                noisy_owner.clone(),
                "noisy-terminal-0",
                20,
                StoredOperation::Verify {
                    archive: temp.path().join("noisy-0.pithos"),
                },
            ))
            .expect("submit noisy job");
        registry
            .complete(
                &noisy_owner,
                &noisy.job_id,
                serde_json::json!({"owner": "noisy", "number": 0}),
            )
            .expect("complete noisy job");

        let full = registry
            .submit(submission(
                noisy_owner.clone(),
                "noisy-terminal-1",
                21,
                StoredOperation::Verify {
                    archive: temp.path().join("noisy-1.pithos"),
                },
            ))
            .expect_err("one session cannot evict another session at capacity");
        assert_eq!(full.kind, PublicErrorKind::ResourceLimit);

        assert_eq!(
            registry
                .snapshot(&quiet_owner, &quiet.job_id)
                .expect("quiet terminal job remains retained")
                .state,
            ApiJobState::Completed
        );
        assert!(registry.snapshot(&noisy_owner, &noisy.job_id).is_ok());

        let replay = registry
            .submit(submission(
                quiet_owner,
                "quiet-terminal",
                10,
                StoredOperation::Verify {
                    archive: temp.path().join("quiet.pithos"),
                },
            ))
            .expect("quiet idempotency entry remains retained");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.job_id, quiet.job_id);
    }

    #[test]
    fn close_serializes_with_concurrent_submissions_and_leaves_no_nonterminal_jobs() {
        use std::sync::{Arc, Barrier};

        const SUBMITTERS: usize = 16;
        let temp = tempfile::tempdir().expect("temporary test directory");
        let store_path = temp.path().join("jobs.json");
        let registry =
            Arc::new(JobRegistry::open(store_path.clone()).expect("open shutdown-test registry"));
        let barrier = Arc::new(Barrier::new(SUBMITTERS + 2));
        let mut workers = Vec::new();
        for number in 0..SUBMITTERS {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            let archive = temp.path().join(format!("shutdown-{number}.pithos"));
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                registry.submit(submission(
                    owner(12),
                    &format!("shutdown-{number}"),
                    u8::try_from(number).expect("small submitter number"),
                    StoredOperation::Verify { archive },
                ))
            }));
        }
        let closer_registry = Arc::clone(&registry);
        let closer_barrier = Arc::clone(&barrier);
        let closer = std::thread::spawn(move || {
            closer_barrier.wait();
            closer_registry.close_and_cancel_all()
        });
        barrier.wait();

        let _submission_results = workers
            .into_iter()
            .map(|worker| worker.join().expect("submission worker"))
            .collect::<Vec<_>>();
        closer
            .join()
            .expect("shutdown worker")
            .expect("persist shutdown cancellation");

        assert!(registry.nonterminal_jobs().is_empty());
        let rejected = registry
            .submit(submission(
                owner(12),
                "after-close",
                99,
                StoredOperation::Verify {
                    archive: temp.path().join("after-close.pithos"),
                },
            ))
            .expect_err("submissions after close must fail inside the store");
        assert_eq!(rejected.kind, PublicErrorKind::JobConflict);

        let reopened = JobRegistry::open(store_path).expect("reopen persisted shutdown state");
        assert!(reopened.nonterminal_jobs().is_empty());
        assert!(
            reopened.is_healthy(),
            "the closed admission flag is process-local and a sound store reopens healthy"
        );
    }

    #[test]
    fn close_cancels_queued_jobs_but_only_requests_cancellation_of_running_jobs() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let registry =
            JobRegistry::open(temp.path().join("jobs.json")).expect("open shutdown registry");
        let job_owner = owner(15);
        let queued = registry
            .submit(submission(
                job_owner.clone(),
                "queued-at-close",
                31,
                StoredOperation::Verify {
                    archive: temp.path().join("queued.pithos"),
                },
            ))
            .expect("submit queued job");
        let running = registry
            .submit(submission(
                job_owner.clone(),
                "running-at-close",
                32,
                StoredOperation::Verify {
                    archive: temp.path().join("running.pithos"),
                },
            ))
            .expect("submit running job");
        registry
            .transition(&job_owner, &running.job_id, ApiJobState::Running, "running")
            .expect("mark job running");
        let already_cancelling = registry
            .submit(submission(
                job_owner.clone(),
                "cancelling-at-close",
                33,
                StoredOperation::Verify {
                    archive: temp.path().join("cancelling.pithos"),
                },
            ))
            .expect("submit cancelling job");
        registry
            .transition(
                &job_owner,
                &already_cancelling.job_id,
                ApiJobState::Running,
                "running",
            )
            .expect("mark second job running");
        registry
            .request_cancel(&job_owner, &already_cancelling.job_id)
            .expect("request cancellation before close");
        let cancelling_event_count = registry
            .events(&job_owner, &already_cancelling.job_id, 0)
            .expect("events before close")
            .len();

        let affected = registry
            .close_and_cancel_all()
            .expect("persist shutdown state transitions");
        assert_eq!(affected.len(), 3);
        assert_eq!(
            registry
                .snapshot(&job_owner, &queued.job_id)
                .expect("queued snapshot")
                .state,
            ApiJobState::Cancelled
        );
        assert_eq!(
            registry
                .snapshot(&job_owner, &running.job_id)
                .expect("running snapshot")
                .state,
            ApiJobState::Cancelling
        );
        assert_eq!(
            registry
                .snapshot(&job_owner, &already_cancelling.job_id)
                .expect("cancelling snapshot")
                .state,
            ApiJobState::Cancelling
        );
        assert_eq!(
            registry
                .events(&job_owner, &already_cancelling.job_id, 0)
                .expect("events after close")
                .len(),
            cancelling_event_count,
            "closing must not append duplicate cancellation events"
        );

        registry
            .finish_cancelled(&job_owner, &running.job_id)
            .expect("finish formerly running job");
        registry
            .finish_cancelled(&job_owner, &already_cancelling.job_id)
            .expect("finish formerly cancelling job");
        assert!(registry.nonterminal_jobs().is_empty());
    }

    #[test]
    fn deadline_expiration_applies_state_specific_transitions_atomically() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let registry =
            JobRegistry::open(temp.path().join("jobs.json")).expect("open deadline registry");
        let job_owner = owner(17);
        let make_job = |key: &str, hash: u8| {
            registry
                .submit(submission(
                    job_owner.clone(),
                    key,
                    hash,
                    StoredOperation::Verify {
                        archive: temp.path().join(format!("{key}.pithos")),
                    },
                ))
                .expect("submit deadline test job")
        };
        let queued = make_job("deadline-queued", 40);
        let running = make_job("deadline-running", 41);
        registry
            .transition(&job_owner, &running.job_id, ApiJobState::Running, "running")
            .expect("mark job running");
        let cancelling = make_job("deadline-cancelling", 42);
        registry
            .transition(
                &job_owner,
                &cancelling.job_id,
                ApiJobState::Running,
                "running",
            )
            .expect("mark cancelling job running");
        registry
            .request_cancel(&job_owner, &cancelling.job_id)
            .expect("put job in Cancelling");
        let cancelling_event_count = registry
            .events(&job_owner, &cancelling.job_id, 0)
            .expect("events before deadline")
            .len();
        let completed = make_job("deadline-completed", 43);
        registry
            .complete(
                &job_owner,
                &completed.job_id,
                serde_json::json!({"verified": true}),
            )
            .expect("complete terminal control job");

        assert!(
            !registry
                .expire_deadline(&job_owner, &queued.job_id)
                .expect("expire queued job")
        );
        assert!(
            registry
                .expire_deadline(&job_owner, &running.job_id)
                .expect("expire running job")
        );
        assert!(
            registry
                .expire_deadline(&job_owner, &cancelling.job_id)
                .expect("expire already-cancelling job")
        );
        assert!(
            !registry
                .expire_deadline(&job_owner, &completed.job_id)
                .expect("terminal deadline is a no-op")
        );

        let queued_snapshot = registry
            .snapshot(&job_owner, &queued.job_id)
            .expect("queued deadline snapshot");
        assert_eq!(queued_snapshot.state, ApiJobState::Failed);
        assert_eq!(
            queued_snapshot.error.expect("deadline error").kind,
            PublicErrorKind::ResourceLimit
        );
        assert_eq!(
            registry
                .snapshot(&job_owner, &running.job_id)
                .expect("running deadline snapshot")
                .state,
            ApiJobState::Cancelling
        );
        assert_eq!(
            registry
                .events(&job_owner, &cancelling.job_id, 0)
                .expect("events after deadline")
                .len(),
            cancelling_event_count,
            "an existing Cancelling state must not receive a duplicate event"
        );
        assert_eq!(
            registry
                .snapshot(&job_owner, &completed.job_id)
                .expect("completed deadline snapshot")
                .state,
            ApiJobState::Completed
        );
    }

    #[test]
    fn deadline_and_scheduler_transition_never_lose_the_atomic_state_race() {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().expect("temporary test directory");
        let registry =
            Arc::new(JobRegistry::open(temp.path().join("jobs.json")).expect("open race registry"));
        let job_owner = owner(18);
        for number in 0_u8..32 {
            let accepted = registry
                .submit(submission(
                    job_owner.clone(),
                    &format!("deadline-race-{number}"),
                    64 + number,
                    StoredOperation::Verify {
                        archive: temp.path().join(format!("race-{number}.pithos")),
                    },
                ))
                .expect("submit deadline race job");
            let barrier = Arc::new(Barrier::new(3));
            let transition_registry = Arc::clone(&registry);
            let transition_barrier = Arc::clone(&barrier);
            let transition_owner = job_owner.clone();
            let transition_job_id = accepted.job_id.clone();
            let transition = std::thread::spawn(move || {
                transition_barrier.wait();
                transition_registry.transition(
                    &transition_owner,
                    &transition_job_id,
                    ApiJobState::Running,
                    "running",
                )
            });
            let deadline_registry = Arc::clone(&registry);
            let deadline_barrier = Arc::clone(&barrier);
            let deadline_owner = job_owner.clone();
            let deadline_job_id = accepted.job_id.clone();
            let deadline = std::thread::spawn(move || {
                deadline_barrier.wait();
                deadline_registry.expire_deadline(&deadline_owner, &deadline_job_id)
            });
            barrier.wait();
            let transition_result = transition.join().expect("transition worker");
            let cancel_worker = deadline
                .join()
                .expect("deadline worker")
                .expect("atomic deadline mutation");
            let state = registry
                .snapshot(&job_owner, &accepted.job_id)
                .expect("deadline race snapshot")
                .state;

            if transition_result.is_ok() {
                assert!(cancel_worker);
                assert_eq!(state, ApiJobState::Cancelling);
                registry
                    .finish_cancelled(&job_owner, &accepted.job_id)
                    .expect("finish raced running job");
            } else {
                assert!(!cancel_worker);
                assert_eq!(state, ApiJobState::Failed);
            }
        }
    }

    #[test]
    fn open_rejects_dangling_idempotency_and_invalid_event_sequence() {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let store_path = temp.path().join("jobs.json");
        let job_owner = owner(7);
        let registry = JobRegistry::open(store_path.clone()).expect("open registry");
        registry
            .submit(submission(
                job_owner.clone(),
                "indexed-job",
                7,
                StoredOperation::Verify {
                    archive: temp.path().join("archive.pithos"),
                },
            ))
            .expect("submit indexed job");
        drop(registry);

        let mut state = load_state(&store_path).expect("load state");
        state.idempotency.insert(
            "session-deadbeef:dangling".to_owned(),
            "job-deadbeef".to_owned(),
        );
        write_unchecked_state(&store_path, &state);
        assert!(JobRegistry::open(store_path.clone()).is_err());

        state.idempotency.remove("session-deadbeef:dangling");
        let job = state.jobs.values_mut().next().expect("stored job");
        job.next_sequence = job.events.last().expect("queued event").sequence;
        write_unchecked_state(&store_path, &state);
        assert!(JobRegistry::open(store_path).is_err());
    }

    #[test]
    fn concurrent_submission_limit_is_checked_inside_the_store_transaction() {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().expect("temporary test directory");
        let registry = Arc::new(
            JobRegistry::open(temp.path().join("jobs.json")).expect("open bounded registry"),
        );
        let job_owner = owner(8);
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for number in 0_u8..2 {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            let job_owner = job_owner.clone();
            let archive = temp.path().join(format!("concurrent-{number}.pithos"));
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                registry.submit_with_limit(
                    submission(
                        job_owner,
                        &format!("concurrent-{number}"),
                        number,
                        StoredOperation::Verify { archive },
                    ),
                    1,
                )
            }));
        }
        barrier.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().expect("submission worker"))
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .filter(|error| error.kind == PublicErrorKind::ResourceLimit)
                .count(),
            1
        );
    }
}
