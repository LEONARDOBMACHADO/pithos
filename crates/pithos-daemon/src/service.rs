use crate::scheduler::{FairScheduler, ScheduledJob};
use crate::{
    JobRegistry, JobSubmission, PathAuthorizer, QuotaPolicy, SessionRegistry, StoredOperation,
};
use parking_lot::Mutex;
use pithos_agent_api::{
    AGENT_PROTOCOL_VERSION, ApiJobState, ApiProfile, ArchiveJobParams, CapabilitiesParams,
    CapabilitiesResult, EstimateParams, EstimateResult, EventsResult, ExtractParams, JobContext,
    JobId, JobStatusParams, JsonRpcError, JsonRpcResponse, ProtocolLimits, PublicErrorKind,
    ReadRangeParams, ReadRangeResult, RpcMethod, SessionCapability, SessionId,
    SubscribeEventsParams, UnpackParams, parse_request,
};
use pithos_core::{CompressionProfile, DecodeLimits, PithosError};
use pithos_engine::{
    CancellationToken, ExtractRequest, PackLimits, PackRequest, ReadRangeRequest, UnpackRequest,
    extract_with_control_and_limits, inspect_with_control, list_with_control,
    pack_with_limits_and_control, read_range_to_writer_with_control,
    unpack_with_control_and_temp_limit, verify_with_control,
};
use rand::Rng;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;

const MEMORY_PERMIT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub state_dir: PathBuf,
    pub allowed_scope: pithos_agent_api::PathScope,
    pub protocol_limits: ProtocolLimits,
    pub quota_policy: QuotaPolicy,
    pub max_concurrent_jobs: usize,
    pub max_connections: usize,
    pub request_rate_per_second: u32,
    pub request_burst: u32,
    pub session_ttl: Duration,
    pub max_event_wait: Duration,
    pub transfer_ttl: Duration,
}

