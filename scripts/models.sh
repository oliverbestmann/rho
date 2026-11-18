#!/usr/bin/env bash
# Writes the Anthropic and Copilot models from models.dev to rho/src/models.json,
# which rho embeds:
#   { "anthropic": [{ id, name, contextWindow, price, priceAbove }], "github-copilot": [...] }
# price is USD per million tokens [input, output, cache read, cache write];
# priceAbove is [prompt tokens, price] for longer prompts, or null.
# Newest first. Left out: deprecated models, Claude before 4.6 (no adaptive
# thinking) on Anthropic, and anything but GPT, Claude and Gemini on Copilot.
set -euo pipefail

cd "$(dirname "$0")/.."
curl -fsS https://models.dev/api.json | jq '
  def price: [.input, .output, .cache_read // 0, .cache_write // 0];
  def models(keep): [
    .models[]
    | select(.status != "deprecated" and .cost != null and keep)
  ]
  | sort_by(.release_date, .id) | reverse
  | map({
      id,
      name,
      contextWindow: .limit.context,
      price: (.cost | price),
      priceAbove: (.cost.tiers[0] // null | if . then [.tier.size, price] else null end)
    });
  {
    anthropic: (.anthropic | models(.release_date >= "2026-02-04" and (.id | test("-[0-9]{8}$") | not))),
    "github-copilot": (."github-copilot" | models(.id | test("^(gpt|claude|gemini)-")))
  }
' > rho/src/models.json
