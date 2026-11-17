//! Text to speech for the web client (`POST /tts` in `acp_ws`), with Gemini
//! on Vertex AI. Off unless the config has a key (a Vertex express key):
//!
//! ```json
//! { "tts": { "key": "AQ...", "voice": "Charon" } }
//! ```
//!
//! The audio streams as it is generated: raw PCM, 16 bit little endian,
//! 24 kHz, mono. Long text is spoken in chunks, one request each, as one
//! request is limited (8000 bytes in, about 655 s out) and drifts after a
//! few minutes. Code blocks are not read out: a model first replaces them
//! with a description (`describe_code`).

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::{Stream, StreamExt, TryStreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};

use std::path::Path;

use crate::agent::Agent;
use crate::provider::{Content, Event, Message, Request, Role, Thinking, sse};

const DESCRIBE: &str = "You prepare text to be read aloud. Replace every fenced code block \
(between ``` lines) with a short spoken description of what it shows or does, one or two \
sentences, in the language of the surrounding text. Leave all other text exactly as it is. \
Reply with the resulting text only.";

/// `text` with its ``` code blocks described in words by the compaction
/// model (else the default model). Unchanged without code blocks.
pub async fn describe_code(agent: &Agent, text: &str) -> Result<String> {
    if !text.contains("```") {
        return Ok(text.to_string());
    }
    let model = agent.compact_model.as_ref().unwrap_or(&agent.model);
    let provider = agent.registry.resolve(model)?;
    let request = Request {
        model: model.id().clone(),
        system: DESCRIBE.into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::text(text)],
        }],
        tools: vec![],
        max_tokens: 8192,
        thinking: Thinking::Off,
    };
    let info = provider.model_info(&request.model);
    let mut events = provider.stream(request, &|_| {});
    // Recorded like the API's requests, so the stats include it.
    if let Ok(Some(store)) = agent.open_store(Path::new("/")).await {
        events = store.recorded(events, None, model.clone(), info);
    }
    let mut out = String::new();
    while let Some(event) = events.next().await {
        if let Event::TextDelta(t) = event? {
            out.push_str(&t);
        }
    }
    Ok(out)
}

pub const KEY: &str = "tts";

pub const CONTENT_TYPE: &str = "audio/L16; rate=24000; channels=1";

/// `"tts"` in the config.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub key: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_voice")]
    pub voice: String,
}

fn default_model() -> String {
    "gemini-3.8-flash-tts".into()
}

fn default_voice() -> String {
    "Charon".into()
}

/// Bytes of text per request.
const CHUNK: usize = 2000;

/// `text` in pieces of at most `CHUNK` bytes, cut at a paragraph, else a
/// sentence, else a space, else anywhere.
fn chunks(mut text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    while text.len() > CHUNK {
        let mut end = CHUNK;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let window = &text[..end];
        // Cuts in the first third would make needlessly short chunks.
        let cut = ["\n\n", "\n", ". ", "! ", "? ", " "]
            .iter()
            .filter_map(|sep| window.rfind(sep).map(|i| i + sep.len()))
            .find(|&i| i > CHUNK / 3)
            .unwrap_or(end);
        out.push(text[..cut].trim());
        text = &text[cut..];
    }
    out.push(text.trim());
    out.retain(|c| !c.is_empty());
    out
}

/// The audio of `text`, as PCM chunks while Gemini generates them, one
/// request per `chunks` piece after the other. Fails before the first chunk
/// if the first request does.
pub async fn speak(
    http: &reqwest::Client,
    config: &Config,
    text: &str,
) -> Result<impl Stream<Item = Result<Vec<u8>>> + use<>> {
    let mut pieces = chunks(text).into_iter().map(str::to_owned);
    let head = speak_one(http, config, &pieces.next().unwrap_or_default()).await?;
    // Generation is faster than playback, so the client has audio buffered
    // while the next request starts.
    let (http, config) = (http.clone(), config.clone());
    let rest = stream::iter(pieces.collect::<Vec<_>>())
        .then(move |piece| {
            let (http, config) = (http.clone(), config.clone());
            async move { speak_one(&http, &config, &piece).await }
        })
        .try_flatten();
    Ok(head.chain(rest))
}

async fn speak_one(
    http: &reqwest::Client,
    config: &Config,
    text: &str,
) -> Result<impl Stream<Item = Result<Vec<u8>>> + use<>> {
    let url = format!(
        "https://aiplatform.googleapis.com/v1/publishers/google/models/{}:streamGenerateContent?alt=sse",
        config.model
    );
    let body = json!({
        "contents": [{ "role": "user", "parts": [{ "text": text }] }],
        "generationConfig": {
            "responseModalities": ["AUDIO"],
            "speechConfig": { "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": config.voice } } },
        },
    });
    let resp = http
        .post(&url)
        .header("x-goog-api-key", &config.key)
        .json(&body)
        .send()
        .await
        .context("tts request")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("tts: {status}: {text}");
    }
    let mut buf = Vec::new();
    Ok(resp.bytes_stream().map(move |chunk| -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for data in sse::data(&mut buf, &chunk?) {
            out.extend(pcm(&data)?);
        }
        Ok(out)
    }))
}

/// The audio in one streamed response chunk.
fn pcm(data: &str) -> Result<Vec<u8>> {
    let chunk: Value = serde_json::from_str(data).context("tts chunk")?;
    if let Some(error) = chunk.get("error") {
        bail!("tts: {error}");
    }
    let mut out = Vec::new();
    let parts = chunk["candidates"][0]["content"]["parts"].as_array();
    for part in parts.into_iter().flatten() {
        if let Some(b64) = part["inlineData"]["data"].as_str() {
            out.extend(STANDARD.decode(b64).context("tts audio")?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{CHUNK, chunks, pcm};

    #[test]
    fn long_text_is_cut_at_paragraphs_then_sentences() {
        assert_eq!(chunks("Hi."), ["Hi."]);
        assert!(chunks("  ").is_empty());

        let para = "word ".repeat(300); // 1500 bytes
        let text = format!("{para}\n\n{para}\n\n{para}");
        let out = chunks(&text);
        assert_eq!(out, [para.trim(); 3]);

        let sentences = "Ä short sentence. ".repeat(500);
        let out = chunks(&sentences);
        assert!(out.iter().all(|c| c.len() <= CHUNK && c.ends_with('.')));
        assert_eq!(out.join(" "), sentences.trim());
    }

    #[test]
    fn chunks_give_their_audio() {
        let audio = r#"{"candidates":[{"content":{"parts":[{"inlineData":{"data":"AAEC"}},{"inlineData":{"data":"Aw=="}}]}}]}"#;
        assert_eq!(pcm(audio).unwrap(), [0, 1, 2, 3]);
        let end = r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{}}"#;
        assert!(pcm(end).unwrap().is_empty());
        assert!(pcm(r#"{"error":{"message":"quota"}}"#).is_err());
    }
}
