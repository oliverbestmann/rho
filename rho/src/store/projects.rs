//! Named projects: one shared text and a work log per project. Sessions
//! join a project through their extension state (`project.name`). Unlike
//! the rest of the store, extensions write these tables.

use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{SessionId, Store, now_ms, path_str};

pub(super) const MIGRATION: &str = r#"
-- Named projects (ADR 0078): a text all their sessions see.
CREATE TABLE projects (
    name TEXT NOT NULL PRIMARY KEY CHECK (name <> ''),
    text TEXT NOT NULL DEFAULT '',
    -- Goes up with each change of `text`.
    version INTEGER NOT NULL DEFAULT 0 CHECK (version >= 0),
    -- The session that changed `text` last.
    updated_by TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

-- Finished work of a project. No foreign key on the session: a new session
-- is saved after its first prompt.
CREATE TABLE project_log (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL REFERENCES projects (name) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    -- The session's next message number (`items.seq`) when it was written.
    seq INTEGER NOT NULL CHECK (seq >= 0),
    kind TEXT NOT NULL CHECK (kind IN ('entry', 'compaction')),
    title TEXT NOT NULL CHECK (title <> ''),
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX project_log_by_project ON project_log (project, id);
"#;

/// The extension state key that holds a session's project name.
pub const PROJECT_KEY: &str = "project.name";

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub name: String,
    pub text: String,
    pub version: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogKind {
    /// Written by the model.
    Entry,
    /// Written after a compaction.
    Compaction,
}

impl LogKind {
    fn as_str(self) -> &'static str {
        match self {
            LogKind::Entry => "entry",
            LogKind::Compaction => "compaction",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogEntry {
    pub id: i64,
    pub session_id: SessionId,
    pub seq: i64,
    pub kind: LogKind,
    pub title: String,
    pub text: String,
    /// Unix milliseconds.
    pub created_at: i64,
    /// `created_at` as `YYYY-MM-DD`, in UTC.
    pub date: String,
}

fn read_project(r: &Row) -> rusqlite::Result<Project> {
    Ok(Project {
        name: r.get(0)?,
        text: r.get(1)?,
        version: r.get(2)?,
        updated_at: r.get(3)?,
    })
}

fn read_entry(r: &Row) -> rusqlite::Result<LogEntry> {
    Ok(LogEntry {
        id: r.get(0)?,
        session_id: r.get(1)?,
        seq: r.get(2)?,
        kind: match r.get_ref(3)?.as_str()? {
            "compaction" => LogKind::Compaction,
            _ => LogKind::Entry,
        },
        title: r.get(4)?,
        text: r.get(5)?,
        created_at: r.get(6)?,
        date: r.get(7)?,
    })
}

const ENTRY: &str =
    "id, session_id, seq, kind, title, text, created_at, date(created_at / 1000, 'unixepoch')";

fn project(c: &Connection, name: &str) -> Result<Option<Project>> {
    Ok(c.query_row(
        "SELECT name, text, version, updated_at FROM projects WHERE name = ?1",
        [name],
        read_project,
    )
    .optional()?)
}

impl Store {
    pub async fn project(&self, name: &str) -> Result<Option<Project>> {
        let name = name.to_string();
        self.call(move |c| project(c, &name)).await
    }

    /// The project `name`, created empty if it does not exist.
    pub async fn open_project(&self, name: &str) -> Result<Project> {
        let name = name.to_string();
        self.call(move |c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO projects (name, created_at, updated_at) VALUES (?1, ?2, ?2)
                 ON CONFLICT DO NOTHING",
                params![name, now],
            )?;
            Ok(project(c, &name)?.expect("just inserted"))
        })
        .await
    }

    /// All projects, the latest changed first.
    pub async fn projects(&self) -> Result<Vec<Project>> {
        self.call(|c| {
            let mut stmt = c.prepare(
                "SELECT name, text, version, updated_at FROM projects
                 ORDER BY updated_at DESC, name",
            )?;
            Ok(stmt
                .query_map([], read_project)?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// Sets the project's text if it is still at `version`. Returns the new
    /// version, or `None` if another session changed it in between.
    pub async fn set_project_text(
        &self,
        name: &str,
        version: i64,
        text: &str,
        by: &SessionId,
    ) -> Result<Option<i64>> {
        let (name, text, by) = (name.to_string(), text.to_string(), by.clone());
        self.call(move |c| {
            Ok(c.query_row(
                "UPDATE projects SET text = ?3, version = version + 1, updated_by = ?4,
                     updated_at = ?5
                 WHERE name = ?1 AND version = ?2 RETURNING version",
                params![name, version, text, by, now_ms()],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await
    }

    /// Adds a log entry to the project and returns its id. Its `seq` is the
    /// session's next message number in the store.
    pub async fn add_log(
        &self,
        name: &str,
        session: &SessionId,
        kind: LogKind,
        title: &str,
        text: &str,
    ) -> Result<i64> {
        let (name, session) = (name.to_string(), session.clone());
        let (title, text) = (title.to_string(), text.to_string());
        self.call(move |c| {
            Ok(c.query_row(
                "INSERT INTO project_log (project, session_id, seq, kind, title, text, created_at)
                 VALUES (?1, ?2,
                     (SELECT coalesce(max(seq) + 1, 0) FROM items WHERE session_id = ?2),
                     ?3, ?4, ?5, ?6)
                 RETURNING id",
                params![name, session, kind.as_str(), title, text, now_ms()],
                |r| r.get(0),
            )?)
        })
        .await
    }

    /// The project's latest `limit` log entries, newest first.
    pub async fn log(&self, name: &str, limit: usize) -> Result<Vec<LogEntry>> {
        let name = name.to_string();
        self.call(move |c| {
            let sql = format!(
                "SELECT {ENTRY} FROM project_log WHERE project = ?1 ORDER BY id DESC LIMIT ?2"
            );
            let mut stmt = c.prepare(&sql)?;
            Ok(stmt
                .query_map(params![name, limit as i64], read_entry)?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    pub async fn log_entry(&self, name: &str, id: i64) -> Result<Option<LogEntry>> {
        let name = name.to_string();
        self.call(move |c| {
            let sql = format!("SELECT {ENTRY} FROM project_log WHERE project = ?1 AND id = ?2");
            Ok(c.query_row(&sql, params![name, id], read_entry)
                .optional()?)
        })
        .await
    }

    /// Log entries whose title or text contains `query` (any case for
    /// ASCII), newest first.
    pub async fn search_log(&self, name: &str, query: &str, limit: usize) -> Result<Vec<LogEntry>> {
        let name = name.to_string();
        let pattern = format!(
            "%{}%",
            query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        self.call(move |c| {
            let sql = format!(
                "SELECT {ENTRY} FROM project_log
                 WHERE project = ?1
                   AND (title LIKE ?2 ESCAPE '\\' OR text LIKE ?2 ESCAPE '\\')
                 ORDER BY id DESC LIMIT ?3"
            );
            let mut stmt = c.prepare(&sql)?;
            Ok(stmt
                .query_map(params![name, pattern, limit as i64], read_entry)?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// The project of the latest saved session in `cwd` that had one.
    pub async fn last_project(&self, cwd: &Path) -> Result<Option<String>> {
        let cwd = path_str(cwd)?.to_string();
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT extension_state ->> ?2 FROM sessions
                 WHERE cwd = ?1 AND extension_state ->> ?2 IS NOT NULL
                 ORDER BY updated_at DESC LIMIT 1",
                params![cwd, format!("$.\"{PROJECT_KEY}\"")],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await
    }

    /// Replaces `phrase` with `mask` in project texts and log entries.
    /// Returns the number of changed rows.
    pub async fn redact_projects(&self, phrase: &str, mask: &str) -> Result<usize> {
        let (phrase, mask) = (phrase.to_string(), mask.to_string());
        self.call(move |c| {
            let tx = c.transaction()?;
            let mut n = tx.execute(
                "UPDATE projects SET text = replace(text, ?1, ?2) WHERE instr(text, ?1) > 0",
                [&phrase, &mask],
            )?;
            n += tx.execute(
                "UPDATE project_log SET title = replace(title, ?1, ?2), text = replace(text, ?1, ?2)
                 WHERE instr(title, ?1) > 0 OR instr(text, ?1) > 0",
                [&phrase, &mask],
            )?;
            tx.commit()?;
            Ok(n)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_changes_only_from_the_version_it_saw() {
        let store = Store::memory().unwrap();
        let by = SessionId::new("s1");
        let p = store.open_project("rho").await.unwrap();
        assert_eq!((p.text.as_str(), p.version), ("", 0));
        assert_eq!(store.open_project("rho").await.unwrap(), p);

        let v = store.set_project_text("rho", 0, "goal", &by).await.unwrap();
        assert_eq!(v, Some(1));
        let stale = store
            .set_project_text("rho", 0, "other", &by)
            .await
            .unwrap();
        assert_eq!(stale, None);
        assert_eq!(store.project("rho").await.unwrap().unwrap().text, "goal");
        assert_eq!(store.projects().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn log_lists_newest_first_and_searches() {
        let store = Store::memory().unwrap();
        let s = SessionId::new("s1");
        store.open_project("rho").await.unwrap();
        let a = store
            .add_log("rho", &s, LogKind::Entry, "WebSearch", "100% done")
            .await
            .unwrap();
        let b = store
            .add_log("rho", &s, LogKind::Compaction, "Compacted", "diff cache")
            .await
            .unwrap();
        let ids = |l: Vec<LogEntry>| l.iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(ids(store.log("rho", 50).await.unwrap()), [b, a]);
        assert_eq!(
            ids(store.search_log("rho", "websearch", 10).await.unwrap()),
            [a]
        );
        assert_eq!(ids(store.search_log("rho", "%", 10).await.unwrap()), [a]);
        let e = store.log_entry("rho", a).await.unwrap().unwrap();
        assert_eq!((e.seq, e.kind), (0, LogKind::Entry));
        assert_eq!(store.log_entry("other", a).await.unwrap(), None);

        assert_eq!(store.redact_projects("done", "X").await.unwrap(), 1);
        assert_eq!(
            store.log_entry("rho", a).await.unwrap().unwrap().text,
            "100% X"
        );
    }

    #[tokio::test]
    async fn last_project_of_a_directory() {
        let store = Store::memory().unwrap();
        assert_eq!(store.last_project(Path::new("/w")).await.unwrap(), None);
        let mut st = super::super::tests::state("s1", "/w");
        st.extension_state = serde_json::json!({ PROJECT_KEY: "rho" })
            .as_object()
            .unwrap()
            .clone();
        store.save_state(st).await.unwrap();
        assert_eq!(
            store
                .last_project(Path::new("/w"))
                .await
                .unwrap()
                .as_deref(),
            Some("rho")
        );
    }
}
