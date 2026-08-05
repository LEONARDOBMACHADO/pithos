use pithos_daemon::{DaemonConfig, DaemonService, IpcEndpoint, IpcServer};
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

async fn invoke(arguments: &[&str]) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_pithos"))
        .args(arguments)
        .output()
        .await
        .expect("CLI process starts");
    if !output.status.success() {
        eprintln!("CLI arguments failed: {arguments:?}");
    }
    output
}

async fn invoke_in(current_dir: &Path, arguments: &[&str]) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_pithos"))
        .current_dir(current_dir)
        .args(arguments)
        .output()
        .await
        .expect("CLI process starts");
    if !output.status.success() {
        eprintln!(
            "CLI arguments failed in {}: {arguments:?}",
            current_dir.display()
        );
    }
    output
}

async fn invoke_to_file(arguments: &[&str], destination: &Path) -> std::process::Output {
    let output_file = std::fs::File::create(destination).expect("stdout destination is created");
    let output = Command::new(env!("CARGO_BIN_EXE_pithos"))
        .args(arguments)
        .stdout(Stdio::from(output_file))
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI process starts")
        .wait_with_output()
        .await
        .expect("CLI process completes");
    if !output.status.success() {
        eprintln!("CLI arguments failed: {arguments:?}");
    }
    output
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "CLI failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn file_blake3(path: &Path) -> blake3::Hash {
    let mut file = std::fs::File::open(path).unwrap();
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            return hasher.finalize();
        }
        hasher.update(&buffer[..read]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_mode_runs_the_complete_cli_archive_workflow_over_local_ipc() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("file.txt"), b"daemon cli payload").unwrap();
    std::fs::write(source.join("empty.bin"), b"").unwrap();
    let archive = temp.path().join("archive.pithos");

    let endpoint = IpcEndpoint::for_state_dir(state.clone());
    let mut config = DaemonConfig::new(state.clone());
    let current_dir = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    config.allowed_scope = pithos_agent_api::PathScope {
        read_roots: vec![temp.path().to_path_buf(), current_dir.clone()],
        write_roots: vec![temp.path().to_path_buf(), current_dir],
    };
    let server = IpcServer::spawn(DaemonService::open(config).unwrap(), endpoint)
        .await
        .unwrap();
    let state = path_text(&state);
    let source = path_text(&source);
    let archive = path_text(&archive);

    let capabilities = invoke(&[
        "--mode",
        "daemon",
        "--daemon-state-dir",
        &state,
        "--output-format",
        "json",
        "capabilities",
    ])
    .await;
    assert_success(&capabilities);
    let capabilities: Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    assert_eq!(capabilities["protocol_version"], 1);
    assert!(
        capabilities["supported_methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|method| method == "pack")
    );
    assert!(capabilities["session"].get("capability_token").is_none());
    assert!(capabilities["session"].get("resume_token").is_none());

    let packed = invoke(&[
        "--mode",
        "daemon",
        "--daemon-state-dir",
        &state,
        "--output-format",
        "json",
        "pack",
        &source,
        "--output",
        &archive,
    ])
    .await;
    assert_success(&packed);
    let packed: Value = serde_json::from_slice(&packed.stdout).unwrap();
    assert_eq!(packed["status"], "packed");
    assert!(Path::new(&archive).is_file());

    let relative = invoke_in(
        temp.path(),
        &[
            "--mode",
            "daemon",
            "--daemon-state-dir",
            &state,
            "--output-format",
            "json",
            "pack",
            "source",
            "--output",
            "relative.pithos",
        ],
    )
    .await;
    assert_success(&relative);
    let relative: Value = serde_json::from_slice(&relative.stdout).unwrap();
    assert_eq!(relative["archive"], "relative.pithos");
    assert!(temp.path().join("relative.pithos").is_file());

    for command in ["list", "inspect", "verify"] {
        let output = invoke(&[
            "--mode",
            "daemon",
            "--daemon-state-dir",
            &state,
            "--output-format",
            "json",
            command,
            &archive,
        ])
        .await;
        assert_success(&output);
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        match command {
            "list" => assert!(
                result
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["path"] == "file.txt")
            ),
            "inspect" => assert_eq!(result["metadata_verified"], true),
            "verify" => assert_eq!(result["entry_count"], 2),
            _ => unreachable!(),
        }
    }

    let human_verify = invoke(&[
        "--mode",
        "daemon",
        "--daemon-state-dir",
        &state,
        "verify",
        &archive,
    ])
    .await;
    assert_success(&human_verify);
    assert!(String::from_utf8_lossy(&human_verify.stdout).contains("Integridade verificada"));

    let human_list = invoke(&[
        "--mode",
        "daemon",
        "--daemon-state-dir",
        &state,
        "list",
        &archive,
    ])
    .await;
    assert_success(&human_list);
    assert!(String::from_utf8_lossy(&human_list.stdout).contains("  File  file.txt"));

    let extracted = temp.path().join("extracted");
    let extracted_text = path_text(&extracted);
    let output = invoke(&[
        "--mode",
        "daemon",
        "--daemon-state-dir",
        &state,
        "--output-format",
        "json",
        "extract",
        &archive,
        "file.txt",
        "--output",
        &extracted_text,
    ])
    .await;
    assert_success(&output);
    assert_eq!(
        std::fs::read(extracted.join("file.txt")).unwrap(),
        b"daemon cli payload"
    );

    let stdout = invoke(&[
        "--mode",
        "daemon",
        "--daemon-state-dir",
        &state,
        "extract",
        &archive,
        "file.txt",
        "--stdout",
    ])
    .await;
    assert_success(&stdout);
    assert_eq!(stdout.stdout, b"daemon cli payload");

    let empty_stdout = invoke(&[
        "--mode",
        "daemon",
        "--daemon-state-dir",
        &state,
        "extract",
        &archive,
        "empty.bin",
        "--stdout",
    ])
    .await;
    assert_success(&empty_stdout);
    assert!(empty_stdout.stdout.is_empty());

    let relative_unpack = invoke_in(
        temp.path(),
        &[
            "--mode",
            "daemon",
            "--daemon-state-dir",
            &state,
            "--output-format",
            "json",
            "unpack",
            "relative.pithos",
            "--output",
            "relative-unpacked",
        ],
    )
    .await;
    assert_success(&relative_unpack);
    let relative_unpack: Value = serde_json::from_slice(&relative_unpack.stdout).unwrap();
    assert_eq!(relative_unpack["output"], "relative-unpacked");
    assert_eq!(
        std::fs::read(temp.path().join("relative-unpacked/file.txt")).unwrap(),
        b"daemon cli payload"
    );

    let unpacked = temp.path().join("unpacked");
    let unpacked_text = path_text(&unpacked);
    let output = invoke(&[
        "--mode",
        "daemon",
        "--daemon-state-dir",
        &state,
        "--output-format",
        "json",
        "unpack",
        &archive,
        "--output",
        &unpacked_text,
    ])
    .await;
    assert_success(&output);
    assert_eq!(
        std::fs::read(unpacked.join("file.txt")).unwrap(),
        b"daemon cli payload"
    );

    server.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_stdout_verifies_each_transfer_in_a_multi_chunk_entry() {
    const LARGE_BYTES: u64 = 64 * 1024 * 1024 + 1;

    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let source = temp.path().join("large-source");
    std::fs::create_dir_all(&source).unwrap();
    let input = source.join("large.bin");
    std::fs::File::create(&input)
        .unwrap()
        .set_len(LARGE_BYTES)
        .unwrap();
    let archive = temp.path().join("large.pithos");
    let restored = temp.path().join("large-restored.bin");

    let endpoint = IpcEndpoint::for_state_dir(state.clone());
    let mut config = DaemonConfig::new(state.clone());
    config.allowed_scope = pithos_agent_api::PathScope {
        read_roots: vec![temp.path().to_path_buf()],
        write_roots: vec![temp.path().to_path_buf()],
    };
    let server = IpcServer::spawn(DaemonService::open(config).unwrap(), endpoint)
        .await
        .unwrap();

    let state_text = path_text(&state);
    let source_text = path_text(&source);
    let archive_text = path_text(&archive);
    let packed = invoke(&[
        "--mode",
        "daemon",
        "--daemon-state-dir",
        &state_text,
        "--output-format",
        "json",
        "pack",
        &source_text,
        "--output",
        &archive_text,
    ])
    .await;
    assert_success(&packed);

    let extracted = invoke_to_file(
        &[
            "--mode",
            "daemon",
            "--daemon-state-dir",
            &state_text,
            "extract",
            &archive_text,
            "large.bin",
            "--stdout",
        ],
        &restored,
    )
    .await;
    assert_success(&extracted);
    assert_eq!(std::fs::metadata(&restored).unwrap().len(), LARGE_BYTES);
    assert_eq!(file_blake3(&restored), file_blake3(&input));

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn standalone_remains_the_default_without_a_daemon() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("standalone-source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("file.txt"), b"standalone payload").unwrap();
    std::fs::write(source.join("empty.bin"), b"").unwrap();
    let archive = temp.path().join("standalone.pithos");
    let source = path_text(&source);
    let archive = path_text(&archive);

    let capabilities = invoke(&["--output-format", "json", "capabilities"]).await;
    assert_success(&capabilities);
    let result: Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    assert_eq!(result["codecs"][0], "STORE");

    let packed = invoke(&[
        "--output-format",
        "json",
        "pack",
        &source,
        "--output",
        &archive,
    ])
    .await;
    assert_success(&packed);

    for command in ["list", "inspect", "verify"] {
        let output = invoke(&["--output-format", "json", command, &archive]).await;
        assert_success(&output);
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        match command {
            "list" => assert!(
                result
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["path"] == "file.txt")
            ),
            "inspect" => assert_eq!(result["metadata_verified"], true),
            "verify" => assert_eq!(result["entry_count"], 2),
            _ => unreachable!(),
        }
    }

    let stdout = invoke(&["extract", &archive, "file.txt", "--stdout"]).await;
    assert_success(&stdout);
    assert_eq!(stdout.stdout, b"standalone payload");

    let empty_stdout = invoke(&["extract", &archive, "empty.bin", "--stdout"]).await;
    assert_success(&empty_stdout);
    assert!(empty_stdout.stdout.is_empty());

    let extracted = temp.path().join("standalone-extracted");
    let extracted_text = path_text(&extracted);
    let extracted_result = invoke(&[
        "--output-format",
        "json",
        "extract",
        &archive,
        "file.txt",
        "--output",
        &extracted_text,
    ])
    .await;
    assert_success(&extracted_result);
    assert_eq!(
        std::fs::read(extracted.join("file.txt")).unwrap(),
        b"standalone payload"
    );

    let unpacked = temp.path().join("standalone-unpacked");
    let unpacked_text = path_text(&unpacked);
    let unpacked_result = invoke(&[
        "--output-format",
        "json",
        "unpack",
        &archive,
        "--output",
        &unpacked_text,
    ])
    .await;
    assert_success(&unpacked_result);
    assert_eq!(
        std::fs::read(unpacked.join("file.txt")).unwrap(),
        b"standalone payload"
    );

    let invalid = invoke(&[
        "--output-format",
        "json",
        "--daemon-state-dir",
        "unused",
        "capabilities",
    ])
    .await;
    assert!(!invalid.status.success());
    let error: Value = serde_json::from_slice(&invalid.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "command_failed");
}