impl DaemonConfig {
    pub fn new(state_dir: PathBuf) -> Self {
        let default_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            state_dir,
            allowed_scope: pithos_agent_api::PathScope {
                read_roots: vec![default_root.clone()],
                write_roots: vec![default_root],
            },
            protocol_limits: ProtocolLimits::default(),
            quota_policy: QuotaPolicy::default(),
            max_concurrent_jobs: 4,
            max_connections: 32,
            request_rate_per_second: 100,
            request_burst: 200,
            session_ttl: Duration::from_secs(8 * 60 * 60),
            max_event_wait: Duration::from_secs(30),
            transfer_ttl: Duration::from_secs(5 * 60),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(state_dir: PathBuf) -> Self {
        let allowed_root = state_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let mut config = Self::new(state_dir);
        config.allowed_scope = pithos_agent_api::PathScope {
            read_roots: vec![allowed_root.clone()],
            write_roots: vec![allowed_root],
        };
        config.max_concurrent_jobs = 2;
        config.request_rate_per_second = 10_000;
        config.request_burst = 10_000;
        config.max_event_wait = Duration::from_millis(100);
        config
    }

    fn validate(&self) -> Result<(), JsonRpcError> {
        let memory_permits = permits_for_bytes(self.quota_policy.maximum_job_limits.max_memory)?;
        if self.max_concurrent_jobs == 0
            || self.max_connections == 0
            || self.max_concurrent_jobs > Semaphore::MAX_PERMITS
            || self.max_connections > Semaphore::MAX_PERMITS
            || self.quota_policy.maximum_job_limits.max_threads == 0
            || u32::try_from(memory_permits).is_err()
            || memory_permits > Semaphore::MAX_PERMITS
        {
            return Err(JsonRpcError::invalid_params("invalid daemon configuration"));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct DaemonService {
    inner: Arc<Inner>,
}

struct Inner {
    config: DaemonConfig,
    registry: Arc<JobRegistry>,
    sessions: Arc<SessionRegistry>,
    allowed_authorizer: PathAuthorizer,
    connections: Mutex<HashMap<u64, ConnectionState>>,
    running: Mutex<HashMap<String, CancellationToken>>,
    job_slots: Arc<Semaphore>,
    thread_slots: Arc<Semaphore>,
    memory_slots: Arc<Semaphore>,
    scheduler: Mutex<SchedulerState>,
    storage_degraded: AtomicBool,
    stopping: AtomicBool,
}

struct RunningJobGuard {
    inner: Arc<Inner>,
    job_id: String,
}

impl Drop for RunningJobGuard {
    fn drop(&mut self) {
        self.inner.running.lock().remove(&self.job_id);
    }
}

#[derive(Default)]
struct SchedulerState {
    queue: FairScheduler,
    draining: bool,
}

struct ConnectionState {
    limiter: crate::ConnectionRateLimiter,
    session: Option<Session>,
}

#[derive(Clone)]
struct Session {
    session_id: SessionId,
    token_hash: [u8; 32],
    expires_at_unix_ms: u64,
    authorizer: PathAuthorizer,
}

impl DaemonService {
    pub fn open(mut config: DaemonConfig) -> Result<Self, JsonRpcError> {
        config.validate()?;
        config.state_dir = crate::transport::prepare_private_state_dir(&config.state_dir)
            .map_err(|_| internal("cannot prepare private daemon state directory"))?;
        let allowed_authorizer = PathAuthorizer::new(&config.allowed_scope)?;
        config.allowed_scope = allowed_authorizer.scope();
        cleanup_expired_transfers(&config.state_dir)?;
        let sessions = Arc::new(SessionRegistry::open(
            config.state_dir.join("sessions.json"),
        )?);
        let registry = Arc::new(JobRegistry::open_with_event_limit(
            config.state_dir.join("jobs.json"),
            config.quota_policy.max_events_per_job,
        )?);
        let thread_permits = usize::from(config.quota_policy.maximum_job_limits.max_threads);
        let memory_permits = permits_for_bytes(config.quota_policy.maximum_job_limits.max_memory)?;
        Ok(Self {
            inner: Arc::new(Inner {
                job_slots: Arc::new(Semaphore::new(config.max_concurrent_jobs)),
                thread_slots: Arc::new(Semaphore::new(thread_permits)),
                memory_slots: Arc::new(Semaphore::new(memory_permits)),
                config,
                registry,
                sessions,
                allowed_authorizer,
                connections: Mutex::new(HashMap::new()),
                running: Mutex::new(HashMap::new()),
                scheduler: Mutex::new(SchedulerState::default()),
                storage_degraded: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
            }),
        })
    }

    pub async fn handle_frame(&self, connection_id: u64, frame: &[u8]) -> Vec<u8> {
        if !self.inner.registry.is_healthy() {
            self.mark_storage_degraded();
        }
        if self.inner.storage_degraded.load(Ordering::Acquire)
            || self.inner.stopping.load(Ordering::Acquire)
        {
            return serialize_response(JsonRpcResponse::<Value>::error(
                None,
                internal("daemon persistence is unavailable"),
            ));
        }
        if let Err(error) = self.check_connection_rate(connection_id) {
            return serialize_response(JsonRpcResponse::<Value>::error(None, error));
        }
        let request = match parse_request(frame, &self.inner.config.protocol_limits) {
            Ok(request) => request,
            Err(error) => return serialize_response(JsonRpcResponse::<Value>::error(None, error)),
        };
        let id = request.id.clone();
        let response = match self
            .dispatch(connection_id, request.method, request.params)
            .await
        {
            Ok(result) => JsonRpcResponse::success(id, result),
            Err(error) => JsonRpcResponse::error(Some(id), error),
        };
        serialize_response(response)
    }

    pub fn disconnect(&self, connection_id: u64) {
        self.inner.connections.lock().remove(&connection_id);
    }

    pub async fn shutdown(&self, grace: Duration) -> Result<(), JsonRpcError> {
        self.inner.stopping.store(true, Ordering::Release);
        self.inner.scheduler.lock().queue.drain();

        let registry = Arc::clone(&self.inner.registry);
        let close_result =
            tokio::task::spawn_blocking(move || registry.close_and_cancel_all()).await;
        match close_result {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                self.mark_storage_degraded();
                return Err(error);
            }
            Err(_) => {
                self.mark_storage_degraded();
                return Err(internal("shutdown registry task failed"));
            }
        }

        for token in self.inner.running.lock().values() {
            token.cancel();
        }
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            if self.inner.registry.nonterminal_jobs().is_empty()
                && self.inner.running.lock().is_empty()
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(JsonRpcError::resource_limit(
                    "daemon shutdown grace period exceeded",
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub(crate) fn max_connections(&self) -> usize {
        self.inner.config.max_connections
    }

    pub(crate) fn max_frame_bytes(&self) -> usize {
        self.inner.config.protocol_limits.max_request_bytes
    }

    #[cfg(test)]
    pub(crate) fn registry_for_test(&self) -> Arc<JobRegistry> {
        Arc::clone(&self.inner.registry)
    }

    #[cfg(test)]
    pub(crate) async fn hold_execution_for_test(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.inner
            .job_slots
            .clone()
            .acquire_many_owned(self.inner.config.max_concurrent_jobs as u32)
            .await
            .expect("test service semaphore remains open")
    }

    async fn dispatch(
        &self,
        connection_id: u64,
        method: RpcMethod,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        match method {
            RpcMethod::Capabilities => {
                let params: CapabilitiesParams = decode_params(params)?;
                serde_json::to_value(self.open_session(connection_id, params).await?)
                    .map_err(|_| internal("cannot encode capabilities"))
            }
            RpcMethod::Estimate => {
                let params: EstimateParams = decode_params(params)?;
                let session = self.authenticate(connection_id, &params.capability_token)?;
                let eligible = !params.inputs.is_empty();
                let total = run_blocking(move || {
                    let request_authorizer = PathAuthorizer::new(&params.path_scope)?;
                    let mut total = 0_u64;
                    for input in &params.inputs {
                        let path = authorize_read(&session, &request_authorizer, input)?;
                        total = total
                            .checked_add(measure_path(&path)?)
                            .ok_or_else(|| JsonRpcError::resource_limit("input size overflow"))?;
                    }
                    Ok(total)
                })
                .await?;
                let result = EstimateResult {
                    input_bytes: total,
                    estimated_memory: total.clamp(64 * 1024, 256 * 1024 * 1024),
                    estimated_temp: total,
                    output_upper_bound: total,
                    eligible,
                };
                serde_json::to_value(result).map_err(|_| internal("cannot encode estimate"))
            }
            RpcMethod::Pack => {
                let params: pithos_agent_api::PackParams = decode_params(params)?;
                let session = self.authenticate(connection_id, &params.context.capability_token)?;
                self.validate_context(&session, &params.context)?;
                if params.inputs.is_empty() {
                    return Err(JsonRpcError::invalid_params("pack requires inputs"));
                }
                let path_session = session.clone();
                let request_scope = params.context.path_scope.clone();
                let requested_inputs = params.inputs;
                let requested_output = params.output;
                let (inputs, input_bytes, output) = run_blocking(move || {
                    let request_authorizer = PathAuthorizer::new(&request_scope)?;
                    let mut inputs = Vec::with_capacity(requested_inputs.len());
                    let mut input_bytes = 0_u64;
                    for input in &requested_inputs {
                        let path = authorize_read(&path_session, &request_authorizer, input)?;
                        input_bytes = input_bytes
                            .checked_add(measure_path(&path)?)
                            .ok_or_else(|| JsonRpcError::resource_limit("input size overflow"))?;
                        inputs.push(path);
                    }
                    let output =
                        authorize_write(&path_session, &request_authorizer, &requested_output)?;
                    Ok((inputs, input_bytes, output))
                })
                .await?;
                enforce_bound(
                    input_bytes,
                    params.context.limits.max_temp,
                    "temporary quota",
                )?;
                enforce_bound(
                    input_bytes,
                    params.context.limits.max_output,
                    "output quota",
                )?;
                let operation = StoredOperation::Pack {
                    inputs,
                    output,
                    profile: params.profile,
                };
                self.submit(session, RpcMethod::Pack, params.context, operation)
                    .await
            }
            RpcMethod::Unpack => {
                let params: UnpackParams = decode_params(params)?;
                let session = self.authenticate(connection_id, &params.context.capability_token)?;
                self.validate_context(&session, &params.context)?;
                let path_session = session.clone();
                let request_scope = params.context.path_scope.clone();
                let preflight_limits = params.context.limits.clone();
                let requested_archive = params.archive;
                let requested_output = params.output;
                let (archive, output, inspection) = run_blocking(move || {
                    let request_authorizer = PathAuthorizer::new(&request_scope)?;
                    let archive =
                        authorize_read(&path_session, &request_authorizer, &requested_archive)?;
                    let output =
                        authorize_write(&path_session, &request_authorizer, &requested_output)?;
                    let decode_limits = output_decode_limits(&preflight_limits);
                    let inspection =
                        inspect_with_control(&archive, &decode_limits, &CancellationToken::new())
                            .map_err(map_engine_error)?;
                    Ok((archive, output, inspection))
                })
                .await?;
                enforce_bound(
                    inspection.original_bytes,
                    params.context.limits.max_output,
                    "output quota",
                )?;
                enforce_bound(
                    inspection.original_bytes,
                    params.context.limits.max_temp,
                    "temporary quota",
                )?;
                self.submit(
                    session,
                    RpcMethod::Unpack,
                    params.context,
                    StoredOperation::Unpack { archive, output },
                )
                .await
            }
            RpcMethod::List | RpcMethod::Inspect | RpcMethod::Verify => {
                let params: ArchiveJobParams = decode_params(params)?;
                let session = self.authenticate(connection_id, &params.context.capability_token)?;
                self.validate_context(&session, &params.context)?;
                let path_session = session.clone();
                let request_scope = params.context.path_scope.clone();
                let requested_archive = params.archive;
                let archive = run_blocking(move || {
                    let request_authorizer = PathAuthorizer::new(&request_scope)?;
                    authorize_read(&path_session, &request_authorizer, &requested_archive)
                })
                .await?;
                let operation = match method {
                    RpcMethod::List => StoredOperation::List { archive },
                    RpcMethod::Inspect => StoredOperation::Inspect { archive },
                    RpcMethod::Verify => StoredOperation::Verify { archive },
                    _ => unreachable!(),
                };
                self.submit(session, method, params.context, operation)
                    .await
            }
            RpcMethod::Extract => {
                let params: ExtractParams = decode_params(params)?;
                let session = self.authenticate(connection_id, &params.context.capability_token)?;
                self.validate_context(&session, &params.context)?;
                let path_session = session.clone();
                let request_scope = params.context.path_scope.clone();
                let requested_archive = params.archive;
                let requested_output = params.output;
                let (archive, output) = run_blocking(move || {
                    let request_authorizer = PathAuthorizer::new(&request_scope)?;
                    let archive =
                        authorize_read(&path_session, &request_authorizer, &requested_archive)?;
                    let output =
                        authorize_write(&path_session, &request_authorizer, &requested_output)?;
                    Ok((archive, output))
                })
                .await?;
                self.submit(
                    session,
                    RpcMethod::Extract,
                    params.context,
                    StoredOperation::Extract {
                        archive,
                        entry: params.entry,
                        output,
                    },
                )
                .await
            }
            RpcMethod::ReadRange => {
                let params: ReadRangeParams = decode_params(params)?;
                let session = self.authenticate(connection_id, &params.context.capability_token)?;
                self.validate_context(&session, &params.context)?;
                if params.length > self.inner.config.quota_policy.max_read_range {
                    return Err(JsonRpcError::resource_limit("range quota exceeded"));
                }
                enforce_bound(
                    params.length,
                    params.context.limits.max_output,
                    "output quota",
                )?;
                enforce_bound(
                    params.length,
                    params.context.limits.max_temp,
                    "temporary quota",
                )?;
                let path_session = session.clone();
                let request_scope = params.context.path_scope.clone();
                let requested_archive = params.archive;
                let archive = run_blocking(move || {
                    let request_authorizer = PathAuthorizer::new(&request_scope)?;
                    authorize_read(&path_session, &request_authorizer, &requested_archive)
                })
                .await?;
                self.submit(
                    session,
                    RpcMethod::ReadRange,
                    params.context,
                    StoredOperation::ReadRange {
                        archive,
                        entry: params.entry,
                        offset: params.offset,
                        length: params.length,
                    },
                )
                .await
            }
            RpcMethod::Cancel => {
                let params: JobStatusParams = decode_params(params)?;
                let session = self.authenticate(connection_id, &params.capability_token)?;
                let registry = Arc::clone(&self.inner.registry);
                let worker_registry = Arc::clone(&registry);
                let owner = session.session_id.clone();
                let cancel_job_id = params.job_id.clone();
                let cancel_result =
                    run_blocking(move || worker_registry.request_cancel(&owner, &cancel_job_id))
                        .await;
                let state = match cancel_result {
                    Ok(state) => state,
                    Err(error) => {
                        if !registry.is_healthy() || error.kind == PublicErrorKind::Internal {
                            self.mark_storage_degraded();
                        }
                        return Err(error);
                    }
                };
                if let Some(token) = self.inner.running.lock().get(params.job_id.as_str()) {
                    token.cancel();
                }
                Ok(json!({"job_id": params.job_id, "state": state}))
            }
            RpcMethod::JobStatus => {
                let params: JobStatusParams = decode_params(params)?;
                let session = self.authenticate(connection_id, &params.capability_token)?;
                let snapshot = self
                    .inner
                    .registry
                    .snapshot(&session.session_id, &params.job_id)?;
                serde_json::to_value(snapshot).map_err(|_| internal("cannot encode job status"))
            }
            RpcMethod::SubscribeEvents => {
                let params: SubscribeEventsParams = decode_params(params)?;
                let session = self.authenticate(connection_id, &params.capability_token)?;
                let maximum = self.inner.config.max_event_wait.as_millis() as u64;
                let wait_ms = u64::from(params.wait_ms).min(maximum);
                let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
                let events = loop {
                    let events = self.inner.registry.events(
                        &session.session_id,
                        &params.job_id,
                        params.after_sequence,
                    )?;
                    let snapshot = self
                        .inner
                        .registry
                        .snapshot(&session.session_id, &params.job_id)?;
                    if !events.is_empty() || snapshot.state.is_terminal() || wait_ms == 0 {
                        break (events, snapshot.state.is_terminal());
                    }
                    if tokio::time::Instant::now() >= deadline {
                        break (Vec::new(), false);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                };
                let latest_sequence = events
                    .0
                    .last()
                    .map_or(params.after_sequence, |event| event.sequence);
                serde_json::to_value(EventsResult {
                    events: events.0,
                    latest_sequence,
                    terminal: events.1,
                })
                .map_err(|_| internal("cannot encode events"))
            }
        }
    }

    async fn open_session(
        &self,
        connection_id: u64,
        params: CapabilitiesParams,
    ) -> Result<CapabilitiesResult, JsonRpcError> {
        if params.protocol_version != AGENT_PROTOCOL_VERSION
            || params.client_name.is_empty()
            || params.client_name.len() > 128
        {
            return Err(JsonRpcError::invalid_params("unsupported client protocol"));
        }
        if self
            .inner
            .connections
            .lock()
            .get(&connection_id)
            .is_some_and(|connection| connection.session.is_some())
        {
            return Err(JsonRpcError::domain(
                PublicErrorKind::JobConflict,
                "connection already has a session",
            ));
        }
        let sessions = Arc::clone(&self.inner.sessions);
        let allowed = self.inner.allowed_authorizer.clone();
        let requested_scope = params.requested_scope;
        let now_unix_ms = unix_ms();
        let expires_at_unix_ms = now_unix_ms.saturating_add(
            self.inner
                .config
                .session_ttl
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        );
        let session_result = tokio::task::spawn_blocking(move || -> Result<_, JsonRpcError> {
            match params.resume {
                Some(resume) => {
                    let resumed = sessions.resume(
                        &resume.session_id,
                        &resume.resume_token,
                        now_unix_ms,
                        expires_at_unix_ms,
                    )?;
                    let authorizer = allowed.grant(&resumed.scope)?;
                    Ok((resume.session_id, resumed.resume_token, authorizer))
                }
                None => {
                    let authorizer = allowed.grant(&requested_scope)?;
                    let created =
                        sessions.create(authorizer.scope(), now_unix_ms, expires_at_unix_ms)?;
                    Ok((created.session_id, created.resume_token, authorizer))
                }
            }
        })
        .await;
        let (session_id, resume_token, authorizer) = match session_result {
            Ok(Ok(session)) => session,
            Ok(Err(error)) => {
                if error.kind == PublicErrorKind::Internal {
                    self.mark_storage_degraded();
                }
                return Err(error);
            }
            Err(_) => {
                self.mark_storage_degraded();
                return Err(internal("session registry task failed"));
            }
        };
        let capability_token = random_hex(32);
        let mut connections = self.inner.connections.lock();
        let connection = connections
            .get_mut(&connection_id)
            .ok_or_else(|| internal("connection state missing"))?;
        if connection.session.is_some() {
            return Err(JsonRpcError::domain(
                PublicErrorKind::JobConflict,
                "connection already has a session",
            ));
        }
        connection.session = Some(Session {
            session_id: session_id.clone(),
            token_hash: *blake3::hash(capability_token.as_bytes()).as_bytes(),
            expires_at_unix_ms,
            authorizer,
        });
        Ok(CapabilitiesResult {
            product: "Pithos R1".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            format_versions: vec!["PAF 0.1-draft".to_owned()],
            protocol_version: AGENT_PROTOCOL_VERSION,
            supported_methods: RpcMethod::ALL
                .iter()
                .map(|method| method.as_str().to_owned())
                .collect(),
            supported_codecs: ["STORE", "Zstandard", "Brotli", "LZMA2"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            supported_transforms: Vec::new(),
            supported_profiles: ["raw", "stream", "random", "balanced", "archive-max"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            mount: false,
            maximum_job_limits: self.inner.config.quota_policy.maximum_job_limits.clone(),
            session: SessionCapability {
                session_id,
                capability_token,
                resume_token,
                expires_at_unix_ms,
            },
        })
    }

    fn authenticate(&self, connection_id: u64, token: &str) -> Result<Session, JsonRpcError> {
        if token.len() != 64 {
            return Err(permission_denied());
        }
        let connections = self.inner.connections.lock();
        let session = connections
            .get(&connection_id)
            .and_then(|connection| connection.session.as_ref())
            .ok_or_else(permission_denied)?;
        if unix_ms() >= session.expires_at_unix_ms {
            return Err(permission_denied());
        }
        let supplied = blake3::hash(token.as_bytes());
        if !constant_time_equal(session.token_hash.as_ref(), supplied.as_bytes()) {
            return Err(permission_denied());
        }
        Ok(session.clone())
    }

    fn check_connection_rate(&self, connection_id: u64) -> Result<(), JsonRpcError> {
        let mut connections = self.inner.connections.lock();
        if !connections.contains_key(&connection_id)
            && connections.len() >= self.inner.config.max_connections
        {
            return Err(JsonRpcError::resource_limit("connection limit exceeded"));
        }
        let connection = connections
            .entry(connection_id)
            .or_insert_with(|| ConnectionState {
                limiter: crate::ConnectionRateLimiter::new(
                    self.inner.config.request_rate_per_second,
                    self.inner.config.request_burst,
                ),
                session: None,
            });
        if !connection.limiter.allow(unix_ms()) {
            return Err(JsonRpcError::resource_limit("request rate exceeded"));
        }
        Ok(())
    }

    fn validate_context(
        &self,
        _session: &Session,
        context: &JobContext,
    ) -> Result<(), JsonRpcError> {
        self.inner.config.quota_policy.validate(&context.limits)?;
        if context.idempotency_key.is_empty() || context.idempotency_key.len() > 256 {
            return Err(JsonRpcError::invalid_params("invalid idempotency key"));
        }
        if let Some(deadline) = context.limits.deadline_unix_ms
            && deadline <= unix_ms()
        {
            return Err(JsonRpcError::resource_limit("job deadline expired"));
        }
        Ok(())
    }

    async fn submit(
        &self,
        session: Session,
        method: RpcMethod,
        context: JobContext,
        operation: StoredOperation,
    ) -> Result<Value, JsonRpcError> {
        let priority = context.priority;
        let deadline_unix_ms = context.limits.deadline_unix_ms;
        let params_hash = operation_hash(method, &operation, &context.limits, priority)?;
        let submission = JobSubmission {
            owner: session.session_id.clone(),
            method,
            priority,
            idempotency_key: context.idempotency_key,
            params_hash,
            limits: context.limits,
            operation,
        };
        let registry = Arc::clone(&self.inner.registry);
        let max_jobs = self.inner.config.quota_policy.max_jobs_per_session;
        let worker_registry = Arc::clone(&registry);
        let submit_result = tokio::task::spawn_blocking(move || {
            worker_registry.submit_with_limit(submission, max_jobs)
        })
        .await;
        let accepted = match submit_result {
            Ok(Ok(accepted)) => accepted,
            Ok(Err(error)) => {
                if !registry.is_healthy() || error.kind == PublicErrorKind::Internal {
                    self.mark_storage_degraded();
                }
                return Err(error);
            }
            Err(_) => {
                self.mark_storage_degraded();
                return Err(internal("job registry task failed"));
            }
        };
        if !registry.is_healthy() {
            self.mark_storage_degraded();
            return Err(internal("daemon persistence is unavailable"));
        }
        if !accepted.idempotent_replay {
            self.schedule(
                session.session_id,
                accepted.job_id.clone(),
                priority,
                deadline_unix_ms,
            );
        }
        serde_json::to_value(accepted).map_err(|_| internal("cannot encode job acceptance"))
    }

    fn schedule(
        &self,
        owner: SessionId,
        job_id: JobId,
        priority: pithos_agent_api::JobPriority,
        deadline_unix_ms: Option<u64>,
    ) {
        let should_start = {
            let mut scheduler = self.inner.scheduler.lock();
            if self.inner.stopping.load(Ordering::Acquire) {
                return;
            }
            scheduler.queue.push(ScheduledJob {
                owner: owner.clone(),
                job_id: job_id.clone(),
                priority,
                deadline_unix_ms,
            });
            if scheduler.draining {
                false
            } else {
                scheduler.draining = true;
                true
            }
        };
        if should_start {
            let service = self.clone();
            tokio::spawn(async move {
                service.drain_scheduler().await;
            });
        }
        if let Some(deadline) = deadline_unix_ms {
            let service = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(deadline.saturating_sub(unix_ms()))).await;
                service.expire_job(owner, job_id).await;
            });
        }
    }

    async fn drain_scheduler(&self) {
        loop {
            {
                let mut scheduler = self.inner.scheduler.lock();
                if scheduler.queue.is_empty() {
                    scheduler.draining = false;
                    return;
                }
            }
            let job_permit = match self.inner.job_slots.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    self.inner.scheduler.lock().draining = false;
                    return;
                }
            };
            let next = self.inner.scheduler.lock().queue.pop();
            let Some(job) = next else {
                drop(job_permit);
                continue;
            };
            let service = self.clone();
            tokio::spawn(async move {
                service.execute_scheduled(job, job_permit).await;
            });
        }
    }

    async fn expire_job(&self, owner: SessionId, job_id: JobId) {
        let registry = Arc::clone(&self.inner.registry);
        let worker_registry = Arc::clone(&registry);
        let deadline_owner = owner.clone();
        let deadline_job_id = job_id.clone();
        let cancel_worker = run_blocking(move || {
            worker_registry.expire_deadline(&deadline_owner, &deadline_job_id)
        })
        .await;
        let cancel_worker = match cancel_worker {
            Ok(cancel_worker) => cancel_worker,
            Err(error) => {
                if !registry.is_healthy() || error.kind == PublicErrorKind::Internal {
                    self.mark_storage_degraded();
                }
                false
            }
        };
        if cancel_worker && let Some(token) = self.inner.running.lock().get(job_id.as_str()) {
            token.cancel();
        }
    }

    async fn execute_scheduled(
        &self,
        scheduled: ScheduledJob,
        job_permit: tokio::sync::OwnedSemaphorePermit,
    ) {
        let ScheduledJob {
            owner,
            job_id,
            deadline_unix_ms,
            ..
        } = scheduled;
        let token = CancellationToken::new();
        self.inner
            .running
            .lock()
            .insert(job_id.as_str().to_owned(), token.clone());
        let _running_job = RunningJobGuard {
            inner: Arc::clone(&self.inner),
            job_id: job_id.as_str().to_owned(),
        };
        let (operation, limits, _priority) = match self.inner.registry.operation(&owner, &job_id) {
            Ok(value) => value,
            Err(_) => return,
        };
        let is_pack = matches!(&operation, StoredOperation::Pack { .. });
        let snapshot = match self.inner.registry.snapshot(&owner, &job_id) {
            Ok(snapshot) => snapshot,
            Err(_) => return,
        };
        if snapshot.state != ApiJobState::Queued {
            self.inner.running.lock().remove(job_id.as_str());
            return;
        }
        if deadline_unix_ms.is_some_and(|deadline| deadline <= unix_ms()) {
            let registry = Arc::clone(&self.inner.registry);
            let deadline_owner = owner.clone();
            let deadline_job_id = job_id.clone();
            let result = run_blocking(move || {
                registry.fail(
                    &deadline_owner,
                    &deadline_job_id,
                    JsonRpcError::resource_limit("job deadline exceeded"),
                )
            })
            .await;
            if let Err(error) = result
                && (!self.inner.registry.is_healthy() || error.kind == PublicErrorKind::Internal)
            {
                self.mark_storage_degraded();
            }
            return;
        }
        let thread_permit = match acquire_resource_permits(
            self.inner.thread_slots.clone(),
            u32::from(limits.max_threads),
            &token,
            limits.deadline_unix_ms,
        )
        .await
        {
            Ok(permit) => permit,
            Err(wait_error) => {
                self.finish_wait_failure(&owner, &job_id, wait_error).await;
                self.inner.running.lock().remove(job_id.as_str());
                return;
            }
        };
        let memory_permits = permits_for_bytes(limits.max_memory).unwrap_or(usize::MAX);
        let memory_permits = match u32::try_from(memory_permits) {
            Ok(value) => value,
            Err(_) => return,
        };
        let memory_permit = match acquire_resource_permits(
            self.inner.memory_slots.clone(),
            memory_permits,
            &token,
            limits.deadline_unix_ms,
        )
        .await
        {
            Ok(permit) => permit,
            Err(wait_error) => {
                self.finish_wait_failure(&owner, &job_id, wait_error).await;
                self.inner.running.lock().remove(job_id.as_str());
                return;
            }
        };
        let registry = Arc::clone(&self.inner.registry);
        let transition_owner = owner.clone();
        let transition_job_id = job_id.clone();
        let transition = run_blocking(move || {
            registry.transition(
                &transition_owner,
                &transition_job_id,
                ApiJobState::Running,
                "running",
            )
        })
        .await;
        if let Err(error) = transition {
            if !self.inner.registry.is_healthy() || error.kind == PublicErrorKind::Internal {
                self.mark_storage_degraded();
            }
            return;
        }
        let state_dir = self.inner.config.state_dir.clone();
        let cleanup_state_dir = state_dir.clone();
        let transfer_ttl = self.inner.config.transfer_ttl;
        let result_limit = self
            .inner
            .config
            .protocol_limits
            .max_request_bytes
            .saturating_sub(1024)
            .max(1);
        let operation_limits = limits.clone();
        let worker_token = token.clone();
        let mut worker = tokio::task::spawn_blocking(move || {
            execute_operation(
                operation,
                &worker_token,
                &operation_limits,
                &state_dir,
                transfer_ttl,
                result_limit,
            )
        });
        let result = if let Some(deadline_ms) = limits.deadline_unix_ms {
            let remaining = deadline_ms.saturating_sub(unix_ms());
            tokio::select! {
                result = &mut worker => join_worker(result),
                _ = tokio::time::sleep(Duration::from_millis(remaining)) => {
                    token.cancel();
                    let registry = Arc::clone(&self.inner.registry);
                    let deadline_owner = owner.clone();
                    let deadline_job_id = job_id.clone();
                    let cancel_result = run_blocking(move || {
                        registry.request_cancel(&deadline_owner, &deadline_job_id)
                    }).await;
                    if let Err(error) = cancel_result
                        && (!self.inner.registry.is_healthy()
                            || error.kind == PublicErrorKind::Internal)
                    {
                        self.mark_storage_degraded();
                    }
                    match worker.await {
                        Ok(Ok(value)) => Ok(value),
                        Ok(Err(error)) if error.kind == PublicErrorKind::Cancelled => {
                            Err(JsonRpcError::resource_limit("job deadline exceeded"))
                        }
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(internal("job worker failed")),
                    }
                }
            }
        } else {
            join_worker(worker.await)
        };
        let cleanup = result
            .as_ref()
            .ok()
            .and_then(|value| transfer_cleanup_record(value, &cleanup_state_dir));
        let registry = Arc::clone(&self.inner.registry);
        let terminal_owner = owner.clone();
        let terminal_job_id = job_id.clone();
        let terminal_write = tokio::task::spawn_blocking(move || match result {
            Ok(value) => {
                if is_pack
                    && let Err(error) =
                        registry.record_pack_publication(&terminal_owner, &terminal_job_id)
                {
                    if !registry.is_healthy() {
                        return Err(error);
                    }
                    let terminal = registry
                        .snapshot(&terminal_owner, &terminal_job_id)
                        .is_ok_and(|snapshot| snapshot.state.is_terminal());
                    if terminal {
                        return Ok(());
                    }
                    return registry.fail(&terminal_owner, &terminal_job_id, error);
                }
                registry.complete(&terminal_owner, &terminal_job_id, value)
            }
            Err(error) if error.kind == PublicErrorKind::Cancelled => {
                let snapshot = registry.snapshot(&terminal_owner, &terminal_job_id);
                if matches!(
                    snapshot.map(|value| value.state),
                    Ok(ApiJobState::Cancelling)
                ) {
                    registry.finish_cancelled(&terminal_owner, &terminal_job_id)
                } else {
                    registry.fail(&terminal_owner, &terminal_job_id, error)
                }
            }
            Err(error) => registry.fail(&terminal_owner, &terminal_job_id, error),
        })
        .await
        .map_err(|_| internal("job registry task failed"))
        .and_then(|result| result);
        match terminal_write {
            Ok(()) => {
                if let Some((path, expires_at)) = cleanup {
                    schedule_transfer_cleanup(cleanup_state_dir, path, expires_at);
                }
            }
            Err(error) => {
                if let Some((path, expires_at)) = cleanup {
                    let completed = self
                        .inner
                        .registry
                        .snapshot(&owner, &job_id)
                        .is_ok_and(|snapshot| snapshot.state == ApiJobState::Completed);
                    if completed {
                        schedule_transfer_cleanup(cleanup_state_dir.clone(), path, expires_at);
                    } else {
                        let _ = remove_registered_transfer(&cleanup_state_dir, &path);
                    }
                }
                if !self.inner.registry.is_healthy() || error.kind == PublicErrorKind::Internal {
                    self.mark_storage_degraded();
                }
            }
        }
        self.inner.running.lock().remove(job_id.as_str());
        drop(memory_permit);
        drop(thread_permit);
        drop(job_permit);
    }

    async fn finish_wait_failure(
        &self,
        owner: &SessionId,
        job_id: &JobId,
        error: ResourceWaitError,
    ) {
        let registry = Arc::clone(&self.inner.registry);
        let owner = owner.clone();
        let job_id = job_id.clone();
        let result = run_blocking(move || {
            let snapshot = registry.snapshot(&owner, &job_id);
            if snapshot.is_ok_and(|snapshot| snapshot.state.is_terminal()) {
                return Ok(());
            }
            let error = match error {
                ResourceWaitError::Cancelled => {
                    JsonRpcError::domain(PublicErrorKind::Cancelled, "job cancelled")
                }
                ResourceWaitError::Deadline => {
                    JsonRpcError::resource_limit("job deadline exceeded")
                }
                ResourceWaitError::Closed => internal("resource scheduler closed"),
            };
            registry.fail(&owner, &job_id, error)
        })
        .await;
        if let Err(error) = result
            && (!self.inner.registry.is_healthy() || error.kind == PublicErrorKind::Internal)
        {
            self.mark_storage_degraded();
        }
    }

    fn mark_storage_degraded(&self) {
        let first_failure = !self.inner.storage_degraded.swap(true, Ordering::AcqRel);
        self.inner.stopping.store(true, Ordering::Release);
        self.inner.scheduler.lock().queue.drain();
        for token in self.inner.running.lock().values() {
            token.cancel();
        }
        if first_failure && let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let registry = Arc::clone(&self.inner.registry);
            runtime.spawn_blocking(move || {
                let _ = registry.close_and_cancel_all();
            });
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ResourceWaitError {
    Cancelled,
    Deadline,
    Closed,
}

async fn acquire_resource_permits(
    semaphore: Arc<Semaphore>,
    permits: u32,
    cancellation: &CancellationToken,
    deadline_unix_ms: Option<u64>,
) -> Result<tokio::sync::OwnedSemaphorePermit, ResourceWaitError> {
    loop {
        if cancellation.is_cancelled() {
            return Err(ResourceWaitError::Cancelled);
        }
        if deadline_unix_ms.is_some_and(|deadline| deadline <= unix_ms()) {
            return Err(ResourceWaitError::Deadline);
        }
        match semaphore.clone().try_acquire_many_owned(permits) {
            Ok(permit) => return Ok(permit),
            Err(tokio::sync::TryAcquireError::Closed) => return Err(ResourceWaitError::Closed),
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

fn execute_operation(
    operation: StoredOperation,
    cancellation: &CancellationToken,
    limits: &pithos_agent_api::JobLimits,
    state_dir: &Path,
    transfer_ttl: Duration,
    result_limit: usize,
) -> Result<Value, JsonRpcError> {
    revalidate_operation_paths(&operation)?;
    let metadata_decode_limits = metadata_decode_limits(limits);
    let output_decode_limits = output_decode_limits(limits);
    let metadata_budget = metadata_decode_limits.max_metadata_bytes;
    let entry_budget = metadata_decode_limits.max_entries;
    let metadata_result = matches!(
        &operation,
        StoredOperation::List { .. }
            | StoredOperation::Inspect { .. }
            | StoredOperation::Verify { .. }
    );
    let value = match operation {
        StoredOperation::Pack {
            inputs,
            output,
            profile,
        } => {
            pack_with_limits_and_control(
                PackRequest {
                    inputs,
                    output: output.clone(),
                    profile: match profile {
                        ApiProfile::Raw => CompressionProfile::Raw,
                        ApiProfile::Stream => CompressionProfile::Stream,
                        ApiProfile::Random => CompressionProfile::Random,
                        ApiProfile::Balanced => CompressionProfile::Balanced,
                        ApiProfile::ArchiveMax => CompressionProfile::ArchiveMax,
                    },
                },
                &PackLimits {
                    max_input_bytes: limits.max_output.min(limits.max_temp),
                    max_memory_bytes: limits.max_memory,
                    max_temp_bytes: limits.max_temp,
                    max_output_bytes: limits.max_output,
                    max_metadata_bytes: metadata_budget,
                    max_entries: entry_budget,
                },
                cancellation,
            )
            .map_err(map_engine_error)?;
            json!({"archive": output})
        }
        StoredOperation::Unpack { archive, output } => {
            unpack_with_control_and_temp_limit(
                UnpackRequest {
                    archive,
                    output_dir: output.clone(),
                },
                &output_decode_limits,
                limits.max_temp,
                cancellation,
            )
            .map_err(map_engine_error)?;
            json!({"output": output})
        }
        StoredOperation::List { archive } => serde_json::to_value(
            list_with_control(&archive, &metadata_decode_limits, cancellation)
                .map_err(map_engine_error)?,
        )
        .map_err(|_| internal("cannot encode list result"))?,
        StoredOperation::Inspect { archive } => serde_json::to_value(
            inspect_with_control(&archive, &metadata_decode_limits, cancellation)
                .map_err(map_engine_error)?,
        )
        .map_err(|_| internal("cannot encode inspect result"))?,
        StoredOperation::Extract {
            archive,
            entry,
            output,
        } => serde_json::to_value(
            extract_with_control_and_limits(
                ExtractRequest {
                    archive,
                    entry,
                    output_dir: output,
                },
                &metadata_decode_limits,
                limits.max_output,
                limits.max_temp,
                cancellation,
            )
            .map_err(map_engine_error)?,
        )
        .map_err(|_| internal("cannot encode extract result"))?,
        StoredOperation::ReadRange {
            archive,
            entry,
            offset,
            length,
        } => {
            let transfers = state_dir.join("transfers");
            fs::create_dir_all(&transfers)
                .map_err(|_| internal("cannot create transfer directory"))?;
            let mut temporary = tempfile::NamedTempFile::new_in(&transfers)
                .map_err(|_| internal("cannot create range transfer"))?;
            let report = read_range_to_writer_with_control(
                ReadRangeRequest {
                    archive,
                    entry,
                    offset,
                    length,
                },
                &mut temporary,
                &metadata_decode_limits,
                cancellation,
            )
            .map_err(map_engine_error)?;
            temporary
                .flush()
                .and_then(|_| temporary.as_file().sync_all())
                .map_err(|_| internal("cannot sync range transfer"))?;
            let transfer_id = random_hex(16);
            let expires_at_unix_ms =
                unix_ms().saturating_add(transfer_ttl.as_millis().try_into().unwrap_or(u64::MAX));
            let path = transfers.join(format!("range-{expires_at_unix_ms}-{transfer_id}.bin"));
            temporary
                .persist_noclobber(&path)
                .map_err(|_| internal("cannot publish range transfer"))?;
            serde_json::to_value(ReadRangeResult {
                transfer_id,
                path,
                offset: report.offset,
                length: report.length,
                blake3: hex(&report.blake3),
                expires_at_unix_ms,
            })
            .map_err(|_| internal("cannot encode range transfer"))?
        }
        StoredOperation::Verify { archive } => serde_json::to_value(
            verify_with_control(&archive, &metadata_decode_limits, cancellation)
                .map_err(map_engine_error)?,
        )
        .map_err(|_| internal("cannot encode verify result"))?,
    };
    let encoded = serde_json::to_vec(&value).map_err(|_| internal("cannot encode job result"))?;
    if encoded.len() > result_limit || (metadata_result && encoded.len() as u64 > limits.max_output)
    {
        return Err(JsonRpcError::resource_limit(
            "job result exceeds output quota",
        ));
    }
    Ok(value)
}

fn revalidate_operation_paths(operation: &StoredOperation) -> Result<(), JsonRpcError> {
    match operation {
        StoredOperation::Pack { inputs, output, .. } => {
            for input in inputs {
                PathAuthorizer::revalidate_read(input)?;
            }
            PathAuthorizer::revalidate_write(output)
        }
        StoredOperation::Unpack { archive, output }
        | StoredOperation::Extract {
            archive, output, ..
        } => {
            PathAuthorizer::revalidate_read(archive)?;
            PathAuthorizer::revalidate_write(output)
        }
        StoredOperation::List { archive }
        | StoredOperation::Inspect { archive }
        | StoredOperation::ReadRange { archive, .. }
        | StoredOperation::Verify { archive } => PathAuthorizer::revalidate_read(archive),
    }
}

fn metadata_decode_limits(limits: &pithos_agent_api::JobLimits) -> DecodeLimits {
    let defaults = DecodeLimits::default();
    let entry_budget = (limits.max_memory / 512).max(1).min(defaults.max_entries);
    DecodeLimits {
        max_entries: entry_budget,
        max_groups: entry_budget.min(defaults.max_groups),
        max_chunks: entry_budget.min(defaults.max_chunks),
        max_metadata_bytes: (limits.max_memory / 2)
            .max(1)
            .min(defaults.max_metadata_bytes),
        ..defaults
    }
}

fn output_decode_limits(limits: &pithos_agent_api::JobLimits) -> DecodeLimits {
    let defaults = DecodeLimits::default();
    DecodeLimits {
        max_original_bytes: limits.max_output,
        max_group_output: limits.max_output.min(defaults.max_group_output),
        ..metadata_decode_limits(limits)
    }
}

fn decode_params<T: DeserializeOwned>(value: Value) -> Result<T, JsonRpcError> {
    serde_json::from_value(value).map_err(|_| JsonRpcError::invalid_params("invalid parameters"))
}

async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, JsonRpcError> + Send + 'static,
) -> Result<T, JsonRpcError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| internal("blocking daemon task failed"))?
}

fn authorize_read(
    session: &Session,
    request: &PathAuthorizer,
    path: &Path,
) -> Result<PathBuf, JsonRpcError> {
    let path = session.authorizer.authorize_read(path)?;
    request.authorize_read(&path)
}

fn authorize_write(
    session: &Session,
    request: &PathAuthorizer,
    path: &Path,
) -> Result<PathBuf, JsonRpcError> {
    let path = session.authorizer.authorize_write(path)?;
    request.authorize_write(&path)
}

fn measure_path(path: &Path) -> Result<u64, JsonRpcError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| permission_denied())?;
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Err(JsonRpcError::domain(
            PublicErrorKind::UnsupportedFeature,
            "unsupported input type",
        ));
    }
    let mut total = 0_u64;
    let mut pending = vec![path.to_path_buf()];
    let mut entries = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).map_err(|_| permission_denied())? {
            let entry = entry.map_err(|_| permission_denied())?;
            entries = entries.saturating_add(1);
            if entries > 10_000_000 {
                return Err(JsonRpcError::resource_limit("input entry limit exceeded"));
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(|_| permission_denied())?;
            if metadata.is_file() {
                total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| JsonRpcError::resource_limit("input size overflow"))?;
            } else if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(entry.path());
            }
        }
    }
    Ok(total)
}

