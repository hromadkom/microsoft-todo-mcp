//! The typed To Do endpoints. Every path starts `/me` (m6 §8). This is the
//! only file besides `mod.rs` that spells a Graph path.

use serde_json::Value;

use super::client::GraphClient;
use super::models::{Attachment, ChecklistItem, Task, TaskList};
use super::{Budget, GRAPH_BASE, Method, PAGE_SIZE, encode_segment};
use crate::errors::AppError;

pub fn lists_url() -> String {
    format!("{GRAPH_BASE}/me/todo/lists?$top={PAGE_SIZE}")
}

/// The one deliberate non-paged read: `todo_account_status`'s connectivity probe.
pub fn lists_probe_url() -> String {
    format!("{GRAPH_BASE}/me/todo/lists?$top=1")
}

pub fn tasks_url(list_id: &str) -> String {
    format!("{GRAPH_BASE}{}", tasks_rel(list_id))
}

/// Relative form for `$batch`.
pub fn tasks_rel(list_id: &str) -> String {
    format!(
        "/me/todo/lists/{}/tasks?$top={PAGE_SIZE}",
        encode_segment(list_id)
    )
}

pub fn tasks_collection_url(list_id: &str) -> String {
    format!(
        "{GRAPH_BASE}/me/todo/lists/{}/tasks",
        encode_segment(list_id)
    )
}

pub fn task_url(list_id: &str, task_id: &str) -> String {
    format!(
        "{GRAPH_BASE}/me/todo/lists/{}/tasks/{}",
        encode_segment(list_id),
        encode_segment(task_id)
    )
}

pub fn checklist_url(list_id: &str, task_id: &str) -> String {
    format!("{}/checklistItems", task_url(list_id, task_id))
}

pub fn checklist_item_url(list_id: &str, task_id: &str, item_id: &str) -> String {
    format!(
        "{}/{}",
        checklist_url(list_id, task_id),
        encode_segment(item_id)
    )
}

pub fn attachments_url(list_id: &str, task_id: &str) -> String {
    format!("{}/attachments", task_url(list_id, task_id))
}

fn parse<T: serde::de::DeserializeOwned>(v: Value, what: &str) -> Result<T, AppError> {
    serde_json::from_value(v).map_err(|_| {
        AppError::Transport(format!(
            "Microsoft Graph returned an unexpected {what} shape"
        ))
    })
}

/// A per-task collection (checklist, attachments) read whole, or cut short by
/// this call's read budget or deadline. A partial walk's items are dropped here,
/// never returned: a caller would refuse ops on items it never saw, or report a
/// wrong total. Callers word the remedy (`tools::render::incomplete_walk`),
/// because only they know whether a task lookup ran and whether the cache is on.
#[derive(Debug)]
pub enum Walk<T> {
    Complete(Vec<T>),
    Incomplete,
}

fn parse_many<T: serde::de::DeserializeOwned>(
    items: Vec<Value>,
    what: &str,
) -> Result<Vec<T>, AppError> {
    items.into_iter().map(|v| parse(v, what)).collect()
}

impl GraphClient {
    pub fn list_lists(&self, budget: &mut Budget) -> Result<(Vec<TaskList>, bool), AppError> {
        let (items, complete) = self.get_collection(&lists_url(), budget)?;
        Ok((parse_many(items, "todoTaskList")?, complete))
    }

    pub fn list_tasks(
        &self,
        list_id: &str,
        budget: &mut Budget,
    ) -> Result<(Vec<Task>, bool), AppError> {
        let (items, complete) = self.get_collection(&tasks_url(list_id), budget)?;
        Ok((parse_many(items, "todoTask")?, complete))
    }

    pub fn get_task(
        &self,
        list_id: &str,
        task_id: &str,
        budget: &mut Budget,
    ) -> Result<Task, AppError> {
        let v = self.get_json(&task_url(list_id, task_id), budget)?;
        parse(v, "todoTask")
    }

    pub fn create_task(
        &self,
        list_id: &str,
        body: Value,
        budget: &mut Budget,
    ) -> Result<Task, AppError> {
        let v = self.send_json(Method::Post, &tasks_collection_url(list_id), body, budget)?;
        parse(v, "todoTask")
    }

    pub fn patch_task(
        &self,
        list_id: &str,
        task_id: &str,
        body: Value,
        budget: &mut Budget,
    ) -> Result<Task, AppError> {
        let v = self.send_json(Method::Patch, &task_url(list_id, task_id), body, budget)?;
        parse(v, "todoTask")
    }

    /// `Ok(false)` when Graph said 404 — already absent.
    pub fn delete_task(
        &self,
        list_id: &str,
        task_id: &str,
        budget: &mut Budget,
    ) -> Result<bool, AppError> {
        self.delete(&task_url(list_id, task_id), budget)
    }

    /// The WHOLE checklist, or `Walk::Incomplete`, never a silently short one.
    pub fn list_checklist(
        &self,
        list_id: &str,
        task_id: &str,
        budget: &mut Budget,
    ) -> Result<Walk<ChecklistItem>, AppError> {
        let (items, complete) = self.get_collection(&checklist_url(list_id, task_id), budget)?;
        if !complete {
            return Ok(Walk::Incomplete);
        }
        Ok(Walk::Complete(parse_many(items, "checklistItem")?))
    }

    pub fn create_checklist_item(
        &self,
        list_id: &str,
        task_id: &str,
        body: Value,
        budget: &mut Budget,
    ) -> Result<ChecklistItem, AppError> {
        let v = self.send_json(Method::Post, &checklist_url(list_id, task_id), body, budget)?;
        parse(v, "checklistItem")
    }

    pub fn patch_checklist_item(
        &self,
        list_id: &str,
        task_id: &str,
        item_id: &str,
        body: Value,
        budget: &mut Budget,
    ) -> Result<ChecklistItem, AppError> {
        let v = self.send_json(
            Method::Patch,
            &checklist_item_url(list_id, task_id, item_id),
            body,
            budget,
        )?;
        parse(v, "checklistItem")
    }

    pub fn delete_checklist_item(
        &self,
        list_id: &str,
        task_id: &str,
        item_id: &str,
        budget: &mut Budget,
    ) -> Result<bool, AppError> {
        self.delete(&checklist_item_url(list_id, task_id, item_id), budget)
    }

    /// The WHOLE attachment list, or `Walk::Incomplete`, like `list_checklist`.
    pub fn list_attachments(
        &self,
        list_id: &str,
        task_id: &str,
        budget: &mut Budget,
    ) -> Result<Walk<Attachment>, AppError> {
        let (items, complete) = self.get_collection(&attachments_url(list_id, task_id), budget)?;
        if !complete {
            return Ok(Walk::Incomplete);
        }
        Ok(Walk::Complete(parse_many(items, "attachment")?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_is_under_me_and_carries_top_100() {
        assert_eq!(
            lists_url(),
            "https://graph.microsoft.com/v1.0/me/todo/lists?$top=100"
        );
        assert_eq!(tasks_rel("L1"), "/me/todo/lists/L1/tasks?$top=100");
        assert_eq!(
            task_url("L1", "T1"),
            "https://graph.microsoft.com/v1.0/me/todo/lists/L1/tasks/T1"
        );
        assert_eq!(
            checklist_item_url("L1", "T1", "C1"),
            "https://graph.microsoft.com/v1.0/me/todo/lists/L1/tasks/T1/checklistItems/C1"
        );
        assert!(attachments_url("L1", "T1").starts_with("https://graph.microsoft.com/v1.0/me/"));
    }
}
