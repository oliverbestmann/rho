//! Session persistence in SQLite.
//!
//! Every session opens its own [`Store`] (its own connection), so sessions can
//! use different database files (`Agent::sessions_db`). Several connections
//! and processes can share one file: WAL mode plus a busy timeout.
//!
//! Compaction does not delete anything: it starts a new `epoch` and writes the
//! summary history under it. A session's history is the messages of its
//! current epoch; older epochs stay for search. `seq` grows across epochs.
//!
//! Extensions get read access through `Context::store`; writes are crate-only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

use crate::extension::{Diff, ProfileId};
use crate::feature::FeatureId;
use crate::provider::{Content, Message, ModelSpec, Role, Thinking, ToolCallId, Usage};

/// Schema migrations; `PRAGMA user_version` is the number applied.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE sessions (
    id TEXT NOT NULL PRIMARY KEY CHECK (id <> ''),
    cwd TEXT NOT NULL CHECK (cwd <> ''),
    additional_dirs TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(additional_dirs) AND json_type(additional_dirs) = 'array'),
    title TEXT,
    model TEXT NOT NULL CHECK (model <> ''),
    thinking TEXT NOT NULL
        CHECK (thinking IN ('off', 'low', 'medium', 'high', 'xhigh', 'max')),
    input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cache_read_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_read_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_write_tokens >= 0),
    cost REAL NOT NULL DEFAULT 0 CHECK (cost >= 0),
    epoch INTEGER NOT NULL DEFAULT 0 CHECK (epoch >= 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX sessions_by_cwd ON sessions (cwd, updated_at DESC, id DESC);
CREATE INDEX sessions_by_update ON sessions (updated_at DESC, id DESC);

CREATE TABLE messages (
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    seq INTEGER NOT NULL CHECK (seq >= 0),
    epoch INTEGER NOT NULL CHECK (epoch >= 0),
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    -- Vec<Content> as JSON, the provider wire format.
    content TEXT NOT NULL CHECK (json_valid(content) AND json_type(content) = 'array'),
    -- Text and tool calls as plain text, for search.
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, seq)
) STRICT;

CREATE INDEX messages_by_epoch ON messages (session_id, epoch, seq);
"#,
    r#"
-- The latest request, for the context in use.
ALTER TABLE sessions ADD COLUMN last_input_tokens INTEGER NOT NULL DEFAULT 0
    CHECK (last_input_tokens >= 0);
ALTER TABLE sessions ADD COLUMN last_output_tokens INTEGER NOT NULL DEFAULT 0
    CHECK (last_output_tokens >= 0);
ALTER TABLE sessions ADD COLUMN last_cache_read_tokens INTEGER NOT NULL DEFAULT 0
    CHECK (last_cache_read_tokens >= 0);
ALTER TABLE sessions ADD COLUMN last_cache_write_tokens INTEGER NOT NULL DEFAULT 0
    CHECK (last_cache_write_tokens >= 0);
"#,
    r#"
-- `Profile::id`, e.g. 'write' or 'plan'.
ALTER TABLE sessions ADD COLUMN profile TEXT NOT NULL DEFAULT 'write' CHECK (profile <> '');
"#,
    r#"
-- Ids of the features that are on, a JSON array.
ALTER TABLE sessions ADD COLUMN features TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(features) AND json_type(features) = 'array');
"#,
    r#"
-- Edit diffs, for the replay; kept for `DIFF_DAYS`.
CREATE TABLE diffs (
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    tool_use_id TEXT NOT NULL,
    path TEXT NOT NULL,
    old TEXT NOT NULL,
    new TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, tool_use_id)
) STRICT;

CREATE INDEX diffs_by_age ON diffs (created_at);
"#,
    r#"
-- `Context::set_stored` values of extensions, a JSON object.
ALTER TABLE sessions ADD COLUMN extension_state TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(extension_state) AND json_type(extension_state) = 'object');
"#,
];

/// Edit diffs older than this are deleted; the edit then shows its output only.
const DIFF_DAYS: i64 = 7;

const COLUMNS: &str = "id, cwd, additional_dirs, title, model, thinking, input_tokens, \
    output_tokens, cache_read_tokens, cache_write_tokens, last_input_tokens, last_output_tokens, \
    last_cache_read_tokens, last_cache_write_tokens, cost, epoch, profile, features, \
    extension_state, created_at, updated_at";

