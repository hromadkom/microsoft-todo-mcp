//! Client-side predicates and sorting for `todo_search_tasks` (m4 §4–§5).
//! Nothing here reaches a URL: `$filter`, `$orderby`, `$search`, `$skip` never
//! exist in this crate (gate 2).

use chrono::{Datelike, NaiveDate};
use chrono_tz::Tz;

use super::datetime::{add_days, local_date, parse_instant};
use super::resolve::fold;
use super::text::html_to_text;
use crate::graph::models::Task;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Open,
    Completed,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueFilter {
    Any,
    Overdue,
    Today,
    Tomorrow,
    ThisWeek,
    Next7Days,
    NoDueDate,
    HasDueDate,
}

impl DueFilter {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "any" => DueFilter::Any,
            "overdue" => DueFilter::Overdue,
            "today" => DueFilter::Today,
            "tomorrow" => DueFilter::Tomorrow,
            "this_week" => DueFilter::ThisWeek,
            "next_7_days" => DueFilter::Next7Days,
            "no_due_date" => DueFilter::NoDueDate,
            "has_due_date" => DueFilter::HasDueDate,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    DueAsc,
    DueDesc,
    CreatedDesc,
    CreatedAsc,
    ModifiedDesc,
    TitleAsc,
    ImportanceDesc,
}

impl Sort {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "due_asc" => Sort::DueAsc,
            "due_desc" => Sort::DueDesc,
            "created_desc" => Sort::CreatedDesc,
            "created_asc" => Sort::CreatedAsc,
            "modified_desc" => Sort::ModifiedDesc,
            "title_asc" => Sort::TitleAsc,
            "importance_desc" => Sort::ImportanceDesc,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Criteria {
    pub query_terms: Vec<String>,
    pub status: StatusFilter,
    pub due: DueFilter,
    pub due_from: Option<NaiveDate>,
    pub due_to: Option<NaiveDate>,
    pub importance: Option<String>,
    /// Folded category names; a task matches if it carries ANY of them.
    pub categories: Vec<String>,
    pub sort: Sort,
}

/// A task with its resolved local due date, computed once per predicate pass.
pub struct Scored<'a> {
    pub task: &'a Task,
    pub due_local: Option<NaiveDate>,
    pub due_unresolved: bool,
}

pub fn score<'a>(task: &'a Task, tz: Tz) -> Scored<'a> {
    match &task.due_date_time {
        None => Scored {
            task,
            due_local: None,
            due_unresolved: false,
        },
        Some(d) => match local_date(d, tz) {
            Ok(date) => Scored {
                task,
                due_local: Some(date),
                due_unresolved: false,
            },
            Err(_) => Scored {
                task,
                due_local: None,
                due_unresolved: true,
            },
        },
    }
}

fn importance_rank(s: &str) -> u8 {
    match s {
        "high" => 2,
        "normal" => 1,
        _ => 0,
    }
}

pub fn matches(s: &Scored<'_>, c: &Criteria, today: NaiveDate) -> bool {
    let t = s.task;
    match c.status {
        StatusFilter::Open if t.is_completed() => return false,
        StatusFilter::Completed if !t.is_completed() => return false,
        _ => {}
    }
    if let Some(imp) = &c.importance
        && !t.importance.eq_ignore_ascii_case(imp)
    {
        return false;
    }
    if !c.categories.is_empty() {
        let has = t
            .categories
            .iter()
            .any(|cat| c.categories.contains(&fold(cat)));
        if !has {
            return false;
        }
    }
    // Due window. An unresolved zone is NOT "no due date" (m5 §6).
    let due_ok = match c.due {
        DueFilter::Any => true,
        DueFilter::Overdue => s.due_local.is_some_and(|d| d < today),
        DueFilter::Today => s.due_local == Some(today),
        DueFilter::Tomorrow => s.due_local == Some(add_days(today, 1)),
        DueFilter::ThisWeek => {
            let end = add_days(today, 6 - u64::from(today.weekday().num_days_from_monday()));
            s.due_local.is_some_and(|d| d >= today && d <= end)
        }
        DueFilter::Next7Days => s
            .due_local
            .is_some_and(|d| d >= today && d <= add_days(today, 7)),
        DueFilter::NoDueDate => t.due_date_time.is_none(),
        DueFilter::HasDueDate => t.due_date_time.is_some(),
    };
    if !due_ok {
        return false;
    }
    if let Some(from) = c.due_from
        && !s.due_local.is_some_and(|d| d >= from)
    {
        return false;
    }
    if let Some(to) = c.due_to
        && !s.due_local.is_some_and(|d| d <= to)
    {
        return false;
    }
    if !c.query_terms.is_empty() {
        let hay = haystack(t);
        if !c.query_terms.iter().all(|term| hay.contains(term.as_str())) {
            return false;
        }
    }
    true
}

