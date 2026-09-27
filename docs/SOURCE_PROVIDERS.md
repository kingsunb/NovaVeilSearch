# Source Providers（搜索源引擎）

NovaVeilSearch 的「补充来源 + 失败兜底」是一条有序 provider 链。本文记录**已接入的引擎**，以及**评估过、但有意不接入**的引擎及其原因，作为后续决策的基线。链顺序、env 变量与运行时行为见 [CONFIGURATION.md](./CONFIGURATION.md) 与 [ARCHITECTURE.md](./ARCHITECTURE.md)。

## 已接入

| 引擎 | 类型 | 说明 |
|---|---|---|
| Tavily | keyed（另有 `TAVILY_KEYLESS` 免 key 档） | RAG 搜索 / 正文提取 / 站点地图 |
| Exa | keyed（另有 `EXA_KEYLESS` 免 key 档） | 语义搜索 + 原生域/日期过滤 |
| TinyFish | keyed | 免费关键词搜索 + JS 渲染抓取 |
| Serper | keyed | Google SERP 搜索（免费档 ~2500 次/月），search-only，域/日期过滤转 `site:`/`tbs:qdr:` |
| DuckDuckGo | keyless 抓取 | 免 key，search-only |
| Bing | keyless 抓取 | 免 key，search-only |
| Firecrawl | keyed（另有 `FIRECRAWL_KEYLESS` 免 key 档） | 兜底抓取 / 搜索 |

---

## 评估过、决定不接入

### Brave Search — 不接入

Brave Search 拥有独立网页索引与原生时效过滤，本轮在对比参考项目（search-boost 引擎池）时被评估为潜在搜索渠道。

**不接入原因：**

- 官方 Brave Search API 为订阅制，**注册需要绑定信用卡/付费**，没有可持续的免费档；这与项目「零成本免 key 引擎 + 免费档优先」的定位冲突。
- 官方 API **没有 keyless / 匿名端点**，因此无法做成 `BRAVE_KEYLESS` 这类免 key 模式。
- 网页抓取（仿 DuckDuckGo/Bing 抓 `search.brave.com`）非官方、易被反爬、结果易碎，不符合「可维护 provider」的标准。

**结论：** 不加入 MCP 源链；`GROK_SEARCH_SOURCE_PROVIDERS` 不接受 `brave`，也不新增 `BRAVE_API_KEY` 等配置项。

> 对比：**Serper** 同样需要 API key，但它有真实的免费档（约 2500 次/月）、注册无需信用卡/订阅，且返回结构化 JSON，因此被纳入「已接入」而非像 Brave 一样被拒。

### 其它候选（仅记录，未决定）

- **Yahoo** — 免 key，但 Nova 已有 DuckDuckGo + Bing 两家免 key 引擎，边际收益低。
- **AnySearch** — 自带免 key 匿名档 + 带 key 档；未决定是否接入。
- **X/Twitter 免登录通道** — 现有 `x_search` 只走 Grok 原生工具；独立免 key 抓取通道成本高、易碎，未决定。