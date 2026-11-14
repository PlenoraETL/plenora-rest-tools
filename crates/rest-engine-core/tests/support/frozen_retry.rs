// Included only in transport's cfg(test) module: exercises the actual private
// policy without exporting it or adding a second production implementation.
#[tokio::test(start_paused = true)]
async fn frozen_retry_policy_with_injected_clock_and_rational_oracle() {
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("../../../../tests/frozen/0.4.0/retry.json")).unwrap();
    let mut observations = serde_json::Map::new();
    for case in corpus["cases"].as_array().unwrap() {
        let policy = crate::RetryPolicy {
            backoff_base_ms: case["base_ms"].as_u64().unwrap(),
            backoff_factor: case["factor"].as_f64().unwrap(),
            max_backoff_ms: case["cap_ms"].as_u64().unwrap(),
            respect_retry_after: case["respect"].as_bool().unwrap(),
            max_retry_after_ms: case["retry_cap_ms"].as_u64().unwrap(),
            ..crate::RetryPolicy::default()
        };
        let mut backoff = super::backoff_for(&policy).unwrap();
        let mut observed = Vec::new();
        for (header, expected) in case["headers_ms"]
            .as_array()
            .unwrap()
            .iter()
            .zip(case["waits_ms"].as_array().unwrap())
        {
            let delay = super::retry_wait(&policy, &mut backoff, header.as_u64());
            assert_eq!(
                delay.map(|value| value.as_millis()),
                expected.as_u64().map(u128::from),
                "{}",
                case["id"]
            );
            observed.push(delay.map(|value| u64::try_from(value.as_millis()).unwrap()));
            if let Some(delay) = delay.filter(|value| value.as_millis() <= 1000) {
                let start = tokio::time::Instant::now();
                tokio::time::sleep(delay).await;
                // Tokio's timer wheel has millisecond resolution and may wake
                // one tick later. It must never wake before the chosen delay.
                let elapsed = start.elapsed();
                assert!(elapsed >= delay && elapsed <= delay + Duration::from_millis(1));
            }
        }
        observations.insert(
            case["id"].as_str().unwrap().to_owned(),
            serde_json::json!(observed),
        );
    }
    let date = &corpus["http_date"];
    let fixed_now = UNIX_EPOCH + Duration::from_secs(date["now_unix_seconds"].as_u64().unwrap());
    assert_eq!(
        parse_retry_after(date["header"].as_str().unwrap(), fixed_now),
        date["wait_ms"].as_u64()
    );
    super_frozen_observe("retry-policy", serde_json::json!(observations));
}

fn super_frozen_observe(id: &str, value: serde_json::Value) {
    // Kept in a shared test helper, not in the private policy under test.
    frozen_snapshot::observe(id, value);
}

mod frozen_snapshot {
    include!("snapshot.rs");
}