fn transfer_cleanup_record(value: &Value, state_dir: &Path) -> Option<(PathBuf, u64)> {
    let path = PathBuf::from(value.get("path")?.as_str()?);
    let expires_at = value.get("expires_at_unix_ms")?.as_u64()?;
    registered_transfer_path(state_dir, &path).then_some((path, expires_at))
}

fn schedule_transfer_cleanup(state_dir: PathBuf, path: PathBuf, expires_at_unix_ms: u64) {
    tokio::spawn(async move {
        let wait = expires_at_unix_ms.saturating_sub(unix_ms());
        tokio::time::sleep(Duration::from_millis(wait)).await;
        let _ = remove_registered_transfer(&state_dir, &path);
    });
}

fn cleanup_expired_transfers(state_dir: &Path) -> Result<(), JsonRpcError> {
    let directory = state_dir.join("transfers");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(internal("cannot inspect transfer directory")),
    };
    for entry in entries {
        let entry = entry.map_err(|_| internal("cannot inspect transfer"))?;
        let path = entry.path();
        let Some(expires_at) = transfer_expiry_from_name(&path) else {
            continue;
        };
        if expires_at <= unix_ms() {
            remove_registered_transfer(state_dir, &path)?;
        } else if tokio::runtime::Handle::try_current().is_ok() {
            schedule_transfer_cleanup(state_dir.to_path_buf(), path, expires_at);
        }
    }
    Ok(())
}

