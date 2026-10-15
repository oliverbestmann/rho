//! `History`: searches the earlier conversations of a project (ADR 0032).
//! The project is the nearest directory above the cwd with a
//! `.rho-project-root` file; sessions whose cwd is in it are searched, older
//! epochs (compacted away) included. Without the file there is no tool.
//!
//! The index is `history.db` in `$XDG_CACHE_HOME/rho`, built from the
//! session store: each epoch's text is cut into overlapping windows,
//! embedded like `Memory` and put into FTS5. Search merges the semantic and
//! keyword rankings (reciprocal rank fusion). Changed sessions are indexed
//! after every run and before every search.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail};
use rho::extension::{
    CommandName, Context, Extension, RunEnd, SessionSetup, SlashCommand, ToolOutput,
};
use rho::provider::{Content, Message, Role, ToolName, ToolSpec};
use rho::store::{SessionId, SessionInfo, Store, StoredMessage};
use rho::tools::{Tool, input_schema, parse, truncate};
use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::index;

pub const MARKER: &str = ".rho-project-root";
/// `/history <query>`: the user searches; the results are not shown to the model.
const HISTORY: CommandName = CommandName::from_static("history");
/// Window size and overlap in bytes; ~200 tokens suits MiniLM.
const CHUNK: usize = 800;
const OVERLAP: usize = 150;
/// Tool call inputs and outputs are cut to this many bytes.
const MAX_CALL: usize = 300;
/// Candidates taken from each ranking before fusing.
const CANDIDATES: u32 = 50;
const RRF_K: f64 = 60.0;
/// Messages shown by `read`.
const READ: i64 = 10;
const ANSWER_SYSTEM: &str = "You get search results from earlier conversations between a user and a coding agent, and a query. Answer the query from the results only, briefly. Keep exact file paths, names, commands and error messages. Cite each point as (session <id>, seq <first>-<last>). If the results don't answer it, say so.";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT NOT NULL PRIMARY KEY,
    cwd TEXT NOT NULL,
    title TEXT,
    -- From the session store, to find changed sessions.
    updated_at INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    first_seq INTEGER NOT NULL,
    last_seq INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    text TEXT NOT NULL,
    -- SHA-256 of what was embedded (title and text), to reuse embeddings.
    hash TEXT NOT NULL,
    -- f32 little-endian, as sqlite-vec reads it.
    embedding BLOB NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS chunks_by_session ON chunks (session_id);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, content='chunks', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS chunks_insert AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts (rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_delete AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts (chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
"#;

type Embed = Box<dyn Fn(&[String]) -> Result<Vec<Vec<f32>>> + Send + Sync>;

/// One instance serves all sessions; the index is opened on first use.
pub struct History(Arc<Inner>);

struct Inner {
    path: PathBuf,
    conn: Mutex<Option<Connection>>,
    embed: Embed,
    /// One indexing pass at a time in this process.
    syncing: tokio::sync::Mutex<()>,
}

impl Default for History {
    fn default() -> Self {
        History::new(
            index::cache_dir().join("history.db"),
            Box::new(index::embed),
        )
    }
}

impl History {
    fn new(path: PathBuf, embed: Embed) -> History {
        History(Arc::new(Inner {
            path,
            conn: Mutex::new(None),
            embed,
            syncing: Default::default(),
        }))
    }
}

/// The nearest directory at or above `cwd` with the marker file.
fn project_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|d| d.join(MARKER).exists())
        .map(Path::to_path_buf)
}

impl Extension for History {
    async fn session_start(&self, session: &mut SessionSetup) {
        if session.ctx.store().is_none() {
            return;
        }
        if let Some(root) = project_root(session.ctx.cwd()) {
            session.commands.push(SlashCommand {
                name: HISTORY,
                description: "Search earlier conversations of the project (not shown to the model)"
                    .into(),
                hint: Some("query".into()),
            });
            session.tools.push(Arc::new(HistoryTool {
                inner: self.0.clone(),
                root,
            }));
        }
    }

