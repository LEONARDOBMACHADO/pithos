//! Stable Agent-First JSON-RPC 2.0 contracts for `pithosd`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::str::FromStr;

pub const JSON_RPC_VERSION: &str = "2.0";
pub const AGENT_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum JsonRpcErrorCode {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
    PithosError = -32000,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicErrorKind {
    InvalidArgument,
    UnsupportedFormat,
    UnsupportedFeature,
    UnsafePath,
    PermissionDenied,
    ResourceLimit,
    CorruptArchive,
    IntegrityMismatch,
    InputChanged,
    JobNotFound,
    JobConflict,
    Cancelled,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub kind: PublicErrorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub fn new(code: JsonRpcErrorCode, kind: PublicErrorKind, message: impl Into<String>) -> Self {
        Self {
            code: code as i32,
            message: message.into(),
            kind,
            data: None,
        }
    }

    pub fn parse_error() -> Self {
        Self::new(
            JsonRpcErrorCode::ParseError,
            PublicErrorKind::InvalidArgument,
            "invalid JSON",
        )
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(
            JsonRpcErrorCode::InvalidRequest,
            PublicErrorKind::InvalidArgument,
            message,
        )
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(
            JsonRpcErrorCode::InvalidParams,
            PublicErrorKind::InvalidArgument,
            message,
        )
    }

    pub fn method_not_found() -> Self {
        Self::new(
            JsonRpcErrorCode::MethodNotFound,
            PublicErrorKind::UnsupportedFeature,
            "method not found",
        )
    }

    pub fn resource_limit(message: impl Into<String>) -> Self {
        Self::new(
            JsonRpcErrorCode::PithosError,
            PublicErrorKind::ResourceLimit,
            message,
        )
    }

    pub fn domain(kind: PublicErrorKind, message: impl Into<String>) -> Self {
        Self::new(JsonRpcErrorCode::PithosError, kind, message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResponse<T> {
    Success(JsonRpcSuccess<T>),
    Error(JsonRpcFailure),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcSuccess<T> {
    pub jsonrpc: String,
    pub result: T,
    pub id: RpcId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcFailure {
    pub jsonrpc: String,
    pub error: JsonRpcError,
    pub id: Option<RpcId>,
}

impl<T> JsonRpcResponse<T> {
    pub fn success(id: RpcId, result: T) -> Self {
        Self::Success(JsonRpcSuccess {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            result,
            id,
        })
    }

    pub fn error(id: Option<RpcId>, error: JsonRpcError) -> Self {
        Self::Error(JsonRpcFailure {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            error,
            id,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcMethod {
    Capabilities,
    Estimate,
    Pack,
    Unpack,
    List,
    Inspect,
    Extract,
    ReadRange,
    Verify,
    Cancel,
    JobStatus,
    SubscribeEvents,
}

impl RpcMethod {
    pub const ALL: [Self; 12] = [
        Self::Capabilities,
        Self::Estimate,
        Self::Pack,
        Self::Unpack,
        Self::List,
        Self::Inspect,
        Self::Extract,
        Self::ReadRange,
        Self::Verify,
        Self::Cancel,
        Self::JobStatus,
        Self::SubscribeEvents,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Capabilities => "capabilities",
            Self::Estimate => "estimate",
            Self::Pack => "pack",
            Self::Unpack => "unpack",
            Self::List => "list",
            Self::Inspect => "inspect",
            Self::Extract => "extract",
            Self::ReadRange => "read_range",
            Self::Verify => "verify",
            Self::Cancel => "cancel",
            Self::JobStatus => "job_status",
            Self::SubscribeEvents => "subscribe_events",
        }
    }
}

impl FromStr for RpcMethod {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|method| method.as_str() == value)
            .ok_or(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawJsonRpcRequest {
    pub method: RpcMethod,
    pub params: Value,
    pub id: RpcId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolLimits {
    pub max_request_bytes: usize,
    pub max_depth: usize,
    pub max_method_bytes: usize,
    pub max_id_bytes: usize,
    pub max_string_bytes: usize,
    pub max_array_items: usize,
    pub max_object_fields: usize,
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self {
            max_request_bytes: 1024 * 1024,
            max_depth: 64,
            max_method_bytes: 128,
            max_id_bytes: 128,
            max_string_bytes: 32 * 1024,
            max_array_items: 4096,
            max_object_fields: 256,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Value,
    id: Value,
}

pub fn parse_request(
    bytes: &[u8],
    limits: &ProtocolLimits,
) -> Result<RawJsonRpcRequest, JsonRpcError> {
    if bytes.len() > limits.max_request_bytes {
        return Err(JsonRpcError::resource_limit("request too large"));
    }
    let wire: WireRequest = serde_json::from_slice(bytes).map_err(|error| {
        if matches!(
            error.classify(),
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof
        ) {
            JsonRpcError::parse_error()
        } else {
            JsonRpcError::invalid_request("invalid envelope")
        }
    })?;
    let value: Value = serde_json::from_slice(bytes).map_err(|_| JsonRpcError::parse_error())?;
    validate_json_value(&value, limits, 0)?;
    if wire.jsonrpc != JSON_RPC_VERSION {
        return Err(JsonRpcError::invalid_request("jsonrpc must be 2.0"));
    }
    if wire.method.is_empty() || wire.method.len() > limits.max_method_bytes {
        return Err(JsonRpcError::invalid_request("invalid method"));
    }
    let method = RpcMethod::from_str(&wire.method).map_err(|_| JsonRpcError::method_not_found())?;
    let id = match wire.id {
        Value::Number(number) => number
            .as_i64()
            .map(RpcId::Number)
            .ok_or_else(|| JsonRpcError::invalid_request("id must be an integer"))?,
        Value::String(value) if !value.is_empty() && value.len() <= limits.max_id_bytes => {
            RpcId::String(value)
        }
        _ => return Err(JsonRpcError::invalid_request("invalid id")),
    };
    Ok(RawJsonRpcRequest {
        method,
        params: wire.params,
        id,
    })
}

fn validate_json_value(
    value: &Value,
    limits: &ProtocolLimits,
    depth: usize,
) -> Result<(), JsonRpcError> {
    if depth > limits.max_depth {
        return Err(JsonRpcError::resource_limit("JSON depth exceeded"));
    }
    match value {
        Value::String(value) if value.len() > limits.max_string_bytes => {
            Err(JsonRpcError::resource_limit("string too large"))
        }
        Value::Array(values) => {
            if values.len() > limits.max_array_items {
                return Err(JsonRpcError::resource_limit("array too large"));
            }
            for value in values {
                validate_json_value(value, limits, depth + 1)?;
            }
            Ok(())
        }
        Value::Object(values) => {
            if values.len() > limits.max_object_fields {
                return Err(JsonRpcError::resource_limit("object too large"));
            }
            for (key, value) in values {
                if key.len() > limits.max_string_bytes {
                    return Err(JsonRpcError::resource_limit("object key too large"));
                }
                validate_json_value(value, limits, depth + 1)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

macro_rules! validated_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, JsonRpcError> {
                let value = value.into();
                if value.len() < $prefix.len() + 8
                    || value.len() > 96
                    || !value.starts_with($prefix)
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                {
                    return Err(JsonRpcError::invalid_params("invalid identifier"));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

validated_id!(JobId, "job-");
validated_id!(SessionId, "session-");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiJobState {
    Queued,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

impl ApiJobState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPriority {
    InteractiveRead,
    InteractiveExtract,
    VerifyRequested,
    #[default]
    PackForeground,
    PackBackground,
    Benchmark,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobLimits {
    pub max_threads: u16,
    pub max_memory: u64,
    pub max_temp: u64,
    pub max_output: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
}

impl Default for JobLimits {
    fn default() -> Self {
        Self {
            max_threads: 1,
            max_memory: 256 * 1024 * 1024,
            max_temp: 1024 * 1024 * 1024,
            max_output: 1024 * 1024 * 1024,
            deadline_unix_ms: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathScope {
    #[serde(default)]
    pub read_roots: Vec<PathBuf>,
    #[serde(default)]
    pub write_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobContext {
    pub capability_token: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub limits: JobLimits,
    pub path_scope: PathScope,
    #[serde(default)]
    pub priority: JobPriority,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiProfile {
    Raw,
    Stream,
    Random,
    Balanced,
    ArchiveMax,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesParams {
    pub client_name: String,
    pub protocol_version: u16,
    #[serde(default)]
    pub requested_scope: PathScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<SessionResume>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResume {
    pub session_id: SessionId,
    pub resume_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCapability {
    pub session_id: SessionId,
    pub capability_token: String,
    pub resume_token: String,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesResult {
    pub product: String,
    pub version: String,
    pub format_versions: Vec<String>,
    pub protocol_version: u16,
    pub supported_methods: Vec<String>,
    pub supported_codecs: Vec<String>,
    pub supported_transforms: Vec<String>,
    pub supported_profiles: Vec<String>,
    pub mount: bool,
    pub maximum_job_limits: JobLimits,
    pub session: SessionCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstimateParams {
    pub capability_token: String,
    pub inputs: Vec<PathBuf>,
    pub path_scope: PathScope,
    #[serde(default = "raw_profile")]
    pub profile: ApiProfile,
}

fn raw_profile() -> ApiProfile {
    ApiProfile::Raw
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstimateResult {
    pub input_bytes: u64,
    pub estimated_memory: u64,
    pub estimated_temp: u64,
    pub output_upper_bound: u64,
    pub eligible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackParams {
    pub context: JobContext,
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    #[serde(default = "raw_profile")]
    pub profile: ApiProfile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnpackParams {
    pub context: JobContext,
    pub archive: PathBuf,
    pub output: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveJobParams {
    pub context: JobContext,
    pub archive: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractParams {
    pub context: JobContext,
    pub archive: PathBuf,
    pub entry: PathBuf,
    pub output: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRangeParams {
    pub context: JobContext,
    pub archive: PathBuf,
    pub entry: PathBuf,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedParams {
    pub capability_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobStatusParams {
    pub capability_token: String,
    pub job_id: JobId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribeEventsParams {
    pub capability_token: String,
    pub job_id: JobId,
    #[serde(default)]
    pub after_sequence: u64,
    #[serde(default)]
    pub wait_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobAccepted {
    pub job_id: JobId,
    pub state: ApiJobState,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSnapshot {
    pub job_id: JobId,
    pub state: ApiJobState,
    pub operation: RpcMethod,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressEvent {
    pub job_id: JobId,
    pub sequence: u64,
    pub state: ApiJobState,
    pub phase: String,
    pub completed_units: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_units: Option<u64>,
    pub occurred_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsResult {
    pub events: Vec<ProgressEvent>,
    pub latest_sequence: u64,
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRangeResult {
    pub transfer_id: String,
    pub path: PathBuf,
    pub offset: u64,
    pub length: u64,
    pub blake3: String,
    pub expires_at_unix_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn strict_request_parser_accepts_valid_json_rpc_and_rejects_protocol_abuse() {
        let limits = ProtocolLimits {
            max_request_bytes: 256,
            max_depth: 4,
            ..ProtocolLimits::default()
        };
        let valid = br#"{"jsonrpc":"2.0","method":"capabilities","params":{"client_name":"test","protocol_version":1},"id":7}"#;
        let request = parse_request(valid, &limits).unwrap();
        assert_eq!(request.method, RpcMethod::Capabilities);
        assert_eq!(request.id, RpcId::Number(7));

        let wrong_version = br#"{"jsonrpc":"1.0","method":"capabilities","params":{},"id":1}"#;
        assert_eq!(
            parse_request(wrong_version, &limits).unwrap_err().code,
            JsonRpcErrorCode::InvalidRequest as i32
        );
        let fractional_id = br#"{"jsonrpc":"2.0","method":"capabilities","params":{},"id":1.5}"#;
        assert_eq!(
            parse_request(fractional_id, &limits).unwrap_err().code,
            JsonRpcErrorCode::InvalidRequest as i32
        );
        assert_eq!(
            parse_request(&vec![b' '; 257], &limits).unwrap_err().kind,
            PublicErrorKind::ResourceLimit
        );
        let too_deep = br#"{"jsonrpc":"2.0","method":"capabilities","params":{"a":{"b":{"c":{"d":{"e":1}}}}},"id":1}"#;
        assert_eq!(
            parse_request(too_deep, &limits).unwrap_err().kind,
            PublicErrorKind::ResourceLimit
        );
    }

    #[test]
    fn json_rpc_response_can_never_contain_result_and_error_together() {
        let success =
            JsonRpcResponse::success(RpcId::String("request-1".into()), json!({"ok": true}));
        let success_json = serde_json::to_value(success).unwrap();
        assert!(success_json.get("result").is_some());
        assert!(success_json.get("error").is_none());

        let error = JsonRpcResponse::<serde_json::Value>::error(
            Some(RpcId::Number(9)),
            JsonRpcError::method_not_found(),
        );
        let error_json = serde_json::to_value(error).unwrap();
        assert!(error_json.get("error").is_some());
        assert!(error_json.get("result").is_none());

        let protocol_error = serde_json::to_value(JsonRpcResponse::<Value>::error(
            None,
            JsonRpcError::parse_error(),
        ))
        .unwrap();
        assert_eq!(protocol_error.get("id"), Some(&Value::Null));
    }

    #[test]
    fn capabilities_contract_supports_explicit_session_resumption() {
        let params: CapabilitiesParams = serde_json::from_value(json!({
            "client_name": "restart-test",
            "protocol_version": 1,
            "requested_scope": {"read_roots": [], "write_roots": []},
            "resume": {
                "session_id": "session-0000000000000001",
                "resume_token": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }))
        .unwrap();
        let resume = params.resume.unwrap();
        assert_eq!(resume.session_id.as_str(), "session-0000000000000001");
        assert_eq!(resume.resume_token.len(), 64);

        let capability = SessionCapability {
            session_id: resume.session_id,
            capability_token: "b".repeat(64),
            resume_token: "c".repeat(64),
            expires_at_unix_ms: 123,
        };
        let encoded = serde_json::to_value(capability).unwrap();
        assert_eq!(encoded["resume_token"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn public_job_contract_is_stable_and_strict() {
        let event = ProgressEvent {
            job_id: JobId::new("job-0000000000000001").unwrap(),
            sequence: 42,
            state: ApiJobState::Running,
            phase: "writing".into(),
            completed_units: 800,
            total_units: Some(1000),
            occurred_at_unix_ms: 1234,
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["state"], "running");
        assert_eq!(value["sequence"], 42);

        let unknown = json!({
            "capability_token": "token",
            "job_id": "job-0000000000000001",
            "unexpected": true
        });
        assert!(serde_json::from_value::<JobStatusParams>(unknown).is_err());
        assert!(JobId::new("../not-a-job").is_err());
    }

    #[test]
    fn compression_profile_contract_is_complete_strict_and_backward_compatible() {
        let cases = [
            (ApiProfile::Raw, "raw"),
            (ApiProfile::Stream, "stream"),
            (ApiProfile::Random, "random"),
            (ApiProfile::Balanced, "balanced"),
            (ApiProfile::ArchiveMax, "archive_max"),
        ];
        for (profile, expected) in cases {
            assert_eq!(serde_json::to_value(&profile).unwrap(), expected);
            assert_eq!(
                serde_json::from_value::<ApiProfile>(json!(expected)).unwrap(),
                profile
            );
        }
        assert!(serde_json::from_value::<ApiProfile>(json!("ultra")).is_err());

        let params: EstimateParams = serde_json::from_value(json!({
            "capability_token": "token",
            "inputs": [],
            "path_scope": {"read_roots": [], "write_roots": []}
        }))
        .unwrap();
        assert_eq!(params.profile, ApiProfile::Raw);
    }

    #[test]
    fn duplicate_envelope_fields_and_batches_are_rejected() {
        let duplicate =
            br#"{"jsonrpc":"2.0","method":"list","method":"verify","params":{},"id":1}"#;
        assert_eq!(
            parse_request(duplicate, &ProtocolLimits::default())
                .unwrap_err()
                .code,
            JsonRpcErrorCode::InvalidRequest as i32
        );
        let batch = br#"[{"jsonrpc":"2.0","method":"capabilities","params":{},"id":1}]"#;
        assert_eq!(
            parse_request(batch, &ProtocolLimits::default())
                .unwrap_err()
                .code,
            JsonRpcErrorCode::InvalidRequest as i32
        );
    }

    proptest! {
        #[test]
        fn request_parser_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let _ = parse_request(&bytes, &ProtocolLimits::default());
        }
    }
}
