use pithos_daemon::{IpcClient, IpcEndpoint};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn spawn_daemon(state_dir: &std::path::Path, allowed_root: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_pithosd"))
        .arg("--state-dir")
        .arg(state_dir)
        .arg("--allow-read-root")
        .arg(allowed_root)
        .arg("--allow-write-root")
        .arg(allowed_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

async fn connect_eventually(endpoint: &IpcEndpoint) -> IpcClient {
    for _ in 0..300 {
        if let Ok(client) = IpcClient::connect(endpoint).await {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("pithosd did not expose its local endpoint")
}

async fn request(client: &mut IpcClient, id: u64, method: &str, params: Value) -> Value {
    client
        .request(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id
        }))
        .await
        .unwrap()
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test]
async fn forced_restart_never_leaves_a_corrupt_final_archive() {
    let temp = tempfile::tempdir().unwrap();
    let state_dir = temp.path().join("state");
    let source = temp.path().join("source");
    fs::create_dir_all(&source).unwrap();
    File::create(source.join("large.bin"))
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let archive = temp.path().join("crash.pithos");
    let endpoint = IpcEndpoint::for_state_dir(state_dir.clone());
    let mut daemon = spawn_daemon(&state_dir, temp.path());
    let mut client = connect_eventually(&endpoint).await;
    let capabilities = request(
        &mut client,
        1,
        "capabilities",
        json!({
            "client_name": "restart-test",
            "protocol_version": 1,
            "requested_scope": {
                "read_roots": [temp.path()],
                "write_roots": [temp.path()]
            }
        }),
    )
    .await;
    let token = capabilities["result"]["session"]["capability_token"]
        .as_str()
        .unwrap();
    let session_id = capabilities["result"]["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let resume_token = capabilities["result"]["session"]["resume_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let accepted = request(
        &mut client,
        2,
        "pack",
        json!({
            "context": {
                "capability_token": token,
                "idempotency_key": "restart-pack",
                "limits": {
                    "max_threads": 1,
                    "max_memory": 268435456_u64,
                    "max_temp": 2147483648_u64,
                    "max_output": 2147483648_u64
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
        }),
    )
    .await;
    let job_id = accepted["result"]["job_id"].as_str().unwrap().to_owned();
    for id in 3..103 {
        let status = request(
            &mut client,
            id,
            "job_status",
            json!({"capability_token": token, "job_id": job_id}),
        )
        .await;
        if status["result"]["state"] == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stop(&mut daemon);

    if archive.exists() {
        pithos_engine::verify(&archive).unwrap();
    }

    let mut restarted = spawn_daemon(&state_dir, temp.path());
    let mut resumed_client = connect_eventually(&endpoint).await;
    let resumed = request(
        &mut resumed_client,
        200,
        "capabilities",
        json!({
            "client_name": "restart-test",
            "protocol_version": 1,
            "requested_scope": {
                "read_roots": [temp.path()],
                "write_roots": [temp.path()]
            },
            "resume": {
                "session_id": session_id,
                "resume_token": resume_token
            }
        }),
    )
    .await;
    let resumed_token = resumed["result"]["session"]["capability_token"]
        .as_str()
        .unwrap();
    let recovered = request(
        &mut resumed_client,
        201,
        "job_status",
        json!({"capability_token": resumed_token, "job_id": job_id}),
    )
    .await;
    if archive.exists() {
        assert_eq!(recovered["result"]["state"], "completed");
    } else {
        assert_eq!(recovered["result"]["state"], "failed");
    }
    stop(&mut restarted);
    if archive.exists() {
        pithos_engine::verify(&archive).unwrap();
    }
}
