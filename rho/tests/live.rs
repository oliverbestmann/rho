//! Live tests against the Anthropic API. They use real subscription quota, so
//! they are ignored by default:
//!
//!   RHO_AUTH_FILE=_rho-auth.json cargo test -p rho --test live -- --ignored --nocapture
//!
//! `RHO_MODEL` picks the model (default claude-sonnet-5-5).

use std::sync::{Arc, Mutex};

use rho::agent::{Agent, AgentEvent, Session};
use rho::anthropic::Anthropic;
use rho::auth::AuthStore;
use rho::extension::Frontend;
use rho::provider::{Content, StopReason, Thinking, Usage};

fn agent() -> Agent {
    let path = std::env::var("RHO_AUTH_FILE").expect("set RHO_AUTH_FILE");
    let http = reqwest::Client::new();
    let store = Arc::new(AuthStore::new(path.into(), http.clone()));
    let model = std::env::var("RHO_MODEL").unwrap_or_else(|_| "claude-sonnet-5-5".into());
    let mut agent = Agent::new(
        Arc::new(Anthropic::new(http, store)),
        model,
        rho::tools::builtin(),
    );
    agent.thinking = Thinking::Low;
    agent
}

/// Collects what one prompt produced: usages, text, thinking, notices.
#[derive(Default)]
struct Collect(Mutex<(Vec<Usage>, String, String, Vec<String>)>);

impl Frontend for Collect {
    fn event(&self, ev: AgentEvent<'_>) {
        let mut c = self.0.lock().unwrap();
        match ev {
            AgentEvent::Usage { usage, .. } => c.0.push(usage),
            AgentEvent::Text(t) => c.1.push_str(t),
            AgentEvent::Thinking(t) => c.2.push_str(t),
            AgentEvent::Notice { title, text, .. } => c.3.push(format!("{title}\n{text}")),
            _ => {}
        }
    }
}

async fn session(agent: &Agent) -> (Session, Arc<Collect>) {
    let collect = Arc::new(Collect::default());
    (
        agent
            .start_session("live", ".", vec![], collect.clone())
            .await
            .unwrap(),
        collect,
    )
}

async fn prompt(
    agent: &Agent,
    (session, collect): &mut (Session, Arc<Collect>),
    text: &str,
) -> (Vec<Usage>, String, String, Vec<String>) {
    let stop = agent
        .run(session, vec![Content::text(text)])
        .await
        .expect("turn failed");
    assert_eq!(stop, StopReason::EndTurn);
    std::mem::take(&mut *collect.0.lock().unwrap())
}

/// System prompt large enough for every model's minimum cacheable prefix
/// (up to 4096 tokens). The nonce makes the first request a guaranteed miss.
fn big_prompt_agent() -> Agent {
    let mut agent = agent();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    agent.system = format!("{}\nReference table {nonce}:\n", agent.system)
        + &(0..600)
            .map(|i| format!("entry {i}: code {nonce}-{} status ok\n", i * 7))
            .collect::<String>();
    agent
}

