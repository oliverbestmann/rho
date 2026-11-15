//! The request log for stats (ADR 0075).

use anyhow::Result;
use rusqlite::{Connection, params};

use super::{SessionId, Store};
use crate::provider::{LimitWindow, Limits, ModelSpec, Usage};

/// One model request, see `Store::record_request`.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestRecord {
    /// Unix milliseconds, when it ended.
    pub created_at: i64,
    /// `None` for API proxy requests.
    pub session_id: Option<SessionId>,
    pub model: ModelSpec,
    pub usage: Usage,
    pub cost: f64,
    /// USD for input, output, cache read and cache write, if the price is known.
    pub costs: Option<[f64; 4]>,
    pub limits: Limits,
}

impl Store {
    pub async fn record_request(&self, r: RequestRecord) -> Result<()> {
        self.call(move |c| insert(c, &r)).await
    }
}

fn insert(c: &Connection, r: &RequestRecord) -> Result<()> {
    let [input, output, read, write] = r.costs.map_or([None; 4], |c| c.map(Some));
    let split = |w: Option<LimitWindow>| (w.map(|w| w.utilization), w.map(|w| w.resets_at));
    let (l5, r5) = split(r.limits.five_hour);
    let (l7, r7) = split(r.limits.seven_day);
    let u = &r.usage;
    // A session that was never saved has no row to refer to.
    c.execute(
        "INSERT INTO requests (created_at, session_id, provider, model, input_tokens, \
         output_tokens, cache_read_tokens, cache_write_tokens, cost, input_cost, output_cost, \
         cache_read_cost, cache_write_cost, limit_5h, limit_5h_reset, limit_7d, limit_7d_reset) \
         VALUES (?1, (SELECT id FROM sessions WHERE id = ?2), ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
         ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            r.created_at,
            r.session_id,
            r.model.provider().as_str(),
            r.model.id().as_str(),
            u.input as i64,
            u.output as i64,
            u.cache_read as i64,
            u.cache_write as i64,
            r.cost,
            input,
            output,
            read,
            write,
            l5,
            r5,
            l7,
            r7,
        ],
    )?;
    Ok(())
}

/// A turn as its `Turn: ...` note reports it.
#[derive(Debug, PartialEq)]
struct Turn {
    requests: u32,
    usage: Usage,
    cost: f64,
}

/// Parses "Turn: 2 requests · 3.4s · input 1,030 (98% cached, 12 written) ·
/// output 12 · $0.0123" (`agent::usage::turn_report`). The cached share is
/// rounded, so the cache read tokens are off by up to 0.5% of the input.
fn parse_turn(title: &str) -> Option<Turn> {
    let parts: Vec<&str> = title.strip_prefix("Turn: ")?.split(" · ").collect();
    let num = |s: &str| s.replace(',', "").parse::<u64>().ok();
    let field = |prefix: &str| parts.iter().find_map(|p| p.strip_prefix(prefix));
    let requests = parts.first()?.split(' ').next()?.parse().ok()?;
    let (prompt, detail) = field("input ")?.split_once(" (")?;
    let prompt = num(prompt)?;
    let (share, written) = detail.strip_suffix(" written)")?.split_once(" cached, ")?;
    let written = num(written)?.min(prompt);
    let share: f64 = match share {
        "–" => 0.0,
        s => s.strip_suffix('%')?.parse().ok()?,
    };
    let read = ((prompt as f64 * share / 100.0).round() as u64).min(prompt - written);
    let cost = match field("$") {
        Some(c) => c.parse().ok()?,
        None => 0.0,
    };
    Some(Turn {
        requests,
        usage: Usage {
            input: prompt - written - read,
            output: num(field("output ")?)?,
            cache_read: read,
            cache_write: written,
        },
        cost,
    })
}

