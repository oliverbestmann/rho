//! `Memory`: persistent memories in `memory.db` in the config dir (ADR 0030).
//! Content is embedded locally with fastembed (all-MiniLM-L6-v2, downloaded
//! on first use to `$XDG_CACHE_HOME/rho/fastembed`); search ranks by cosine
//! similarity with sqlite-vec's `vec_distance_cosine`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use rho::extension::{CommandName, Context, Extension, SessionSetup, ToolOutput};
use rho::provider::{ToolName, ToolSpec};
use rho::tools::{Tool, input_schema, parse, truncate};
use rusqlite::{Connection, OptionalExtension, Row, params};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::index::{blob, embed, open, purge};

/// Stores refuse content this similar to an existing memory, unless a
/// conversation id is given.
const DUPLICATE_SIMILARITY: f64 = 0.95;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    -- SHA-256 of the content, hex.
    hash TEXT NOT NULL PRIMARY KEY,
    content TEXT NOT NULL CHECK (content <> ''),
    tags TEXT NOT NULL CHECK (json_valid(tags) AND json_type(tags) = 'array'),
    memory_type TEXT,
    metadata TEXT NOT NULL CHECK (json_valid(metadata) AND json_type(metadata) = 'object'),
    -- f32 little-endian, as sqlite-vec reads it.
    embedding BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS memories_by_update ON memories (updated_at DESC, hash);
"#;

/// The columns `Mem::read` expects; tags joined with ", " (NULL if none).
const COLUMNS: &str =
    "hash, content, (SELECT group_concat(value, ', ') FROM json_each(tags)), memory_type";

type Embed = Box<dyn Fn(&str) -> Result<Vec<f32>> + Send + Sync>;

/// One instance serves all sessions; the database and the model are opened
/// on first use.
pub struct Memory(Arc<Inner>);

struct Inner {
    path: PathBuf,
    conn: Mutex<Option<Connection>>,
    embed: Embed,
}

impl Default for Memory {
    fn default() -> Self {
        Memory::new(
            rho::config_dir().join("memory.db"),
            Box::new(|text| Ok(embed(&[text.to_string()])?.remove(0))),
        )
    }
}

impl Memory {
    fn new(path: PathBuf, embed: Embed) -> Memory {
        Memory(Arc::new(Inner {
            path,
            conn: Mutex::new(None),
            embed,
        }))
    }
}

impl Extension for Memory {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.tools.push(Arc::new(MemoryTool(self.0.clone())));
    }

    /// `/redact` (run by the core for every extension): deletes memories
    /// containing the phrase.
    async fn command(&self, _ctx: &Context, name: &CommandName, phrase: &str) -> Option<String> {
        if name.as_str() != "redact" {
            return None;
        }
        let (inner, phrase) = (self.0.clone(), phrase.to_string());
        let n = tokio::task::spawn_blocking(move || inner.redact(&phrase)).await;
        Some(match n.map_err(anyhow::Error::from).and_then(|n| n) {
            Ok(n) => format!("Memories: {n} deleted"),
            Err(e) => format!("Memories: failed: {e:#}"),
        })
    }
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Search,
    Store,
    List,
    Get,
    Update,
    Delete,
    Health,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Input {
    action: Action,
    /// Search query (search)
    query: Option<String>,
    /// Memory content (store, update; update changes the hash)
    content: Option<String>,
    /// Memory content hash (get, update, delete)
    hash: Option<MemoryHash>,
    /// Memory tags (store, update)
    tags: Option<Vec<String>>,
    /// Memory type (store, list, update)
    memory_type: Option<String>,
    /// Memory metadata (store, update)
    metadata: Option<Map<String, Value>>,
    /// Search result limit (default: 10)
    #[schemars(range(min = 1, max = 100))]
    limit: Option<u32>,
    /// Minimum search similarity
    #[schemars(range(min = 0.0, max = 1.0))]
    threshold: Option<f64>,
    /// List page (default: 1)
    #[schemars(range(min = 1))]
    page: Option<u32>,
    /// List page size (default: 10)
    #[schemars(range(min = 1, max = 100))]
    page_size: Option<u32>,
    /// Tag filter (list)
    tag: Option<String>,
    /// Conversation ID; bypasses semantic deduplication (store)
    conversation_id: Option<ConversationId>,
    /// Agent ID added as an agent:<id> tag (store)
    agent_id: Option<AgentId>,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

rho::id_type! {
    /// A memory's id: the SHA-256 of its content, in hex.
    MemoryHash
}

rho::id_type! {
    /// Groups memories from one conversation (`store`).
    ConversationId
}

rho::id_type! {
    /// The agent a memory came from, kept as an `agent:<id>` tag.
    AgentId
}

struct MemoryTool(Arc<Inner>);

impl Tool for MemoryTool {
    fn titled(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_static("Memory"),
            description: format!(
                "Search, store, list, get, update, or delete persistent memories, shared by all sessions. \
                 Search is semantic (embeddings): it matches by meaning, so the query need not share words with the memory. Output is cut at {}KB.\n\
                 Use memory search at the start of a task for relevant decisions, conventions, errors, and unfinished work.\n\
                 Use memory store after substantial work to preserve reusable decisions and learnings, never secrets or transient progress.",
                rho::tools::MAX_OUTPUT / 1024
            ),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, _ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        let inner = self.0.clone();
        let out = tokio::task::spawn_blocking(move || inner.run(&i)).await??;
        Ok(truncate(out).into())
    }
}

struct Mem {
    hash: MemoryHash,
    content: String,
    tags: Option<String>,
    memory_type: Option<String>,
}

impl Mem {
    fn read(r: &Row) -> rusqlite::Result<Mem> {
        Ok(Mem {
            hash: r.get(0)?,
            content: r.get(1)?,
            tags: r.get(2)?,
            memory_type: r.get(3)?,
        })
    }

    /// Title (number, type, score), content, tags and hash.
    fn show(&self, number: Option<usize>, score: Option<f64>) -> String {
        let title = [
            number.map(|n| format!("{n}.")),
            self.memory_type.as_ref().map(|t| format!("[{t}]")),
            score.map(|s| format!("score {s:.3}")),
        ];
        let title: Vec<String> = title.into_iter().flatten().collect();
        let mut lines = vec![title.join(" "), self.content.clone()];
        lines.extend(self.tags.as_ref().map(|t| format!("Tags: {t}")));
        lines.push(format!("Hash: {}", self.hash));
        lines.retain(|l| !l.is_empty());
        lines.join("\n")
    }
}

fn nonempty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !s.is_empty())
}

