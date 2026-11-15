#!/usr/bin/env bash
# Prints the current Anthropic and Copilot models from models.dev as JSON:
#   { "anthropic": [{ id, name, contextWindow, price, priceAbove }], "copilot": [...] }
# price is USD per million tokens [input, output, cache read, cache write];
# priceAbove is [prompt tokens, price] for longer prompts, or null.
# Deprecated models are left out. Usage: scripts/models.sh > models.json
set -euo pipefail

curl -fsS https://models.dev/api.json | jq '
  def price: [.input, .output, .cache_read // 0, .cache_write // 0];
  def models: [
    .models[]
    | select(.status != "deprecated" and .cost != null)
    | {
        id,
        name,
        contextWindow: .limit.context,
        price: (.cost | price),
        priceAbove: (.cost.tiers[0] // null | if . then [.tier.size, price] else null end)
      }
  ] | sort_by(.id);
  { anthropic: (.anthropic | models), copilot: (."github-copilot" | models) }
'
