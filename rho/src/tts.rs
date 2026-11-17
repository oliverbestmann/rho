//! Text to speech for the web client (`POST /tts` in `acp_ws`), with Gemini
//! on Vertex AI. Off unless the config has a key (a Vertex express key):
//!
//! ```json
//! { "tts": { "key": "AQ...", "voice": "Charon" } }
//! ```
//!
//! The audio streams as it is generated: raw PCM, 16 bit little endian,
//! 24 kHz, mono.

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::provider::sse;

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
    "gemini-3.1-flash-tts-preview".into()
}

fn default_voice() -> String {
    "Charon".into()
}

/// The audio of `text`, as PCM chunks while Gemini generates them. Fails
/// before the first chunk if the request does.
pub async fn speak(
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
    use super::pcm;

    #[test]
    fn chunks_give_their_audio() {
        let audio = r#"{"candidates":[{"content":{"parts":[{"inlineData":{"data":"AAEC"}},{"inlineData":{"data":"Aw=="}}]}}]}"#;
        assert_eq!(pcm(audio).unwrap(), [0, 1, 2, 3]);
        let end = r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{}}"#;
        assert!(pcm(end).unwrap().is_empty());
        assert!(pcm(r#"{"error":{"message":"quota"}}"#).is_err());
    }
}
