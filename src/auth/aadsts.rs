//! AADSTS diagnosis — a pure function from error code to named remediation.
//! No I/O, so it is exhaustively testable. The user-facing catalogue lives in
//! `docs/app-registration.md#troubleshooting`;
//! `tests::every_code_is_in_the_troubleshooting_table` keeps the two in step, and
//! `tests::known_codes_match_the_by_code_arms` keeps that test's list honest.
//!
//! **Delete `token.json` only on 530036, 700082, 70008.** Never on a bare
//! `invalid_grant`: a malformed refresh request returns `invalid_grant` +
//! AADSTS9002313, and deleting a good token on that is a self-inflicted outage.
//! Only the refresh path (`TokenProvider::access_token`) acts on `delete_token`,
//! and only while token.json is unchanged since the refresh began; `login` never
//! deletes. So no remediation here may claim a deletion —
//! `EntraFailure::remediation` appends that note when one really happened.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub summary: &'static str,
    pub remediation: &'static str,
    /// The device-code loop must stop; retrying the same code cannot succeed.
    pub terminal: bool,
    /// Microsoft says the stored refresh token can never be used again.
    pub delete_token: bool,
}

const fn d(
    summary: &'static str,
    remediation: &'static str,
    terminal: bool,
    delete_token: bool,
) -> Diagnosis {
    Diagnosis {
        summary,
        remediation,
        terminal,
        delete_token,
    }
}

/// Appends where to report an error this server cannot fix locally: a public
/// issue with identifiers redacted, and the private route for anything
/// security-relevant. A macro because `Diagnosis` holds `&'static str`, and
/// `concat!` is how the repository URL gets into one.
macro_rules! with_report_pointer {
    ($lead:literal) => {
        concat!(
            $lead,
            " If it persists, report it at ",
            env!("CARGO_PKG_REPOSITORY"),
            "/issues, quoting the AADSTS code and Trace ID with your client ID, tenant ID and account name redacted; never paste token.json, bearer.token or a device code. Anything security-relevant goes to ",
            env!("CARGO_PKG_REPOSITORY"),
            "/security instead of a public issue."
        )
    };
}

/// Diagnose by AADSTS code first, then by the OAuth `error` string.
pub fn diagnose(codes: &[i64], error: &str) -> Diagnosis {
    for code in codes {
        if let Some(diag) = by_code(*code) {
            return diag;
        }
    }
    by_error(error)
}

