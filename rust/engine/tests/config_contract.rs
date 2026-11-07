//! Wire-contract test pinning the camelCase processor -> engine config push;
//! port of `engine/tests/test_config_contract.py`. The fixture is shared
//! with the Java test — keep in sync.

use std::path::PathBuf;

use serde_json::Value;
use stirling_engine::contracts::config::ConfigPushRequest;

fn fixture() -> Value {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dir.join("engine/tests/fixtures/processor_config_push.json");
        if candidate.exists() {
            return serde_json::from_str(&std::fs::read_to_string(candidate).unwrap()).unwrap();
        }
        if !dir.pop() {
            panic!("engine/tests/fixtures/processor_config_push.json not found");
        }
    }
}

#[test]
fn processor_contract_round_trips_every_field() {
    let payload = fixture();
    let req: ConfigPushRequest = serde_json::from_value(payload.clone()).unwrap();

    let m = &payload["models"];
    assert_eq!(req.models.provider, m["provider"].as_str().unwrap());
    assert_eq!(req.models.smart_model, m["smartModel"].as_str().unwrap());
    assert_eq!(req.models.fast_model, m["fastModel"].as_str().unwrap());
    assert_eq!(
        req.models.smart_max_tokens,
        m["smartMaxTokens"].as_u64().map(|v| v as u32)
    );
    assert_eq!(
        req.models.fast_max_tokens,
        m["fastMaxTokens"].as_u64().map(|v| v as u32)
    );
    assert_eq!(req.models.api_key, m["apiKey"].as_str().unwrap());
    assert_eq!(req.models.base_url, m["baseUrl"].as_str().unwrap());

    let r = &payload["rag"];
    assert_eq!(
        req.rag.embedding_provider,
        r["embeddingProvider"].as_str().unwrap()
    );
    assert_eq!(
        req.rag.embedding_model,
        r["embeddingModel"].as_str().unwrap()
    );
    assert_eq!(
        req.rag.embedding_api_key,
        r["embeddingApiKey"].as_str().unwrap()
    );
    assert_eq!(
        req.rag.embedding_base_url,
        r["embeddingBaseUrl"].as_str().unwrap()
    );
    assert_eq!(req.rag.top_k, r["topK"].as_u64().map(|v| v as u32));
    assert_eq!(
        req.rag.max_searches,
        r["maxSearches"].as_u64().map(|v| v as u32)
    );

    let limits = &payload["limits"];
    assert_eq!(
        req.limits.max_pages,
        limits["maxPages"].as_u64().map(|v| v as u32)
    );
    assert_eq!(
        req.limits.max_characters,
        limits["maxCharacters"].as_u64().map(|v| v as u32)
    );
    assert_eq!(
        req.limits.model_max_concurrency,
        limits["modelMaxConcurrency"].as_u64().map(|v| v as u32)
    );
}

#[test]
fn processor_contract_has_no_unmapped_keys() {
    // Guard the fixture itself: every wire key must map to a model field,
    // none silently absorbed by `extra="ignore"`.
    let payload = fixture();
    let expected: &[(&str, &[&str])] = &[
        (
            "models",
            &[
                "provider",
                "smartModel",
                "fastModel",
                "smartMaxTokens",
                "fastMaxTokens",
                "apiKey",
                "baseUrl",
            ],
        ),
        (
            "rag",
            &[
                "embeddingProvider",
                "embeddingModel",
                "embeddingApiKey",
                "embeddingBaseUrl",
                "topK",
                "maxSearches",
            ],
        ),
        (
            "limits",
            &["maxPages", "maxCharacters", "modelMaxConcurrency"],
        ),
    ];
    let top: Vec<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut expected_top: Vec<&str> = expected.iter().map(|(k, _)| *k).collect();
    expected_top.sort_unstable();
    let mut top_sorted = top.clone();
    top_sorted.sort_unstable();
    assert_eq!(top_sorted, expected_top);
    for (section, keys) in expected {
        let mut actual: Vec<&str> = payload[section]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        actual.sort_unstable();
        let mut expected_keys = keys.to_vec();
        expected_keys.sort_unstable();
        assert_eq!(actual, expected_keys, "section {section}");
    }
}
