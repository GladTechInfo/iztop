#[test]
fn one_shot_collector_and_filter() {
    let status = std::process::Command::new("sh")
        .args(["tests/once.sh", env!("CARGO_BIN_EXE_iztop")])
        .status()
        .expect("start shell fixture");
    assert!(status.success());
}