/// /compact right after a run reads the history from cache with the session
/// model; with an expired cache it goes to the cheap model instead.
#[tokio::test]
#[ignore]
async fn compaction_uses_cache_when_warm() {
    let mut agent = big_prompt_agent();
    agent.registry.register("anthropic", agent.provider.clone());
    agent.compact_model = Some("anthropic/claude-haiku-5-5".into());
    let mut s = session(&agent).await;
    let (usages, ..) = prompt(
        &agent,
        &mut s,
        "Run `echo compact-test` with the Bash tool, then reply with just its output.",
    )
    .await;
    let last = *usages.last().unwrap();

    agent.compact(&mut s.0, "").await.expect("warm compaction");
    let notice = s.1.0.lock().unwrap().3.pop().unwrap();
    eprintln!("{notice}");
    let first_line = notice.lines().next().unwrap();
    assert!(
        first_line.contains(&agent.model) && first_line.contains("cache warm"),
        "{notice}"
    );
    let read: u64 = notice
        .split("cache read ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        read >= last.cache_read + last.cache_write,
        "warm compaction missed the cache: {notice}"
    );
    assert!(notice.contains("## Goal"), "{notice}");

    // Pretend the cache expired: compacts the summary with the cheap model.
    s.0.last_request = Some(std::time::Instant::now() - rho::compact::CACHE_TTL);
    let _ = prompt(&agent, &mut s, "Reply with just: ok").await;
    s.0.last_request = Some(std::time::Instant::now() - rho::compact::CACHE_TTL);
    agent.compact(&mut s.0, "").await.expect("cold compaction");
    let notice = s.1.0.lock().unwrap().3.pop().unwrap();
    eprintln!("{notice}");
    assert!(
        notice.starts_with("Compacted (anthropic/claude-haiku-5-5, cache expired)"),
        "{notice}"
    );
    assert!(
        notice.contains("compact-test"),
        "summary lost context: {notice}"
    );
}

/// Every request should read the previous request's full prompt from cache:
/// tools + system + all messages up to the last breakpoint.
#[tokio::test]
#[ignore]
async fn cache_hits_across_tool_loop_and_turns() {
    let agent = big_prompt_agent();

    let mut history = session(&agent).await;
    let (turn1, answer1, thoughts, _) = prompt(
        &agent,
        &mut history,
        "Run `echo cache-test` with the Bash tool, then reply with just its output.",
    )
    .await;
    let (turn2, answer2, _, _) = prompt(&agent, &mut history, "Now reply with just: done").await;

    let all: Vec<Usage> = turn1.iter().chain(&turn2).copied().collect();
    for (i, u) in all.iter().enumerate() {
        eprintln!(
            "request {i}: input {:>5}  cache_read {:>6}  cache_write {:>6}  output {:>4}",
            u.input, u.cache_read, u.cache_write, u.output
        );
    }
    eprintln!(
        "answers: {answer1:?} / {answer2:?}; thinking chars: {}",
        thoughts.len()
    );

    assert!(answer1.contains("cache-test"));
    assert!(turn1.len() >= 2, "expected a tool round trip");
    assert_eq!(all[0].cache_read, 0, "fresh nonce must miss");
    assert!(
        all[0].cache_write > 4096,
        "first request must write the cache"
    );
    for (i, pair) in all.windows(2).enumerate() {
        let (prev, next) = (pair[0], pair[1]);
        assert!(
            next.cache_read >= prev.cache_read + prev.cache_write,
            "request {} did not read the previous prefix: prev {prev:?}, next {next:?}",
            i + 1
        );
        // Only the new tail should be uncached.
        assert!(next.input < 500, "too much uncached input: {next:?}");
    }
}

/// Thinking is streamed, kept in history with its signature, and accepted by
/// the API when sent back during a tool loop.
#[tokio::test]
#[ignore]
async fn thinking_survives_tool_round_trip() {
    let mut agent = agent();
    agent.thinking = Thinking::High;
    let mut history = session(&agent).await;
    let (usages, answer, thoughts, _) = prompt(
        &agent,
        &mut history,
        "Think carefully: which is larger, 17^5 or 5^17? Reason it out first, then verify \
         with one Bash command using `echo $((...))`, then answer with just the larger expression.",
    )
    .await;
    eprintln!(
        "thinking: {} chars, requests: {}, answer: {answer:?}",
        thoughts.len(),
        usages.len()
    );

    assert!(!thoughts.is_empty(), "no thinking streamed");
    assert!(usages.len() >= 2, "expected a tool round trip");
    assert!(answer.contains("5^17"), "{answer}");
    let signed = history
        .0
        .history
        .iter()
        .flat_map(|m| &m.content)
        .any(|c| matches!(c, Content::Thinking { signature, .. } if !signature.is_empty()));
    assert!(signed, "no signed thinking block in history");
}
