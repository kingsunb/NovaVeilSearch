# Changelog

All notable changes to NovaVeilSearch are documented here.

## Unreleased

- **Keyless search engines** — DuckDuckGo (`DUCKDUCKGO_ENABLED`, `DUCKDUCKGO_REGION`) and Bing (`BING_ENABLED`, `BING_MARKET`) are now source-chain providers, enabled by default, that need no API key. Search-only: they honor domain/recency filters but are excluded from `web_fetch`/`web_map`.
- **Anonymous (keyless) modes** for paid providers — `TAVILY_KEYLESS`, `FIRECRAWL_KEYLESS`, and `EXA_KEYLESS` instantiate the provider without a key on its free tier (Tavily via the `x-tavily-access-mode: keyless` header, Firecrawl via unauthenticated `/v2`, Exa via its public MCP `web_search_exa`).
- **Query-result cache** — `GROK_SEARCH_RESULT_CACHE_SIZE` / `GROK_SEARCH_RESULT_CACHE_TTL_SECONDS` deduplicate repeat provider searches to spare rate-limited keyless engines; `0` TTL disables it.
- **`doctor`** now reports DuckDuckGo and Bing status (enabled, region/market) alongside the existing providers.

## 0.1.26

- Initial NovaVeilSearch release.