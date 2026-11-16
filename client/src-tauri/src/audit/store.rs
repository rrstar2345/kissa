//! Audit journal storage (`audit_log`): one row per thing that happened.
//! Rows older than 90 days are pruned (and the table is capped), see `prune`.

use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, SqlitePool};

use super::{clip, Entry};
use crate::trading::now_ts;

pub const KEEP_DAYS: i64 = 90;
/// Safety net on top of the age limit.
pub const KEEP_ROWS: i64 = 200_000;
const MAX_PAGE: i64 = 500;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AuditRow {
    pub id: i64,
    pub at: String,
    /// `order` | `risk` | `algo` | `copier` | `account` | `settings` | `system`
    pub category: String,
    pub action: String,
    /// `user` | `system` | `algo` | `copier` | `broker`
    pub actor: String,
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    /// What the entry is about: an order id, an algo id, a setting key.
    pub subject: String,
    pub summary: String,
    /// JSON text with the details, if any.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AuditFilter {
    pub categories: Vec<String>,
    pub account_id: Option<String>,
    /// IST dates (`YYYY-MM-DD`), both inclusive.
    pub from: Option<String>,
    pub to: Option<String>,
    pub search: Option<String>,
}

pub async fn init(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS audit_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            at TEXT NOT NULL,
            category TEXT NOT NULL,
            action TEXT NOT NULL,
            actor TEXT NOT NULL,
            account_id TEXT,
            account_name TEXT,
            subject TEXT NOT NULL DEFAULT '',
            summary TEXT NOT NULL,
            detail TEXT
        );"#,
    )
    .execute(pool)
    .await?;
    for ddl in [
        "CREATE INDEX IF NOT EXISTS idx_audit_at ON audit_log(at)",
        "CREATE INDEX IF NOT EXISTS idx_audit_category ON audit_log(category)",
        "CREATE INDEX IF NOT EXISTS idx_audit_account ON audit_log(account_id)",
    ] {
        sqlx::query(ddl).execute(pool).await?;
    }
    Ok(())
}

pub async fn insert(pool: &SqlitePool, e: &Entry, account_name: Option<&str>) -> Result<i64> {
    insert_at(pool, &now_ts(), e, account_name).await
}

pub(crate) async fn insert_at(pool: &SqlitePool, at: &str, e: &Entry, account_name: Option<&str>) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO audit_log (at, category, action, actor, account_id, account_name, subject, summary, detail) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(at)
    .bind(e.category.as_str())
    .bind(&e.action)
    .bind(e.actor.as_str())
    .bind(e.account_id.as_deref())
    .bind(account_name)
    .bind(clip(&e.subject, 200))
    .bind(clip(&e.summary, 400))
    .bind(e.detail_json())
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Start of an IST calendar day and of the next one, as UTC timestamps in
/// the format of `now_ts` (so string comparison matches time order).
fn ist_day_bounds(date: &str) -> Option<(String, String)> {
    let d = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
    let start = d.and_hms_opt(0, 0, 0)? - Duration::minutes(330);
    let next = start + Duration::days(1);
    let fmt = |t: NaiveDateTime| Utc.from_utc_datetime(&t).to_rfc3339_opts(SecondsFormat::Millis, true);
    Some((fmt(start), fmt(next)))
}

/// `%`, `_` and `\` in a search term are plain characters.
fn like_pattern(term: &str) -> String {
    let mut out = String::from("%");
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// The WHERE conditions (each starting with ` AND`) and their bind values.
/// Only `?` placeholders and fixed text go into the SQL.
pub(crate) fn build_where(f: &AuditFilter) -> (String, Vec<String>) {
    let mut sql = String::new();
    let mut binds: Vec<String> = Vec::new();

    let cats: Vec<&str> = f.categories.iter().map(|c| c.trim()).filter(|c| !c.is_empty()).collect();
    if !cats.is_empty() {
        sql.push_str(&format!(" AND category IN ({})", vec!["?"; cats.len()].join(",")));
        binds.extend(cats.iter().map(|c| c.to_string()));
    }
    if let Some(a) = f.account_id.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        sql.push_str(" AND account_id = ?");
        binds.push(a.to_string());
    }
    if let Some((start, _)) = f.from.as_deref().and_then(ist_day_bounds) {
        sql.push_str(" AND at >= ?");
        binds.push(start);
    }
    if let Some((_, next)) = f.to.as_deref().and_then(ist_day_bounds) {
        sql.push_str(" AND at < ?");
        binds.push(next);
    }
    if let Some(term) = f.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let cols = ["summary", "subject", "action", "account_name", "detail"];
        let ors: Vec<String> = cols.iter().map(|c| format!("{c} LIKE ? ESCAPE '\\'")).collect();
        sql.push_str(&format!(" AND ({})", ors.join(" OR ")));
        for _ in cols {
            binds.push(like_pattern(term));
        }
    }
    (sql, binds)
}

