//! Local JSON-RPC daemon, persistent job registry and engine orchestration.

mod limits;
mod paths;
mod scheduler;
mod service;
mod session_store;
mod store;
mod transport;

pub use limits::{ConnectionRateLimiter, QuotaPolicy};
pub use paths::PathAuthorizer;
pub use service::{DaemonConfig, DaemonService};
pub use session_store::{CreatedSession, ResumedSession, SessionRegistry};
pub use store::{JobRegistry, JobSubmission, StoredOperation};
#[cfg(test)]
pub(crate) use transport::read_frame;
pub use transport::{IpcClient, IpcEndpoint, IpcServer};

pub fn daemon_version() -> &'static str {
    "0.1.0"
}

#[cfg(test)]
mod tests {
    use super::*;
    use pithos_agent_api::{
        ApiJobState, JobLimits, JobPriority, PathScope, PublicErrorKind, RpcMethod, SessionId,
    };
    use std::fs;

    async fn rpc(
        service: &DaemonService,
        connection_id: u64,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let frame = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        }))
        .unwrap();
        serde_json::from_slice(&service.handle_frame(connection_id, &frame).await).unwrap()
    }

    async fn open_session(
        service: &DaemonService,
        connection_id: u64,
        root: &std::path::Path,
    ) -> String {
        open_session_capability(service, connection_id, root, None).await["capability_token"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn open_session_capability(
        service: &DaemonService,
        connection_id: u64,
        root: &std::path::Path,
        resume: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let mut params = serde_json::json!({
            "client_name": "integration-test",
            "protocol_version": 1,
            "requested_scope": {
                "read_roots": [root],
                "write_roots": [root]
            }
        });
        if let Some(resume) = resume {
            params["resume"] = resume;
        }
        let response = rpc(service, connection_id, 1, "capabilities", params).await;
        response["result"]["session"].clone()
    }

    fn job_context(token: &str, key: &str, root: &std::path::Path) -> serde_json::Value {
        serde_json::json!({
            "capability_token": token,
            "idempotency_key": key,
            "limits": {
                "max_threads": 1,
                "max_memory": 268435456_u64,
                "max_temp": 1073741824_u64,
                "max_output": 1073741824_u64
            },
            "path_scope": {
                "read_roots": [root],
                "write_roots": [root]
            },
            "priority": "interactive_read"
        })
    }

    async fn wait_for_job(
        service: &DaemonService,
        connection_id: u64,
        token: &str,
        job_id: &str,
    ) -> serde_json::Value {
        for request_id in 1000..1200 {
            let response = rpc(
                service,
                connection_id,
                request_id,
                "job_status",
                serde_json::json!({"capability_token": token, "job_id": job_id}),
            )
            .await;
            if matches!(
                response["result"]["state"].as_str(),
                Some("completed" | "failed" | "cancelled")
            ) {
                return response;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("job did not reach a terminal state")
    }

    fn session(number: u64) -> SessionId {
        SessionId::new(format!("session-{number:016x}")).unwrap()
    }

    fn submission(owner: SessionId, key: &str, hash_byte: u8) -> JobSubmission {
        JobSubmission {
            owner,
            method: RpcMethod::Verify,
            priority: JobPriority::VerifyRequested,
            idempotency_key: key.to_owned(),
            params_hash: [hash_byte; 32],
            limits: JobLimits::default(),
            operation: StoredOperation::Verify {
                archive: "archive.pithos".into(),
            },
        }
    }

    #[test]
    fn registry_enforces_idempotency_session_isolation_and_ordered_events() {
        let temp = tempfile::tempdir().unwrap();
        let registry = JobRegistry::open(temp.path().join("jobs.json")).unwrap();
        let owner = session(1);
        let other = session(2);

        let accepted = registry
            .submit(submission(owner.clone(), "same-key", 1))
            .unwrap();
        assert!(!accepted.idempotent_replay);
        let replay = registry
            .submit(submission(owner.clone(), "same-key", 1))
            .unwrap();
        assert_eq!(replay.job_id, accepted.job_id);
        assert!(replay.idempotent_replay);
        let conflict = registry
            .submit(submission(owner.clone(), "same-key", 2))
            .unwrap_err();
        assert_eq!(conflict.kind, PublicErrorKind::JobConflict);
        assert_eq!(
            registry
                .snapshot(&other, &accepted.job_id)
                .unwrap_err()
                .kind,
            PublicErrorKind::JobNotFound
        );

        registry
            .transition(&owner, &accepted.job_id, ApiJobState::Running, "running")
            .unwrap();
        registry.request_cancel(&owner, &accepted.job_id).unwrap();
        registry.finish_cancelled(&owner, &accepted.job_id).unwrap();
        let events = registry.events(&owner, &accepted.job_id, 0).unwrap();
        assert_eq!(events.len(), 4);
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
        );
        assert_eq!(events.last().unwrap().state, ApiJobState::Cancelled);
    }

    #[test]
    fn resumable_session_secret_survives_registry_restart_without_being_stored_in_plaintext() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("allowed");
        fs::create_dir_all(&root).unwrap();
        let path = temp.path().join("sessions.json");
        let scope = PathScope {
            read_roots: vec![root.clone()],
            write_roots: vec![root],
        };

        let created = SessionRegistry::open(path.clone())
            .unwrap()
            .create(scope.clone(), 1_000, 2_000)
            .unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(&created.resume_token));

        let reopened = SessionRegistry::open(path.clone()).unwrap();
        let resumed = reopened
            .resume(&created.session_id, &created.resume_token, 1_500, 2_500)
            .unwrap();
        assert_eq!(resumed.scope, scope);
        assert_ne!(resumed.resume_token, created.resume_token);
        assert_eq!(
            reopened
                .resume(&created.session_id, &created.resume_token, 1_500, 2_500)
                .unwrap_err()
                .kind,
            PublicErrorKind::PermissionDenied
        );
        assert!(
            reopened
                .resume(&created.session_id, &"0".repeat(64), 1_500, 2_500)
                .is_err()
        );
        drop(reopened);
        assert_eq!(
            SessionRegistry::open(path)
                .unwrap()
                .resume(&created.session_id, &resumed.resume_token, 2_000, 3_000)
                .unwrap()
                .scope,
            scope
        );
    }

    #[test]
    fn expired_resume_sessions_fail_closed_and_are_pruned_on_create() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("allowed");
        fs::create_dir_all(&root).unwrap();
        let registry = SessionRegistry::open(temp.path().join("sessions.json")).unwrap();
        let scope = PathScope {
            read_roots: vec![root.clone()],
            write_roots: vec![root],
        };
        let expired = registry.create(scope.clone(), 10, 11).unwrap();
        drop(registry);
        let registry = SessionRegistry::open(temp.path().join("sessions.json")).unwrap();

        assert_eq!(
            registry
                .resume(&expired.session_id, &expired.resume_token, 11, 20)
                .unwrap_err()
                .kind,
            PublicErrorKind::PermissionDenied
        );
        let replacement = registry.create(scope, 11, 20).unwrap();
        assert_ne!(replacement.session_id, expired.session_id);
        assert!(
            !fs::read_to_string(temp.path().join("sessions.json"))
                .unwrap()
                .contains(expired.session_id.as_str())
        );
    }

    #[test]
    fn registry_recovers_interrupted_jobs_without_losing_terminal_results() {
        let temp = tempfile::tempdir().unwrap();
        let store_path = temp.path().join("jobs.json");
        let owner = session(1);
        let running_id;
        let completed_id;
        {
            let registry = JobRegistry::open(store_path.clone()).unwrap();
            running_id = registry
                .submit(submission(owner.clone(), "running", 1))
                .unwrap()
                .job_id;
            registry
                .transition(&owner, &running_id, ApiJobState::Running, "running")
                .unwrap();
            completed_id = registry
                .submit(submission(owner.clone(), "completed", 2))
                .unwrap()
                .job_id;
            registry
                .complete(&owner, &completed_id, serde_json::json!({"verified": true}))
                .unwrap();
        }

        let recovered = JobRegistry::open(store_path).unwrap();
        let interrupted = recovered.snapshot(&owner, &running_id).unwrap();
        assert_eq!(interrupted.state, ApiJobState::Failed);
        assert_eq!(interrupted.error.unwrap().kind, PublicErrorKind::Internal);
        let completed = recovered.snapshot(&owner, &completed_id).unwrap();
        assert_eq!(completed.state, ApiJobState::Completed);
        assert_eq!(completed.result.unwrap()["verified"], true);
    }

    #[test]
    fn quotas_rate_limit_and_path_scopes_fail_closed() {
        let policy = QuotaPolicy::default();
        let mut excessive = policy.maximum_job_limits.clone();
        excessive.max_threads += 1;
        assert_eq!(
            policy.validate(&excessive).unwrap_err().kind,
            PublicErrorKind::ResourceLimit
        );
        let mut limiter = ConnectionRateLimiter::new(2, 2);
        assert!(limiter.allow(1_000));
        assert!(limiter.allow(1_000));
        assert!(!limiter.allow(1_000));
        assert!(limiter.allow(2_000));

        let temp = tempfile::tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(allowed.join("inside"), b"inside").unwrap();
        fs::write(outside.join("secret"), b"outside").unwrap();
        let authorizer = PathAuthorizer::new(&PathScope {
            read_roots: vec![allowed.clone()],
            write_roots: vec![allowed.clone()],
        })
        .unwrap();
        assert!(authorizer.authorize_read(&allowed.join("inside")).is_ok());
        assert_eq!(
            authorizer
                .authorize_read(&outside.join("secret"))
                .unwrap_err()
                .kind,
            PublicErrorKind::PermissionDenied
        );
        assert!(
            authorizer
                .authorize_write(&allowed.join("new-file"))
                .is_ok()
        );
        assert!(
            authorizer
                .authorize_write(&outside.join("new-file"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn server_scope_caps_the_roots_a_client_can_request() {
        let temp = tempfile::tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        fs::create_dir_all(&allowed).unwrap();
        let mut config = DaemonConfig::for_test(temp.path().join("state"));
        config.allowed_scope = PathScope {
            read_roots: vec![allowed.clone()],
            write_roots: vec![allowed.clone()],
        };
        let service = DaemonService::open(config).unwrap();

        let denied = rpc(
            &service,
            8,
            1,
            "capabilities",
            serde_json::json!({
                "client_name": "overbroad-client",
                "protocol_version": 1,
                "requested_scope": {
                    "read_roots": [temp.path()],
                    "write_roots": [temp.path()]
                }
            }),
        )
        .await;
        assert_eq!(denied["error"]["kind"], "permission_denied");

        let granted = rpc(
            &service,
            9,
            1,
            "capabilities",
            serde_json::json!({
                "client_name": "scoped-client",
                "protocol_version": 1,
                "requested_scope": {
                    "read_roots": [allowed],
                    "write_roots": [allowed]
                }
            }),
        )
        .await;
        assert_eq!(granted["result"]["protocol_version"], 1);
    }

    #[tokio::test]
    async fn rpc_pack_job_is_idempotent_observable_and_isolated_between_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"daemon payload").unwrap();
        let archive = temp.path().join("daemon.pithos");
        let service =
            DaemonService::open(DaemonConfig::for_test(temp.path().join("state"))).unwrap();
        let first_token = open_session(&service, 10, temp.path()).await;
        let second_token = open_session(&service, 20, temp.path()).await;
        assert_ne!(first_token, second_token);

        let params = serde_json::json!({
            "context": {
                "capability_token": first_token,
                "idempotency_key": "pack-once",
                "limits": {
                    "max_threads": 1,
                    "max_memory": 268435456,
                    "max_temp": 1073741824,
                    "max_output": 1073741824
                },
                "path_scope": {
                    "read_roots": [temp.path()],
                    "write_roots": [temp.path()]
                },
                "priority": "pack_foreground"
            },
            "inputs": [source],
            "output": archive,
            "profile": "raw"
        });
        let accepted = rpc(&service, 10, 2, "pack", params.clone()).await;
        let job_id = accepted["result"]["job_id"].as_str().unwrap().to_owned();
        let replay = rpc(&service, 10, 3, "pack", params).await;
        assert_eq!(replay["result"]["job_id"], job_id);
        assert_eq!(replay["result"]["idempotent_replay"], true);

        let cross_session = rpc(
            &service,
            20,
            4,
            "job_status",
            serde_json::json!({"capability_token": second_token, "job_id": job_id}),
        )
        .await;
        assert_eq!(cross_session["error"]["kind"], "job_not_found");

        let mut terminal = serde_json::Value::Null;
        for request_id in 5..105 {
            terminal = rpc(
                &service,
                10,
                request_id,
                "job_status",
                serde_json::json!({"capability_token": first_token, "job_id": job_id}),
            )
            .await;
            if terminal["result"]["state"] == "completed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(terminal["result"]["state"], "completed");
        assert!(temp.path().join("daemon.pithos").is_file());

        let events = rpc(
            &service,
            10,
            106,
            "subscribe_events",
            serde_json::json!({
                "capability_token": first_token,
                "job_id": job_id,
                "after_sequence": 0,
                "wait_ms": 0
            }),
        )
        .await;
        let events = events["result"]["events"].as_array().unwrap();
        assert!(events.len() >= 3);
        assert!(events.windows(2).all(|pair| {
            pair[0]["sequence"].as_u64().unwrap() < pair[1]["sequence"].as_u64().unwrap()
        }));
    }

    #[tokio::test]
    async fn daemon_applies_the_configured_event_retention_limit() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("retention-source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"retained event payload").unwrap();
        let archive = temp.path().join("retention.pithos");
        let mut config = DaemonConfig::for_test(temp.path().join("state"));
        config.quota_policy.max_events_per_job = 2;
        let service = DaemonService::open(config).unwrap();
        let token = open_session(&service, 30, temp.path()).await;

        let accepted = rpc(
            &service,
            30,
            2,
            "pack",
            serde_json::json!({
                "context": job_context(&token, "bounded-events", temp.path()),
                "inputs": [source],
                "output": archive,
                "profile": "raw"
            }),
        )
        .await;
        let job_id = accepted["result"]["job_id"].as_str().unwrap();
        let terminal = wait_for_job(&service, 30, &token, job_id).await;
        assert_eq!(terminal["result"]["state"], "completed");

        let events = rpc(
            &service,
            30,
            3,
            "subscribe_events",
            serde_json::json!({
                "capability_token": token,
                "job_id": job_id,
                "after_sequence": 0,
                "wait_ms": 0
            }),
        )
        .await;
        let events = events["result"]["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events.last().unwrap()["state"], "completed");
        assert!(events[0]["sequence"].as_u64().unwrap() < events[1]["sequence"].as_u64().unwrap());
    }

    #[tokio::test]
    async fn rpc_session_resume_recovers_jobs_and_idempotency_after_daemon_restart() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("resume-source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"persistent session").unwrap();
        let output = temp.path().join("resume.pithos");
        let state_dir = temp.path().join("state");

        let first = DaemonService::open(DaemonConfig::for_test(state_dir.clone())).unwrap();
        let capability = open_session_capability(&first, 50, temp.path(), None).await;
        let first_token = capability["capability_token"].as_str().unwrap();
        let accepted = rpc(
            &first,
            50,
            2,
            "pack",
            serde_json::json!({
                "context": job_context(first_token, "resume-idempotency", temp.path()),
                "inputs": [source],
                "output": output,
                "profile": "raw"
            }),
        )
        .await;
        let job_id = accepted["result"]["job_id"].as_str().unwrap().to_owned();
        assert_eq!(
            wait_for_job(&first, 50, first_token, &job_id).await["result"]["state"],
            "completed"
        );
        first.disconnect(50);
        drop(first);

        let resumed = DaemonService::open(DaemonConfig::for_test(state_dir)).unwrap();
        let resumed_capability = open_session_capability(
            &resumed,
            51,
            temp.path(),
            Some(serde_json::json!({
                "session_id": capability["session_id"],
                "resume_token": capability["resume_token"]
            })),
        )
        .await;
        let resumed_token = resumed_capability["capability_token"].as_str().unwrap();
        let status = rpc(
            &resumed,
            51,
            3,
            "job_status",
            serde_json::json!({"capability_token": resumed_token, "job_id": job_id}),
        )
        .await;
        assert_eq!(status["result"]["state"], "completed");

        let replay = rpc(
            &resumed,
            51,
            4,
            "pack",
            serde_json::json!({
                "context": job_context(resumed_token, "resume-idempotency", temp.path()),
                "inputs": [source],
                "output": output,
                "profile": "raw"
            }),
        )
        .await;
        assert_eq!(replay["result"]["job_id"], job_id);
        assert_eq!(replay["result"]["idempotent_replay"], true);

        let unrelated_token = open_session(&resumed, 52, temp.path()).await;
        let hidden = rpc(
            &resumed,
            52,
            5,
            "job_status",
            serde_json::json!({"capability_token": unrelated_token, "job_id": job_id}),
        )
        .await;
        assert_eq!(hidden["error"]["kind"], "job_not_found");
    }

    #[tokio::test]
    async fn rpc_rejects_oversized_frames_before_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = DaemonConfig::for_test(temp.path().join("state"));
        config.protocol_limits.max_request_bytes = 64;
        let service = DaemonService::open(config).unwrap();
        let response: serde_json::Value =
            serde_json::from_slice(&service.handle_frame(1, &[b' '; 65]).await).unwrap();
        assert_eq!(response["error"]["kind"], "resource_limit");
        assert_eq!(response.get("id"), Some(&serde_json::Value::Null));
    }

    #[tokio::test]
    async fn local_ipc_roundtrip_uses_a_single_private_endpoint() {
        let temp = tempfile::tempdir().unwrap();
        let service =
            DaemonService::open(DaemonConfig::for_test(temp.path().join("state"))).unwrap();
        let endpoint = IpcEndpoint::for_state_dir(temp.path().join("state"));
        let server = IpcServer::spawn(service, endpoint.clone()).await.unwrap();
        assert!(
            IpcServer::spawn(
                DaemonService::open(DaemonConfig::for_test(temp.path().join("other-state")))
                    .unwrap(),
                endpoint.clone(),
            )
            .await
            .is_err()
        );

        let mut client = IpcClient::connect(&endpoint).await.unwrap();
        let response = client
            .request(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "capabilities",
                "params": {
                    "client_name": "ipc-test",
                    "protocol_version": 1,
                    "requested_scope": {
                        "read_roots": [temp.path()],
                        "write_roots": [temp.path()]
                    }
                },
                "id": "hello"
            }))
            .await
            .unwrap();
        assert_eq!(response["id"], "hello");
        assert_eq!(response["result"]["protocol_version"], 1);
        assert_eq!(
            response["result"]["supported_profiles"],
            serde_json::json!(["raw", "stream", "random", "balanced", "archive-max"])
        );
        assert_eq!(
            response["result"]["supported_codecs"],
            serde_json::json!(["STORE", "Zstandard", "Brotli", "LZMA2"])
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(endpoint.socket_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn frame_reader_rejects_declared_size_before_allocating_payload() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_all(&(1025_u32).to_le_bytes()).await.unwrap();
        let error = read_frame(&mut reader, 1024, std::time::Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn all_agent_methods_complete_a_real_archive_workflow() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"0123456789").unwrap();
        let archive = temp.path().join("workflow.pithos");
        let mut config = DaemonConfig::for_test(temp.path().join("state"));
        // A transfer must remain available long enough for a client that learns
        // its path from the persisted terminal result. Tiny millisecond TTLs
        // race with scheduler/CI load and test the clock rather than semantics.
        config.transfer_ttl = std::time::Duration::from_secs(2);
        let service = DaemonService::open(config).unwrap();
        let token = open_session(&service, 30, temp.path()).await;

        let raw_estimate = rpc(
            &service,
            30,
            2,
            "estimate",
            serde_json::json!({
                "capability_token": token,
                "inputs": [source],
                "path_scope": {"read_roots": [temp.path()], "write_roots": [temp.path()]},
                "profile": "raw"
            }),
        )
        .await;
        let estimate = rpc(
            &service,
            30,
            3,
            "estimate",
            serde_json::json!({
                "capability_token": token,
                "inputs": [source],
                "path_scope": {"read_roots": [temp.path()], "write_roots": [temp.path()]},
                "profile": "balanced"
            }),
        )
        .await;
        assert_eq!(estimate["result"]["input_bytes"], 10);
        assert!(
            estimate["result"]["estimated_memory"].as_u64().unwrap()
                > raw_estimate["result"]["estimated_memory"].as_u64().unwrap()
        );
        assert!(
            estimate["result"]["estimated_temp"].as_u64().unwrap()
                > raw_estimate["result"]["estimated_temp"].as_u64().unwrap()
        );
        assert!(estimate["result"]["output_upper_bound"].as_u64().unwrap() > 10);

        let packed = rpc(
            &service,
            30,
            4,
            "pack",
            serde_json::json!({
                "context": job_context(&token, "workflow-pack", temp.path()),
                "inputs": [source],
                "output": archive,
                "profile": "balanced"
            }),
        )
        .await;
        let pack_id = packed["result"]["job_id"].as_str().unwrap();
        assert_eq!(
            wait_for_job(&service, 30, &token, pack_id).await["result"]["state"],
            "completed"
        );

        for (method, key) in [
            ("list", "workflow-list"),
            ("inspect", "workflow-inspect"),
            ("verify", "workflow-verify"),
        ] {
            let accepted = rpc(
                &service,
                30,
                10,
                method,
                serde_json::json!({
                    "context": job_context(&token, key, temp.path()),
                    "archive": archive
                }),
            )
            .await;
            let job_id = accepted["result"]["job_id"].as_str().unwrap();
            let terminal = wait_for_job(&service, 30, &token, job_id).await;
            assert_eq!(terminal["result"]["state"], "completed");
        }

        let range = rpc(
            &service,
            30,
            20,
            "read_range",
            serde_json::json!({
                "context": job_context(&token, "workflow-range", temp.path()),
                "archive": archive,
                "entry": "file.txt",
                "offset": 2,
                "length": 4
            }),
        )
        .await;
        let range_id = range["result"]["job_id"].as_str().unwrap();
        let range_terminal = wait_for_job(&service, 30, &token, range_id).await;
        let transfer_path = range_terminal["result"]["result"]["path"].as_str().unwrap();
        assert_eq!(fs::read(transfer_path).unwrap(), b"2345");
        let cleanup_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::path::Path::new(transfer_path).exists()
            && tokio::time::Instant::now() < cleanup_deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            !std::path::Path::new(transfer_path).exists(),
            "expired transfer was not cleaned within the bounded test window"
        );

        let extract_root = temp.path().join("extracted");
        let extracted = rpc(
            &service,
            30,
            21,
            "extract",
            serde_json::json!({
                "context": job_context(&token, "workflow-extract", temp.path()),
                "archive": archive,
                "entry": "file.txt",
                "output": extract_root
            }),
        )
        .await;
        let extract_id = extracted["result"]["job_id"].as_str().unwrap();
        assert_eq!(
            wait_for_job(&service, 30, &token, extract_id).await["result"]["state"],
            "completed"
        );
        assert_eq!(
            fs::read(extract_root.join("file.txt")).unwrap(),
            b"0123456789"
        );

        let unpack_root = temp.path().join("unpacked");
        let unpacked = rpc(
            &service,
            30,
            22,
            "unpack",
            serde_json::json!({
                "context": job_context(&token, "workflow-unpack", temp.path()),
                "archive": archive,
                "output": unpack_root
            }),
        )
        .await;
        let unpack_id = unpacked["result"]["job_id"].as_str().unwrap();
        assert_eq!(
            wait_for_job(&service, 30, &token, unpack_id).await["result"]["state"],
            "completed"
        );
        assert_eq!(
            fs::read(unpack_root.join("file.txt")).unwrap(),
            b"0123456789"
        );
    }

    #[tokio::test]
    async fn rpc_extract_enforces_temporary_bytes_before_creating_output() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("large.bin"), vec![0x5a; 4096]).unwrap();
        let archive = temp.path().join("temp-quota.pithos");
        let service =
            DaemonService::open(DaemonConfig::for_test(temp.path().join("state"))).unwrap();
        let token = open_session(&service, 35, temp.path()).await;

        let packed = rpc(
            &service,
            35,
            2,
            "pack",
            serde_json::json!({
                "context": job_context(&token, "temp-quota-pack", temp.path()),
                "inputs": [source],
                "output": archive,
                "profile": "raw"
            }),
        )
        .await;
        let pack_id = packed["result"]["job_id"].as_str().unwrap();
        assert_eq!(
            wait_for_job(&service, 35, &token, pack_id).await["result"]["state"],
            "completed"
        );

        let mut context = job_context(&token, "temp-quota-extract", temp.path());
        context["limits"]["max_temp"] = serde_json::json!(64_u64);
        context["limits"]["max_output"] = serde_json::json!(8192_u64);
        let output = temp.path().join("extracted");
        let accepted = rpc(
            &service,
            35,
            3,
            "extract",
            serde_json::json!({
                "context": context,
                "archive": archive,
                "entry": "large.bin",
                "output": output
            }),
        )
        .await;
        let extract_id = accepted["result"]["job_id"].as_str().unwrap();
        let terminal = wait_for_job(&service, 35, &token, extract_id).await;
        assert_eq!(terminal["result"]["state"], "failed");
        assert_eq!(terminal["result"]["error"]["kind"], "resource_limit");
        assert!(!output.join("large.bin").exists());
    }

    #[tokio::test]
    async fn rpc_pack_enforces_memory_while_scanning_input_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("memory-source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("entry.bin"), b"payload").unwrap();
        let output = temp.path().join("memory-budget.pithos");
        let service =
            DaemonService::open(DaemonConfig::for_test(temp.path().join("state"))).unwrap();
        let token = open_session(&service, 36, temp.path()).await;
        let mut context = job_context(&token, "memory-budget-pack", temp.path());
        context["limits"]["max_memory"] = serde_json::json!(1_u64);

        let accepted = rpc(
            &service,
            36,
            2,
            "pack",
            serde_json::json!({
                "context": context,
                "inputs": [source],
                "output": output,
                "profile": "raw"
            }),
        )
        .await;
        let job_id = accepted["result"]["job_id"].as_str().unwrap();
        let terminal = wait_for_job(&service, 36, &token, job_id).await;
        assert_eq!(terminal["result"]["state"], "failed");
        assert_eq!(terminal["result"]["error"]["kind"], "resource_limit");
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn rpc_cancel_stops_a_queued_job_without_publishing_output() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"cancel").unwrap();
        let output = temp.path().join("cancelled.pithos");
        let service =
            DaemonService::open(DaemonConfig::for_test(temp.path().join("state"))).unwrap();
        let held_slot = service.hold_execution_for_test().await;
        let token = open_session(&service, 40, temp.path()).await;
        let accepted = rpc(
            &service,
            40,
            2,
            "pack",
            serde_json::json!({
                "context": job_context(&token, "cancel-pack", temp.path()),
                "inputs": [source],
                "output": output,
                "profile": "raw"
            }),
        )
        .await;
        let job_id = accepted["result"]["job_id"].as_str().unwrap();
        let cancelled = rpc(
            &service,
            40,
            3,
            "cancel",
            serde_json::json!({"capability_token": token, "job_id": job_id}),
        )
        .await;
        assert_eq!(cancelled["result"]["state"], "cancelled");
        drop(held_slot);
        let terminal = wait_for_job(&service, 40, &token, job_id).await;
        assert_eq!(terminal["result"]["state"], "cancelled");
        assert!(!temp.path().join("cancelled.pithos").exists());
    }

    #[tokio::test]
    async fn rpc_deadline_expires_while_a_job_is_still_queued() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("deadline-source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"deadline").unwrap();
        let output = temp.path().join("deadline.pithos");
        let service =
            DaemonService::open(DaemonConfig::for_test(temp.path().join("state"))).unwrap();
        let held_slots = service.hold_execution_for_test().await;
        let token = open_session(&service, 55, temp.path()).await;
        let mut context = job_context(&token, "deadline-pack", temp.path());
        context["limits"]["deadline_unix_ms"] = serde_json::json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 50
        );
        let accepted = rpc(
            &service,
            55,
            2,
            "pack",
            serde_json::json!({
                "context": context,
                "inputs": [source],
                "output": output,
                "profile": "raw"
            }),
        )
        .await;
        let job_id = accepted["result"]["job_id"].as_str().unwrap();
        let status = wait_for_job(&service, 55, &token, job_id).await;
        assert_eq!(status["result"]["state"], "failed");
        assert_eq!(status["result"]["error"]["kind"], "resource_limit");
        assert!(!output.exists());
        drop(held_slots);
    }

    #[tokio::test]
    async fn graceful_service_shutdown_cancels_queued_jobs_and_rejects_new_work() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("shutdown-source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"shutdown").unwrap();
        let output = temp.path().join("shutdown.pithos");
        let service =
            DaemonService::open(DaemonConfig::for_test(temp.path().join("state"))).unwrap();
        let held_slots = service.hold_execution_for_test().await;
        let token = open_session(&service, 58, temp.path()).await;
        let accepted = rpc(
            &service,
            58,
            2,
            "pack",
            serde_json::json!({
                "context": job_context(&token, "shutdown-pack", temp.path()),
                "inputs": [source],
                "output": output,
                "profile": "raw"
            }),
        )
        .await;
        let job_id = accepted["result"]["job_id"].as_str().unwrap();

        service
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        let stopped = rpc(
            &service,
            58,
            3,
            "job_status",
            serde_json::json!({"capability_token": token, "job_id": job_id}),
        )
        .await;
        assert_eq!(stopped["error"]["kind"], "internal");
        assert!(!output.exists());
        drop(held_slots);
    }

    #[tokio::test]
    async fn graceful_service_shutdown_atomically_closes_registry_admission() {
        let temp = tempfile::tempdir().unwrap();
        let service =
            DaemonService::open(DaemonConfig::for_test(temp.path().join("state"))).unwrap();
        service
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();

        let error = service
            .registry_for_test()
            .submit(submission(session(99), "after-shutdown", 99))
            .unwrap_err();
        assert_eq!(error.kind, PublicErrorKind::JobConflict);
        assert!(service.registry_for_test().nonterminal_jobs().is_empty());
    }

    #[test]
    fn daemon_configuration_rejects_memory_budgets_that_cannot_be_scheduled() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = DaemonConfig::for_test(temp.path().join("state"));
        config.quota_policy.maximum_job_limits.max_memory = (u64::from(u32::MAX) + 1) * 1024 * 1024;
        assert!(DaemonService::open(config).is_err());
    }

    #[tokio::test]
    async fn persistence_failure_stops_even_read_only_rpc_work() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let service = DaemonService::open(DaemonConfig::for_test(state_dir.clone())).unwrap();
        let token = open_session(&service, 61, temp.path()).await;
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), b"storage fault").unwrap();

        let displaced_state = temp.path().join("displaced-state");
        fs::rename(&state_dir, &displaced_state).unwrap();
        fs::write(&state_dir, b"blocks state directory recreation").unwrap();

        let rejected = rpc(
            &service,
            61,
            2,
            "pack",
            serde_json::json!({
                "context": job_context(&token, "storage-fault", temp.path()),
                "inputs": [source.clone()],
                "output": temp.path().join("fault.pithos"),
                "profile": "raw"
            }),
        )
        .await;
        assert_eq!(rejected["error"]["kind"], "internal");

        let after_fault = rpc(
            &service,
            61,
            3,
            "estimate",
            serde_json::json!({
                "capability_token": token,
                "inputs": [source],
                "path_scope": {
                    "read_roots": [temp.path()],
                    "write_roots": [temp.path()]
                }
            }),
        )
        .await;
        assert_eq!(after_fault["error"]["kind"], "internal");
        assert!(after_fault.get("result").is_none());
    }

    #[tokio::test]
    async fn session_persistence_failure_also_stops_the_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let service = DaemonService::open(DaemonConfig::for_test(state_dir.clone())).unwrap();
        let displaced_state = temp.path().join("displaced-session-state");
        fs::rename(&state_dir, &displaced_state).unwrap();
        fs::write(&state_dir, b"blocks session directory recreation").unwrap();

        let rejected = rpc(
            &service,
            62,
            1,
            "capabilities",
            serde_json::json!({
                "client_name": "session-storage-fault",
                "protocol_version": 1,
                "requested_scope": {
                    "read_roots": [temp.path()],
                    "write_roots": [temp.path()]
                }
            }),
        )
        .await;
        assert_eq!(rejected["error"]["kind"], "internal");

        let after_fault = rpc(
            &service,
            63,
            2,
            "capabilities",
            serde_json::json!({
                "client_name": "must-not-open",
                "protocol_version": 1,
                "requested_scope": {
                    "read_roots": [temp.path()],
                    "write_roots": [temp.path()]
                }
            }),
        )
        .await;
        assert_eq!(after_fault["error"]["kind"], "internal");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn queued_job_revalidates_a_path_replaced_by_a_symlink_before_execution() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("swap-source");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(source.join("original.txt"), b"original").unwrap();
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let output = temp.path().join("swap.pithos");
        let service =
            DaemonService::open(DaemonConfig::for_test(temp.path().join("state"))).unwrap();
        let held_slots = service.hold_execution_for_test().await;
        let token = open_session(&service, 59, temp.path()).await;
        let accepted = rpc(
            &service,
            59,
            2,
            "pack",
            serde_json::json!({
                "context": job_context(&token, "swap-pack", temp.path()),
                "inputs": [source],
                "output": output,
                "profile": "raw"
            }),
        )
        .await;
        let job_id = accepted["result"]["job_id"].as_str().unwrap();
        fs::rename(&source, temp.path().join("original-source")).unwrap();
        symlink(&outside, &source).unwrap();
        drop(held_slots);

        let terminal = wait_for_job(&service, 59, &token, job_id).await;
        assert_eq!(terminal["result"]["state"], "failed");
        assert_eq!(terminal["result"]["error"]["kind"], "permission_denied");
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn rpc_cancel_interrupts_a_running_job_and_removes_its_spool() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("running-source");
        fs::create_dir_all(&source).unwrap();
        fs::File::create(source.join("large.bin"))
            .unwrap()
            .set_len(256 * 1024 * 1024)
            .unwrap();
        let output = temp.path().join("running-cancelled.pithos");
        let service =
            DaemonService::open(DaemonConfig::for_test(temp.path().join("state"))).unwrap();
        let token = open_session(&service, 60, temp.path()).await;
        let accepted = rpc(
            &service,
            60,
            2,
            "pack",
            serde_json::json!({
                "context": job_context(&token, "running-cancel", temp.path()),
                "inputs": [source],
                "output": output,
                "profile": "raw"
            }),
        )
        .await;
        let job_id = accepted["result"]["job_id"].as_str().unwrap();

        let mut observed_running = false;
        for request_id in 10..500 {
            let status = rpc(
                &service,
                60,
                request_id,
                "job_status",
                serde_json::json!({"capability_token": token, "job_id": job_id}),
            )
            .await;
            match status["result"]["state"].as_str() {
                Some("running") => {
                    observed_running = true;
                    break;
                }
                Some("completed" | "failed" | "cancelled") => break,
                _ => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
            }
        }
        assert!(
            observed_running,
            "the test must cancel an actively running job"
        );

        let response = rpc(
            &service,
            60,
            600,
            "cancel",
            serde_json::json!({"capability_token": token, "job_id": job_id}),
        )
        .await;
        assert_eq!(response["result"]["state"], "cancelling");
        let terminal = wait_for_job(&service, 60, &token, job_id).await;
        assert_eq!(terminal["result"]["state"], "cancelled");
        assert!(!temp.path().join("running-cancelled.pithos").exists());
        let transfer_dir = temp.path().join("state").join("transfers");
        assert!(
            !transfer_dir.exists() || fs::read_dir(transfer_dir).unwrap().next().is_none(),
            "cancellation must not leave a spool"
        );
    }
}