    /// `/redact` (run by the core for every extension, after the session
    /// store): drops the index of every session with the phrase; the next
    /// sync rebuilds it from the redacted store.
    async fn command(&self, ctx: &Context, name: &CommandName, phrase: &str) -> Option<String> {
        if *name == HISTORY {
            let root = project_root(ctx.cwd())?;
            if phrase.trim().is_empty() {
                return Some("Usage: /history <query>".into());
            }
            let found = self.0.find(ctx, &root, phrase, Mode::Hybrid, 10).await;
            return Some(found.map_or_else(|e| format!("{e:#}"), |(_, results)| results));
        }
        if name.as_str() != "redact" {
            return None;
        }
        let (inner, phrase) = (self.0.clone(), phrase.to_string());
        let n = tokio::task::spawn_blocking(move || inner.redact(&phrase)).await;
        Some(match n.map_err(anyhow::Error::from).and_then(|n| n) {
            Ok(n) => format!("History index: {n} sessions dropped, re-indexed on the next search"),
            Err(e) => format!("History index: failed: {e:#}"),
        })
    }

    async fn run_end(&self, ctx: &Context, _run: &RunEnd) {
        let (Some(root), Some(store)) = (project_root(ctx.cwd()), ctx.store().cloned()) else {
            return;
        };
        let (inner, ctx) = (self.0.clone(), ctx.clone());
        tokio::spawn(async move {
            if let Err(e) = inner.sync(&store, &root).await {
                ctx.notify("History not indexed", &format!("{e:#}"), true);
            }
        });
    }
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Search,
    Read,
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
enum Mode {
    #[default]
    Hybrid,
    Semantic,
    Keyword,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Input {
    action: Action,
    /// What to look for (search)
    query: Option<String>,
    /// hybrid (default): by meaning and by keywords; semantic; keyword
    mode: Option<Mode>,
    /// Result limit (search, default: 10)
    #[schemars(range(min = 1, max = 50))]
    limit: Option<u32>,
    /// Session id from a search result (read)
    session: Option<SessionId>,
    /// First message to show, from a search result (read)
    seq: Option<i64>,
    /// With more results than this, a cheap model answers the query from them
    /// (citing session and seq) instead of showing them all (search)
    summarize_over: Option<u32>,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

struct HistoryTool {
    inner: Arc<Inner>,
    root: PathBuf,
}

impl Tool for HistoryTool {
    fn kind(&self) -> rho::tools::ToolKind {
        rho::tools::ToolKind::Search
    }

    fn titled(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_static("History"),
            description: format!(
                "Search earlier conversations of the project in {root}: all sessions whose working directory is {root} or below it, \
                 including parts that were compacted away. Use it to find what was decided, tried, or fixed before. \
                 Search matches by meaning and by keywords; overlapping hits are merged. `summarizeOver` gets a short answer with citations instead of many results. `read` shows the {READ} messages from a result's session and message number on. \
                 Output is cut at {kb}KB.",
                root = self.root.display(),
                kb = rho::tools::MAX_OUTPUT / 1024
            ),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        let store = ctx.store().context("sessions are not saved")?;
        let out = match i.action {
            Action::Search => {
                let query = i
                    .query
                    .filter(|q| !q.trim().is_empty())
                    .context("query is required for search")?;
                let limit = i.limit.unwrap_or(10).clamp(1, 50) as usize;
                let mode = i.mode.unwrap_or_default();
                let (hits, results) = self
                    .inner
                    .find(ctx, &self.root, &query, mode, limit)
                    .await?;
                let info = ctx.session_info();
                match (i.summarize_over, info.cheap_model()) {
                    (Some(over), Some(model)) if hits.len() > over as usize => {
                        let prompt = format!("<results>\n{results}\n</results>\n\nQuery: {query}");
                        match ctx.complete(model, ANSWER_SYSTEM, &prompt).await {
                            Ok(answer) => format!(
                                "Answer from {model} over {} results for {query:?}:\n\n{}",
                                hits.len(),
                                answer.trim()
                            ),
                            Err(e) => format!("Could not summarize ({e:#}).\n\n{results}"),
                        }
                    }
                    _ => results,
                }
            }
            Action::Read => {
                let id = i.session.context("session is required for read")?;
                let seq = i.seq.context("seq is required for read")?;
                let info = store.session(&id).await?;
                let Some(info) = info.filter(|s| s.cwd.starts_with(&self.root)) else {
                    bail!("no session {id} in {}", self.root.display());
                };
                let messages = store.messages(&id).await?;
                let shown: Vec<String> = messages
                    .iter()
                    .filter(|m| (seq..seq + READ).contains(&m.seq))
                    .map(|m| format!("[{}] {}", m.seq, render(&m.message, true)))
                    .collect();
                let more = messages.last().is_some_and(|m| m.seq >= seq + READ);
                let mut out = format!(
                    "{} · {} · session {id}\n\n{}",
                    info.title.as_deref().unwrap_or("(untitled)"),
                    info.cwd.display(),
                    shown.join("\n\n")
                );
                if more {
                    out += &format!("\n\n(more from seq {})", seq + READ);
                }
                out
            }
        };
        Ok(truncate(out).into())
    }
}

impl Inner {
    /// Syncs the index and searches it, without the session's current
    /// epoch (it is in the context already). Returns the hits and them as text.
    async fn find(
        self: &Arc<Self>,
        ctx: &Context,
        root: &Path,
        query: &str,
        mode: Mode,
        limit: usize,
    ) -> Result<(Vec<Hit>, String)> {
        let store = ctx.store().context("sessions are not saved")?;
        self.sync(store, root).await?;
        let epoch = store
            .session(ctx.session_id())
            .await?
            .map_or(-1, |s| s.epoch);
        let current = (ctx.session_id().to_string(), epoch);
        let (inner, r, q) = (self.clone(), root.to_path_buf(), query.to_string());
        let hits = tokio::task::spawn_blocking(move || inner.search(&r, &current, &q, mode, limit))
            .await??;
        let found = format!(
            "Found {} results for {query:?} in {}.",
            hits.len(),
            root.display()
        );
        let mut parts = vec![found];
        parts.extend(hits.iter().enumerate().map(|(n, h)| h.show(n + 1)));
        Ok((hits, parts.join("\n\n")))
    }

    fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().unwrap();
        if conn.is_none() {
            *conn = Some(index::open(&self.path, SCHEMA)?);
        }
        f(conn.as_mut().unwrap())
    }

    /// Deletes the chunks and index rows of sessions whose text or title
    /// contain `phrase`. Returns their number.
    fn redact(&self, phrase: &str) -> Result<usize> {
        self.with_conn(|c| {
            index::purge(c, |tx| {
                let ids: Vec<String> = tx
                    .prepare(
                        "SELECT session_id FROM chunks WHERE instr(text, ?1) > 0
                         UNION SELECT id FROM sessions WHERE instr(title, ?1) > 0",
                    )?
                    .query_map([phrase], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                for id in &ids {
                    tx.execute("DELETE FROM chunks WHERE session_id = ?1", [id])?;
                    tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
                }
                // Merges the FTS segments, so deleted terms are gone too.
                tx.execute(
                    "INSERT INTO chunks_fts (chunks_fts) VALUES ('optimize')",
                    [],
                )?;
                Ok(ids.len())
            })
        })
    }

    /// Indexes the sessions under `root` that changed since the last pass.
    async fn sync(self: &Arc<Self>, store: &Store, root: &Path) -> Result<()> {
        let _one = self.syncing.lock().await;
        let mut sessions = vec![];
        let mut cursor = None;
        loop {
            let page = store.list(None, cursor.as_deref(), 500).await?;
            sessions.extend(
                page.sessions
                    .into_iter()
                    .filter(|s| s.cwd.starts_with(root)),
            );
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let inner = self.clone();
        let indexed: HashMap<SessionId, (i64, Option<String>)> =
            tokio::task::spawn_blocking(move || {
                inner.with_conn(|c| {
                    let mut stmt = c.prepare("SELECT id, updated_at, title FROM sessions")?;
                    let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
                    Ok(rows.collect::<rusqlite::Result<_>>()?)
                })
            })
            .await??;
        for s in sessions {
            // `set_title` does not touch `updated_at`.
            if indexed.get(&s.id) == Some(&(s.updated_at, s.title.clone())) {
                continue;
            }
            let messages = store.messages(&s.id).await?;
            let inner = self.clone();
            tokio::task::spawn_blocking(move || inner.index(&s, &messages)).await??;
        }
        Ok(())
    }

    /// Replaces the session's chunks; unchanged ones keep their embedding.
    fn index(&self, s: &SessionInfo, messages: &[StoredMessage]) -> Result<()> {
        let chunks = chunks(messages);
        let embedded: Vec<String> = chunks
            .iter()
            .map(|c| match &s.title {
                Some(t) => format!("{t}\n\n{}", c.text),
                None => c.text.clone(),
            })
            .collect();
        let hashes: Vec<ChunkHash> = embedded
            .iter()
            .map(|t| ChunkHash::new(hex(&Sha256::digest(t))))
            .collect();
        let mut known: HashMap<ChunkHash, Vec<u8>> = self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT hash, embedding FROM chunks WHERE session_id = ?1")?;
            let rows = stmt.query_map([&s.id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })?;
        let missing: Vec<usize> = (0..chunks.len())
            .filter(|&n| !known.contains_key(&hashes[n]))
            .collect();
        if !missing.is_empty() {
            let texts: Vec<String> = missing.iter().map(|&n| embedded[n].clone()).collect();
            for (n, v) in missing.iter().zip((self.embed)(&texts)?) {
                known.insert(hashes[*n].clone(), index::blob(&v));
            }
        }
        self.with_conn(|c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM chunks WHERE session_id = ?1", [&s.id])?;
            let mut insert = tx.prepare(
                "INSERT INTO chunks (session_id, epoch, first_seq, last_seq, created_at, text,
                     hash, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (chunk, hash) in chunks.iter().zip(&hashes) {
                insert.execute(params![
                    s.id,
                    chunk.epoch,
                    chunk.first_seq,
                    chunk.last_seq,
                    chunk.created_at,
                    chunk.text,
                    hash,
                    known[hash],
                ])?;
            }
            drop(insert);
            tx.execute(
                "INSERT OR REPLACE INTO sessions (id, cwd, title, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    s.id,
                    s.cwd.to_str().context("cwd is not UTF-8")?,
                    s.title,
                    s.updated_at
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Chunks of sessions under `root` best matching `query`, without the
    /// `current` (session, epoch). Hits on consecutive chunks are merged
    /// into one, ranked by its best chunk; then the best `limit` are kept.
    fn search(
        &self,
        root: &Path,
        current: &(String, i64),
        query: &str,
        mode: Mode,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        let root_str = root.to_str().context("project root is not UTF-8")?;
        let prefix = format!("{}/", root_str.trim_end_matches('/'));
        let words = keywords(query);
        if mode == Mode::Keyword && words.is_none() {
            bail!("no keywords in {query:?}");
        }
        let embedding = match mode {
            Mode::Keyword => None,
            _ => Some(index::blob(&(self.embed)(&[query.to_string()])?.remove(0))),
        };
        self.with_conn(|c| {
            let scope = "(s.cwd = ?1 OR substr(s.cwd, 1, length(?2)) = ?2)
                AND NOT (c.session_id = ?3 AND c.epoch = ?4)";
            let ranked = |sql: &str, query: &dyn rusqlite::ToSql| -> Result<Vec<i64>> {
                let mut stmt = c.prepare(sql)?;
                let rows = stmt.query_map(
                    params![root_str, prefix, current.0, current.1, query, CANDIDATES],
                    |r| r.get(0),
                )?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            };
            let mut lists = vec![];
            if let Some(e) = &embedding {
                lists.push(ranked(
                    &format!(
                        "SELECT c.id FROM chunks c JOIN sessions s ON s.id = c.session_id
                         WHERE {scope} ORDER BY vec_distance_cosine(c.embedding, ?5) LIMIT ?6"
                    ),
                    e,
                )?);
            }
            if let Some(w) = words.as_ref().filter(|_| mode != Mode::Semantic) {
                lists.push(ranked(
                    &format!(
                        "SELECT c.id FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid
                         JOIN sessions s ON s.id = c.session_id
                         WHERE chunks_fts MATCH ?5 AND {scope} ORDER BY chunks_fts.rank LIMIT ?6"
                    ),
                    w,
                )?);
            }
            let mut stmt = c.prepare(
                "SELECT c.session_id, c.epoch, c.first_seq, c.last_seq, c.text, s.title, s.cwd,
                     date(c.created_at / 1000, 'unixepoch')
                 FROM chunks c JOIN sessions s ON s.id = c.session_id WHERE c.id = ?1",
            )?;
            let mut found = vec![];
            for (id, score) in fuse(&lists) {
                let (mut hit, cwd) = stmt.query_row([id], |r| {
                    let hit = Hit {
                        ids: id..id + 1,
                        session: r.get(0)?,
                        epoch: r.get(1)?,
                        first_seq: r.get(2)?,
                        last_seq: r.get(3)?,
                        text: r.get(4)?,
                        title: r.get(5)?,
                        dir: String::new(),
                        date: r.get(7)?,
                        score,
                    };
                    Ok((hit, r.get::<_, String>(6)?))
                })?;
                let dir = Path::new(&cwd)
                    .strip_prefix(root)
                    .unwrap_or(Path::new(&cwd));
                hit.dir = match dir.as_os_str().is_empty() {
                    true => ".".into(),
                    false => dir.display().to_string(),
                };
                found.push(hit);
            }
            Ok(merge(found, limit))
        })
    }
}

rho::id_type! {
    /// SHA-256 of a chunk's embedded text; chunks with the same hash reuse the embedding.
    ChunkHash
}

/// A search result: one chunk or several consecutive ones.
#[derive(Debug)]
struct Hit {
    /// Chunk ids. A session's chunks get consecutive ids in window order.
    ids: Range<i64>,
    session: SessionId,
    epoch: i64,
    first_seq: i64,
    last_seq: i64,
    text: String,
    title: Option<String>,
    /// The session's cwd relative to the project root.
    dir: String,
    date: String,
    /// The best fused score of its chunks.
    score: f64,
}

impl Hit {
    fn show(&self, n: usize) -> String {
        format!(
            "{n}. {} · {} · {} · session {}, seq {}-{}\n{}",
            self.date,
            self.title.as_deref().unwrap_or("(untitled)"),
            self.dir,
            self.session,
            self.first_seq,
            self.last_seq,
            self.text
        )
    }
}

/// Joins hits on consecutive chunks of the same epoch (their texts overlap),
/// then keeps the best `limit` by score.
fn merge(mut hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    hits.sort_by_key(|h| h.ids.start);
    let mut out: Vec<Hit> = vec![];
    for h in hits {
        match out.last_mut() {
            Some(last)
                if last.ids.end == h.ids.start
                    && (&last.session, last.epoch) == (&h.session, h.epoch) =>
            {
                last.text.truncate(overlap_start(&last.text));
                last.text.push_str(&h.text);
                (last.ids.end, last.last_seq) = (h.ids.end, h.last_seq);
                last.score = last.score.max(h.score);
            }
            _ => out.push(h),
        }
    }
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.ids.start.cmp(&b.ids.start))
    });
    out.truncate(limit);
    out
}

/// Reciprocal rank fusion of ranked id lists: ids with scores, best first.
fn fuse(lists: &[Vec<i64>]) -> Vec<(i64, f64)> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut ids: Vec<(i64, f64)> = scores.into_iter().collect();
    ids.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    ids
}

/// The query's words as an FTS5 OR query; quoted, so no syntax errors.
fn keywords(query: &str) -> Option<String> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\""))
        .collect();
    (!words.is_empty()).then(|| words.join(" OR "))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// At most `max` bytes of `s`, marked if cut.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    format!("{}…", &s[..s.floor_char_boundary(max)])
}

/// "User: …" or "Assistant: …": text and cut tool calls, plus cut tool
/// outputs if `outputs`. Empty if there is nothing to show.
fn render(m: &Message, outputs: bool) -> String {
    let parts: Vec<String> = m
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            Content::ToolUse { name, input, .. } => {
                Some(format!("[{name} {}]", cut(&input.to_string(), MAX_CALL)))
            }
            Content::ToolResult { content, .. } if outputs => {
                Some(format!("[output: {}]", cut(content, MAX_CALL)))
            }
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    let who = match m.role {
        Role::User => "User",
        Role::Assistant => "Assistant",
    };
    format!("{who}: {}", parts.join("\n"))
}

struct Chunk {
    epoch: i64,
    first_seq: i64,
    last_seq: i64,
    /// Of the first message.
    created_at: i64,
    text: String,
}

/// Each epoch's messages as text, cut into overlapping windows.
fn chunks(messages: &[StoredMessage]) -> Vec<Chunk> {
    let mut out = vec![];
    for epoch in messages.chunk_by(|a, b| a.epoch == b.epoch) {
        let mut text = String::new();
        // Where each message starts in `text`.
        let mut starts: Vec<(usize, &StoredMessage)> = vec![];
        for m in epoch {
            let t = render(&m.message, false);
            if t.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            starts.push((text.len(), m));
            text.push_str(&t);
        }
        let at = |offset| starts[starts.partition_point(|s| s.0 <= offset) - 1].1;
        for w in windows(&text) {
            let (first, last) = (at(w.start), at(w.end - 1));
            out.push(Chunk {
                epoch: first.epoch,
                first_seq: first.seq,
                last_seq: last.seq,
                created_at: first.created_at,
                text: text[w].to_string(),
            });
        }
    }
    out
}

/// Windows of about `CHUNK` bytes, cut at whitespace, each starting about
/// `OVERLAP` bytes before the previous one ends. A window depends only on
/// the text before its end, so appending text keeps all but the last.
fn windows(text: &str) -> Vec<Range<usize>> {
    let mut out = vec![];
    let mut start = 0;
    while start < text.len() {
        let mut end = text.floor_char_boundary(start + CHUNK);
        if end < text.len() {
            let mid = text.floor_char_boundary(start + CHUNK / 2);
            if let Some(i) = text[mid..end].rfind(char::is_whitespace) {
                end = mid + i;
            }
        }
        out.push(start..end);
        if end == text.len() {
            break;
        }
        start = overlap_start(&text[..end]);
    }
    out
}

/// Where the window after one ending at `text.len()` starts: about
/// `OVERLAP` bytes back, after whitespace.
fn overlap_start(text: &str) -> usize {
    let back = text.floor_char_boundary(text.len().saturating_sub(OVERLAP));
    text[back..]
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map_or(back, |(i, c)| back + i + c.len_utf8())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream, StreamExt};
    use rho::agent::Agent;
    use rho::extension::NoFrontend;
    use rho::provider::{Event, Provider, Request as ModelRequest, StopReason};
    use rho::provider::{ModelSpec, ProviderName};
    use serde_json::json;

