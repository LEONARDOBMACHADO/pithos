use pithos_agent_api::{
    AGENT_PROTOCOL_VERSION, ApiJobState, ApiProfile, ArchiveJobParams, CapabilitiesParams,
    CapabilitiesResult, ExtractParams, JobAccepted, JobContext, JobPriority, JobSnapshot,
    JsonRpcError, JsonRpcResponse, PackParams, PathScope, PublicErrorKind, ReadRangeParams,
    ReadRangeResult, RpcId, RpcMethod, SessionResume, UnpackParams,
};
use pithos_daemon::{IpcClient, IpcEndpoint};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RECONNECT_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(windows)]
const RECONNECT_ATTEMPTS: usize = 5;
#[cfg(not(windows))]
const RECONNECT_ATTEMPTS: usize = 50;
const CLIENT_MEMORY_RESERVATION: u64 = 256 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DaemonClientError {
    #[error("cannot communicate with the local pithos daemon: {0}")]
    Io(#[from] io::Error),
    #[error("invalid JSON exchanged with the local pithos daemon: {0}")]
    Json(#[from] serde_json::Error),
    #[error("daemon rejected the request ({kind:?}): {message}")]
    Rpc {
        kind: PublicErrorKind,
        message: String,
    },
    #[error("invalid response from the local pithos daemon: {0}")]
    Protocol(&'static str),
}

impl DaemonClientError {
    pub fn public_error(&self) -> Option<(PublicErrorKind, &str)> {
        match self {
            Self::Rpc { kind, message } => Some((*kind, message)),
            _ => None,
        }
    }
}

impl From<JsonRpcError> for DaemonClientError {
    fn from(error: JsonRpcError) -> Self {
        Self::Rpc {
            kind: error.kind,
            message: error.message,
        }
    }
}

pub struct DaemonClient {
    endpoint: IpcEndpoint,
    stream: IpcClient,
    requested_scope: PathScope,
    capabilities: CapabilitiesResult,
    next_request_id: i64,
    next_job_number: u64,
}

impl DaemonClient {
    pub async fn connect(
        state_dir: PathBuf,
        requested_scope: PathScope,
    ) -> Result<Self, DaemonClientError> {
        let endpoint = IpcEndpoint::for_state_dir(state_dir);
        let mut stream = connect_with_retry(&endpoint).await?;
        let id = RpcId::Number(1);
        let params = CapabilitiesParams {
            client_name: format!("pithos-cli/{}", env!("CARGO_PKG_VERSION")),
            protocol_version: AGENT_PROTOCOL_VERSION,
            requested_scope: requested_scope.clone(),
            resume: None,
        };
        let response = request_value(&mut stream, RpcMethod::Capabilities, &params, &id).await?;
        let capabilities = decode_response(response, &id)?;
        validate_capabilities(&capabilities)?;
        Ok(Self {
            endpoint,
            stream,
            requested_scope,
            capabilities,
            next_request_id: 2,
            next_job_number: 1,
        })
    }

    pub fn public_capabilities(&self) -> Result<Value, DaemonClientError> {
        let mut value = serde_json::to_value(&self.capabilities)?;
        let session = value
            .get_mut("session")
            .and_then(Value::as_object_mut)
            .ok_or(DaemonClientError::Protocol("missing session capability"))?;
        session.remove("capability_token");
        session.remove("resume_token");
        Ok(value)
    }

    pub async fn pack(
        &mut self,
        inputs: Vec<PathBuf>,
        output: PathBuf,
        scope: PathScope,
        profile: ApiProfile,
    ) -> Result<Value, DaemonClientError> {
        let key = self.next_idempotency_key();
        let limits = client_job_limits(&self.capabilities.maximum_job_limits);
        self.submit_and_wait(RpcMethod::Pack, move |token| {
            serde_json::to_value(PackParams {
                context: job_context(token, &key, &scope, &limits, JobPriority::PackForeground),
                inputs: inputs.clone(),
                output: output.clone(),
                profile: profile.clone(),
            })
        })
        .await
    }

    pub async fn unpack(
        &mut self,
        archive: PathBuf,
        output: PathBuf,
        scope: PathScope,
    ) -> Result<Value, DaemonClientError> {
        let key = self.next_idempotency_key();
        let limits = client_job_limits(&self.capabilities.maximum_job_limits);
        self.submit_and_wait(RpcMethod::Unpack, move |token| {
            serde_json::to_value(UnpackParams {
                context: job_context(
                    token,
                    &key,
                    &scope,
                    &limits,
                    JobPriority::InteractiveExtract,
                ),
                archive: archive.clone(),
                output: output.clone(),
            })
        })
        .await
    }

    pub async fn list(
        &mut self,
        archive: PathBuf,
        scope: PathScope,
    ) -> Result<Value, DaemonClientError> {
        self.archive_job(
            RpcMethod::List,
            archive,
            scope,
            JobPriority::InteractiveRead,
        )
        .await
    }

    pub async fn inspect(
        &mut self,
        archive: PathBuf,
        scope: PathScope,
    ) -> Result<Value, DaemonClientError> {
        self.archive_job(
            RpcMethod::Inspect,
            archive,
            scope,
            JobPriority::InteractiveRead,
        )
        .await
    }

    pub async fn verify(
        &mut self,
        archive: PathBuf,
        scope: PathScope,
    ) -> Result<Value, DaemonClientError> {
        self.archive_job(
            RpcMethod::Verify,
            archive,
            scope,
            JobPriority::VerifyRequested,
        )
        .await
    }

    pub async fn extract(
        &mut self,
        archive: PathBuf,
        entry: PathBuf,
        output: PathBuf,
        scope: PathScope,
    ) -> Result<Value, DaemonClientError> {
        let key = self.next_idempotency_key();
        let limits = client_job_limits(&self.capabilities.maximum_job_limits);
        self.submit_and_wait(RpcMethod::Extract, move |token| {
            serde_json::to_value(ExtractParams {
                context: job_context(
                    token,
                    &key,
                    &scope,
                    &limits,
                    JobPriority::InteractiveExtract,
                ),
                archive: archive.clone(),
                entry: entry.clone(),
                output: output.clone(),
            })
        })
        .await
    }

    pub async fn read_range(
        &mut self,
        archive: PathBuf,
        entry: PathBuf,
        offset: u64,
        length: u64,
        scope: PathScope,
    ) -> Result<ReadRangeResult, DaemonClientError> {
        let key = self.next_idempotency_key();
        let limits = client_job_limits(&self.capabilities.maximum_job_limits);
        let result = self
            .submit_and_wait(RpcMethod::ReadRange, move |token| {
                serde_json::to_value(ReadRangeParams {
                    context: job_context(
                        token,
                        &key,
                        &scope,
                        &limits,
                        JobPriority::InteractiveRead,
                    ),
                    archive: archive.clone(),
                    entry: entry.clone(),
                    offset,
                    length,
                })
            })
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    async fn archive_job(
        &mut self,
        method: RpcMethod,
        archive: PathBuf,
        scope: PathScope,
        priority: JobPriority,
    ) -> Result<Value, DaemonClientError> {
        let key = self.next_idempotency_key();
        let limits = client_job_limits(&self.capabilities.maximum_job_limits);
        self.submit_and_wait(method, move |token| {
            serde_json::to_value(ArchiveJobParams {
                context: job_context(token, &key, &scope, &limits, priority),
                archive: archive.clone(),
            })
        })
        .await
    }

    async fn submit_and_wait<F>(
        &mut self,
        method: RpcMethod,
        build_params: F,
    ) -> Result<Value, DaemonClientError>
    where
        F: Fn(&str) -> Result<Value, serde_json::Error>,
    {
        let accepted: JobAccepted = self.authenticated_request(method, &build_params).await?;
        loop {
            let job_id = accepted.job_id.clone();
            let snapshot: JobSnapshot = self
                .authenticated_request(RpcMethod::JobStatus, move |token| {
                    Ok(json!({"capability_token": token, "job_id": job_id}))
                })
                .await?;
            if snapshot.job_id != accepted.job_id || snapshot.operation != method {
                return Err(DaemonClientError::Protocol("job identity mismatch"));
            }
            match snapshot.state {
                ApiJobState::Completed => {
                    return snapshot
                        .result
                        .ok_or(DaemonClientError::Protocol("completed job has no result"));
                }
                ApiJobState::Failed => {
                    return Err(snapshot
                        .error
                        .ok_or(DaemonClientError::Protocol("failed job has no error"))?
                        .into());
                }
                ApiJobState::Cancelled => {
                    return Err(DaemonClientError::Rpc {
                        kind: PublicErrorKind::Cancelled,
                        message: "job was cancelled".to_owned(),
                    });
                }
                ApiJobState::Queued | ApiJobState::Running | ApiJobState::Cancelling => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    async fn authenticated_request<T, F>(
        &mut self,
        method: RpcMethod,
        build_params: F,
    ) -> Result<T, DaemonClientError>
    where
        T: DeserializeOwned,
        F: Fn(&str) -> Result<Value, serde_json::Error>,
    {
        if self.session_expired() {
            self.reconnect().await?;
        }
        let id = self.take_request_id();
        let first = self.request_once(method, &build_params, &id).await;
        match first {
            Ok(value) => Ok(value),
            Err(DaemonClientError::Io(_)) => {
                self.reconnect().await?;
                self.request_once(method, &build_params, &id).await
            }
            Err(
                error @ DaemonClientError::Rpc {
                    kind: PublicErrorKind::PermissionDenied,
                    ..
                },
            ) => {
                if self.session_expired() {
                    self.reconnect().await?;
                    self.request_once(method, &build_params, &id).await
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn request_once<T, F>(
        &mut self,
        method: RpcMethod,
        build_params: &F,
        id: &RpcId,
    ) -> Result<T, DaemonClientError>
    where
        T: DeserializeOwned,
        F: Fn(&str) -> Result<Value, serde_json::Error>,
    {
        let params = build_params(&self.capabilities.session.capability_token)?;
        let response = request_value(&mut self.stream, method, &params, id).await?;
        decode_response(response, id)
    }

    async fn reconnect(&mut self) -> Result<(), DaemonClientError> {
        let mut stream = connect_with_retry(&self.endpoint).await?;
        let id = self.take_request_id();
        let expected_session_id = self.capabilities.session.session_id.clone();
        let params = CapabilitiesParams {
            client_name: format!("pithos-cli/{}", env!("CARGO_PKG_VERSION")),
            protocol_version: AGENT_PROTOCOL_VERSION,
            requested_scope: self.requested_scope.clone(),
            resume: Some(SessionResume {
                session_id: self.capabilities.session.session_id.clone(),
                resume_token: self.capabilities.session.resume_token.clone(),
            }),
        };
        let response = request_value(&mut stream, RpcMethod::Capabilities, &params, &id).await?;
        let capabilities = decode_response(response, &id)?;
        validate_capabilities(&capabilities)?;
        validate_resumed_session(&expected_session_id, &capabilities.session.session_id)?;
        self.stream = stream;
        self.capabilities = capabilities;
        Ok(())
    }

    fn next_idempotency_key(&mut self) -> String {
        let number = self.next_job_number;
        self.next_job_number = self.next_job_number.saturating_add(1);
        format!(
            "cli-{}-{number}",
            self.capabilities.session.session_id.as_str()
        )
    }

    fn take_request_id(&mut self) -> RpcId {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        RpcId::Number(id)
    }

    fn session_expired(&self) -> bool {
        session_expired_at(self.capabilities.session.expires_at_unix_ms, unix_ms())
    }
}

fn job_context(
    token: &str,
    key: &str,
    scope: &PathScope,
    limits: &pithos_agent_api::JobLimits,
    priority: JobPriority,
) -> JobContext {
    JobContext {
        capability_token: token.to_owned(),
        idempotency_key: key.to_owned(),
        limits: limits.clone(),
        path_scope: scope.clone(),
        priority,
    }
}

fn client_job_limits(maximum: &pithos_agent_api::JobLimits) -> pithos_agent_api::JobLimits {
    pithos_agent_api::JobLimits {
        max_threads: maximum.max_threads.min(1),
        max_memory: maximum.max_memory.min(CLIENT_MEMORY_RESERVATION),
        max_temp: maximum.max_temp,
        max_output: maximum.max_output,
        deadline_unix_ms: None,
    }
}

async fn request_value<P: serde::Serialize>(
    stream: &mut IpcClient,
    method: RpcMethod,
    params: &P,
    id: &RpcId,
) -> Result<Value, DaemonClientError> {
    let request = json!({
        "jsonrpc": "2.0",
        "method": method.as_str(),
        "params": params,
        "id": id,
    });
    Ok(stream.request(&request).await?)
}

fn decode_response<T: DeserializeOwned>(
    value: Value,
    expected_id: &RpcId,
) -> Result<T, DaemonClientError> {
    match serde_json::from_value::<JsonRpcResponse<T>>(value)? {
        JsonRpcResponse::Success(success) => {
            if success.jsonrpc != "2.0" || &success.id != expected_id {
                return Err(DaemonClientError::Protocol("JSON-RPC response mismatch"));
            }
            Ok(success.result)
        }
        JsonRpcResponse::Error(failure) => {
            if failure.jsonrpc != "2.0" || failure.id.as_ref() != Some(expected_id) {
                return Err(DaemonClientError::Protocol("JSON-RPC response mismatch"));
            }
            Err(failure.error.into())
        }
    }
}

fn validate_capabilities(value: &CapabilitiesResult) -> Result<(), DaemonClientError> {
    if value.protocol_version != AGENT_PROTOCOL_VERSION
        || value.session.capability_token.len() != 64
        || value.session.resume_token.len() != 64
        || value.maximum_job_limits.max_threads == 0
        || value.maximum_job_limits.max_memory == 0
        || value.maximum_job_limits.max_temp == 0
        || value.maximum_job_limits.max_output == 0
    {
        return Err(DaemonClientError::Protocol(
            "incompatible daemon capabilities",
        ));
    }
    Ok(())
}

fn validate_resumed_session(
    expected: &pithos_agent_api::SessionId,
    actual: &pithos_agent_api::SessionId,
) -> Result<(), DaemonClientError> {
    if actual != expected {
        return Err(DaemonClientError::Protocol(
            "daemon changed the resumed session identity",
        ));
    }
    Ok(())
}

fn session_expired_at(expires_at_unix_ms: u64, now_unix_ms: u64) -> bool {
    now_unix_ms >= expires_at_unix_ms
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn connect_with_retry(endpoint: &IpcEndpoint) -> Result<IpcClient, DaemonClientError> {
    retry_io(RECONNECT_ATTEMPTS, || IpcClient::connect(endpoint)).await
}

async fn retry_io<F, Fut>(attempts: usize, mut operation: F) -> Result<IpcClient, DaemonClientError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<IpcClient>>,
{
    let mut last_error = None;
    for attempt in 0..attempts {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(RECONNECT_INTERVAL).await;
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::other("daemon endpoint unavailable"))
        .into())
}

pub fn default_daemon_state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Pithos")
            .join("pithosd")
    }
    #[cfg(unix)]
    {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime).join("pithosd");
        }
        let user = std::env::var_os("USER").unwrap_or_else(|| "local".into());
        let suffix = blake3::hash(user.to_string_lossy().as_bytes());
        std::env::temp_dir().join(format!(
            "pithosd-{}",
            suffix.as_bytes()[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    }
    #[cfg(not(any(windows, unix)))]
    {
        std::env::temp_dir().join("pithosd")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_json_rpc_error_id_is_rejected_as_a_protocol_violation() {
        let response = json!({
            "jsonrpc": "2.0",
            "error": {
                "code": -32000,
                "message": "rejected",
                "kind": "resource_limit"
            },
            "id": 8
        });
        let error = decode_response::<Value>(response, &RpcId::Number(7)).unwrap_err();
        assert!(matches!(error, DaemonClientError::Protocol(_)));
    }

    #[test]
    fn missing_json_rpc_error_id_is_rejected_as_a_protocol_violation() {
        let response = json!({
            "jsonrpc": "2.0",
            "error": {
                "code": -32000,
                "message": "rejected",
                "kind": "resource_limit"
            },
            "id": null
        });
        let error = decode_response::<Value>(response, &RpcId::Number(7)).unwrap_err();
        assert!(matches!(error, DaemonClientError::Protocol(_)));
    }

    #[test]
    fn client_job_limits_do_not_reserve_the_entire_daemon_for_one_job() {
        let maximum = pithos_agent_api::JobLimits {
            max_threads: 8,
            max_memory: 4 * 1024 * 1024 * 1024,
            max_temp: 16 * 1024 * 1024 * 1024,
            max_output: 1024 * 1024 * 1024 * 1024,
            deadline_unix_ms: Some(u64::MAX),
        };
        let selected = client_job_limits(&maximum);
        assert_eq!(selected.max_threads, 1);
        assert_eq!(selected.max_memory, 256 * 1024 * 1024);
        assert_eq!(selected.max_temp, maximum.max_temp);
        assert_eq!(selected.max_output, maximum.max_output);
        assert_eq!(selected.deadline_unix_ms, None);
    }

    #[test]
    fn resumed_session_must_keep_the_original_identity() {
        let expected = pithos_agent_api::SessionId::new("session-0000000000000001").unwrap();
        let replacement = pithos_agent_api::SessionId::new("session-0000000000000002").unwrap();
        validate_resumed_session(&expected, &expected).unwrap();
        assert!(matches!(
            validate_resumed_session(&expected, &replacement),
            Err(DaemonClientError::Protocol(_))
        ));
    }

    #[test]
    fn capability_expiration_uses_the_daemon_deadline_boundary() {
        assert!(!session_expired_at(101, 100));
        assert!(session_expired_at(100, 100));
        assert!(session_expired_at(99, 100));
    }
}
