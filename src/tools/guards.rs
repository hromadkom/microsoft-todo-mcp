//! Pre-flight guards for the write tools (m6 §6). G2–G4 are pure functions of
//! the arguments and make zero requests; G1 needs the list catalogue, which
//! the delete needed anyway.

/// Fails CLOSED: the enum is evolvable, so `unknownFutureValue` — and any
/// string this build does not recognise — counts as protected.
pub fn is_protected(wellknown: Option<&str>) -> bool {
    !matches!(wellknown, None | Some("none"))
}

pub fn is_flagged_emails(wellknown: Option<&str>) -> bool {
    wellknown == Some("flaggedEmails")
}

/// G1 — verbatim.
pub fn refuse_flagged_delete(title: &str) -> String {
    format!(
        "Refused: \"{title}\" lives in the built-in Flagged emails list, which mirrors flagged Outlook mail. \
         Deleting it here would not unflag the message and Microsoft To Do does not support it. \
         Unflag the email in Outlook instead."
    )
}

/// G2 — verbatim.
pub const REFUSE_DELETE_CONFIRM: &str = "Refused: todo_delete_tasks permanently deletes tasks and requires confirm: true. Nothing was deleted.";

/// G3 — verbatim.
pub const REFUSE_CHECKLIST_REMOVE_CONFIRM: &str = "Refused: removing checklist items is permanent and requires confirm: true. No operations in this call were applied.";

/// G4 — verbatim.
pub fn refuse_batch_cap(tool: &str, cap: usize, supplied: usize) -> String {
    format!(
        "Refused: {tool} accepts at most {cap} items per call; {supplied} were supplied. Split the call."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_lists_are_protected() {
        assert!(is_protected(Some("defaultList")));
        assert!(is_protected(Some("flaggedEmails")));
    }

    #[test]
    fn unknown_future_value_is_protected() {
        assert!(is_protected(Some("unknownFutureValue")));
        assert!(is_protected(Some("somethingNew")));
        assert!(!is_protected(None));
        assert!(!is_protected(Some("none")));
    }
}
