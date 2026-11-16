// Shared by integration tests and the private-policy test module.
pub fn observe(id: &str, mut value: serde_json::Value) {
    // elapsed_ms is discarded here. Ports and artifact paths must
    // be normalized explicitly by the scenario, never by a recursive scrubber.
    if let Some(metrics) = value
        .get_mut("metrics")
        .and_then(serde_json::Value::as_object_mut)
        && let Some(elapsed) = metrics.remove("elapsed_ms")
    {
        assert!(elapsed.is_u64());
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let recording = std::env::var("SEMPL_CAPTURE_DIR").ok();
    let capturing = recording
        .clone()
        .or_else(|| std::env::var("SEMPL_CHECK_CAPTURE_DIR").ok());
    if let Some(directory) = capturing {
        let destination = std::path::PathBuf::from(directory);
        assert!(
            destination.is_absolute()
                && destination
                    .canonicalize()
                    .unwrap()
                    .starts_with(root.join("target").canonicalize().unwrap())
        );
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination.join(format!("{id}.json")))
            .unwrap();
        file.write_all(&serde_json::to_vec_pretty(&value).unwrap())
            .unwrap();
    }
    if recording.is_none() {
        let expected: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("tests/frozen/0.4.0/observed.json")).unwrap(),
        )
        .unwrap();
        assert_observation(id, value, expected["cases"][id].clone());
    }
}

fn assert_observation(id: &str, mut actual: serde_json::Value, mut expected: serde_json::Value) {
    if id.starts_with("private-cli-") {
        // Only the CLI envelope version is derived from the current workspace.
        // Keep captured bytes and historical provenance intact.
        assert_eq!(
            actual["document"]["component_version"],
            env!("CARGO_PKG_VERSION")
        );
        for value in [&mut actual, &mut expected] {
            let version = value.pointer_mut("/document/component_version").unwrap();
            assert!(version.is_string());
            *version = serde_json::json!("<workspace-version>");
        }
    }
    assert_eq!(actual, expected, "observed baseline: {id}");
}

#[test]
fn oracle_accepts_historical_version_only() {
    let actual = serde_json::json!({"document": {"component_version": env!("CARGO_PKG_VERSION"), "status": "error"}});
    let mut expected = actual.clone();
    expected["document"]["component_version"] = serde_json::json!("historical-version");
    assert_observation("private-cli-probe", actual, expected);
}

#[test]
#[should_panic(expected = "observed baseline")]
fn oracle_rejects_changed_behavior() {
    let actual = serde_json::json!({"document": {"component_version": env!("CARGO_PKG_VERSION"), "status": "error"}});
    let mut expected = actual.clone();
    expected["document"]["status"] = serde_json::json!("ok");
    assert_observation("private-cli-probe", actual, expected);
}

#[test]
#[should_panic(expected = "assertion `left == right` failed")]
fn oracle_rejects_wrong_current_version_even_when_baseline_matches() {
    let actual = serde_json::json!({"document": {"component_version": "wrong-version"}});
    assert_observation("private-cli-probe", actual.clone(), actual);
}