fn transfer_expiry_from_name(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("range-")?;
    let (expiry, suffix) = rest.split_once('-')?;
    if !suffix.ends_with(".bin") || suffix.len() != 36 {
        return None;
    }
    expiry.parse().ok()
}

fn registered_transfer_path(state_dir: &Path, path: &Path) -> bool {
    path.parent() == Some(state_dir.join("transfers").as_path())
        && transfer_expiry_from_name(path).is_some()
}

fn remove_registered_transfer(state_dir: &Path, path: &Path) -> Result<(), JsonRpcError> {
    if !registered_transfer_path(state_dir, path) {
        return Err(permission_denied());
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            fs::remove_file(path).map_err(|_| internal("cannot remove expired transfer"))
        }
        Ok(_) => Err(permission_denied()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(internal("cannot inspect expired transfer")),
    }
}

fn operation_hash(
    method: RpcMethod,
    operation: &StoredOperation,
    limits: &pithos_agent_api::JobLimits,
    priority: pithos_agent_api::JobPriority,
) -> Result<[u8; 32], JsonRpcError> {
    let bytes = serde_json::to_vec(&(method, operation, limits, priority))
        .map_err(|_| internal("cannot canonicalize job parameters"))?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

fn enforce_bound(value: u64, maximum: u64, label: &'static str) -> Result<(), JsonRpcError> {
    if value > maximum {
        Err(JsonRpcError::resource_limit(label))
    } else {
        Ok(())
    }
}

fn random_hex(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    rand::rng().fill(value.as_mut_slice());
    hex(&value)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn permits_for_bytes(bytes: u64) -> Result<usize, JsonRpcError> {
    let permits = bytes
        .saturating_add(MEMORY_PERMIT_BYTES - 1)
        .checked_div(MEMORY_PERMIT_BYTES)
        .unwrap_or(0)
        .max(1);
    usize::try_from(permits).map_err(|_| JsonRpcError::resource_limit("memory quota too large"))
}

fn join_worker(
    result: Result<Result<Value, JsonRpcError>, tokio::task::JoinError>,
) -> Result<Value, JsonRpcError> {
    result.map_err(|_| internal("job worker failed"))?
}

fn serialize_response(response: JsonRpcResponse<Value>) -> Vec<u8> {
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        br#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"internal error","kind":"internal"}}"#
            .to_vec()
    })
}

