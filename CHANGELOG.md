# Changelog

All notable changes to NovaVeilSearch are documented here.

## Unreleased

- **Multi-key rotation for every keyed provider** — `FIRECRAWL_API_KEY`, `TINYFISH_API_KEY`, and `EXA_API_KEY` now accept comma-separated key lists (`key1,key2,…`) and rotate round-robin per request with automatic failover on key-scoped errors (401/403/429/432/433), exactly like `TAVILY_API_KEY`. The rotation loop is shared (`rotate_keys`), so all four providers agree on which failures indict a key; each rotating request also egresses through the `{account}` proxy account of whichever key signed it.
- **Per-key `{account}` proxy accounts** — the `{account}` placeholder in `NOVA_PROXY_KEY` is now resolved from every key of a provider's comma-separated key list, not once per provider: Tavily's round-robin rotation egresses through the proxy account of whichever key signed the request. One startup summary line per configured proxy names the endpoint (credentials stripped) and each provider's alias.
- **Keyless proxy placeholder guard** — an `{account}` in `NOVA_PROXY_KEYLESS` used to be sent to the proxy verbatim as the username; it is now refused with a startup note and the keyless engines degrade to direct (they have no key to bind an account to).
- **HTTP transport honors config.toml proxies** — with `NOVA_CONFIG_UI=true`, the shared clients (and their `{account}` overrides) are built from the same merged config (env > config.toml) the requests use, so keys/proxies living only in `config.toml` no longer silently bypass the proxy.
- **Keyless search engines** — DuckDuckGo (`DUCKDUCKGO_ENABLED`, `DUCKDUCKGO_REGION`) and Bing (`BING_ENABLED`, `BING_MARKET`) are now source-chain providers, enabled by default, that need no API key. Search-only: they honor domain/recency filters but are excluded from `web_fetch`/`web_map`.
- **Anonymous (keyless) modes** for paid providers — `TAVILY_KEYLESS`, `FIRECRAWL_KEYLESS`, and `EXA_KEYLESS` instantiate the provider without a key on its free tier (Tavily via the `x-tavily-access-mode: keyless` header, Firecrawl via unauthenticated `/v2`, Exa via its public MCP `web_search_exa`).
- **Query-result cache** — `GROK_SEARCH_RESULT_CACHE_SIZE` / `GROK_SEARCH_RESULT_CACHE_TTL_SECONDS` deduplicate repeat provider searches to spare rate-limited keyless engines; `0` TTL disables the cache.
- **`doctor`** now reports DuckDuckGo and Bing status (enabled, region/market) alongside the existing providers.

## 0.1.26

- Initial NovaVeilSearch release.