pub const MASK: &str = "REDACTED";

/// A session database. Cheap to clone; clones share the connection.
#[derive(Clone)]
pub struct Store {
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

/// A stored session.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub id: SessionId,
    pub cwd: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
    pub title: Option<String>,
    pub model: ModelSpec,
    pub thinking: Thinking,
    pub usage: Usage,
    /// Usage of the latest request.
    pub last_usage: Usage,
    pub cost: f64,
    /// Number of compactions.
    pub epoch: i64,
    pub profile: ProfileId,
    /// The features that are on.
    pub features: Vec<FeatureId>,
    /// Values extensions keep with the session, see `Context::set_stored`.
    pub extension_state: serde_json::Map<String, serde_json::Value>,
    /// Unix milliseconds.
    pub created_at: i64,
    pub updated_at: i64,
}

/// One page of `Store::list`, newest first.
#[derive(Debug)]
pub struct Page {
    pub sessions: Vec<SessionInfo>,
    /// Pass to `Store::list` for the next page; `None` on the last one.
    pub next: Option<String>,
}

/// A stored message of any epoch, see `Store::messages`.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredMessage {
    pub seq: i64,
    pub epoch: i64,
    pub message: Message,
    /// Unix milliseconds.
    pub created_at: i64,
}

/// What `Store::save_state` writes besides messages.
pub(crate) struct State {
    pub id: SessionId,
    pub cwd: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
    pub title: Option<String>,
    pub model: ModelSpec,
    pub thinking: Thinking,
    pub usage: Usage,
    pub last_usage: Usage,
    pub cost: f64,
    pub profile: ProfileId,
    pub features: Vec<FeatureId>,
    pub extension_state: serde_json::Map<String, serde_json::Value>,
}

/// A stored session ready to continue: its info, the `seq` of the first
/// message of the current epoch, and that epoch's messages.
pub(crate) struct Loaded {
    pub info: SessionInfo,
    pub base: i64,
    pub history: Vec<Message>,
}

/// `$RHO_SESSIONS_DB`, else `sessions.db` in the config dir.
pub fn default_path() -> PathBuf {
    std::env::var_os("RHO_SESSIONS_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config_dir().join("sessions.db"))
}

crate::id_type! {
    /// A session's id; ACP clients and the store address sessions by it.
    pub SessionId
}

impl SessionId {
    /// A UUIDv7: unique across processes, sorts by creation time.
    pub fn generate() -> Self {
        Self::new(new_session_id())
    }
}

fn new_session_id() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("random session id");
    b[..6].copy_from_slice(&(now_ms() as u64).to_be_bytes()[2..]);
    b[6] = 0x70 | (b[6] & 0x0f);
    b[8] = 0x80 | (b[8] & 0x3f);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not UTF-8: {}", path.display()))
}

fn int(n: u64) -> Result<i64> {
    Ok(i64::try_from(n)?)
}

