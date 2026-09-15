//! The device-code polling loop (m1 §2). `sleep` and `now` are injected so the
//! tests do not take fifteen minutes; the server's `message` is rendered verbatim.

use std::time::{Duration, Instant};

use super::entra::{DeviceCodeFlow, DeviceCodeResponse, PollOutcome};
use super::{AuthError, TokenSuccess};

const MAX_INTERVAL: u64 = 60;

/// Everything the loop needs from the outside world.
pub struct LoginIo<'a> {
    pub sleep: &'a dyn Fn(Duration),
    pub now: &'a dyn Fn() -> Instant,
    /// Renders the verification message for the human. Exactly one call.
    pub show: &'a dyn Fn(&DeviceCodeResponse),
}

/// Run the flow to completion. Returns the token response on success; every
/// terminal Entra error carries a named remediation.
pub fn login(
    flow: &dyn DeviceCodeFlow,
    scope: &str,
    io: &LoginIo<'_>,
) -> Result<TokenSuccess, AuthError> {
    let dc = flow.device_authorization(scope)?;
    (io.show)(&dc);
    let started = (io.now)();
    let deadline = started + Duration::from_secs(dc.expires_in.max(1));
    let mut interval = dc.interval.clamp(1, MAX_INTERVAL);
    loop {
        (io.sleep)(Duration::from_secs(interval));
        if (io.now)() > deadline {
            return Err(AuthError::Transport(
                "the device code expired before sign-in completed; run `login` again".into(),
            ));
        }
        match flow.redeem_device_code(dc.device_code.expose()) {
            Ok(PollOutcome::Success(t)) => return Ok(t),
            Ok(PollOutcome::Pending) => {}
            Ok(PollOutcome::SlowDown) => interval = (interval + 5).min(MAX_INTERVAL),
            Err(AuthError::Entra(f)) if !f.diagnosis.terminal => {
                // e.g. temporarily_unavailable: keep polling within the window.
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::auth::Secret;
    use std::cell::RefCell;
    use std::sync::Mutex;

    struct Script {
        polls: Mutex<Vec<Result<PollOutcome, AuthError>>>,
        interval: u64,
    }

    impl DeviceCodeFlow for Script {
        fn device_authorization(&self, _scope: &str) -> Result<DeviceCodeResponse, AuthError> {
            Ok(DeviceCodeResponse {
                device_code: Secret::new("DC"),
                user_code: "ABCD-EFGH".into(),
                verification_uri: "https://example.invalid/device".into(),
                expires_in: 900,
                interval: self.interval,
                message: "Open https://example.invalid/device and enter ABCD-EFGH".into(),
            })
        }

        fn redeem_device_code(&self, code: &str) -> Result<PollOutcome, AuthError> {
            assert_eq!(code, "DC");
            self.polls.lock().unwrap().remove(0)
        }
    }

    fn success() -> TokenSuccess {
        TokenSuccess {
            access_token: Secret::new("AT"),
            token_type: "Bearer".into(),
            expires_in: 3600,
            scope: Some("Tasks.ReadWrite offline_access".into()),
            refresh_token: Some(Secret::new("RT")),
        }
    }

    #[test]
    fn pending_then_slow_down_then_success_with_growing_interval() {
        let script = Script {
            polls: Mutex::new(vec![
                Ok(PollOutcome::Pending),
                Ok(PollOutcome::SlowDown),
                Ok(PollOutcome::Pending),
                Ok(PollOutcome::Success(success())),
            ]),
            interval: 5,
        };
        let sleeps = RefCell::new(Vec::new());
        let shown = RefCell::new(String::new());
        let start = Instant::now();
        let io = LoginIo {
            sleep: &|d| sleeps.borrow_mut().push(d.as_secs()),
            now: &|| start,
            show: &|dc| *shown.borrow_mut() = dc.message.clone(),
        };
        let t = login(&script, "scope", &io).unwrap();
        assert_eq!(t.refresh_token.unwrap().expose(), "RT");
        assert_eq!(*sleeps.borrow(), vec![5, 5, 10, 10]);
        assert!(shown.borrow().contains("ABCD-EFGH"));
    }

    #[test]
    fn expiry_is_enforced_by_the_injected_clock() {
        let script = Script {
            polls: Mutex::new(Vec::new()),
            interval: 5,
        };
        let start = Instant::now();
        let ticks = RefCell::new(0u64);
        let io = LoginIo {
            sleep: &|_| *ticks.borrow_mut() += 1000,
            now: &|| start + Duration::from_secs(*ticks.borrow()),
            show: &|_| {},
        };
        let err = login(&script, "scope", &io).unwrap_err();
        assert!(err.message().contains("expired"));
    }

    /// `login` never deletes token.json, so even a code Microsoft marks dead must
    /// not be reported as a deletion on this path.
    #[test]
    fn a_failed_login_never_claims_a_deletion() {
        use crate::auth::entra::{TokenErrorBody, failure_from};
        let blocked = failure_from(TokenErrorBody {
            error: "invalid_grant".into(),
            error_description: Some("AADSTS530036: blocked by Conditional Access".into()),
            error_codes: vec![530036],
            suberror: None,
            trace_id: None,
            correlation_id: None,
        });
        let script = Script {
            polls: Mutex::new(vec![Err(AuthError::Entra(Box::new(blocked)))]),
            interval: 5,
        };
        let start = Instant::now();
        let io = LoginIo {
            sleep: &|_| {},
            now: &|| start,
            show: &|_| {},
        };
        let Err(AuthError::Entra(f)) = login(&script, "scope", &io) else {
            panic!("expected the Entra refusal");
        };
        assert!(
            f.diagnosis.delete_token,
            "530036 still marks the token dead"
        );
        assert!(!f.token_deleted);
        let rendered = f.render();
        assert!(!rendered.contains("deleted"), "{rendered}");
        let message = AuthError::Entra(f).message();
        assert!(!message.contains("deleted"), "{message}");
    }
}
