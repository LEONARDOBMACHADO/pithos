use std::path::Path;
use std::process::{Command, Output};

fn invoke(current_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pithos"))
        .current_dir(current_dir)
        .args(args)
        .output()
        .expect("pithos CLI starts")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "CLI failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn single_input_uses_original_name_plus_pts_and_legacy_extension_still_reads() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("report.pdf"), b"pithos pts naming fixture").unwrap();

    let packed = invoke(temp.path(), &["pack", "report.pdf", "--profile", "raw"]);
    assert_success(&packed);

    let archive = temp.path().join("report.pdf.pts");
    assert!(archive.is_file());
    assert_success(&invoke(temp.path(), &["verify", "report.pdf.pts"]));

    let legacy = temp.path().join("legacy.pithos");
    std::fs::copy(&archive, &legacy).unwrap();
    assert_success(&invoke(temp.path(), &["verify", "legacy.pithos"]));
}

#[test]
fn multiple_inputs_default_to_files_pts() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("a.txt"), b"alpha").unwrap();
    std::fs::write(temp.path().join("b.bin"), b"beta").unwrap();

    let packed = invoke(
        temp.path(),
        &["pack", "a.txt", "b.bin", "--profile", "raw"],
    );
    assert_success(&packed);

    assert!(temp.path().join("files.pts").is_file());
    assert_success(&invoke(temp.path(), &["verify", "files.pts"]));
}