/// Title + body text + category names, folded. Whitespace terms are ANDed.
fn haystack(t: &Task) -> String {
    let mut s = fold(&t.title);
    if let Some(b) = &t.body {
        s.push(' ');
        if b.content_type.eq_ignore_ascii_case("html") {
            s.push_str(&fold(&html_to_text(&b.content)));
        } else {
            s.push_str(&fold(&b.content));
        }
    }
    for c in &t.categories {
        s.push(' ');
        s.push_str(&fold(c));
    }
    s
}

pub fn sort_scored(items: &mut [Scored<'_>], sort: Sort) {
    items.sort_by(|a, b| {
        use std::cmp::Ordering;
        let by_title = || fold(&a.task.title).cmp(&fold(&b.task.title));
        match sort {
            // Nulls last on both due orders.
            Sort::DueAsc => match (a.due_local, b.due_local) {
                (Some(x), Some(y)) => x.cmp(&y).then_with(by_title),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => by_title(),
            },
            Sort::DueDesc => match (a.due_local, b.due_local) {
                (Some(x), Some(y)) => y.cmp(&x).then_with(by_title),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => by_title(),
            },
            Sort::CreatedDesc => created(b.task).cmp(&created(a.task)).then_with(by_title),
            Sort::CreatedAsc => created(a.task).cmp(&created(b.task)).then_with(by_title),
            Sort::ModifiedDesc => modified(b.task).cmp(&modified(a.task)).then_with(by_title),
            Sort::TitleAsc => by_title(),
            Sort::ImportanceDesc => importance_rank(&b.task.importance)
                .cmp(&importance_rank(&a.task.importance))
                .then_with(by_title),
        }
    });
}

fn created(t: &Task) -> i64 {
    t.created_date_time
        .as_deref()
        .and_then(parse_instant)
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

fn modified(t: &Task) -> i64 {
    t.last_modified_date_time
        .as_deref()
        .and_then(parse_instant)
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

/// Split a free-text query into folded, ANDed terms.
pub fn query_terms(q: &str) -> Vec<String> {
    fold(q).split_whitespace().map(str::to_string).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::graph::models::{DateTimeTimeZone, ItemBody};

    fn task(title: &str, due: Option<&str>, status: &str) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": title, "title": title, "status": status, "importance": "normal",
            "categories": ["Home"],
            "body": {"content": "<p>Buy <b>milk</b></p>", "contentType": "html"},
        }))
        .map(|mut t: Task| {
            t.due_date_time = due.map(|d| DateTimeTimeZone {
                date_time: format!("{d}T00:00:00.0000000"),
                time_zone: "UTC".into(),
            });
            t.body = Some(ItemBody {
                content: "<p>Buy <b>milk</b></p>".into(),
                content_type: "html".into(),
            });
            t
        })
        .unwrap()
    }

    fn crit() -> Criteria {
        Criteria {
            query_terms: vec![],
            status: StatusFilter::Open,
            due: DueFilter::Any,
            due_from: None,
            due_to: None,
            importance: None,
            categories: vec![],
            sort: Sort::DueAsc,
        }
    }

    #[test]
    fn due_buckets_use_local_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 25).unwrap();
        let tz = Tz::UTC;
        let a = task("a", Some("2026-08-24"), "notStarted");
        let b = task("b", Some("2026-08-25"), "notStarted");
        let c = task("c", None, "notStarted");
        let mut cr = crit();
        cr.due = DueFilter::Overdue;
        assert!(matches(&score(&a, tz), &cr, today));
        assert!(!matches(&score(&b, tz), &cr, today));
        cr.due = DueFilter::Today;
        assert!(matches(&score(&b, tz), &cr, today));
        cr.due = DueFilter::NoDueDate;
        assert!(matches(&score(&c, tz), &cr, today));
        assert!(!matches(&score(&a, tz), &cr, today));
    }

    #[test]
    fn query_matches_html_body_text_and_categories() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 25).unwrap();
        let t = task("Groceries", None, "notStarted");
        let mut cr = crit();
        cr.query_terms = query_terms("buy MILK");
        assert!(matches(&score(&t, Tz::UTC), &cr, today));
        cr.query_terms = query_terms("home");
        assert!(matches(&score(&t, Tz::UTC), &cr, today));
        cr.query_terms = query_terms("<b>");
        assert!(!matches(&score(&t, Tz::UTC), &cr, today));
    }

    #[test]
    fn sort_puts_nulls_last() {
        let a = task("a", None, "notStarted");
        let b = task("b", Some("2026-08-25"), "notStarted");
        let c = task("c", Some("2026-08-20"), "notStarted");
        let mut v = vec![score(&a, Tz::UTC), score(&b, Tz::UTC), score(&c, Tz::UTC)];
        sort_scored(&mut v, Sort::DueAsc);
        let ids: Vec<&str> = v.iter().map(|s| s.task.id.as_str()).collect();
        assert_eq!(ids, ["c", "b", "a"]);
        sort_scored(&mut v, Sort::DueDesc);
        let ids: Vec<&str> = v.iter().map(|s| s.task.id.as_str()).collect();
        assert_eq!(ids, ["b", "c", "a"]);
    }
}
