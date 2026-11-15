//! Server-sent event streams, shared by the providers.

use anyhow::Result;
use futures::stream::{self, BoxStream};
use futures::{StreamExt, TryStreamExt};

use crate::provider::Event;

/// Events of a streamed response; `send` makes the request, `feed` parses
/// the body chunks (e.g. `SseParser::feed`).
pub fn events(
    send: impl Future<Output = Result<reqwest::Response>> + Send + 'static,
    feed: impl FnMut(&[u8]) -> Vec<Result<Event>> + Send + 'static,
) -> BoxStream<'static, Result<Event>> {
    events_with(send, |_| None, feed)
}

/// `events`, starting with the event `head` makes of the response headers.
pub fn events_with(
    send: impl Future<Output = Result<reqwest::Response>> + Send + 'static,
    head: impl FnOnce(&reqwest::header::HeaderMap) -> Option<Event> + Send + 'static,
    feed: impl FnMut(&[u8]) -> Vec<Result<Event>> + Send + 'static,
) -> BoxStream<'static, Result<Event>> {
    let mut parts = Some((head, feed));
    stream::once(send)
        .map_ok(move |resp| {
            let (head, mut feed) = parts.take().expect("one response");
            let first = head(resp.headers()).map(Ok);
            let body = resp.bytes_stream().flat_map(move |chunk| {
                stream::iter(match chunk {
                    Ok(bytes) => feed(&bytes),
                    Err(e) => vec![Err(e.into())],
                })
            });
            stream::iter(first).chain(body)
        })
        .try_flatten()
        .boxed()
}

/// The `data:` payloads of the complete server-sent event lines in `buf`.
pub fn data(buf: &mut Vec<u8>, bytes: &[u8]) -> Vec<String> {
    buf.extend_from_slice(bytes);
    let mut out = Vec::new();
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=pos).collect();
        if let Some(data) = String::from_utf8_lossy(&line)
            .trim_end()
            .strip_prefix("data:")
            .map(str::trim)
            // OpenAI's end marker; Copilot sends it on Messages streams too.
            .filter(|data| *data != "[DONE]")
        {
            out.push(data.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_marker_is_ignored() {
        let mut buf = vec![];
        let data = data(
            &mut buf,
            b"data: {\"type\":\"message_stop\"}\n\ndata: [DONE]\n",
        );
        assert_eq!(data, [r#"{"type":"message_stop"}"#]);
    }
}
