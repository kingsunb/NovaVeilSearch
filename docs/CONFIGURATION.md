# Configuration

NovaVeilSearch reads configuration from two sources, merged with the following precedence:

1. **Process environment variables** (highest — what your MCP client passes in `env`).
2. **Global TOML config file** — `$GROK_SEARCH_CONFIG` if set, otherwise `<home>/.config/nova-veil-search/config.toml` on every platform. `<home>` is `$HOME` on Unix / Git Bash, `%USERPROFILE%` on native Windows shells (PowerShell, cmd).
3. **Built-in defaults** (lowest).

The config file is optional; missing files are skipped silently. See the [Config file](#config-file) section below for the TOML schema. The AI provider contract is intentionally narrow: configure a Grok/OpenAI-compatible root URL and the server calls `/v1/responses`.

> **Configuring the remote HTTP transport?** There is one credential model — **server-held keys** — and authentication is mandatory. Set `GROK_MCP_API_TOKEN` (required) and put the provider keys in the server's own environment; every request authenticates with `Authorization: Bearer <token>` (the master token, or a short-lived session token from `POST /login`). Callers never supply their own keys. See [Configuration channels](#configuration-channels-stdio-env-vs-remote-env) directly below.

## Configuration channels (stdio env vs remote env)

Which channel carries your config is decided by the MCP **transport**, not a project setting:

- **stdio (local):** the MCP client spawns `nova-veil-search` as a child process and hands it **environment variables** (the `env` block in your client config).
- **Streamable HTTP (remote):** the client talks to an already-running server whose **own environment** holds every key and all operator settings. Key names are identical to the stdio env keys — nothing is configurable per caller request:

| Setting | env key |
|---|---|
| Grok API key | `GROK_SEARCH_API_KEY` |
| Grok gateway URL | `GROK_SEARCH_URL` |
| Grok model | `GROK_SEARCH_MODEL` |
| Tavily API key | `TAVILY_API_KEY` |
| Firecrawl API key | `FIRECRAWL_API_KEY` |
| TinyFish API key | `TINYFISH_API_KEY` |
| Exa API key | `EXA_API_KEY` |
| Tavily / Firecrawl / Exa anonymous mode | `TAVILY_KEYLESS` / `FIRECRAWL_KEYLESS` / `EXA_KEYLESS` |
| DuckDuckGo / Bing enable & flavor | `DUCKDUCKGO_ENABLED`, `DUCKDUCKGO_REGION`, `BING_ENABLED`, `BING_MARKET` |
| GitHub token | `GITHUB_TOKEN` |
| Outbound proxy | `NOVA_PROXY_KEY` / `NOVA_PROXY_KEYLESS` / `NOVA_PROXY_GROK` |

All entries are **operator-fixed** on the remote transport — set once in the server's own environment, never per request. **Two groups are stdio-only:** OAuth (`GROK_SEARCH_AUTH_MODE` / `GROK_SEARCH_AUTH_FILE`) and the OpenAI-compatible chat-completions transport (`OPENAI_COMPATIBLE_API_URL` / `_API_KEY` / `_MODEL`). The remote server serves Grok **Responses** only; to run a chat-completions relay, use the stdio transport.

### Minimal setup — remote

The operator holds the keys; the client sends one token:

```bash
claude mcp add --transport http nova-veil-search https://<host>/nova-veil-search/mcp \
  --header "Authorization: Bearer <token>"
```

Local (stdio) — the same values as `env`:

```json
{
  "mcpServers": {
    "nova-veil-search": {
      "command": "nova-veil-search",
      "env": {
        "GROK_SEARCH_API_KEY": "<key>",
        "GROK_SEARCH_URL": "https://<gateway>/v1",
        "GROK_SEARCH_MODEL": "<model>",
        "TAVILY_API_KEY": "tvly-..."
      }
    }
  }
}
```

### Settings/config frontend (opt-in, `NOVA_CONFIG_UI`)

The embedded settings SPA (`GET /`) and the config read/write API (`GET/PUT
`/api/config`) are **off by default**: the server stays a pure backend
(`/mcp`, `/messages`, `/login`) with no frontend routes and no per-request
`config.toml` read. Set `NOVA_CONFIG_UI=true` (also `1`/`yes`, case-insensitive)
to enable both. ON serves `GET /` and lets each request honor `config.toml`
edits with the usual precedence (env > file > defaults); OFF returns `404` on
`/` and `/api/config` and keeps request config env-only (zero per-request
disk I/O).

## Grok Responses

| Variable | Default | Description |
|---|---|---|
| `GROK_SEARCH_AUTH_MODE` | `api_key` | `api_key` uses `GROK_SEARCH_API_KEY`; `oauth` uses the local token file created by `nova-veil-search login`. |
| `GROK_SEARCH_API_KEY` | required in `api_key` mode | Bearer token for the configured Grok-compatible gateway. |
| `GROK_SEARCH_AUTH_FILE` | `<home>/.config/nova-veil-search/auth.json` | Optional OAuth token file override. |
| `GROK_SEARCH_URL` | `https://api.x.ai` | Root URL, `/v1` base URL, or endpoint-like URL. The service normalizes it to a `/v1` base. |
| `GROK_SEARCH_MODEL` | `grok-4-1-fast-reasoning` | Model sent in the Responses payload. |
| `GROK_SEARCH_WEB_SEARCH` | `true` | Sends Responses `{"type":"web_search"}`. |
| `GROK_SEARCH_X_SEARCH` | `false` | Sends Responses `{"type":"x_search"}` only when enabled. |

Boolean values accept `1`, `true`, or `yes` as enabled. Any other value is treated as disabled.

Example:

```bash
GROK_SEARCH_API_KEY=...
GROK_SEARCH_URL=https://api.x.ai
GROK_SEARCH_MODEL=grok-4-1-fast-reasoning
GROK_SEARCH_X_SEARCH=false
```

The example above calls `https://api.x.ai/v1/responses`.

### OAuth mode

OAuth mode keeps the normal Responses payload and only changes where the Bearer token comes from. The binary handles login and MCP stdio; it does not start a background HTTP proxy.

```bash
nova-veil-search login
nova-veil-search status
nova-veil-search logout
```

`login` opens xAI OAuth in a browser, listens once on `http://127.0.0.1:56121/callback`, and writes `access_token`, `refresh_token`, `id_token`, `token_endpoint`, `base_url`, and `last_refresh` to `auth.json`. `status` prints token presence, expiry, and the auth file path without printing the token. `logout` removes the local auth file.

OAuth mode reuses Hermes' xAI OAuth client id. This may violate xAI terms or create account risk, and Windows stores the token as a normal local file. Do not share the token file.

Minimal Codex config:

```toml
[mcp_servers.nova-veil-search]
command = "nova-veil-search"

[mcp_servers.nova-veil-search.env]
GROK_SEARCH_AUTH_MODE = "oauth"
GROK_SEARCH_MODEL = "grok-4.3"
GROK_SEARCH_WEB_SEARCH = "true"
```

## Tavily

| Variable | Default | Description |
|---|---|---|
| `TAVILY_API_KEY` | unset | Enables Tavily-backed source enrichment, fallback, fetch, and map. Accepts a single key or a comma-separated list (`tvly-a,tvly-b`); multiple keys rotate round-robin per request, with automatic failover to the next key on key-scoped errors (HTTP 401/403/429/432/433). |
| `TAVILY_API_URL` | `https://api.tavily.com` | Tavily API base URL. |
| `TAVILY_ENABLED` | `true` | Optional override. Set to `false` only when you want to disable Tavily even if `TAVILY_API_KEY` is configured. |
| `GROK_SEARCH_EXTRA_SOURCES` | `3` | Adds enrichment sources after a verifiable Grok result, served by the first source-chain provider with results. Set `0` to disable enrichment. |
| `GROK_SEARCH_FALLBACK_SOURCES` | `5` | Number of fallback sources to cache when Grok is unverifiable. |

## Firecrawl

| Variable | Default | Description |
|---|---|---|
| `FIRECRAWL_API_KEY` | unset | Enables Firecrawl fallback for `web_fetch` and supplemental fallback sources. |
| `FIRECRAWL_API_URL` | `https://api.firecrawl.dev` | Firecrawl API base URL. Defaults to `/v2`; explicit `/v1` or `/v2` is preserved. |
| `FIRECRAWL_ENABLED` | `true` | Optional override. Set to `false` to disable Firecrawl even if a key is configured. |

Firecrawl uses [`POST /v2/search`](https://docs.firecrawl.dev/api-reference/endpoint/search)
and `POST /v2/scrape` by default. Search consumes web results from the v2
`data.web` response; legacy `data` arrays and flat `results` arrays are also accepted.
The search request uses Firecrawl's default web source.

For a gateway mounted at `http://localhost:9010/firecrawl`, set
`FIRECRAWL_API_URL=http://localhost:9010/firecrawl` or
`http://localhost:9010/firecrawl/v2`. Both send requests to
`/firecrawl/v2/search` and `/firecrawl/v2/scrape`, preserving the gateway prefix.
For a legacy v1 deployment, explicitly set a base ending in `/v1`.

## TinyFish

Free Search & Fetch APIs built for agents (no credits consumed; rate limits apply — 30 req/min on the free plan, and the account still needs Search API access). Search results are keyword-ranked with structured titles/snippets; fetch renders JS-heavy pages and extracts PDFs.

| Variable | Default | Description |
|---|---|---|
| `TINYFISH_API_KEY` | unset | Enables TinyFish in the source chain (supplemental sources + generic fetch). One key serves both endpoints. |
| `TINYFISH_SEARCH_API_URL` | `https://api.search.tinyfish.ai` | Search endpoint (GET). |
| `TINYFISH_FETCH_API_URL` | `https://api.fetch.tinyfish.ai` | Fetch endpoint (POST). |
| `TINYFISH_ENABLED` | `true` | Optional override. Set to `false` to disable TinyFish even if a key is configured. |

Domain filters map to TinyFish's dedicated `include_domains` / `exclude_domains` parameters (the `site:` / `-site:` query operators are deprecated upstream for domain filtering because they collide with other query syntax); `recency_days` maps to `recency_minutes`.

## Exa

Semantic (embeddings-first) search with native `includeDomains` / `excludeDomains` / published-date filtering — strong on descriptive queries, papers, and official-domain discovery. Paid per request.

| Variable | Default | Description |
|---|---|---|
| `EXA_API_KEY` | unset | Enables Exa in the source chain (supplemental sources + `/contents` fetch). |
| `EXA_API_URL` | `https://api.exa.ai` | Exa API base URL. |
| `EXA_ENABLED` | `true` | Optional override. Set to `false` to disable Exa even if a key is configured. |

## DuckDuckGo & Bing (keyless fallback engines)

Two key-free scrapers back the source chain when no paid key is present. Both are
**enabled by default** and need neither a key nor a signup, so `web_search`
still surfaces external sources on a zero-key install. They are **search-only**:
`web_fetch`/`web_map` do not route arbitrary URLs through them (that stays with
the fetch-capable providers).

| Variable | Default | Description |
|---|---|---|
| `DUCKDUCKGO_ENABLED` | `true` | Scrape DuckDuckGo's HTML endpoint (falling back to the Lite endpoint) as a chain provider. |
| `DUCKDUCKGO_REGION` | unset | Optional `kl` region/ad unit passed to DuckDuckGo (e.g. `us-en`, `cn-zh`, `wt-wt`). |
| `BING_ENABLED` | `true` | Scrape `www.bing.com/search` as a chain provider. |
| `BING_MARKET` | `en-US` | Optional Bing `mkt` market override (e.g. `zh-CN`). |

Both honor domain/recency filters by rewriting the query or adding parameters;
DuckDuckGo supports time filtering via its `df` parameter and Bing via `qft`
recency intervals. Bing additionally runs a relevance guard that discards
results whose title+snippet share no meaningful token with the query (this
defeats the case where Bing returns its cached "no results" SERP and a stale
page sneaks through).

## Keyless (anonymous) modes for paid providers

Tavily, Firecrawl, and Exa each expose a free keyless tier. Setting the matching
flag instantiates the provider **even with no key**:

| Variable | Default | Description |
|---|---|---|
| `TAVILY_KEYLESS` | `false` | Use Tavily's anonymous mode (`x-tavily-access-mode: keyless` header). Rate-limited and may lack premium sources. |
| `FIRECRAWL_KEYLESS` | `false` | Call Firecrawl's hosted `/v2` endpoints with no `Authorization` header. |
| `EXA_KEYLESS` | `false` | Use Exa's hosted public MCP `web_search_exa` tool (search-only; `web_fetch` and domain/recency filters are unavailable in this mode). |

A configured key always wins over the keyless flag: set both and requests use
the key. Exa's keyless search takes only `query` + `numResults`, so a filtered
request (`include_domains`, `exclude_domains`, or `recency_days`) is skipped and
the chain falls through to the next provider.

## Source chain

Supplemental sources and generic (non-specialist) fetch walk an ordered provider chain; the first provider with usable output wins and later ones are pure fallback. Providers that cannot honor domain/recency filters (Firecrawl, and Exa in keyless mode) are skipped for filtered requests. The whole chain shares one request deadline (`GROK_SEARCH_TIMEOUT_SECONDS`) — a slow provider cannot multiply the budget by the chain length.

Set `GROK_SEARCH_PARALLEL_SOURCES=true` to fan out instead: every configured provider is consulted concurrently under the same deadline and their results are merged (deduped by URL, earlier chain positions win) and returned in full — there is no `extra_sources`/`fallback_sources` count cap, only identical-URL dedupe. Per-source `provider` labels stay at each provider's native name (no `_enrichment`/`_fallback` suffix), and the query-result cache is bypassed in this mode.

`web_map` is a separate capability, not part of this chain: it always uses Tavily whenever `TAVILY_API_KEY` is configured, even when the chain excludes Tavily.

**The chain and the specialist extractors are different things.** A *source provider* (Tavily, Exa, TinyFish, DuckDuckGo, Bing, Firecrawl) is an external service — the first four are normally gated behind an API key, the last two are keyless scrapers. A *specialist extractor* (GitHub, StackExchange, arXiv, Wikipedia) is a key-free parser for one family of URLs; it is never configured and never part of the chain. So with **no source provider configured at all**, `web_fetch` still handles those four families, and fails on every ordinary URL — there is nothing left that can retrieve one. Inline enrichment in `web_search` behaves the same way and says so by name.

| Variable | Default | Description |
|---|---|---|
| `GROK_SEARCH_SOURCE_PROVIDERS` | unset | Comma-separated explicit chain order, e.g. `tinyfish,tavily,firecrawl` (valid names: `tavily`, `exa`, `tinyfish`, `duckduckgo`, `bing`, `firecrawl`). Unset = configured providers in canonical order `tavily, exa, tinyfish, duckduckgo, bing, firecrawl`. Unknown names fail at startup. |
| `GROK_SEARCH_PARALLEL_SOURCES` | `false` | `true` = fan out to every configured provider concurrently and merge + dedupe their results, instead of the sequential first-wins chain. |

## Cache

Bounded session + query-result caching. The session cache (`get_sources`) keeps full content; the query-result cache deduplicates repeat provider searches so keyless engines (which rate-limit) return cached hits instead of exhausting their budget.

| Variable | Default | Description |
|---|---|---|
| `GROK_SEARCH_CACHE_SIZE` | `256` | Maximum cached search sessions for `get_sources`. |
| `GROK_SEARCH_RESULT_CACHE_SIZE` | `50` | Maximum cached query→sources entries (LRU). |
| `GROK_SEARCH_RESULT_CACHE_TTL_SECONDS` | `300` | Per-entry age limit; `0` disables the query-result cache. The key is the normalized query + sorted include/exclude domains + recency + requested count. |
| `GROK_SEARCH_TIMEOUT_SECONDS` | `60` | HTTP timeout for Grok, Tavily, and Firecrawl requests. |
| `GROK_SEARCH_FETCH_MAX_CHARS` | unset | Default character cap on `web_fetch` content. Overridden per call by `max_chars`. Unset means no truncation. |

## Source extraction

Specialist `web_fetch` extractors (GitHub, StackExchange, arXiv, Wikipedia) and
`web_search` inline enrichment. The specialists call public APIs directly — no
Tavily/Firecrawl key required.

| Variable | Default | Description |
|---|---|---|
| `GITHUB_TOKEN` | unset | GitHub token for issue/PR/release fetches. Anonymous works but is capped at ~60 req/hr; a token raises the limit and allows private repos. |
| `GROK_SEARCH_SOURCE_MAX_ANSWERS` | `5` | StackExchange answers rendered before the "more answers" fold. |
| `GROK_SEARCH_SOURCE_MAX_COMMENTS` | `30` | GitHub / StackExchange comments rendered before folding. |
| `GROK_SEARCH_ENRICH_CONCURRENCY` | `3` | Parallel source enrichments when `web_search` is called with `include_content: true`. Clamped to `1..=5`. |
| `GROK_SEARCH_ENRICH_MAX_CHARS` | `15000` | Character cap per enriched source body. |

## Response budget

Caps the size of a single `web_search` response so large source sets cannot
blow past MCP client context limits. The session cache always keeps full
content; truncated sources carry a note pointing at `web_fetch(url)` /
`get_sources(session_id)` for recovery.

| Variable | Default | Description |
|---|---|---|
| `GROK_SEARCH_MAX_INLINE_SOURCES` | `5` | Maximum sources that carry inline `content` per `web_search` response; the rest return metadata only. |
| `GROK_SEARCH_RESPONSE_MAX_CHARS` | `60000` | Whole-response character budget (answer + per-source metadata and inline content). Over-budget responses truncate inline content tail-first, then drop trailing sources (always keeping at least one) and set `truncated: true`. |

## Outbound proxy (off by default)

Every upstream connects **directly by default**. Optionally route per category:

| Variable | Default | Description |
|---|---|---|
| `NOVA_PROXY_KEY` | unset | Proxy for the **keyed** providers (Tavily / Exa / TinyFish / Firecrawl). |
| `NOVA_PROXY_KEYLESS` | unset | Proxy for the **keyless** engines (DuckDuckGo / Bing) plus specialist extractors and generic fetch. |
| `NOVA_PROXY_GROK` | unset | Proxy for the **Grok** engine. Unset = Grok connects directly. |

Schemes: `http://`, `https://`, `socks5://`, `socks5h://`, optionally with
`user:pass@` credentials. An unparsable URL logs a warning and degrades to direct.

Any of the three accepts an `{account}` placeholder in the **username** to
derive one proxy sub-account per provider (parity with NovaVeil). The
placeholder is replaced with a deterministic 8-hex alias — `sha256("provider:key")`'s
first 8 hex chars, non-reversible — so each keyed provider (Tavily / Exa /
TinyFish / Firecrawl) lands on its own account from a single template, and Grok
resolves `{account}` from its own key, and the proxy vendor can track a key
without ever seeing it. Only the username is substituted; the password and the
rest of the URL are preserved:

```toml
proxy_key  = "socks5h://Default.{account}:123@resin:2260"
proxy_grok = "socks5://grok-only:1080"   # Grok has its own URL; unset = direct
```

A provider with no key (keyless mode) has nothing to bind an account to, so it
logs a warning and connects directly. A fixed URL with no placeholder behaves
exactly as before — all matching providers share it.

## Config file

Drop a TOML file at `<home>/.config/nova-veil-search/config.toml` (or any path pointed to by `GROK_SEARCH_CONFIG`) to set defaults once and skip the per-client `env` block. Process env still wins, so individual clients can override any field at runtime.

Resolved per platform:

- **macOS / Linux**: `$HOME/.config/nova-veil-search/config.toml` — e.g. `/Users/alice/.config/nova-veil-search/config.toml`.
- **Windows (PowerShell / cmd)**: `%USERPROFILE%\.config\nova-veil-search\config.toml` — e.g. `C:\Users\chen\.config\nova-veil-search\config.toml`.
- **Windows (Git Bash / MSYS)**: same as Unix — `$HOME/.config/nova-veil-search/config.toml`.

`nova-veil-search --init` picks the right path automatically; no platform-specific shell setup required.

**Parsing is strict: one unknown key voids the whole file**, so a typo surfaces as an error rather than silently applying half your settings. Because MCP clients swallow stderr, the `doctor` tool reports what became of the file — its resolved path plus `absent`, `loaded`, or `rejected` with the reason attached. If your settings all look like defaults, check that field first.

### Scaffolding the file — `--init`

```bash
nova-veil-search --init
```

This writes an annotated template at the resolved config path with **every key commented out**. The scaffold is identical in behavior to "no config file" until you uncomment lines, so it never silently changes runtime behavior. Re-running `--init` is a no-op when the file already exists; delete the file first to regenerate.

### Why two casings?

Env vars use `UPPER_CASE` because that is the Unix shell tradition (`PATH`, `HOME`, `LANG`, `AWS_REGION` …). TOML files use lowercase `snake_case` because that is the Rust ecosystem convention (`Cargo.toml`, `pyproject.toml`, Codex `~/.codex/config.toml`). `nova-veil-search` follows each convention in its native context. Mapping rule for the table below: drop the `GROK_SEARCH_` prefix where present, then lowercase the rest.

Unknown keys are rejected by the loader — typos surface as parse errors instead of silently dropping.

| TOML key | Env equivalent |
|---|---|
| `grok_api_url` | `GROK_SEARCH_URL` |
| `grok_api_key` | `GROK_SEARCH_API_KEY` |
| `grok_auth_mode` | `GROK_SEARCH_AUTH_MODE` |
| `grok_auth_file` | `GROK_SEARCH_AUTH_FILE` |
| `grok_model` | `GROK_SEARCH_MODEL` |
| `web_search_enabled` | `GROK_SEARCH_WEB_SEARCH` |
| `x_search_enabled` | `GROK_SEARCH_X_SEARCH` |
| `tavily_api_url` | `TAVILY_API_URL` |
| `tavily_api_key` | `TAVILY_API_KEY` |
| `tavily_enabled` | `TAVILY_ENABLED` |
| `tavily_keyless` | `TAVILY_KEYLESS` |
| `firecrawl_api_url` | `FIRECRAWL_API_URL` |
| `firecrawl_api_key` | `FIRECRAWL_API_KEY` |
| `firecrawl_enabled` | `FIRECRAWL_ENABLED` |
| `firecrawl_keyless` | `FIRECRAWL_KEYLESS` |
| `tinyfish_search_api_url` | `TINYFISH_SEARCH_API_URL` |
| `tinyfish_fetch_api_url` | `TINYFISH_FETCH_API_URL` |
| `tinyfish_api_key` | `TINYFISH_API_KEY` |
| `tinyfish_enabled` | `TINYFISH_ENABLED` |
| `exa_api_url` | `EXA_API_URL` |
| `exa_api_key` | `EXA_API_KEY` |
| `exa_enabled` | `EXA_ENABLED` |
| `exa_keyless` | `EXA_KEYLESS` |
| `duckduckgo_enabled` | `DUCKDUCKGO_ENABLED` |
| `duckduckgo_region` | `DUCKDUCKGO_REGION` |
| `bing_enabled` | `BING_ENABLED` |
| `bing_market` | `BING_MARKET` |
| `proxy_key` | `NOVA_PROXY_KEY` |
| `proxy_keyless` | `NOVA_PROXY_KEYLESS` |
| `proxy_grok` | `NOVA_PROXY_GROK` |
| `source_providers` | `GROK_SEARCH_SOURCE_PROVIDERS` |
| `default_extra_sources` | `GROK_SEARCH_EXTRA_SOURCES` |
| `fallback_sources` | `GROK_SEARCH_FALLBACK_SOURCES` |
| `fetch_max_chars` | `GROK_SEARCH_FETCH_MAX_CHARS` |
| `cache_size` | `GROK_SEARCH_CACHE_SIZE` |
| `result_cache_size` | `GROK_SEARCH_RESULT_CACHE_SIZE` |
| `result_cache_ttl_seconds` | `GROK_SEARCH_RESULT_CACHE_TTL_SECONDS` |
| `timeout_seconds` | `GROK_SEARCH_TIMEOUT_SECONDS` |
| `github_token` | `GITHUB_TOKEN` |
| `source_max_answers` | `GROK_SEARCH_SOURCE_MAX_ANSWERS` |
| `source_max_comments` | `GROK_SEARCH_SOURCE_MAX_COMMENTS` |
| `enrich_concurrency` | `GROK_SEARCH_ENRICH_CONCURRENCY` |
| `enrich_max_chars` | `GROK_SEARCH_ENRICH_MAX_CHARS` |
| `max_inline_sources` | `GROK_SEARCH_MAX_INLINE_SOURCES` |
| `response_max_chars` | `GROK_SEARCH_RESPONSE_MAX_CHARS` |

Example — minimum useful file:

```toml
grok_api_key   = "xai-..."
tavily_api_key = "tvly-..."
grok_model     = "grok-4-1-fast-reasoning"
```

Example — OAuth mode:

```toml
grok_auth_mode = "oauth"
grok_model     = "grok-4.3"
```

Example — full reference:

```toml
grok_api_url          = "https://api.x.ai"
grok_api_key          = "xai-..."
grok_auth_mode        = "api_key"
# grok_auth_file      = "C:\\Users\\chen\\.config\\nova-veil-search\\auth.json"
grok_model            = "grok-4-1-fast-reasoning"
web_search_enabled    = true
x_search_enabled      = false
tavily_api_url        = "https://api.tavily.com"
tavily_api_key        = "tvly-..."
tavily_enabled        = true
firecrawl_api_url     = "https://api.firecrawl.dev"
firecrawl_api_key     = "fc-..."
firecrawl_enabled     = true
# firecrawl_keyless   = false
# exa_keyless         = false
# duckduckgo_enabled  = true
# duckduckgo_region   = "us-en"
# bing_enabled        = true
# bing_market         = "en-US"
# proxy_key           = "socks5://proxy-a:1080"   # keyed providers (Tavily/Exa/TinyFish/Firecrawl)
#                       # or a per-key template: "socks5h://Default.{account}:123@resin:2260"
# proxy_keyless       = "socks5://proxy-b:1081"   # keyless engines + specialists
# proxy_grok          = "socks5://proxy-c:1082"   # Grok engine (unset = direct)
default_extra_sources = 3
fallback_sources      = 5
fetch_max_chars       = 200000
cache_size            = 256
result_cache_size     = 50
result_cache_ttl_seconds = 300
timeout_seconds       = 60
```