    #[test]
    fn windows_overlap_and_survive_appends() {
        let text: String = (0..400).map(|n| format!("w{n} ")).collect();
        let w = windows(&text);
        assert!(w.len() > 2);
        assert_eq!((w[0].start, w.last().unwrap().end), (0, text.len()));
        for pair in w.windows(2) {
            let overlap = pair[0].end - pair[1].start;
            assert!((OVERLAP - 10..=OVERLAP).contains(&overlap), "{overlap}");
            assert!(text[..pair[0].end].ends_with(|c: char| !c.is_whitespace()));
        }
        let longer = windows(&format!("{text}more words"));
        assert_eq!(longer[..w.len() - 1], w[..w.len() - 1]);
        assert!(windows("").is_empty());
        assert_eq!(windows("short"), vec![0..5]);
    }

    #[test]
    fn fuse_prefers_ids_in_both_lists() {
        let ids: Vec<i64> = fuse(&[vec![1, 2, 3], vec![3, 4]])
            .iter()
            .map(|f| f.0)
            .collect();
        assert_eq!(ids, [3, 1, 2, 4]);
        assert_eq!(keywords("a-b \"c\""), Some(r#""a" OR "b" OR "c""#.into()));
        assert_eq!(keywords("--"), None);
    }

    struct Scripted(Mutex<Vec<Vec<Event>>>);

    impl Provider for Scripted {
        fn stream(
            &self,
            _request: ModelRequest,
            _: &dyn Fn(&mut serde_json::Value),
        ) -> BoxStream<'static, Result<Event>> {
            let events = self.0.lock().unwrap().remove(0);
            stream::iter(events.into_iter().map(Ok)).boxed()
        }
    }

    /// Letter counts: texts sharing letters are similar.
    fn letters(texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0; 26];
                for b in t.to_lowercase().bytes().filter(u8::is_ascii_lowercase) {
                    v[usize::from(b - b'a')] += 1.0;
                }
                v
            })
            .collect())
    }

    #[tokio::test]
    async fn searches_sessions_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("foo/bar");
        let (sub, outside) = (root.join("baz"), dir.path().join("foo"));
        for d in [&sub, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(root.join(MARKER), "").unwrap();
        let answers = ["zebra crossing", "zebra outside", "nothing", "fine"]
            .map(|t| vec![Event::TextDelta(t.into()), Event::Done(StopReason::EndTurn)]);
        let long: String = (0..300).map(|n| format!("okapi w{n} ")).collect();
        let db = dir.path().join("sessions.db");
        let mut agent = Agent::new(
            Arc::new(Scripted(Mutex::new(answers.to_vec()))),
            ModelSpec::new("test", "m"),
            vec![],
        );
        agent.sessions_db = Some(Arc::new(move |_| db.clone()));
        // Answers `summarizeOver`.
        agent.compact_model = Some(ModelSpec::new("cheap", "m"));
        for (id, cwd, prompt) in [
            ("in", &sub, "about zebras"),
            ("out", &outside, "about zebras"),
            ("now", &root, "nothing"),
            ("long", &sub, &long),
        ] {
            let mut s = agent
                .start_session(SessionId::new(id), cwd, vec![], Arc::new(NoFrontend))
                .await
                .unwrap();
            agent
                .run(&mut s, vec![Content::text(prompt)])
                .await
                .unwrap();
        }
        let cheap = vec![vec![
            Event::TextDelta("the answer".into()),
            Event::Done(StopReason::EndTurn),
        ]];
        agent.registry.register(
            ProviderName::new("cheap"),
            Arc::new(Scripted(Mutex::new(cheap))),
        );
        let history = History::new(dir.path().join("history.db"), Box::new(letters));
        agent.extensions.push(Arc::new(History(history.0.clone())));

        let mut session = agent
            .start_session(SessionId::new("s"), &root, vec![], Arc::new(NoFrontend))
            .await
            .unwrap();
        let tool = session
            .tool(&ToolName::from_static("History"))
            .expect("tool offered");
        let run = async |v: Value| tool.run(v, &session.ctx).await.map(|o| o.output);
        let out = run(json!({"action": "search", "query": "zebra", "mode": "keyword"}))
            .await
            .unwrap();
        assert!(out.starts_with("Found 1 results for \"zebra\""), "{out}");
        assert!(
            out.contains(
                " · baz · session in, seq 0-1\nUser: about zebras\n\nAssistant: zebra crossing"
            ),
            "{out}"
        );
        let out = run(json!({"action": "search", "query": "zebra"}))
            .await
            .unwrap();
        assert!(!out.contains("session out"), "{out}");
        assert!(out.contains("session now"), "{out}");

        assert!(session.commands.iter().any(|c| c.name == HISTORY));
        let ext = History(history.0.clone());
        let out = ext.command(&session.ctx, &HISTORY, "zebra").await.unwrap();
        assert!(out.contains("Assistant: zebra crossing"), "{out}");
        let out = ext.command(&session.ctx, &HISTORY, " ").await.unwrap();
        assert_eq!(out, "Usage: /history <query>");

        let out = run(json!({"action": "read", "session": "in", "seq": 1}))
            .await
            .unwrap();
        assert!(
            out.ends_with("session in\n\n[1] Assistant: zebra crossing"),
            "{out}"
        );
        let err = run(json!({"action": "read", "session": "out", "seq": 0}))
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("no session out in"), "{err}");

        // All chunks of the long session match: one result, the whole text.
        let out = run(json!({"action": "search", "query": "okapi", "mode": "keyword"}))
            .await
            .unwrap();
        let text = format!("User: {long}\n\nAssistant: fine");
        assert!(out.starts_with("Found 1 results"), "{out}");
        assert!(
            out.ends_with(&format!("session long, seq 0-1\n{text}")),
            "{out}"
        );
        let out = run(
            json!({"action": "search", "query": "okapi", "mode": "keyword", "summarizeOver": 0}),
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            "Answer from cheap/m over 1 results for \"okapi\":\n\nthe answer"
        );

        agent.redact(&mut session, "zebra").await.unwrap();
        let raw: i64 = history
            .0
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM chunks WHERE instr(text, 'zebra') > 0",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(raw, 0);
        let tool = session.tool(&ToolName::from_static("History")).unwrap();
        let out = tool
            .run(
                json!({"action": "search", "query": "zebra", "mode": "keyword"}),
                &session.ctx,
            )
            .await
            .unwrap()
            .output;
        assert!(!out.contains("crossing"), "{out}");
        let out = tool
            .run(
                json!({"action": "read", "session": "in", "seq": 0}),
                &session.ctx,
            )
            .await
            .unwrap()
            .output;
        assert!(!out.contains("zebra"), "{out}");
        assert!(out.contains(rho::store::MASK), "{out}");

        // No marker, no tool.
        let session = agent
            .start_session(SessionId::new("o"), &outside, vec![], Arc::new(NoFrontend))
            .await
            .unwrap();
        assert!(session.tool(&ToolName::from_static("History")).is_none());
    }
}
