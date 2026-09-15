//! `POST /$batch` (m3 §7): at most 20 sub-requests, paired by `id` never by
//! index, sub-429s re-issued alone after the largest `Retry-After`.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::client::GraphClient;
use super::retry::{MAX_RETRY_AFTER, retry_after};
use super::{BATCH_MAX, Budget, GRAPH_BASE, Method};
use crate::errors::AppError;

#[derive(Debug, Clone)]
pub struct BatchSub {
    pub id: String,
    /// Relative to `/v1.0`, e.g. `/me/todo/lists/{id}/tasks?$top=100`.
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct BatchSubRes {
    pub id: String,
    pub status: u16,
    pub body: Value,
}

impl GraphClient {
    /// Issue one batch of GETs. Sub-request failures are data: each id gets a
    /// `BatchSubRes` with its own status. Sub-429s are retried in a follow-up
    /// batch containing only the failed ids, up to `max_attempts` rounds.
    pub fn batch_get(
        &self,
        subs: &[BatchSub],
        budget: &mut Budget,
    ) -> Result<Vec<BatchSubRes>, AppError> {
        if subs.len() > BATCH_MAX {
            return Err(AppError::InvalidArgs(format!(
                "batch of {} exceeds the {BATCH_MAX}-request cap",
                subs.len()
            )));
        }
        let mut pending: Vec<BatchSub> = subs.to_vec();
        let mut results: HashMap<String, BatchSubRes> = HashMap::new();
        let prefer = self.prefer_value();
        for round in 0..self.max_attempts.max(1) {
            if pending.is_empty() {
                break;
            }
            let requests: Vec<Value> = pending
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "method": "GET",
                        "url": s.url,
                        "headers": { "Accept": "application/json", "Prefer": prefer },
                    })
                })
                .collect();
            let url = format!("{GRAPH_BASE}/$batch");
            let envelope =
                self.send_json(Method::Post, &url, json!({ "requests": requests }), budget)?;
            let responses = envelope
                .get("responses")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut retry_ids: Vec<String> = Vec::new();
            let mut max_wait = Duration::ZERO;
            for r in responses {
                let id = r
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let status = r.get("status").and_then(Value::as_u64).unwrap_or(0) as u16;
                let body = r.get("body").cloned().unwrap_or(Value::Null);
                if status == 429 && round + 1 < self.max_attempts.max(1) {
                    let ra = r
                        .get("headers")
                        .and_then(Value::as_object)
                        .and_then(|h| {
                            h.iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
                                .and_then(|(_, v)| v.as_str())
                        })
                        .and_then(|v| retry_after(Some(v), None))
                        .unwrap_or(Duration::from_secs(2))
                        .min(MAX_RETRY_AFTER);
                    max_wait = max_wait.max(ra);
                    retry_ids.push(id.clone());
                    {
                        let mut st = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                        st.throttled_429 += 1;
                    }
                }
                results.insert(id.clone(), BatchSubRes { id, status, body });
            }
            // Anything Graph did not answer at all counts as failed.
            for s in &pending {
                results.entry(s.id.clone()).or_insert_with(|| BatchSubRes {
                    id: s.id.clone(),
                    status: 0,
                    body: Value::Null,
                });
            }
            pending.retain(|s| retry_ids.contains(&s.id));
            if !pending.is_empty() {
                if max_wait > budget.remaining() {
                    break;
                }
                (self.sleep)(max_wait);
            }
        }
        Ok(subs.iter().filter_map(|s| results.remove(&s.id)).collect())
    }
}