/// Newest first. `before_id` continues after the last row of the previous page.
pub async fn list(pool: &SqlitePool, f: &AuditFilter, before_id: Option<i64>, limit: i64) -> Result<Vec<AuditRow>> {
    let (cond, binds) = build_where(f);
    let mut sql = format!(
        "SELECT id, at, category, action, actor, account_id, account_name, subject, summary, detail \
         FROM audit_log WHERE 1=1{cond}"
    );
    if let Some(b) = before_id {
        sql.push_str(&format!(" AND id < {b}"));
    }
    sql.push_str(&format!(" ORDER BY id DESC LIMIT {}", limit.clamp(1, MAX_PAGE)));
    let mut q = sqlx::query_as::<_, AuditRow>(AssertSqlSafe(sql));
    for b in binds {
        q = q.bind(b);
    }
    Ok(q.fetch_all(pool).await?)
}

/// Drop rows older than `KEEP_DAYS` and anything beyond `KEEP_ROWS`.
pub async fn prune(pool: &SqlitePool) -> Result<u64> {
    prune_at(pool, Utc::now(), KEEP_ROWS).await
}

pub(crate) async fn prune_at(pool: &SqlitePool, now: DateTime<Utc>, keep_rows: i64) -> Result<u64> {
    let cutoff = (now - Duration::days(KEEP_DAYS)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut n = sqlx::query("DELETE FROM audit_log WHERE at < ?").bind(cutoff).execute(pool).await?.rows_affected();
    // The id just past the newest `keep_rows` rows; nothing is deleted while fewer exist.
    n += sqlx::query("DELETE FROM audit_log WHERE id <= (SELECT id FROM audit_log ORDER BY id DESC LIMIT 1 OFFSET ?)")
        .bind(keep_rows)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::super::{Actor, Category};
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        init(&pool).await.unwrap();
        pool
    }

    fn entry(cat: Category, summary: &str) -> Entry {
        Entry::new(cat, "test", summary).actor(Actor::User)
    }

    #[test]
    fn ist_day_bounds_are_utc_instants() {
        let (s, n) = ist_day_bounds("2026-11-15").unwrap();
        assert_eq!(s, "2026-11-14T18:30:00.000Z");
        assert_eq!(n, "2026-11-15T18:30:00.000Z");
        assert!(ist_day_bounds("15/11/2026").is_none());
        assert!(ist_day_bounds("").is_none());
    }

    #[test]
    fn empty_filter_adds_no_conditions() {
        let (sql, binds) = build_where(&AuditFilter::default());
        assert!(sql.is_empty() && binds.is_empty());
        let blank = AuditFilter {
            categories: vec!["  ".into()],
            account_id: Some(" ".into()),
            from: Some("junk".into()),
            to: Some("junk".into()),
            search: Some("  ".into()),
        };
        let (sql, binds) = build_where(&blank);
        assert!(sql.is_empty() && binds.is_empty());
    }

    #[test]
    fn filter_builds_placeholders_in_bind_order() {
        let f = AuditFilter {
            categories: vec!["order".into(), "risk".into()],
            account_id: Some("a1".into()),
            from: Some("2026-11-15".into()),
            to: Some("2026-11-15".into()),
            search: Some("nifty".into()),
        };
        let (sql, binds) = build_where(&f);
        assert_eq!(sql.matches('?').count(), binds.len());
        assert!(sql.starts_with(" AND category IN (?,?) AND account_id = ? AND at >= ? AND at < ?"));
        assert_eq!(&binds[..3], ["order", "risk", "a1"]);
        assert_eq!(binds.last().unwrap(), "%nifty%");
        assert_eq!(binds.len(), 4 + 1 + 5);
    }

    #[test]
    fn search_terms_are_escaped() {
        assert_eq!(like_pattern("50%_off\\"), "%50\\%\\_off\\\\%");
    }

    #[tokio::test]
    async fn insert_list_and_filter() {
        let pool = pool().await;
        let a = insert(&pool, &entry(Category::Order, "BUY NIFTY 24000 CE").account("a1"), Some("Paper 1")).await.unwrap();
        let b = insert(&pool, &entry(Category::Risk, "Kill switch ON"), None).await.unwrap();
        assert!(b > a);

        let all = list(&pool, &AuditFilter::default(), None, 50).await.unwrap();
        assert_eq!(all.iter().map(|r| r.id).collect::<Vec<_>>(), vec![b, a]); // newest first
        assert_eq!(all[1].account_name.as_deref(), Some("Paper 1"));
        assert_eq!(all[1].actor, "user");
        assert!(all[0].account_id.is_none());

        let f = AuditFilter { categories: vec!["risk".into()], ..Default::default() };
        assert_eq!(list(&pool, &f, None, 50).await.unwrap().len(), 1);
        let f = AuditFilter { account_id: Some("a1".into()), ..Default::default() };
        assert_eq!(list(&pool, &f, None, 50).await.unwrap()[0].id, a);
        let f = AuditFilter { search: Some("paper 1".into()), ..Default::default() };
        assert_eq!(list(&pool, &f, None, 50).await.unwrap()[0].id, a); // account name, any case
        let f = AuditFilter { search: Some("100%".into()), ..Default::default() };
        assert!(list(&pool, &f, None, 50).await.unwrap().is_empty()); // % is not a wildcard

        // Paging.
        let page = list(&pool, &AuditFilter::default(), None, 1).await.unwrap();
        assert_eq!(page[0].id, b);
        let next = list(&pool, &AuditFilter::default(), Some(b), 1).await.unwrap();
        assert_eq!(next[0].id, a);
    }

    #[tokio::test]
    async fn date_range_uses_ist_days() {
        let pool = pool().await;
        // 2026-11-14 20:00 UTC is 01:30 IST on the 15th.
        insert_at(&pool, "2026-11-14T20:00:00.000Z", &entry(Category::System, "early"), None).await.unwrap();
        insert_at(&pool, "2026-11-15T20:00:00.000Z", &entry(Category::System, "next day"), None).await.unwrap();
        let f = AuditFilter { from: Some("2026-11-15".into()), to: Some("2026-11-15".into()), ..Default::default() };
        let rows = list(&pool, &f, None, 50).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary, "early");
    }

    #[tokio::test]
    async fn prune_drops_old_rows_and_caps_the_table() {
        let pool = pool().await;
        let now = Utc::now();
        let old = (now - Duration::days(KEEP_DAYS + 1)).to_rfc3339_opts(SecondsFormat::Millis, true);
        insert_at(&pool, &old, &entry(Category::System, "old"), None).await.unwrap();
        for i in 0..5 {
            insert(&pool, &entry(Category::System, &format!("new {i}")), None).await.unwrap();
        }
        assert_eq!(prune_at(&pool, now, 100).await.unwrap(), 1);
        assert_eq!(list(&pool, &AuditFilter::default(), None, 50).await.unwrap().len(), 5);

        // Cap: keep only the newest 3.
        assert_eq!(prune_at(&pool, now, 3).await.unwrap(), 2);
        let rows = list(&pool, &AuditFilter::default(), None, 50).await.unwrap();
        assert_eq!(rows.iter().map(|r| r.summary.as_str()).collect::<Vec<_>>(), vec!["new 4", "new 3", "new 2"]);
    }

    #[tokio::test]
    async fn long_text_is_clipped_and_secrets_scrubbed() {
        let pool = pool().await;
        let e = entry(Category::Settings, &"x".repeat(1000))
            .subject("s".repeat(500))
            .detail(serde_json::json!({ "apiKey": "abc", "nested": { "accessToken": "t", "ok": 1 } }));
        let id = insert(&pool, &e, None).await.unwrap();
        let row = list(&pool, &AuditFilter::default(), None, 1).await.unwrap().remove(0);
        assert_eq!(row.id, id);
        assert_eq!(row.summary.chars().count(), 400);
        assert_eq!(row.subject.chars().count(), 200);
        let d = row.detail.unwrap();
        assert!(!d.contains("abc") && !d.contains("\"t\""));
        assert!(d.contains("\"ok\":1"));
    }
}