fn by_code(code: i64) -> Option<Diagnosis> {
    Some(match code {
        7000218 => d(
            "the app registration is not marked as a public client",
            "In the Entra portal open your app → Authentication → Advanced settings → \"Allow public client flows\" → Yes → Save. This server never sends a client secret. (docs/app-registration.md step 8)",
            true,
            false,
        ),
        50194 => d(
            "the app is single-tenant but the server used the /common endpoint",
            "Either change \"Supported account types\" to include personal accounts and any tenant (step 5), or set TODO_MCP_TENANT to your tenant GUID or domain.",
            true,
            false,
        ),
        90094 => d(
            "your tenant requires an administrator to approve this permission",
            "Ask a Global or Cloud Application Administrator to grant admin consent for Tasks.ReadWrite on this app registration (docs/app-registration.md, \"Work or school accounts\"). A personal Microsoft account needs no admin.",
            true,
            false,
        ),
        65001 => d(
            "nobody has consented to this app yet",
            "Run `login` again and accept the consent screen.",
            true,
            false,
        ),
        530036 => d(
            "a Conditional Access policy blocks device code flow for this account",
            "Ask your administrator to exempt this app from the authentication-flows policy, or use a personal Microsoft account; this server has no other sign-in flow.",
            true,
            true,
        ),
        700016 => d(
            "the client ID was not found in this tenant",
            "Re-check TODO_MCP_CLIENT_ID against Application (client) ID on the app's Overview page, and TODO_MCP_TENANT if set.",
            true,
            false,
        ),
        50105 => d(
            "the enterprise app requires user assignment and you are not assigned",
            "An administrator must add you under Enterprise applications → your app → Users and groups, or turn off \"Assignment required\".",
            true,
            false,
        ),
        7000112 => d(
            "the application is disabled",
            "Re-enable the app registration in the Entra portal (Enterprise applications → Properties → Enabled for users to sign in).",
            true,
            false,
        ),
        70011 => d(
            "Microsoft rejected the requested scope",
            with_report_pointer!(
                "This server only ever requests Microsoft Graph Tasks.ReadWrite or Tasks.Read plus offline_access, and refuses any other TODO_MCP_SCOPE before contacting Microsoft, so this is not a typo in your configuration. The cause has not been verified against a live tenant: check Supported account types (docs/app-registration.md step 5) and API permissions (step 10), then run `login` again."
            ),
            true,
            false,
        ),
        70018 => d(
            "the device code entered in the browser was wrong",
            "Run `login` again and type the code exactly as printed.",
            true,
            false,
        ),
        70019 | 70020 => d(
            "the sign-in window closed before you finished in the browser",
            "Run `login` again and complete the browser step within the time shown.",
            true,
            false,
        ),
        65004 => d(
            "you declined the consent screen",
            "Run `login` again and accept.",
            true,
            false,
        ),
        9002313 => d(
            "Microsoft rejected the request as malformed",
            with_report_pointer!(
                "This is a client bug or a transient service fault, not a token problem: token.json, if present, was left untouched. Retry."
            ),
            true,
            false,
        ),
        70008 | 700082 => d(
            "the refresh token has expired or been revoked",
            "Run `login` again.",
            true,
            true,
        ),
        7000215 | 7000222 => d(
            "Microsoft expected a valid client secret, which this public client never sends",
            "TODO_MCP_CLIENT_SECRET is refused at startup and no secret is ever sent, so Microsoft is treating the app registration as a confidential client: set Authentication → Allow public client flows → Yes (docs/app-registration.md step 8). A client secret already on the registration is unused and can be removed.",
            true,
            false,
        ),
        900023 => d(
            "the tenant identifier is not valid",
            "TODO_MCP_TENANT must be a tenant GUID, a verified domain, or common/organizations/consumers.",
            true,
            false,
        ),
        500011 => d(
            "the resource principal (Microsoft Graph) was not found in the tenant",
            "The tenant has no Microsoft Graph service principal — unusual. An administrator must provision it, or use a personal Microsoft account.",
            true,
            false,
        ),
        _ => return None,
    })
}

fn by_error(error: &str) -> Diagnosis {
    match error {
        "authorization_declined" => d(
            "you declined the sign-in",
            "Run `login` again and approve the request.",
            true,
            false,
        ),
        "bad_verification_code" => d(
            "Microsoft did not recognise the device code",
            "Run `login` again.",
            true,
            false,
        ),
        "expired_token" => d(
            "the device code expired before sign-in completed",
            "Run `login` again and finish in the browser within the time shown.",
            true,
            false,
        ),
        "invalid_scope" => d(
            "the requested scope was rejected",
            with_report_pointer!(
                "This server only ever requests Tasks.ReadWrite or Tasks.Read plus offline_access, and refuses any other TODO_MCP_SCOPE, so this is not a typo in your configuration. The cause has not been verified against a live tenant: check Supported account types (docs/app-registration.md step 5) and API permissions (step 10), then run `login` again."
            ),
            true,
            false,
        ),
        "invalid_client" | "unauthorized_client" => d(
            "Microsoft rejected the client",
            "Re-check TODO_MCP_CLIENT_ID and the \"Allow public client flows\" toggle (docs/app-registration.md).",
            true,
            false,
        ),
        "invalid_grant" => d(
            "Microsoft rejected the token",
            "Run `login` again. token.json, if present, was left untouched in case this was transient.",
            true,
            false,
        ),
        "interaction_required" | "consent_required" | "login_required" => d(
            "Microsoft needs you to sign in interactively",
            "Run `login` again.",
            true,
            false,
        ),
        "temporarily_unavailable" | "server_error" => d(
            "Microsoft's sign-in service is temporarily unavailable",
            "Wait a minute and retry.",
            false,
            false,
        ),
        _ => d(
            "Microsoft returned an error this server does not recognise",
            with_report_pointer!(
                "Look the code up at https://login.microsoftonline.com/error?code=<number>."
            ),
            true,
            false,
        ),
    }
}

