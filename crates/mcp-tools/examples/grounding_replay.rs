//! JSONL replay of the actual selectors, with no model/network calls.
use mcp_tools::domains::grounding::{replay_selection, rollout::POLICY_REVISION};
use serde_json::{json, Value};
use std::io::{self, BufRead};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for line in io::stdin().lock().lines() {
        let input: Value = serde_json::from_str(&line?)?;
        let query_id = input
            .get("query_id")
            .and_then(Value::as_str)
            .ok_or("missing query_id")?;
        let recall = input.get("recall").ok_or("missing recall")?;
        let session = input.get("session_id").and_then(Value::as_str);
        let start = std::time::Instant::now();
        let legacy = replay_selection(recall.clone(), session, false);
        let legacy_us = start.elapsed().as_micros();
        let start = std::time::Instant::now();
        let candidate = replay_selection(recall.clone(), session, true);
        let candidate_us = start.elapsed().as_micros();
        let ids = |hits: &[mcp_tools::domains::grounding::GroundingHit]| {
            hits.iter().map(|hit|
            json!({"id":hit.id_hint,"project_id":hit.source_project_id,"stale":hit.stale,"stale_reason":hit.stale_reason})
        ).collect::<Vec<_>>()
        };
        println!(
            "{}",
            json!({"query_id":query_id,"policy_revision":POLICY_REVISION,
            "retrieval_status":legacy.status,"candidate_status":candidate.status,
            "legacy":ids(&legacy.hits),"candidate":ids(&candidate.hits),
            "selector_only_legacy_us":legacy_us,"selector_only_candidate_us":candidate_us})
        );
    }
    Ok(())
}
