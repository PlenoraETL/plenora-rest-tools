// Shared by integration tests and the private-policy test module.
pub fn observe(id: &str, mut value: serde_json::Value) {
    // elapsed_ms is the only discarded field. Ports and artifact paths must
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
        assert_eq!(value, expected["cases"][id], "observed baseline: {id}");
    }
}