fn content_hash(content: &str) -> MemoryHash {
    MemoryHash::new(
        Sha256::digest(content)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
}

/// `s` as it appears inside a JSON string.
fn json_escaped(s: &str) -> Result<String> {
    let quoted = serde_json::to_string(s)?;
    Ok(quoted[1..quoted.len() - 1].to_string())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn get(c: &Connection, hash: &MemoryHash) -> Result<Mem> {
    let sql = format!("SELECT {COLUMNS} FROM memories WHERE hash = ?1");
    c.query_row(&sql, [hash], Mem::read)
        .optional()?
        .with_context(|| format!("memory {hash} not found"))
}

impl Inner {
    fn redact(&self, phrase: &str) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        if conn.is_none() {
            *conn = Some(open(&self.path, SCHEMA)?);
        }
        purge(conn.as_mut().unwrap(), |tx| {
            Ok(tx.execute(
                "DELETE FROM memories WHERE instr(content, ?1) > 0 OR instr(tags, ?2) > 0
                     OR instr(metadata, ?2) > 0",
                params![phrase, json_escaped(phrase)?],
            )?)
        })
    }

    fn run(&self, i: &Input) -> Result<String> {
        let action = format!("{:?}", i.action).to_lowercase();
        let need = |s: &Option<String>, what: &str| {
            nonempty(s)
                .map(str::to_string)
                .with_context(|| format!("{what} is required for {action}"))
        };
        // Embed before taking the connection: the first call loads the model.
        let embedding = match i.action {
            Action::Search => Some((self.embed)(&need(&i.query, "query")?)?),
            Action::Store => Some((self.embed)(&need(&i.content, "content")?)?),
            Action::Update => nonempty(&i.content).map(&self.embed).transpose()?,
            _ => None,
        };
        let hash = match i.action {
            Action::Get | Action::Update | Action::Delete => i
                .hash
                .clone()
                .filter(|h| !h.as_str().is_empty())
                .with_context(|| format!("hash is required for {action}"))?,
            // Unused by the other actions.
            _ => MemoryHash::from_static(""),
        };

        let mut conn = self.conn.lock().unwrap();
        if conn.is_none() {
            *conn = Some(open(&self.path, SCHEMA)?);
        }
        let c = conn.as_ref().unwrap();

        match i.action {
            Action::Search => {
                let query = i.query.as_deref().unwrap_or_default();
                let sql = format!(
                    "SELECT * FROM (SELECT {COLUMNS}, 1 - vec_distance_cosine(embedding, ?1) AS score
                     FROM memories) WHERE ?2 IS NULL OR score >= ?2 ORDER BY score DESC LIMIT ?3"
                );
                let mut stmt = c.prepare(&sql)?;
                let rows = stmt.query_map(
                    params![
                        blob(&embedding.unwrap()),
                        i.threshold,
                        i.limit.unwrap_or(10).clamp(1, 100)
                    ],
                    |r| Ok((Mem::read(r)?, r.get::<_, f64>(4)?)),
                )?;
                let found = rows.collect::<rusqlite::Result<Vec<_>>>()?;
                let noun = if found.len() == 1 {
                    "memory"
                } else {
                    "memories"
                };
                let mut parts = vec![format!("Found {} {noun} for {query:?}.", found.len())];
                for (n, (m, score)) in found.iter().enumerate() {
                    parts.push(m.show(Some(n + 1), Some(*score)));
                }
                Ok(parts.join("\n\n"))
            }
            Action::Store => {
                let content = i.content.as_deref().unwrap_or_default();
                let embedding = blob(&embedding.unwrap());
                let hash = content_hash(content);
                let conversation = i
                    .conversation_id
                    .as_ref()
                    .filter(|c| !c.as_str().is_empty());
                let duplicate: Option<MemoryHash> = c
                    .query_row(
                        "SELECT hash FROM memories WHERE hash = ?1
                            OR (?2 AND 1 - vec_distance_cosine(embedding, ?3) >= ?4)
                         ORDER BY hash = ?1 DESC LIMIT 1",
                        params![
                            hash,
                            conversation.is_none(),
                            embedding,
                            DUPLICATE_SIMILARITY
                        ],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(d) = duplicate {
                    return Ok(format!(
                        "Not stored: duplicate of an existing memory.\n{}",
                        get(c, &d)?.show(None, None)
                    ));
                }
                let mut tags = i.tags.clone().unwrap_or_default();
                if let Some(agent) = i.agent_id.as_ref().filter(|a| !a.as_str().is_empty()) {
                    let tag = format!("agent:{agent}");
                    if !tags.contains(&tag) {
                        tags.push(tag);
                    }
                }
                let mut metadata = i.metadata.clone().unwrap_or_default();
                if let Some(conversation) = conversation {
                    metadata.insert("conversation_id".into(), conversation.as_str().into());
                }
                let now = now_ms();
                c.execute(
                    "INSERT INTO memories (hash, content, tags, memory_type, metadata, embedding,
                         created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    params![
                        hash,
                        content,
                        serde_json::to_string(&tags)?,
                        nonempty(&i.memory_type),
                        serde_json::to_string(&metadata)?,
                        embedding,
                        now,
                    ],
                )?;
                Ok(format!(
                    "Memory stored.\n{}",
                    get(c, &hash)?.show(None, None)
                ))
            }
            Action::List => {
                let page = i.page.unwrap_or(1).max(1);
                let size = i.page_size.unwrap_or(10).clamp(1, 100);
                let filter =
                    "(?1 IS NULL OR EXISTS (SELECT 1 FROM json_each(tags) WHERE value = ?1))
                    AND (?2 IS NULL OR memory_type = ?2)";
                let (tag, kind) = (nonempty(&i.tag), nonempty(&i.memory_type));
                let total: i64 = c.query_row(
                    &format!("SELECT count(*) FROM memories WHERE {filter}"),
                    params![tag, kind],
                    |r| r.get(0),
                )?;
                let sql = format!(
                    "SELECT {COLUMNS} FROM memories WHERE {filter}
                     ORDER BY updated_at DESC, hash LIMIT ?3 OFFSET ?4"
                );
                let offset = i64::from(page - 1) * i64::from(size);
                let mut stmt = c.prepare(&sql)?;
                let rows = stmt.query_map(params![tag, kind, size, offset], Mem::read)?;
                let found = rows.collect::<rusqlite::Result<Vec<_>>>()?;
                let more = offset + (found.len() as i64) < total;
                let mut parts = vec![format!(
                    "{} of {total} memories, page {page}{}",
                    found.len(),
                    if more { " (more available)" } else { "" }
                )];
                for (n, m) in found.iter().enumerate() {
                    parts.push(m.show(Some(n + 1), None));
                }
                Ok(parts.join("\n\n"))
            }
            Action::Get => Ok(get(c, &hash)?.show(None, None)),
            Action::Update => {
                let tags = i.tags.as_ref().map(serde_json::to_string).transpose()?;
                let metadata = i.metadata.as_ref().map(serde_json::to_string).transpose()?;
                let content = nonempty(&i.content);
                let new_hash = content.map(content_hash);
                if let Some(new) = new_hash.as_ref().filter(|n| **n != hash)
                    && get(c, new).is_ok()
                {
                    bail!("memory {new} already has this content");
                }
                // Metadata is merged (JSON merge patch: null removes a key).
                let changed = c.execute(
                    "UPDATE memories SET tags = COALESCE(?2, tags),
                         memory_type = COALESCE(?3, memory_type),
                         metadata = json_patch(metadata, COALESCE(?4, '{}')),
                         updated_at = ?5,
                         hash = COALESCE(?6, hash),
                         content = COALESCE(?7, content),
                         embedding = COALESCE(?8, embedding)
                     WHERE hash = ?1",
                    params![
                        hash,
                        tags,
                        nonempty(&i.memory_type),
                        metadata,
                        now_ms(),
                        new_hash,
                        content,
                        embedding.as_deref().map(blob),
                    ],
                )?;
                if changed == 0 {
                    bail!("memory {hash} not found");
                }
                Ok(format!(
                    "Memory updated.\n{}",
                    get(c, new_hash.as_ref().unwrap_or(&hash))?.show(None, None)
                ))
            }
            Action::Delete => {
                if c.execute("DELETE FROM memories WHERE hash = ?1", [&hash])? == 0 {
                    bail!("memory {hash} not found");
                }
                Ok(format!("Memory {hash} deleted."))
            }
            Action::Health => {
                let n: i64 = c.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))?;
                let version: String = c.query_row("SELECT vec_version()", [], |r| r.get(0))?;
                Ok(format!(
                    "Memory store: {n} memories in {} (sqlite-vec {version})",
                    self.path.display()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::extension::NoFrontend;
    use rho::store::SessionId;
    use serde_json::json;

    /// Letter counts: texts sharing letters are similar.
    fn letters(text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0.0; 26];
        for b in text.to_lowercase().bytes().filter(u8::is_ascii_lowercase) {
            v[usize::from(b - b'a')] += 1.0;
        }
        Ok(v)
    }

    #[tokio::test]
    async fn stores_searches_and_edits() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::new(dir.path().join("memory.db"), Box::new(letters));
        let tool = MemoryTool(memory.0.clone());
        let ctx = Context::new(
            SessionId::new("s"),
            "/tmp",
            Arc::new(NoFrontend),
            Default::default(),
        );
        let run = async |v: Value| tool.run(v, &ctx).await.map(|o| o.output);

        let out = run(json!({"action": "store", "content": "aaa bbb", "tags": ["x"], "memoryType": "note", "agentId": "rho"})).await.unwrap();
        assert!(
            out.starts_with("Memory stored.\n[note]\naaa bbb\nTags: x, agent:rho\nHash: "),
            "{out}"
        );
        let hash = out.rsplit("Hash: ").next().unwrap().to_string();
        assert_eq!(hash.len(), 64);
        run(json!({"action": "store", "content": "zzz"}))
            .await
            .unwrap();

        // The same content, and similar content without a conversation id.
        for content in ["aaa bbb", "bbb aaa"] {
            let out = run(json!({"action": "store", "content": content}))
                .await
                .unwrap();
            assert!(
                out.starts_with("Not stored: duplicate") && out.ends_with(&hash),
                "{out}"
            );
        }
        let out = run(json!({"action": "store", "content": "bbb aaa", "conversationId": "c"}))
            .await
            .unwrap();
        assert!(out.starts_with("Memory stored."), "{out}");

        let out = run(json!({"action": "search", "query": "zz", "limit": 2}))
            .await
            .unwrap();
        assert!(
            out.starts_with("Found 2 memories for \"zz\".\n\n1. score 1.000\nzzz\nHash: "),
            "{out}"
        );
        let out = run(json!({"action": "search", "query": "zz", "threshold": 0.5}))
            .await
            .unwrap();
        assert!(out.starts_with("Found 1 memory"), "{out}");

        let out = run(json!({"action": "list", "pageSize": 2})).await.unwrap();
        assert!(
            out.starts_with("2 of 3 memories, page 1 (more available)\n\n1."),
            "{out}"
        );
        let out = run(json!({"action": "list", "tag": "agent:rho"}))
            .await
            .unwrap();
        assert!(
            out.starts_with("1 of 1 memories, page 1\n\n1. [note]\naaa bbb"),
            "{out}"
        );
        let out = run(json!({"action": "list", "page": 2, "pageSize": 2}))
            .await
            .unwrap();
        assert!(out.starts_with("1 of 3 memories, page 2\n\n1."), "{out}");

        let out =
            run(json!({"action": "update", "hash": hash, "tags": ["y"], "metadata": {"k": 1}}))
                .await
                .unwrap();
        assert_eq!(
            out,
            format!("Memory updated.\n[note]\naaa bbb\nTags: y\nHash: {hash}")
        );
        let metadata: String = {
            let conn = memory.0.conn.lock().unwrap();
            conn.as_ref()
                .unwrap()
                .query_row(
                    "SELECT metadata FROM memories WHERE hash = ?1",
                    [&hash],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(metadata, r#"{"k":1}"#);

        // New content: a new hash and embedding, tags and metadata stay.
        let out = run(json!({"action": "update", "hash": hash, "content": "ccc"}))
            .await
            .unwrap();
        let old = hash;
        let hash = out.rsplit("Hash: ").next().unwrap().to_string();
        assert_ne!(hash, old);
        assert_eq!(
            out,
            format!("Memory updated.\n[note]\nccc\nTags: y\nHash: {hash}")
        );
        let out = run(json!({"action": "search", "query": "cc", "limit": 1}))
            .await
            .unwrap();
        assert!(out.contains("score 1.000\nccc"), "{out}");
        let err = run(json!({"action": "get", "hash": old}))
            .await
            .unwrap_err();
        assert!(err.to_string().ends_with("not found"));
        let err = run(json!({"action": "update", "hash": hash, "content": "zzz"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().ends_with("already has this content"),
            "{err}"
        );

        let out = run(json!({"action": "delete", "hash": hash}))
            .await
            .unwrap();
        assert_eq!(out, format!("Memory {hash} deleted."));
        let err = run(json!({"action": "get", "hash": hash}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), format!("memory {hash} not found"));
        let out = run(json!({"action": "health"})).await.unwrap();
        assert!(out.starts_with("Memory store: 2 memories in "), "{out}");

        for (v, err) in [
            (json!({"action": "search"}), "query is required for search"),
            (json!({"action": "store"}), "content is required for store"),
            (json!({"action": "get"}), "hash is required for get"),
            (
                json!({"action": "update", "hash": ""}),
                "hash is required for update",
            ),
        ] {
            assert_eq!(run(v).await.unwrap_err().to_string(), err);
        }
    }

    #[tokio::test]
    async fn redact_deletes_matching_memories() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::new(dir.path().join("memory.db"), Box::new(letters));
        let ctx = Context::new(
            SessionId::new("s"),
            "/tmp",
            Arc::new(NoFrontend),
            Default::default(),
        );
        let tool = MemoryTool(memory.0.clone());
        for v in [
            json!({"action": "store", "content": "key is s3\"cr"}),
            json!({"action": "store", "content": "zzz", "tags": ["s3\"cr"]}),
            json!({"action": "store", "content": "qqq"}),
        ] {
            tool.run(v, &ctx).await.unwrap();
        }
        let redact = CommandName::from_static("redact");
        let out = memory.command(&ctx, &redact, "s3\"cr").await;
        assert_eq!(out.as_deref(), Some("Memories: 2 deleted"));
        let other = memory
            .command(&ctx, &CommandName::from_static("x"), "q")
            .await;
        assert_eq!(other, None);
        let out = tool.run(json!({"action": "health"}), &ctx).await.unwrap();
        assert!(
            out.output.starts_with("Memory store: 1 memories"),
            "{}",
            out.output
        );
    }

    /// Downloads the real model (~90 MB) on first run.
    #[test]
    #[ignore]
    fn embeds_with_the_real_model() {
        let memory = Memory::default();
        let embed = |t| (memory.0.embed)(t).unwrap();
        let cos = |a: &[f32], b: &[f32]| {
            let dot = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(p, q)| p * q).sum::<f32>();
            dot(a, b) / (dot(a, a) * dot(b, b)).sqrt()
        };
        let (cat, kitten, tax) = (
            embed("The cat sits on the mat"),
            embed("A kitten rests on a rug"),
            embed("Quarterly tax filing deadline"),
        );
        assert_eq!(cat.len(), 384);
        assert!(cos(&cat, &kitten) > cos(&cat, &tax) + 0.2);
    }
}