impl Store {
    /// Opens (or creates) the database and brings its schema up to date.
    pub fn open(path: impl Into<PathBuf>) -> Result<Store> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn, path)
    }

    /// A private in-memory database, for tests.
    pub fn memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?, ":memory:".into())
    }

    fn init(mut conn: Connection, path: PathBuf) -> Result<Store> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;",
        )?;
        let fk: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
        if !fk {
            bail!("SQLite was built without foreign key support");
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let version = usize::try_from(version)?;
        if version > MIGRATIONS.len() {
            bail!(
                "{} has schema version {version}, this rho knows {}",
                path.display(),
                MIGRATIONS.len()
            );
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            tx.execute_batch(sql)
                .with_context(|| format!("migrating {} to {}", path.display(), i + 1))?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
        }
        tx.commit()?;
        Ok(Store {
            path,
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs `f` on the connection off the async runtime.
    async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&mut conn.lock().unwrap())).await?
    }

    pub async fn session(&self, id: &SessionId) -> Result<Option<SessionInfo>> {
        let id = id.clone();
        self.call(move |c| {
            let sql = format!("SELECT {COLUMNS} FROM sessions WHERE id = ?1");
            c.query_row(&sql, [&id], read_info).optional()?.transpose()
        })
        .await
    }

    /// Sessions with messages by last update, newest first, optionally only
    /// those in `cwd`.
    /// `cursor` is `Page::next` of the previous page.
    pub async fn list(
        &self,
        cwd: Option<&Path>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page> {
        let cwd = cwd.map(path_str).transpose()?.map(str::to_string);
        let after = match cursor {
            Some(c) => {
                let (at, id) = c.split_once(':').context("bad cursor")?;
                Some((at.parse::<i64>().context("bad cursor")?, id.to_string()))
            }
            None => None,
        };
        self.call(move |c| {
            let sql = format!(
                "SELECT {COLUMNS} FROM sessions
                 WHERE (?1 IS NULL OR cwd = ?1)
                   AND (?2 IS NULL OR (updated_at, id) < (?2, ?3))
                   AND EXISTS (SELECT 1 FROM messages WHERE session_id = sessions.id)
                 ORDER BY updated_at DESC, id DESC LIMIT ?4"
            );
            let (at, id) = after.unzip();
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(params![cwd, at, id, limit as i64 + 1], read_info)?;
            let mut sessions = rows.map(|r| r?).collect::<Result<Vec<_>>>()?;
            let next = (sessions.len() > limit).then(|| {
                sessions.truncate(limit);
                let last = sessions.last().unwrap();
                format!("{}:{}", last.updated_at, last.id)
            });
            Ok(Page { sessions, next })
        })
        .await
    }

    /// The session's current history (messages since the last compaction).
    pub async fn history(&self, id: &SessionId) -> Result<Vec<Message>> {
        Ok(self.load(id).await?.map(|l| l.history).unwrap_or_default())
    }

    /// All messages of the session, older epochs included, by `seq`.
    pub async fn messages(&self, id: &SessionId) -> Result<Vec<StoredMessage>> {
        let id = id.clone();
        self.call(move |c| {
            let mut stmt = c.prepare(
                "SELECT seq, epoch, role, content, created_at FROM messages
                 WHERE session_id = ?1 ORDER BY seq",
            )?;
            let mut rows = stmt.query([&id])?;
            let mut out = vec![];
            while let Some(row) = rows.next()? {
                let seq: i64 = row.get(0)?;
                let role = match row.get_ref(2)?.as_str()? {
                    "user" => Role::User,
                    _ => Role::Assistant,
                };
                let content = serde_json::from_str(row.get_ref(3)?.as_str()?)
                    .with_context(|| format!("session {id}: message {seq}"))?;
                out.push(StoredMessage {
                    seq,
                    epoch: row.get(1)?,
                    message: Message { role, content },
                    created_at: row.get(4)?,
                });
            }
            Ok(out)
        })
        .await
    }

    pub(crate) async fn load(&self, id: &SessionId) -> Result<Option<Loaded>> {
        let id = id.clone();
        self.call(move |c| {
            let tx = c.transaction()?;
            let sql = format!("SELECT {COLUMNS} FROM sessions WHERE id = ?1");
            let Some(info) = tx.query_row(&sql, [&id], read_info).optional()? else {
                return Ok(None);
            };
            let info = info?;
            let mut stmt = tx.prepare(
                "SELECT seq, role, content FROM messages
                 WHERE session_id = ?1 AND epoch = ?2 ORDER BY seq",
            )?;
            let mut base = None;
            let mut history = vec![];
            let mut rows = stmt.query(params![id, info.epoch])?;
            while let Some(row) = rows.next()? {
                let seq: i64 = row.get(0)?;
                if let Some(base) = base
                    && seq != base + history.len() as i64
                {
                    bail!("session {id}: messages are not contiguous at seq {seq}");
                }
                base.get_or_insert(seq);
                let role = match row.get_ref(1)?.as_str()? {
                    "user" => Role::User,
                    _ => Role::Assistant,
                };
                let content: Vec<Content> = serde_json::from_str(row.get_ref(2)?.as_str()?)
                    .with_context(|| format!("session {id}: message {seq}"))?;
                history.push(Message { role, content });
            }
            drop(rows);
            drop(stmt);
            let base = match base {
                Some(b) => b,
                None => tx.query_row(
                    "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE session_id = ?1",
                    [&id],
                    |r| r.get(0),
                )?,
            };
            Ok(Some(Loaded {
                info,
                base,
                history,
            }))
        })
        .await
    }

    /// Creates or updates the session row.
    pub(crate) async fn save_state(&self, s: State) -> Result<()> {
        self.call(move |c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO sessions (id, cwd, additional_dirs, title, model, thinking,
                     input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                     last_input_tokens, last_output_tokens, last_cache_read_tokens,
                     last_cache_write_tokens, cost, profile, features, extension_state,
                     created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?17, ?18, ?19, ?16, ?16)
                 ON CONFLICT (id) DO UPDATE SET
                     additional_dirs = excluded.additional_dirs,
                     title = COALESCE(excluded.title, title),
                     model = excluded.model,
                     thinking = excluded.thinking,
                     input_tokens = excluded.input_tokens,
                     output_tokens = excluded.output_tokens,
                     cache_read_tokens = excluded.cache_read_tokens,
                     cache_write_tokens = excluded.cache_write_tokens,
                     last_input_tokens = excluded.last_input_tokens,
                     last_output_tokens = excluded.last_output_tokens,
                     last_cache_read_tokens = excluded.last_cache_read_tokens,
                     last_cache_write_tokens = excluded.last_cache_write_tokens,
                     cost = excluded.cost,
                     profile = excluded.profile,
                     features = excluded.features,
                     extension_state = excluded.extension_state,
                     updated_at = excluded.updated_at",
                params![
                    s.id,
                    path_str(&s.cwd)?,
                    serde_json::to_string(&s.additional_dirs)?,
                    s.title,
                    s.model,
                    s.thinking.as_str(),
                    int(s.usage.input)?,
                    int(s.usage.output)?,
                    int(s.usage.cache_read)?,
                    int(s.usage.cache_write)?,
                    int(s.last_usage.input)?,
                    int(s.last_usage.output)?,
                    int(s.last_usage.cache_read)?,
                    int(s.last_usage.cache_write)?,
                    s.cost,
                    now,
                    s.profile,
                    serde_json::to_string(&s.features)?,
                    serde_json::to_string(&s.extension_state)?,
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Replaces the session's messages from `seq` on with `messages`.
    pub(crate) async fn save_messages(
        &self,
        id: &SessionId,
        epoch: i64,
        seq: i64,
        messages: Vec<Message>,
    ) -> Result<()> {
        let id = id.clone();
        self.call(move |c| {
            let now = now_ms();
            let tx = c.transaction()?;
            tx.execute(
                "DELETE FROM messages WHERE session_id = ?1 AND seq >= ?2",
                params![id, seq],
            )?;
            let mut insert = tx.prepare(
                "INSERT INTO messages (session_id, seq, epoch, role, content, text, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (i, m) in messages.iter().enumerate() {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                insert.execute(params![
                    id,
                    seq + i as i64,
                    epoch,
                    role,
                    serde_json::to_string(&m.content)?,
                    plain_text(&m.content),
                    now,
                ])?;
            }
            drop(insert);
            tx.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![id, now],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Starts a new epoch (after compaction). Returns it and the `seq` its
    /// first message gets.
    pub(crate) async fn next_epoch(&self, id: &SessionId) -> Result<(i64, i64)> {
        let id = id.clone();
        self.call(move |c| {
            let tx = c.transaction()?;
            let epoch = tx.query_row(
                "UPDATE sessions SET epoch = epoch + 1 WHERE id = ?1 RETURNING epoch",
                [&id],
                |r| r.get(0),
            )?;
            let base = tx.query_row(
                "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE session_id = ?1",
                [&id],
                |r| r.get(0),
            )?;
            tx.commit()?;
            Ok((epoch, base))
        })
        .await
    }

    /// Saves the diffs of Edit calls, by tool call, and deletes diffs older
    /// than `DIFF_DAYS`.
    pub(crate) async fn save_diffs(
        &self,
        id: &SessionId,
        diffs: Vec<(ToolCallId, Diff)>,
    ) -> Result<()> {
        let id = id.clone();
        self.call(move |c| {
            let now = now_ms();
            let tx = c.transaction()?;
            tx.execute(
                "DELETE FROM diffs WHERE created_at < ?1",
                [now - DIFF_DAYS * 24 * 3600 * 1000],
            )?;
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO diffs (session_id, tool_use_id, path, old, new, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (call, d) in &diffs {
                insert.execute(params![id, call, path_str(&d.path)?, d.old, d.new, now])?;
            }
            drop(insert);
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// The session's saved Edit diffs, by tool call.
    pub async fn diffs(&self, id: &SessionId) -> Result<HashMap<ToolCallId, Diff>> {
        let id = id.clone();
        self.call(move |c| {
            let mut stmt =
                c.prepare("SELECT tool_use_id, path, old, new FROM diffs WHERE session_id = ?1")?;
            let rows = stmt.query_map([&id], |r| {
                let path: String = r.get(1)?;
                let diff = Diff {
                    path: path.into(),
                    old: r.get(2)?,
                    new: r.get(3)?,
                };
                Ok((r.get(0)?, diff))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// Replaces `phrase` with `MASK` in all messages, diffs and titles of all
    /// sessions, see `redact_content`. Deleted bytes are zeroed and the WAL
    /// is checkpointed, so no copy stays in the files. Returns the number of
    /// changed messages and sessions.
    pub(crate) async fn redact(&self, phrase: &str) -> Result<(usize, usize)> {
        let phrase = phrase.to_string();
        // The content column is serde_json output, so it holds the phrase escaped the same way.
        let escaped = serde_json::to_string(&phrase)?;
        let escaped = escaped[1..escaped.len() - 1].to_string();
        self.call(move |c| {
            c.pragma_update(None, "secure_delete", true)?;
            let tx = c.transaction()?;
            let mut rows = vec![];
            {
                let mut stmt = tx.prepare(
                    "SELECT session_id, seq, content FROM messages WHERE instr(content, ?1) > 0",
                )?;
                let mut q = stmt.query([&escaped])?;
                while let Some(r) = q.next()? {
                    let (id, seq): (SessionId, i64) = (r.get(0)?, r.get(1)?);
                    let mut content: Vec<Content> = serde_json::from_str(r.get_ref(2)?.as_str()?)
                        .with_context(|| format!("session {id}: message {seq}"))?;
                    if redact_content(&mut content, &phrase) {
                        rows.push((id, seq, content));
                    }
                }
            }
            let mut sessions = std::collections::HashSet::new();
            for (id, seq, content) in &rows {
                tx.execute(
                    "UPDATE messages SET content = ?3, text = ?4 WHERE session_id = ?1 AND seq = ?2",
                    params![id, seq, serde_json::to_string(content)?, plain_text(content)],
                )?;
                sessions.insert(id.clone());
            }
            let mut stmt = tx.prepare(
                "UPDATE sessions SET title = replace(title, ?1, ?2) WHERE instr(title, ?1) > 0
                 RETURNING id",
            )?;
            for id in stmt.query_map([phrase.as_str(), MASK], |r| r.get(0))? {
                sessions.insert(id?);
            }
            drop(stmt);
            let mut stmt = tx.prepare(
                "UPDATE diffs SET path = replace(path, ?1, ?2), old = replace(old, ?1, ?2),
                     new = replace(new, ?1, ?2)
                 WHERE instr(path, ?1) > 0 OR instr(old, ?1) > 0 OR instr(new, ?1) > 0
                 RETURNING session_id",
            )?;
            for id in stmt.query_map([phrase.as_str(), MASK], |r| r.get(0))? {
                sessions.insert(id?);
            }
            drop(stmt);
            // A new `updated_at` makes the History index rebuild the session.
            let now = now_ms();
            for id in &sessions {
                tx.execute(
                    "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
            }
            tx.commit()?;
            c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            Ok((rows.len(), sessions.len()))
        })
        .await
    }

    /// Sets the title of a stored session; no-op if it is not stored yet.
    pub(crate) async fn set_title(&self, id: &SessionId, title: &str) -> Result<()> {
        let (id, title) = (id.clone(), title.to_string());
        self.call(move |c| {
            c.execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1",
                params![id, title],
            )?;
            Ok(())
        })
        .await
    }
}

/// Stored as "provider/model".
impl FromSql for ModelSpec {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|e: anyhow::Error| FromSqlError::Other(e.into()))
    }
}

impl ToSql for ModelSpec {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.to_string().into())
    }
}

fn read_info(r: &Row) -> rusqlite::Result<Result<SessionInfo>> {
    let thinking: String = r.get("thinking")?;
    let dirs: String = r.get("additional_dirs")?;
    let features: String = r.get("features")?;
    let extension_state: String = r.get("extension_state")?;
    let usage = |col| r.get::<_, i64>(col).map(|n| n as u64);
    let info = SessionInfo {
        id: r.get("id")?,
        cwd: PathBuf::from(r.get::<_, String>("cwd")?),
        additional_dirs: vec![],
        title: r.get("title")?,
        model: r.get("model")?,
        thinking: Thinking::Off,
        usage: Usage {
            input: usage("input_tokens")?,
            output: usage("output_tokens")?,
            cache_read: usage("cache_read_tokens")?,
            cache_write: usage("cache_write_tokens")?,
        },
        last_usage: Usage {
            input: usage("last_input_tokens")?,
            output: usage("last_output_tokens")?,
            cache_read: usage("last_cache_read_tokens")?,
            cache_write: usage("last_cache_write_tokens")?,
        },
        cost: r.get("cost")?,
        epoch: r.get("epoch")?,
        profile: r.get("profile")?,
        features: vec![],
        extension_state: Default::default(),
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    };
    Ok((|| {
        Ok(SessionInfo {
            additional_dirs: serde_json::from_str(&dirs)?,
            features: serde_json::from_str(&features)?,
            extension_state: serde_json::from_str(&extension_state)?,
            thinking: thinking.parse()?,
            ..info
        })
    })())
}

/// Replaces `phrase` with `MASK` in text, tool calls and tool results.
/// Thinking blocks containing it are dropped: changed text would break their
/// signature. Returns whether anything changed.
pub(crate) fn redact_content(content: &mut Vec<Content>, phrase: &str) -> bool {
    fn replace(s: &mut String, phrase: &str) -> bool {
        let hit = s.contains(phrase);
        if hit {
            *s = s.replace(phrase, MASK);
        }
        hit
    }
    fn walk(v: &mut serde_json::Value, phrase: &str) -> bool {
        match v {
            serde_json::Value::String(s) => replace(s, phrase),
            serde_json::Value::Array(a) => a.iter_mut().fold(false, |h, v| walk(v, phrase) | h),
            serde_json::Value::Object(o) => o.values_mut().fold(false, |h, v| walk(v, phrase) | h),
            _ => false,
        }
    }
    let before = content.len();
    content
        .retain(|c| !matches!(c, Content::Thinking { thinking, .. } if thinking.contains(phrase)));
    let mut changed = content.len() != before;
    // The API rejects empty messages, and dropping one would break role alternation.
    if content.is_empty() && changed {
        content.push(Content::text(MASK));
    }
    for c in content.iter_mut() {
        changed |= match c {
            Content::Text { text } | Content::SystemNote { text } => replace(text, phrase),
            Content::UserNote { title, text, .. } => replace(title, phrase) | replace(text, phrase),
            Content::ToolUse { input, .. } => walk(input, phrase),
            Content::ToolResult { content, .. } => replace(content, phrase),
            _ => false,
        };
    }
    changed
}

/// Text and tool calls of a message, for search. Thinking and tool output
/// are left out.
fn plain_text(content: &[Content]) -> String {
    let parts: Vec<String> = content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            Content::Image { .. } => Some("[image]".into()),
            Content::ToolUse { name, input, .. } => Some(format!("{name} {input}")),
            _ => None,
        })
        .collect();
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ToolCallId, ToolName};

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![Content::text(text)],
        }
    }

    fn state(id: &str, cwd: &str) -> State {
        State {
            id: SessionId::new(id),
            cwd: cwd.into(),
            additional_dirs: vec!["/extra".into()],
            title: None,
            model: ModelSpec::new("p", "m"),
            thinking: Thinking::Low,
            usage: Usage {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4,
            },
            last_usage: Usage {
                input: 5,
                output: 6,
                cache_read: 7,
                cache_write: 8,
            },
            cost: 0.5,
            profile: ProfileId::new("plan"),
            features: vec![FeatureId::new("ssh")],
            extension_state: serde_json::json!({"go-check.off": true})
                .as_object()
                .unwrap()
                .clone(),
        }
    }

    #[tokio::test]
    async fn round_trip_with_rewrites_and_epochs() {
        let store = Store::memory().unwrap();
        store.save_state(state("s", "/w")).await.unwrap();
        let history = vec![msg(Role::User, "a"), msg(Role::Assistant, "b")];
        store
            .save_messages(&SessionId::new("s"), 0, 0, history.clone())
            .await
            .unwrap();
        // Rewrite the last message and add one.
        let more = vec![msg(Role::Assistant, "b2"), msg(Role::User, "c")];
        store
            .save_messages(&SessionId::new("s"), 0, 1, more)
            .await
            .unwrap();
        let loaded = store.load(&SessionId::new("s")).await.unwrap().unwrap();
        assert_eq!(loaded.base, 0);
        assert_eq!(loaded.history[1], msg(Role::Assistant, "b2"));
        assert_eq!(loaded.history.len(), 3);
        assert_eq!(loaded.info.thinking, Thinking::Low);
        assert_eq!(loaded.info.additional_dirs, [PathBuf::from("/extra")]);
        assert_eq!(loaded.info.usage.cache_write, 4);
        assert_eq!(loaded.info.last_usage.cache_write, 8);
        assert_eq!(loaded.info.profile, ProfileId::new("plan"));
        assert_eq!(loaded.info.features, [FeatureId::new("ssh")]);
        assert_eq!(
            loaded.info.extension_state["go-check.off"],
            serde_json::json!(true)
        );

        assert_eq!(
            store.next_epoch(&SessionId::new("s")).await.unwrap(),
            (1, 3)
        );
        store
            .save_messages(&SessionId::new("s"), 1, 3, vec![msg(Role::User, "summary")])
            .await
            .unwrap();
        let loaded = store.load(&SessionId::new("s")).await.unwrap().unwrap();
        assert_eq!((loaded.base, loaded.info.epoch), (3, 1));
        assert_eq!(loaded.history, [msg(Role::User, "summary")]);
        let all = store.messages(&SessionId::new("s")).await.unwrap();
        let seqs: Vec<_> = all.iter().map(|m| (m.seq, m.epoch)).collect();
        assert_eq!(seqs, [(0, 0), (1, 0), (2, 0), (3, 1)]);
        assert_eq!(all[1].message, msg(Role::Assistant, "b2"));

        store
            .set_title(&SessionId::new("s"), "Title")
            .await
            .unwrap();
        // A later save without a title keeps it.
        store.save_state(state("s", "/w")).await.unwrap();
        let info = store.session(&SessionId::new("s")).await.unwrap().unwrap();
        assert_eq!(info.title.as_deref(), Some("Title"));
        assert!(
            store
                .session(&SessionId::new("nope"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn redact_replaces_the_phrase_everywhere() {
        let store = Store::memory().unwrap();
        let id = SessionId::new("s");
        store.save_state(state("s", "/w")).await.unwrap();
        store.set_title(&id, "about s3cr\"et").await.unwrap();
        let assistant = Message {
            role: Role::Assistant,
            content: vec![
                Content::Thinking {
                    thinking: "s3cr\"et".into(),
                    signature: "sig".into(),
                },
                Content::ToolUse {
                    id: ToolCallId::new("t"),
                    name: ToolName::new("Bash"),
                    input: serde_json::json!({"command": ["echo s3cr\"et"]}),
                },
            ],
        };
        let result = Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: ToolCallId::new("t"),
                content: "s3cr\"et".into(),
                is_error: false,
                images: vec![],
            }],
        };
        let history = vec![msg(Role::User, "x s3cr\"et y"), assistant, result];
        store.save_messages(&id, 0, 0, history).await.unwrap();
        store.next_epoch(&id).await.unwrap();
        store
            .save_messages(
                &id,
                1,
                3,
                vec![msg(Role::User, "s3cr\"et"), msg(Role::Assistant, "ok")],
            )
            .await
            .unwrap();

        assert_eq!(store.redact("s3cr\"et").await.unwrap(), (4, 1));
        let all = store.messages(&id).await.unwrap();
        let json =
            serde_json::to_string(&all.iter().map(|m| &m.message).collect::<Vec<_>>()).unwrap();
        assert!(!json.contains("s3cr"), "{json}");
        assert!(!json.contains("thinking"), "{json}");
        assert_eq!(all[0].message, msg(Role::User, &format!("x {MASK} y")));
        let info = store.session(&id).await.unwrap().unwrap();
        assert_eq!(info.title, Some(format!("about {MASK}")));
        // Still loads: seq is contiguous.
        assert_eq!(store.history(&id).await.unwrap().len(), 2);
        assert_eq!(store.redact("s3cr\"et").await.unwrap(), (0, 0));
    }

    #[tokio::test]
    async fn diffs_are_kept_for_a_while_and_redacted() {
        let store = Store::memory().unwrap();
        let id = SessionId::new("s");
        store.save_state(state("s", "/w")).await.unwrap();
        let diff = |old: &str| Diff {
            path: "/w/f".into(),
            old: old.into(),
            new: "new".into(),
        };
        let save =
            |call: &str, old: &str| store.save_diffs(&id, vec![(ToolCallId::new(call), diff(old))]);
        save("old", "x").await.unwrap();
        store
            .call(|c| Ok(c.execute("UPDATE diffs SET created_at = 0", [])?))
            .await
            .unwrap();
        save("t", "a s3cret").await.unwrap();
        // Rewriting messages leaves diffs alone.
        store
            .save_messages(&id, 0, 0, vec![msg(Role::User, "a")])
            .await
            .unwrap();
        let diffs = store.diffs(&id).await.unwrap();
        assert_eq!(diffs.len(), 1, "the old one is deleted");
        assert_eq!(diffs[&ToolCallId::new("t")], diff("a s3cret"));

        assert_eq!(store.redact("s3cret").await.unwrap(), (0, 1));
        let diffs = store.diffs(&id).await.unwrap();
        assert_eq!(diffs[&ToolCallId::new("t")], diff(&format!("a {MASK}")));
    }

    #[tokio::test]
    async fn constraints_are_enforced() {
        let store = Store::memory().unwrap();
        // No session row: foreign key.
        let err = store
            .save_messages(&SessionId::new("ghost"), 0, 0, vec![msg(Role::User, "x")])
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("FOREIGN KEY"), "{err:#}");
        // STRICT: wrong types are rejected, not coerced.
        let conn = store.conn.lock().unwrap();
        let err = conn
            .execute(
                "INSERT INTO sessions (id, cwd, model, thinking, created_at, updated_at)
                 VALUES ('x', '/w', 'p/m', 'high', 'yesterday', 0)",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("cannot store TEXT"), "{err}");
        let err = conn
            .execute(
                "INSERT INTO sessions (id, cwd, model, thinking, created_at, updated_at)
                 VALUES ('x', '/w', 'p/m', 'loud', 0, 0)",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
    }

    #[tokio::test]
    async fn list_pages_by_cwd() {
        let store = Store::memory().unwrap();
        for (id, cwd) in [("a", "/w"), ("b", "/other"), ("c", "/w"), ("d", "/w")] {
            store.save_state(state(id, cwd)).await.unwrap();
            let hi = vec![msg(Role::User, "hi")];
            store
                .save_messages(&SessionId::new(id), 0, 0, hi)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Newest, but empty: not listed.
        store.save_state(state("e", "/w")).await.unwrap();
        let ids = |p: &Page| {
            p.sessions
                .iter()
                .map(|s| s.id.to_string())
                .collect::<Vec<_>>()
        };
        let page = store.list(Some(Path::new("/w")), None, 2).await.unwrap();
        assert_eq!(ids(&page), ["d", "c"]);
        let page = store
            .list(Some(Path::new("/w")), page.next.as_deref(), 2)
            .await
            .unwrap();
        assert_eq!(ids(&page), ["a"]);
        assert!(page.next.is_none());
        assert_eq!(store.list(None, None, 10).await.unwrap().sessions.len(), 4);
    }

    #[test]
    fn session_ids_are_uuid_v7() {
        let (a, b) = (new_session_id(), new_session_id());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "7");
    }

    #[test]
    fn reopening_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/sessions.db");
        let store = Store::open(&path).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(store.save_state(state("s", "/w"))).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(
            rt.block_on(store.session(&SessionId::new("s")))
                .unwrap()
                .is_some()
        );
    }
}
