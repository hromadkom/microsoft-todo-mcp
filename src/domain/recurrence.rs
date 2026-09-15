//! `patternedRecurrence` is passed through verbatim (m6 §5). This checks the
//! object's shape — `pattern.type` and `range.type` members copied from the
//! Graph resource docs — and nothing else. Unknown extra keys are NOT rejected:
//! whether Graph rejects, ignores or stores them is unverified.

use serde_json::Value;

const PATTERN_TYPES: &[&str] = &[
    "daily",
    "weekly",
    "absoluteMonthly",
    "relativeMonthly",
    "absoluteYearly",
    "relativeYearly",
];

const RANGE_TYPES: &[&str] = &["endDate", "noEnd", "numbered"];

/// `Ok(())` when the object is a plausible `patternedRecurrence`.
pub fn check_shape(v: &Value) -> Result<(), String> {
    let obj = v
        .as_object()
        .ok_or_else(|| "recurrence must be an object with \"pattern\" and \"range\"".to_string())?;
    let pattern = obj
        .get("pattern")
        .and_then(Value::as_object)
        .ok_or_else(|| "recurrence.pattern must be an object".to_string())?;
    let ptype = pattern
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "recurrence.pattern.type is required".to_string())?;
    if !PATTERN_TYPES.contains(&ptype) {
        return Err(format!(
            "recurrence.pattern.type \"{ptype}\" is not one of {}",
            PATTERN_TYPES.join(", ")
        ));
    }
    if !pattern
        .get("interval")
        .is_some_and(|i| i.as_u64().is_some_and(|n| n >= 1))
    {
        return Err("recurrence.pattern.interval must be an integer >= 1".to_string());
    }
    let range = obj
        .get("range")
        .and_then(Value::as_object)
        .ok_or_else(|| "recurrence.range must be an object".to_string())?;
    let rtype = range
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "recurrence.range.type is required".to_string())?;
    if !RANGE_TYPES.contains(&rtype) {
        return Err(format!(
            "recurrence.range.type \"{rtype}\" is not one of {}",
            RANGE_TYPES.join(", ")
        ));
    }
    if !range.get("startDate").is_some_and(Value::is_string) {
        return Err("recurrence.range.startDate (YYYY-MM-DD) is required".to_string());
    }
    match rtype {
        "endDate" if !range.get("endDate").is_some_and(Value::is_string) => {
            return Err("recurrence.range.endDate is required for type endDate".to_string());
        }
        "numbered"
            if !range
                .get("numberOfOccurrences")
                .is_some_and(|n| n.as_u64().is_some()) =>
        {
            return Err(
                "recurrence.range.numberOfOccurrences is required for type numbered".to_string(),
            );
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn six_by_three_survive_unchanged() {
        for p in PATTERN_TYPES {
            for r in RANGE_TYPES {
                let mut range = json!({"type": r, "startDate": "2026-08-25"});
                if *r == "endDate" {
                    range["endDate"] = json!("2026-12-31");
                }
                if *r == "numbered" {
                    range["numberOfOccurrences"] = json!(5);
                }
                let v = json!({"pattern": {"type": p, "interval": 1, "daysOfWeek": ["monday"]}, "range": range});
                assert!(check_shape(&v).is_ok(), "{p}/{r}");
                // Verbatim pass-through: serialising the same Value is identity.
                assert_eq!(serde_json::to_string(&v).unwrap_or_default(), v.to_string());
            }
        }
    }

    #[test]
    fn bad_shapes_are_named() {
        assert!(check_shape(&json!({})).is_err());
        assert!(check_shape(&json!({"pattern": {"type": "hourly", "interval": 1}, "range": {"type": "noEnd", "startDate": "x"}})).is_err());
        assert!(check_shape(&json!({"pattern": {"type": "daily", "interval": 0}, "range": {"type": "noEnd", "startDate": "x"}})).is_err());
        assert!(check_shape(&json!({"pattern": {"type": "daily", "interval": 1}, "range": {"type": "numbered", "startDate": "x"}})).is_err());
    }
}