/// Fills `requests` with one estimated row per turn note, for the time
/// before requests were logged. Forked sessions copy notes, so equal notes
/// count once. The model is the session's current one.
pub(super) fn backfill(c: &Connection) -> Result<()> {
    let mut select = c.prepare(
        "SELECT MIN(i.session_id), i.created_at, json_extract(i.block, '$.title'), s.model \
         FROM items i JOIN sessions s ON s.id = i.session_id \
         WHERE json_extract(i.block, '$.type') = 'user_note' \
           AND json_extract(i.block, '$.title') LIKE 'Turn: %' \
         GROUP BY i.created_at, json_extract(i.block, '$.title')",
    )?;
    let mut insert = c.prepare(
        "INSERT INTO requests (created_at, session_id, provider, model, requests, estimated, \
         input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost) \
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10)",
    )?;
    let mut rows = select.query([])?;
    while let Some(row) = rows.next()? {
        let (session, at, title, model): (String, i64, String, String) =
            (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
        let Some(turn) = parse_turn(&title) else {
            continue;
        };
        let (provider, model) = model.split_once('/').unwrap_or(("", &model));
        let u = turn.usage;
        insert.execute(params![
            at,
            session,
            provider,
            model,
            turn.requests,
            u.input as i64,
            u.output as i64,
            u.cache_read as i64,
            u.cache_write as i64,
            turn.cost,
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_turn_notes() {
        let t = parse_turn(
            "Turn: 27 requests · 178.6s · input 1,699,571 (98% cached, 38,126 written) · \
             output 12,322 · $0.7696",
        )
        .unwrap();
        assert_eq!(t.requests, 27);
        assert_eq!(t.cost, 0.7696);
        assert_eq!(t.usage.cache_write, 38_126);
        // 98% would be 1,665,580, more than is left besides the written tokens.
        assert_eq!(t.usage.cache_read, 1_661_445);
        assert_eq!(t.usage.input, 0);
        assert_eq!(t.usage.prompt(), 1_699_571);
        assert_eq!(t.usage.output, 12_322);

        let empty = parse_turn("Turn: 1 request · 3.2s · input 0 (– cached, 0 written) · output 0");
        assert_eq!(
            empty,
            Some(Turn {
                requests: 1,
                usage: Usage::default(),
                cost: 0.0
            })
        );
        assert_eq!(parse_turn("Turn: something else"), None);
        assert_eq!(parse_turn("Session idle"), None);
    }

    fn session(c: &Connection, id: &str) {
        c.execute(
            "INSERT INTO sessions (id, cwd, model, thinking, created_at, updated_at) \
             VALUES (?1, '/w', 'anthropic/claude-x', 'off', 0, 0)",
            [id],
        )
        .unwrap();
    }

    fn note(c: &Connection, id: &str, seq: i64, at: i64, title: &str) {
        let block = serde_json::to_string(&crate::provider::Content::UserNote {
            kind: crate::provider::NoteType::Info,
            title: title.into(),
            text: String::new(),
            is_error: false,
        })
        .unwrap();
        c.execute(
            "INSERT INTO items (session_id, seq, epoch, role, block, text, created_at) \
             VALUES (?1, ?2, 0, 'user', ?3, '', ?4)",
            params![id, seq, block, at],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn backfills_turn_notes_once_and_records_requests() {
        let store = Store::memory().unwrap();
        let turn = "Turn: 2 requests · 1.0s · input 100 (50% cached, 10 written) · output 5 · $0.5";
        store
            .call(move |c| {
                session(c, "a");
                session(c, "fork");
                note(c, "a", 0, 1000, turn);
                // The fork's copy of the same note.
                note(c, "fork", 0, 1000, turn);
                note(c, "a", 1, 2000, "Session idle");
                backfill(c)
            })
            .await
            .unwrap();
        store
            .record_request(RequestRecord {
                created_at: 3000,
                session_id: Some(SessionId::new("unsaved")),
                model: ModelSpec::new("anthropic", "claude-y"),
                usage: Usage {
                    input: 1,
                    output: 2,
                    cache_read: 3,
                    cache_write: 4,
                },
                cost: 0.25,
                costs: Some([0.1, 0.1, 0.02, 0.03]),
                limits: Limits {
                    five_hour: None,
                    seven_day: Some(LimitWindow {
                        utilization: 0.5,
                        resets_at: 9000,
                    }),
                },
            })
            .await
            .unwrap();
        let rows = store
            .call(|c| {
                let mut s = c.prepare(
                    "SELECT created_at, session_id, provider, model, requests, estimated, \
                     input_tokens, cache_read_tokens, cache_write_tokens, cost, output_cost, \
                     limit_7d, limit_7d_reset FROM requests ORDER BY created_at",
                )?;
                let rows = s
                    .query_map([], |r| {
                        Ok(format!(
                            "{:?} {:?}",
                            (
                                r.get::<_, i64>(0)?,
                                r.get::<_, Option<String>>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, i64>(4)?,
                                r.get::<_, i64>(5)?,
                            ),
                            (
                                r.get::<_, i64>(6)?,
                                r.get::<_, i64>(7)?,
                                r.get::<_, i64>(8)?,
                                r.get::<_, f64>(9)?,
                                r.get::<_, Option<f64>>(10)?,
                                r.get::<_, Option<f64>>(11)?,
                                r.get::<_, Option<i64>>(12)?,
                            )
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap();
        assert_eq!(
            rows,
            [
                r#"(1000, Some("a"), "anthropic", "claude-x", 2, 1) (40, 50, 10, 0.5, None, None, None)"#,
                r#"(3000, None, "anthropic", "claude-y", 1, 0) (1, 3, 4, 0.25, Some(0.1), Some(0.5), Some(9000))"#,
            ]
        );
    }
}