fn map_engine_error(error: PithosError) -> JsonRpcError {
    let kind = match error {
        PithosError::InvalidMagic
        | PithosError::InvalidRange
        | PithosError::OverlappingSections
        | PithosError::MissingSection(_)
        | PithosError::DuplicateSection
        | PithosError::InvalidMetadata(_) => PublicErrorKind::CorruptArchive,
        PithosError::ChecksumMismatch | PithosError::HashMismatch => {
            PublicErrorKind::IntegrityMismatch
        }
        PithosError::UnsupportedContainerVersion => PublicErrorKind::UnsupportedFormat,
        PithosError::UnsupportedCodec | PithosError::UnsupportedFileType => {
            PublicErrorKind::UnsupportedFeature
        }
        PithosError::UnsafePath | PithosError::UnsafeSymlink => PublicErrorKind::UnsafePath,
        PithosError::ResourceLimit(_)
        | PithosError::MemoryLimit
        | PithosError::TemporarySpaceLimit
        | PithosError::IntegerOverflow => PublicErrorKind::ResourceLimit,
        PithosError::InputChanged => PublicErrorKind::InputChanged,
        PithosError::Cancelled => PublicErrorKind::Cancelled,
        PithosError::OutputExists => PublicErrorKind::JobConflict,
        PithosError::InvalidPathEncoding
        | PithosError::DependencyCycle
        | PithosError::DependencyDepthExceeded
        | PithosError::CandidateExceededIncumbent
        | PithosError::Io(_) => PublicErrorKind::Internal,
    };
    let message = match kind {
        PublicErrorKind::CorruptArchive => "archive is corrupt",
        PublicErrorKind::IntegrityMismatch => "archive integrity mismatch",
        PublicErrorKind::UnsupportedFormat => "unsupported archive format",
        PublicErrorKind::UnsupportedFeature => "unsupported feature",
        PublicErrorKind::UnsafePath => "unsafe path",
        PublicErrorKind::ResourceLimit => "resource limit exceeded",
        PublicErrorKind::InputChanged => "input changed during processing",
        PublicErrorKind::Cancelled => "job cancelled",
        PublicErrorKind::JobConflict => "job output conflict",
        _ => "internal operation error",
    };
    JsonRpcError::domain(kind, message)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn permission_denied() -> JsonRpcError {
    JsonRpcError::domain(PublicErrorKind::PermissionDenied, "permission denied")
}

fn internal(message: &'static str) -> JsonRpcError {
    JsonRpcError::domain(PublicErrorKind::Internal, message)
}
