//! Cursor pagination for `todo_search_tasks` (m4 §6): opaque to the model,
//! self-describing to the server, loud when the world moved.

use serde::{Deserialize, Serialize};

use crate::cache::fnv1a;

pub const CURSOR_TTL_SECONDS: i64 = 900;
const PREFIX: &str = "c1_";
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    /// Format version.
    pub v: u8,
    /// Offset into the matched, sorted set.
    pub o: usize,
    /// Mailbox id.
    pub a: String,
    /// Page size that produced it.
    pub n: usize,
    /// FNV-1a of the canonical args.
    pub q: u64,
    /// Issued at, unix seconds.
    pub t: i64,
    /// `cache.generation` at issue.
    pub g: u64,
    /// The filter arguments that produced the page, so a cursor passed alone
    /// can re-run the same search. `q` is the hash of their canonical form.
    #[serde(default)]
    pub f: serde_json::Value,
}

pub fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = ALPHABET.iter().position(|a| *a == c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

pub fn encode(c: &Cursor) -> String {
    let json = serde_json::to_vec(c).unwrap_or_default();
    format!("{PREFIX}{}", base64url_encode(&json))
}

pub fn decode(s: &str) -> Option<Cursor> {
    let body = s.strip_prefix(PREFIX)?;
    let bytes = base64url_decode(body)?;
    serde_json::from_slice(&bytes).ok()
}

/// Canonical hash of the filter arguments in a fixed key order with defaults
/// materialised; `limit` excluded.
pub fn args_hash(canonical: &str) -> u64 {
    fnv1a(canonical.as_bytes())
}

/// The seven refusals, verbatim, in order (1 is checked by the caller since it
/// concerns argument presence, not the cursor itself).
pub const REFUSE_MIXED: &str = "Pass \"cursor\" alone (optionally with \"limit\") to continue a search, or pass filters alone to start a new one. Do not pass both.";
pub const REFUSE_INVALID: &str = "Invalid cursor. Cursors come from the \"next_cursor\" field of a previous todo_search_tasks result and cannot be constructed by hand. Re-run the search without a cursor.";
pub const REFUSE_VERSION: &str =
    "Cursor was issued by a different server version. Re-run the search without a cursor.";
pub const REFUSE_ACCOUNT: &str =
    "Cursor belongs to a different account. Re-run the search without a cursor.";
pub const REFUSE_EXPIRED: &str = "Cursor expired after 15 minutes. Re-run todo_search_tasks with the same filters to start a fresh page.";
pub const REFUSE_STALE: &str = "Cursor is stale: tasks were created, updated or deleted since this page was produced. Re-run todo_search_tasks with the same filters — results have moved.";
pub const REFUSE_ARGS: &str = "Cursor does not match these search arguments. Pass \"cursor\" alone to continue a search, or pass filters alone to start a new one.";

/// Validate checks 2–7. Returns the cursor or the verbatim refusal.
pub fn validate(
    raw: &str,
    mailbox_id: &str,
    now_unix: i64,
    generation: u64,
    args_q: u64,
) -> Result<Cursor, &'static str> {
    let c = decode(raw).ok_or(REFUSE_INVALID)?;
    if c.v != 1 {
        return Err(REFUSE_VERSION);
    }
    if c.a != mailbox_id {
        return Err(REFUSE_ACCOUNT);
    }
    if now_unix - c.t > CURSOR_TTL_SECONDS {
        return Err(REFUSE_EXPIRED);
    }
    if c.g != generation {
        return Err(REFUSE_STALE);
    }
    if c.q != args_q {
        return Err(REFUSE_ARGS);
    }
    Ok(c)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_arbitrary_bytes() {
        let mut seed = 0x1234_5678_u64;
        for len in 0..64 {
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    (seed & 0xff) as u8
                })
                .collect();
            let enc = base64url_encode(&bytes);
            assert!(enc.bytes().all(|b| ALPHABET.contains(&b)));
            assert_eq!(base64url_decode(&enc).unwrap(), bytes, "len {len}");
        }
    }

    #[test]
    fn cursor_round_trips_and_stays_short() {
        let c = Cursor {
            v: 1,
            o: 50,
            a: "mbx_0123456789abcdef".into(),
            n: 50,
            q: u64::MAX,
            t: 1_800_000_000,
            g: 12345,
            f: serde_json::json!({"list": "Work", "query": "budget review", "status": "any", "sort": "title_asc"}),
        };
        let s = encode(&c);
        assert!(s.starts_with("c1_"));
        assert!(s.len() <= 400, "cursor grew to {} chars", s.len());
        assert_eq!(decode(&s).unwrap(), c);
    }

    #[test]
    fn the_refusals_fire_in_order() {
        let c = Cursor {
            v: 1,
            o: 0,
            a: "mbx_a".into(),
            n: 50,
            q: 7,
            t: 1000,
            g: 3,
            f: serde_json::json!({}),
        };
        let s = encode(&c);
        assert_eq!(
            validate("garbage", "mbx_a", 1000, 3, 7).unwrap_err(),
            REFUSE_INVALID
        );
        let v2 = encode(&Cursor { v: 2, ..c.clone() });
        assert_eq!(
            validate(&v2, "mbx_a", 1000, 3, 7).unwrap_err(),
            REFUSE_VERSION
        );
        assert_eq!(
            validate(&s, "mbx_b", 1000, 3, 7).unwrap_err(),
            REFUSE_ACCOUNT
        );
        assert_eq!(
            validate(&s, "mbx_a", 1000 + 901, 3, 7).unwrap_err(),
            REFUSE_EXPIRED
        );
        assert_eq!(validate(&s, "mbx_a", 1000, 4, 7).unwrap_err(), REFUSE_STALE);
        assert_eq!(validate(&s, "mbx_a", 1000, 3, 8).unwrap_err(), REFUSE_ARGS);
        assert_eq!(validate(&s, "mbx_a", 1000, 3, 7).unwrap(), c);
    }
}
