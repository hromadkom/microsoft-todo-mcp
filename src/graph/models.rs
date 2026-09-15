//! Wire models for the To Do resources. Unknown fields are ignored; `$select`
//! is never used, so every property is present or genuinely absent (m4 §5).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DateTimeTimeZone {
    pub date_time: String,
    pub time_zone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ItemBody {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub content_type: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskList {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub is_owner: Option<bool>,
    #[serde(default)]
    pub is_shared: Option<bool>,
    /// `none` | `defaultList` | `flaggedEmails` | `unknownFutureValue` | …
    #[serde(default)]
    pub wellknown_list_name: Option<String>,
    #[serde(rename = "@odata.etag", default)]
    pub etag: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistItem {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub is_checked: bool,
    #[serde(default)]
    pub created_date_time: Option<String>,
    #[serde(default)]
    pub checked_date_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedResource {
    pub id: String,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub application_name: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub last_modified_date_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    #[serde(default)]
    pub title: String,
    /// `notStarted` | `inProgress` | `completed` | `waitingOnOthers` | `deferred`
    #[serde(default = "default_status")]
    pub status: String,
    /// `low` | `normal` | `high`
    #[serde(default = "default_importance")]
    pub importance: String,
    #[serde(default)]
    pub is_reminder_on: bool,
    #[serde(default)]
    pub reminder_date_time: Option<DateTimeTimeZone>,
    #[serde(default)]
    pub due_date_time: Option<DateTimeTimeZone>,
    #[serde(default)]
    pub start_date_time: Option<DateTimeTimeZone>,
    #[serde(default)]
    pub completed_date_time: Option<DateTimeTimeZone>,
    #[serde(default)]
    pub created_date_time: Option<String>,
    #[serde(default)]
    pub last_modified_date_time: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub has_attachments: bool,
    #[serde(default)]
    pub body: Option<ItemBody>,
    /// Raw `patternedRecurrence`, passed through verbatim.
    #[serde(default)]
    pub recurrence: Option<Value>,
    #[serde(default)]
    pub checklist_items: Option<Vec<ChecklistItem>>,
    #[serde(default)]
    pub linked_resources: Option<Vec<LinkedResource>>,
    #[serde(rename = "@odata.etag", default)]
    pub etag: Option<String>,
    /// Set by the cache when `body.content` was truncated at ingest.
    #[serde(skip)]
    pub body_bytes_total: Option<usize>,
}

fn default_status() -> String {
    "notStarted".to_string()
}

fn default_importance() -> String {
    "normal".to_string()
}

impl Task {
    pub fn is_completed(&self) -> bool {
        self.status == "completed"
    }
}

/// Graph's error envelope: `{"error":{"code":"…","message":"…"}}`.
#[derive(Debug, Clone, Deserialize)]
pub struct GraphErrorEnvelope {
    pub error: GraphErrorBody,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphErrorBody {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_microsofts_sample_task() {
        let raw = r#"{
          "@odata.etag": "W/\"xzyPKP0BiUGgld+lMKXwbQAAnCsnGw==\"",
          "importance": "low",
          "isReminderOn": false,
          "status": "notStarted",
          "title": "Shop for dinner",
          "createdDateTime": "2020-08-18T09:03:05.8339192Z",
          "lastModifiedDateTime": "2020-08-18T09:03:05.8339192Z",
          "categories": ["Important", "Personal"],
          "id": "AAMkADA1MTHgwAAA=",
          "body": { "content": "", "contentType": "text" },
          "dueDateTime": { "dateTime": "2020-08-25T04:00:00.0000000", "timeZone": "UTC" },
          "linkedResources": [ { "id": "f9cddce2", "webUrl": "https://microsoft.com", "applicationName": "Microsoft", "displayName": "Microsoft" } ]
        }"#;
        let t: Task = serde_json::from_str(raw).unwrap();
        assert_eq!(t.id, "AAMkADA1MTHgwAAA=");
        assert_eq!(t.due_date_time.as_ref().unwrap().time_zone, "UTC");
        assert_eq!(
            t.etag.as_deref().unwrap(),
            "W/\"xzyPKP0BiUGgld+lMKXwbQAAnCsnGw==\""
        );
        assert_eq!(t.linked_resources.as_ref().unwrap().len(), 1);
        assert!(!t.is_completed());
    }

    #[test]
    fn task_list_wellknown_name_is_optional() {
        let l: TaskList = serde_json::from_str(
            r#"{"id":"x","displayName":"Tasks","wellknownListName":"defaultList"}"#,
        )
        .unwrap();
        assert_eq!(l.wellknown_list_name.as_deref(), Some("defaultList"));
        let l: TaskList = serde_json::from_str(r#"{"id":"y","displayName":"Work"}"#).unwrap();
        assert!(l.wellknown_list_name.is_none());
    }
}