/// `"AADSTS<n>: <sentence>\r\nTrace ID: …"` → the sentence. Live responses
/// sometimes use spaces instead of CRLF — split on the marker.
pub fn short_description(d: &str) -> &str {
    d.split("Trace ID:")
        .next()
        .unwrap_or(d)
        .trim()
        .trim_end_matches(['\r', '\n', ' ', '.'])
}

/// Extract `AADSTS<n>` codes from a description when `error_codes` is absent.
pub fn codes_in_description(d: &str) -> Vec<i64> {
    let mut out = Vec::new();
    let mut rest = d;
    while let Some(i) = rest.find("AADSTS") {
        let tail = &rest[i + 6..];
        let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = digits.parse::<i64>() {
            out.push(n);
        }
        rest = &tail[digits.len()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code `by_code` names. Add a code to `by_code`, here, and to the
    /// troubleshooting table in docs/app-registration.md together: the tests below
    /// fail if any of the three disagrees with the others.
    const KNOWN_CODES: [i64; 20] = [
        7000218, 50194, 90094, 65001, 530036, 700016, 50105, 7000112, 70011, 70018, 70019, 70020,
        65004, 9002313, 70008, 700082, 7000215, 7000222, 900023, 500011,
    ];

    /// Numbers split out of a `a | b` match pattern or an `a / b` table cell, or
    /// `None` when any part is not a bare number (the row or arm is not a code).
    fn all_numbers<'a>(parts: impl Iterator<Item = &'a str>) -> Option<Vec<i64>> {
        parts.map(|n| n.trim().parse::<i64>().ok()).collect()
    }

    /// The codes of `by_code`'s match arms, read from this file's own source:
    /// a `match` cannot be enumerated at run time.
    fn by_code_arm_codes() -> Vec<i64> {
        let body = include_str!("aadsts.rs")
            .split_once("fn by_code(")
            .and_then(|(_, rest)| rest.split_once("fn by_error("))
            .map(|(body, _)| body)
            .unwrap_or_default();
        body.lines()
            .filter_map(|line| line.trim().split_once(" => d("))
            .filter_map(|(pattern, _)| all_numbers(pattern.split(" | ")))
            .flatten()
            .collect()
    }

    #[test]
    fn known_codes_match_the_by_code_arms() {
        let mut arms = by_code_arm_codes();
        arms.sort_unstable();
        let mut known = KNOWN_CODES.to_vec();
        known.sort_unstable();
        assert_eq!(
            arms, known,
            "KNOWN_CODES and the by_code match arms disagree"
        );
    }

    #[test]
    fn only_three_codes_delete_the_token() {
        let deleting: Vec<i64> = KNOWN_CODES
            .into_iter()
            .filter(|c| diagnose(&[*c], "invalid_grant").delete_token)
            .collect();
        assert_eq!(deleting, vec![530036, 70008, 700082]);
    }

    #[test]
    fn bare_invalid_grant_keeps_the_token() {
        let diag = diagnose(&[], "invalid_grant");
        assert!(!diag.delete_token);
        let diag = diagnose(&[9002313], "invalid_grant");
        assert!(!diag.delete_token);
    }

    #[test]
    fn every_known_code_names_a_remediation() {
        for code in KNOWN_CODES {
            let diag = by_code(code).unwrap_or_else(|| panic!("AADSTS{code} has no diagnosis"));
            assert!(!diag.remediation.is_empty());
            assert!(!diag.summary.is_empty());
        }
    }

    /// The table rows whose first cell is a bold AADSTS number, or several split by
    /// `/`. A row whose bold first cell is not all numbers, such as the `.All`
    /// refusal, is not an AADSTS row and is skipped.
    #[test]
    fn every_code_is_in_the_troubleshooting_table() {
        let doc = include_str!("../../docs/app-registration.md");
        let section = doc
            .split_once("\n## Troubleshooting")
            .and_then(|(_, rest)| rest.split("\n## ").next())
            .unwrap_or_default();
        let rows: Vec<i64> = section
            .lines()
            .filter(|l| l.starts_with("| **"))
            .filter_map(|l| l.split("**").nth(1))
            .filter_map(|cell| all_numbers(cell.split('/')))
            .flatten()
            .collect();
        for code in KNOWN_CODES {
            assert!(
                rows.contains(&code),
                "AADSTS{code} is missing from docs/app-registration.md#troubleshooting"
            );
        }
        for code in &rows {
            assert!(
                KNOWN_CODES.contains(code),
                "the troubleshooting table lists {code}, which by_code does not diagnose"
            );
        }
    }

    #[test]
    fn no_remediation_claims_a_deletion() {
        let errors = [
            "authorization_declined",
            "bad_verification_code",
            "expired_token",
            "invalid_scope",
            "invalid_client",
            "invalid_grant",
            "interaction_required",
            "temporarily_unavailable",
            "something_new",
        ];
        let diags = KNOWN_CODES
            .iter()
            .map(|c| diagnose(&[*c], ""))
            .chain(errors.iter().map(|e| diagnose(&[], e)));
        for diag in diags {
            assert!(
                !diag.remediation.contains("deleted"),
                "{}",
                diag.remediation
            );
        }
        for diag in [diagnose(&[9002313], ""), diagnose(&[], "invalid_grant")] {
            assert!(
                diag.remediation
                    .contains("token.json, if present, was left untouched"),
                "{}",
                diag.remediation
            );
        }
    }

    #[test]
    fn remediations_do_not_blame_settings_the_config_already_refuses() {
        for diag in [diagnose(&[70011], ""), diagnose(&[], "invalid_scope")] {
            assert!(
                !diag.remediation.contains("Set TODO_MCP_SCOPE"),
                "{}",
                diag.remediation
            );
            assert!(
                diag.remediation.contains("has not been verified"),
                "{}",
                diag.remediation
            );
        }
        for code in [7000215, 7000222] {
            let r = diagnose(&[code], "").remediation;
            assert!(r.contains("Allow public client flows"), "{r}");
            assert!(
                !r.contains("make sure TODO_MCP_CLIENT_SECRET is not set"),
                "{r}"
            );
        }
    }

    #[test]
    fn report_pointers_ask_for_redaction_and_route_security_privately() {
        for diag in [
            diagnose(&[9002313], "invalid_grant"),
            diagnose(&[70011], ""),
            diagnose(&[], "invalid_scope"),
            diagnose(&[], "something_new"),
        ] {
            let r = diag.remediation;
            assert!(r.contains("redacted"), "{r}");
            assert!(
                r.contains(concat!(env!("CARGO_PKG_REPOSITORY"), "/issues")),
                "{r}"
            );
            assert!(
                r.contains(concat!(env!("CARGO_PKG_REPOSITORY"), "/security")),
                "{r}"
            );
        }
    }

    #[test]
    fn description_is_truncated_at_the_trace_marker() {
        let full = "AADSTS7000218: The request body must contain the following parameter: 'client_assertion' or 'client_secret'.\r\nTrace ID: abc\r\nCorrelation ID: def\r\nTimestamp: 2026-08-25";
        assert_eq!(
            short_description(full),
            "AADSTS7000218: The request body must contain the following parameter: 'client_assertion' or 'client_secret'"
        );
        assert_eq!(short_description("plain"), "plain");
        assert_eq!(codes_in_description(full), vec![7000218]);
    }
}
