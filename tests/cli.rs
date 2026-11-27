fn run_fixture(script: &str) {
    let status = std::process::Command::new("sh")
        .args([script, env!("CARGO_BIN_EXE_iztop")])
        .status()
        .expect("start shell fixture");
    assert!(status.success(), "{script} failed");
}

#[test]
fn one_shot_collector_and_filter() {
    run_fixture("tests/once.sh");
}

#[test]
fn pool_faults_and_collector_timeout() {
    run_fixture("tests/faults.sh");
}

#[test]
fn installer_checks_requested_destination() {
    run_fixture("tests/installer.sh");
}
