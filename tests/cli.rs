use std::path::Path;
use std::process::Command;

#[test]
fn invalid_url_does_not_create_output_directory() {
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("never-created");
    assert!(!out.exists());

    let status = Command::new(env!("CARGO_BIN_EXE_x-stitch"))
        .arg("https://example.com/not/a/post")
        .arg("-o")
        .arg(&out)
        .status()
        .unwrap();

    assert!(!status.success());
    let created = out.exists();
    let _ = std::fs::remove_dir_all(&out);
    assert!(!created, "output directory was created for a failed run");
}
