# 77. Web search via DuckDuckGo's HTML page

- Status: Accepted
- Date: 2026-11-21

## Context

The model needs to search the web; `FetchUrl` only reads known URLs. As of
2026-11, Bing's search API is retired and Google's Custom Search JSON API is
closed to new customers. Brave, Tavily and Exa offer keyed APIs with small
free tiers; Anthropic's server-side `web_search` tool only works with
Claude models. The user expects a few searches a day and chose DuckDuckGo's
HTML page over the keyed APIs.

## Decision

The `WebSearch` tool (agent crate, `web_search.rs`) posts DuckDuckGo's
search form (`q=…&b=&kl=&df=`) to `https://html.duckduckgo.com/html/` and
returns the organic results as a numbered markdown list: title link and
snippet. Ads (links to `duckduckgo.com/y.js`) are left out. The model reads
pages with `FetchUrl`.

DuckDuckGo answers clients it takes for bots with a 202 page containing an
"anomaly" challenge. Tests from one IP on 2026-11-21 showed:

- User agents of tools (`curl/…`, `Wget`, `python-requests`,
  `Go-http-client`, anything with "curl") are blocked whatever else is sent.
- A Chrome user agent needs a browser header (`Accept: text/html`,
  `Accept-Language`, `Referer` or `Sec-Fetch-*`); Firefox, `Mozilla/5.0`,
  `rho` or no user agent pass alone. GET is stricter than POST.
- reqwest (rustls) gets the challenge when searches are a few seconds apart,
  curl with the same headers does not. DuckDuckGo likely checks the TLS
  fingerprint. Ten seconds apart passed every time.

So the tool sends `User-Agent: rho` and `Accept-Language: en` with POST,
keeps searches of the whole process at least 10 seconds apart, and retries
once after a challenge before failing with an error.

## Consequences

- No key or account. A burst of searches waits about 10 seconds per search.
- DuckDuckGo can change its page or its rules at any time; the tool then
  fails with the challenge error or returns "No results.". The fix is a
  keyed API (Brave would be the first choice), not more browser imitation.
- Results carry no page text, so reading a result costs a `FetchUrl` call.
