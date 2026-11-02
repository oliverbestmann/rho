# 72. Remote rho as provider, Ollama API, thinking store

- Status: Accepted
- Date: 2026-11-02

## Context

ADR 0068 put an OpenAI-compatible API on the daemon, for text and images
only. Three uses need more:

- One rho should use another rho's models, e.g. a laptop the subscription
  of a home machine. Nothing may get lost on the way: tool calls, thinking
  blocks with signatures, images in tool results, cache usage, model info.
- Standard OpenAI clients run tool loops. With thinking on, Anthropic
  returns signed thinking blocks that these clients do not send back.
- Many local tools speak only Ollama's native `/api`.

## Decision

- `/v1/chat/completions` takes tools, tool calls and `tool` messages. Three
  extensions to OpenAI's format carry what it lacks:
  `reasoning_details` on assistant messages and in stream deltas
  (OpenRouter's shape: `reasoning.text` with `signature`,
  `reasoning.encrypted`), `is_error` on tool messages, and model info
  (`name`, `context_window`, `pricing`, `pricing_above`) in `/v1/models`.
  Tool messages take image parts.
- `"remotes": {"home": {"url": "http://host:8080/v1", "token": "..."}}`
  registers a provider per entry (`rho/src/openai_chat.rs`). Its models are
  `home/<remote model id>`, loaded from `/v1/models` at startup. The config is
  read before the model is resolved, so `-m home/...` works.
- Ollama's `/api/version`, `/api/tags`, `/api/ps`, `/api/show`, `/api/chat`
  and `/api/generate` (basic) are served next to `/v1`
  (`rho/src/api/ollama.rs`), under the same `api` config and token. Tool
  calls carry ids; tool results without `tool_call_id` match the open calls
  in order, by `tool_name` if given. `think` maps to rho's levels.
  `GET /` is not served: the daemon's web page answers there with 200,
  which is what Ollama clients check.
- A thinking store in `sessions.db` (`api_thinking`) keeps the thinking
  blocks of every API response with tool calls. The key is a hash of the
  tool calls' names and inputs (object keys sorted), not their ids, since
  Ollama clients may drop ids. When a request has assistant messages with
  tool calls but no thinking blocks, the saved blocks are put back, in all
  such messages, so the prompt prefix stays the same and stays cached.
  Entries older than 90 days are deleted daily. The store opens on first
  API use.
- When the store has no blocks for a message, it is sent without them. The
  Anthropic docs say adaptive thinking accepts turns without thinking
  blocks. Turning thinking off for the request instead was rejected: it
  would break the prompt cache.

## Consequences

- Two responses with the same tool calls share a key; the later one wins.
  Thinking blocks of one may then be sent with the other. Not seen so far.
- The store holds thinking text of API clients for up to 90 days.
- Live use with a real remote and real Ollama clients, and Anthropic's
  behavior on a store miss, are not tested; the tests use scripted
  providers.
- Ollama's `format`, `keep_alive` and most `options` are ignored;
  `/api/generate` rejects `suffix`, `raw`, `template` and `context`.
