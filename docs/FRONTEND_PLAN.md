# Frontend for Configuring Search Sources — Implementation Plan

This document is a complete, unambiguous implementation plan for adding a web
frontend that configures **search sources** (per-source on/off toggles and API
key add/edit), plus authenticated HTTP endpoints that read/write `config.toml`.
It is written so an implementer with no other context can execute it directly.

All facts below were verified against the current tree (`src/http.rs`,
`src/config.rs`, `src/service.rs`, `src/main.rs`, `Cargo.toml`, `Caddyfile`).

---

## 0. Key decisions (read this first)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | Serve one **embedded SPA** at `GET /` via `include_str!("../web/index.html")`. Login is **inline in the SPA** — no separate `GET /login` route. | Single asset, no `tower-http`, no build step. A separate `GET /login` page adds a second static asset and a redirect/flow for zero benefit; the SPA already toggles login/settings from one file using `sessionStorage`. |
| D2 | Config is written with **`toml_edit`** (new dependency), not `toml::Table`, so comments, key order, and every key we don't edit survive. | `config.toml` is a hand-edited, comment-heavy file (`--init` emits an all-comment template). `toml::Table` re-serialization would strip comments and reorder keys. `toml_edit`'s `DocumentMut` preserves both. |
| D3 | Editable scope is exactly the **4 search sources** (`tavily`, `exa`, `tinyfish`, `firecrawl`) — each `enabled` flag + `api_key` — plus the `source_providers` chain order. Grok/OAuth/OpenAI-compatible/GitHub keys are **out of scope** and remain env/file managed. | Matches the goal ("search sources") and the editor's 4-source allowed-name set (`config::ALLOWED_SOURCE_NAMES`); the chain editor may still name the keyless engines (`duckduckgo`, `bing`), which `validate_source_providers` accepts but which have no key toggles. |
| D4 | Every `/api/*` endpoint reuses the existing `authorize(&headers, &state)` (master token OR login session token). `GET /` (HTML) is unauthenticated. | Reuse, no new crypto. The HTML contains no secrets. |
| D5 | Secrets are **never returned**. Key values are reported only as `"set"` / `"unset"`. | Absolute requirement; the existing `redact()`/masking discipline is extended to the API. |
| D6 | `PUT /api/config` is a **partial merge** with explicit sentinels: omitted/`null` = unchanged, `""` = clear a key, non-empty string = set. | Matches the task's "merge+atomic-write" and lets the SPA send only changed fields (or all fields with nulls). |
| D7 | A small **required companion change** makes HTTP requests honor `config.toml`: `request_config()` switches from `Config::from_env_map` to `Config::load_from` (env > file > defaults). Without it, the persisted file has **no effect** on the running server (see §3.5). | Today the HTTP path builds config from **env only**, so writing `config.toml` would be a silent no-op. `load_from` is the existing, already-tested precedence pipeline the CLI uses. |
| D8 | No `GET /api/health` endpoint. | Not part of the goal; `/api/config` (200 after auth) and `GET /` (200) already signal liveness without adding unauthenticated surface. The existing `doctor` MCP tool remains reachable via `/mcp`. |

---

## 1. Files to create / modify

New:

1. **`src/web.rs`** — HTTP handlers for the SPA and config API:
   `serve_index` (GET `/`), `get_config` (GET `/api/config`), `put_config`
   (PUT `/api/config`). Gated `#[cfg(feature = "http")]` at the top of the file.
2. **`web/index.html`** — the single-file SPA (HTML + inline CSS + inline JS).
   Embedded via `include_str!("../web/index.html")`.
3. **`docs/FRONTEND_PLAN.md`** — this document.

Modified:

4. **`src/http.rs`** —
   - widen visibility of the pieces `web.rs` needs (see §7);
   - register the three new routes;
   - switch `request_config` to `Config::load_from` (D7);
   - update the module doc comment to reflect that server-held keys may now
     come from `config.toml` (env still wins).
5. **`src/config.rs`** —
   - add `SourceView`, `SourcesView`, `SourceEdits`, `FieldError`, `KeyStatus`;
   - add `load_source_config`, `validate_edits`, `write_source_config`, plus
     private helpers (`env_override_map`, `apply_edits`, `atomic_write`,
     `normalize_source_provider`).
6. **`Cargo.toml`** — add `toml_edit` (see §1.1).
7. **`src/lib.rs`** — add `#[cfg(feature = "http")] pub mod web;`.

No change to `src/main.rs`, `Caddyfile`, or the stdio transport.

### 1.1 Cargo.toml change

Add to `[dependencies]`:

```toml
toml_edit = "0.22"
```

- Keep the existing `toml = { version = "0.8", default-features = false, features = ["parse"] }` **unchanged**; it continues to power reads (`toml::from_str::<ConfigFile>`).
- No `toml` feature flags need adding because we never serialize through the
  `toml` crate — `toml_edit` owns serialization.
- `toml_edit 0.22` MSRV (≈1.74) is below the crate's `rust-version = "1.78"`. Do
  not bump to a newer `toml_edit` minor whose MSRV exceeds 1.78.

---

## 2. HTTP API contract

All JSON bodies/`Content-Type` values are exact. `Authorization: Bearer <token>`
accepts **either** the master token (`GROK_MCP_API_TOKEN`) **or** a session
token issued by `POST /login`.

### 2.1 `GET /` — the SPA (unauthenticated)

- **Method/path:** `GET /`
- **Auth:** none.
- **Response:** `200`, `Content-Type: text/html; charset=utf-8`, body is the
  literal contents of `web/index.html` (`include_str!`).
- **Purpose:** serves the login/settings single-page app. The app decides which
  view to show based on the presence of a token in `sessionStorage`.

### 2.2 `POST /login` — unchanged (existing)

- **Method/path:** `POST /login`
- **Request body:** `{"username": "admin", "password": "hunter2"}`
- **Response `200`:** `{"token": "<uuid-v4>", "expires_in_seconds": 43200}`
- **Errors (existing, unchanged):** `401` on bad credentials (with 300 ms
  throttle), `404` when `NOVA_ADMIN_PASSWORD` is unset, `413` when body >
  4 KiB.
- **No code change** to this handler.

### 2.3 `GET /api/config` — read current source config (authenticated)

- **Method/path:** `GET /api/config`
- **Auth:** required (`authorize()`). Failure → `401` + `WWW-Authenticate: Bearer`.
- **Origin:** when `GROK_MCP_ALLOWED_ORIGINS` is set and an `Origin` header is
  present but not allowlisted → `403` (see §5).
- **Response `200`** (`Content-Type: application/json`):

```json
{
  "source_providers": ["tavily", "exa", "tinyfish", "firecrawl"],
  "sources": {
    "tavily":    { "enabled": true,  "api_key": "set" },
    "firecrawl": { "enabled": true,  "api_key": "unset" },
    "tinyfish":  { "enabled": true,  "api_key": "unset" },
    "exa":       { "enabled": true,  "api_key": "set" }
  },
  "env_overrides": {
    "tavily_enabled": false,
    "tavily_api_key": true,
    "firecrawl_enabled": false,
    "firecrawl_api_key": false,
    "tinyfish_enabled": false,
    "tinyfish_api_key": false,
    "exa_enabled": false,
    "exa_api_key": true,
    "source_providers": false
  },
  "config_file": "config.toml",
  "config_file_state": "loaded"
}
```

Field semantics:

- `source_providers` — effective explicit chain order (may be `[]`, meaning
  "built-in canonical order"). This is the "source_order".
- `sources.<name>` — effective view for each of the four sources:
  - `enabled` — effective bool (env > file > default).
  - `api_key` — `"set"` or `"unset"` (presence only; the value is **never**
    emitted, not even a fragment).
- `env_overrides.<toml_field>` — `true` when the corresponding env var is
  present in the operator environment, i.e. the value is **live-overridden by
  env** and the UI must render that field read-only (an edit would not take
  effect). Keyed by TOML field name; see the mapping table in §3.1.
- `config_file` — redacted basename of the resolved path
  (`redact_path`), e.g. `"config.toml"`, or `null` when no path resolves.
- `config_file_state` — `"absent"` | `"loaded"` | `"rejected"`.

### 2.4 `PUT /api/config` — merge + atomic write (authenticated)

- **Method/path:** `PUT /api/config`
- **Auth:** required (`authorize()`). `401` / origin `403` identical to GET.
- **Body cap:** 16 KiB (`413` beyond that).
- **Request body:** a **partial** object; only present fields are applied.

```json
{
  "tavily_enabled": false,
  "tavily_api_key": "tvly-new-key",
  "firecrawl_enabled": true,
  "firecrawl_api_key": null,
  "tinyfish_enabled": true,
  "tinyfish_api_key": "",
  "exa_enabled": true,
  "exa_api_key": null,
  "source_providers": ["tavily", "exa", "firecrawl"]
}
```

Field semantics (exact):

| Field | `null` / omitted | `"some"` | `true` / `false` | `""` (or whitespace-only) | `[]` |
|---|---|---|---|---|---|
| `*_enabled` | unchanged | — (must be bool) | set bool | — (must be bool) | — |
| `*_api_key` | unchanged | set (trimmed) | — (must be string/null) | **clear** (remove key) | — |
| `source_providers` | unchanged | — (must be array) | — | — | clear (built-in order) |

- **Normalization before write:** `*_api_key` values are `.trim()`ed;
  `source_providers` entries are `.trim()`ed, lowercased, and de-duplicated
  (first occurrence wins).
- **Response `200`:** the same shape as `GET /api/config` (§2.3), re-read
  **after** the write (reflects env > file > defaults, so an env-overridden
  field still shows the env value).
- **Response `400`** on validation failure (no write performed):

```json
{
  "errors": [
    { "field": "source_providers", "message": "unknown source provider \"searxng\" (valid: tavily, exa, tinyfish, duckduckgo, bing, firecrawl)" }
  ]
}
```

- **Response `400`** on body deserialization failure (non-object, non-bool
  `*_enabled`, non-string `*_api_key`, or an unknown top-level key):

```json
{ "errors": [ { "field": "", "message": "invalid request body: <serde message>" } ] }
```

- **Response `500`** on I/O failure:

```json
{ "error": "failed to write config: <io error message>" }
```

Validation rules (exact, in order):

1. Body must deserialize to `SourceEdits` (`deny_unknown_fields`), else `400`.
2. Each `*_enabled` must be a JSON boolean (serde enforces).
3. Each `*_api_key` must be a JSON string or `null`; after trimming it must be
   ≤ **4096** chars and contain **no control characters** (`char::is_control`),
   else `FieldError` for that `*_api_key`.
4. Each `source_providers` element (after trim + lowercase) must be one of
   `tavily`, `exa`, `tinyfish`, `firecrawl`; unknown → `FieldError` on
   `source_providers`. Duplicates are **not** an error (deduped).

---

## 3. `config.toml` read/write design

### 3.1 Field mapping (TOML key ↔ env var)

| TOML key (written / `env_overrides` key) | Env var (override detector) | Type |
|---|---|---|
| `tavily_enabled` | `TAVILY_ENABLED` | bool |
| `tavily_api_key` | `TAVILY_API_KEY` | string |
| `firecrawl_enabled` | `FIRECRAWL_ENABLED` | bool |
| `firecrawl_api_key` | `FIRECRAWL_API_KEY` | string |
| `tinyfish_enabled` | `TINYFISH_ENABLED` | bool |
| `tinyfish_api_key` | `TINYFISH_API_KEY` | string |
| `exa_enabled` | `EXA_ENABLED` | bool |
| `exa_api_key` | `EXA_API_KEY` | string |
| `source_providers` | `GROK_SEARCH_SOURCE_PROVIDERS` | array<string> |

These TOML keys already round-trip through the private `ConfigFile::into_env_map`
(see `src/config.rs`), so `source_providers = ["tavily","exa"]` in the file
deserializes correctly and is re-read by `Config::load_from`.

### 3.2 New public API in `src/config.rs` (additive; not feature-gated)

```rust
/// Two-state presence marker for a secret. Serialized as "set" / "unset".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyStatus { Set, Unset }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceView {
    pub enabled: bool,
    pub api_key: KeyStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourcesView {
    pub source_providers: Vec<String>,
    pub sources: std::collections::BTreeMap<String, SourceView>, // keys: tavily/exa/tinyfish/firecrawl
    pub env_overrides: std::collections::BTreeMap<String, bool>, // TOML field -> overridden-by-env
    pub config_file: Option<String>,   // redacted basename (redact_path) or None
    pub config_file_state: String,     // "absent" | "loaded" | "rejected"
}

/// Editable subset accepted by PUT /api/config. None = "unchanged";
/// Some("") on an api_key = "clear".
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SourceEdits {
    pub tavily_enabled: Option<bool>,
    pub tavily_api_key: Option<String>,
    pub firecrawl_enabled: Option<bool>,
    pub firecrawl_api_key: Option<String>,
    pub tinyfish_enabled: Option<bool>,
    pub tinyfish_api_key: Option<String>,
    pub exa_enabled: Option<bool>,
    pub exa_api_key: Option<String>,
    pub source_providers: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError { pub field: String, pub message: String }

/// Effective, masked view of what the server currently uses (env > file > defaults).
pub fn load_source_config(env: &HashMap<String, String>) -> SourcesView;

/// Returns a (possibly empty) list of structural/semantic errors for `edits`.
/// Empty list = safe to write. Never mutates anything.
pub fn validate_edits(edits: &SourceEdits) -> Vec<FieldError>;

/// Merge `edits` into the resolved config.toml (atomic temp+rename) and return
/// the re-read effective view. Errors are I/O-ish and map to HTTP 500.
pub fn write_source_config(
    env: &HashMap<String, String>,
    edits: &SourceEdits,
) -> anyhow::Result<SourcesView>;
```

Private helpers in the same module:

```rust
const ALLOWED_SOURCE_NAMES: [&str; 4] = ["tavily", "exa", "tinyfish", "firecrawl"];

/// TOML field -> its env var, used for env-override detection.
fn env_override_map(env: &HashMap<String, String>) -> std::collections::BTreeMap<String, bool>;
/// Apply only the present (Some) edit fields onto a toml_edit::DocumentMut.
fn apply_edits(doc: &mut toml_edit::DocumentMut, edits: &SourceEdits);
/// Normalize one source_providers entry: trim + lowercase.
fn normalize_source_provider(name: &str) -> String;
/// Temp file in the same dir + rename over target; creates parent dirs.
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()>;
```

### 3.3 `load_source_config` semantics

1. `let cfg = Config::load_from(env.clone());` — reuses the existing, tested
   env > file > defaults pipeline (and records `config_file_path` /
   `config_file_state`).
2. Build `sources` for the four canonical names, `enabled` from
   `cfg.{name}_enabled`, and `api_key` from `cfg.{name}_api_key.is_some()`
   mapped to `KeyStatus::{Set,Unset}` (match the loader's
   `filter(|v| !v.trim().is_empty())`: a blank key reads as `Unset`).
3. `source_providers = cfg.source_providers.clone()`.
4. `env_overrides = env_override_map(env)` (below).
5. `config_file = cfg.config_file_path.as_ref().map(|p| redact_path(&p.display().to_string()))`;
   `config_file_state = cfg.config_file_state.as_str().to_string()`.

**Env-override detection (`env_override_map`):** purely `env.contains_key(env_var)`
— presence, not value. This is correct because precedence is *presence-based*:
once `TAVILY_API_KEY` exists in the environment (even empty, which the loader
then filters to "unset"), the file's `tavily_api_key` is ignored. So a present
env var always means "the file value cannot win", and the UI marks it read-only.

### 3.4 `write_source_config` semantics (merge + atomic write)

```text
1. path = resolve_config_path(env)?           // server-resolved ONLY (§5)
   if None -> Err("cannot resolve config path (set GROK_SEARCH_CONFIG or HOME)")
2. if path.exists():
       body = fs::read_to_string(path)?
       doc  = body.parse::<toml_edit::DocumentMut>()
              .map_err(|e| anyhow!("refusing to overwrite unparseable {path}: {e}"))?
   else:
       doc  = toml_edit::DocumentMut::new()   // missing file will be created
3. apply_edits(&mut doc, edits)               // only present fields; see below
4. atomic_write(&path, &doc.to_string())?     // temp+rename, creates parent dirs
5. return load_source_config(env)             // re-read effective view
```

`apply_edits` (only when the `Option` is `Some`):

- `*_enabled`: `doc["tavily_enabled"] = toml_edit::value(v);` (same for the
  other three).
- `*_api_key`:
  - `Some(s)` with `s.trim()` non-empty → `doc["tavily_api_key"] = toml_edit::value(s.trim());`
  - `Some(s)` with `s.trim()` empty → `doc.remove("tavily_api_key");`
  - `None` → leave untouched.
- `source_providers`:
  - `Some(list)` → normalize each (trim + lowercase) and de-duplicate
    first-wins, then:
    ```rust
    let arr = toml_edit::Array::from_iter(
        normalized.iter().map(|s| toml_edit::Value::from(s.as_str())));
    doc["source_providers"] = toml_edit::Item::Value(toml_edit::Value::Array(arr));
    ```
    An empty normalized list writes an empty array (explicit "built-in order").
  - `None` → leave untouched.

**Preservation guarantee:** `toml_edit::DocumentMut` preserves comments, key
order, and every key the writer does not touch — including keys outside the nine
editable ones (e.g. `grok_api_key`, `cache_size`, `web_search_enabled`) and any
unrecognized key. The writer only inserts/replaces/removes the nine keys above.

**Merge into an existing file vs. `toml` read strictness:** the read path
(`read_config_file` → `toml::from_str::<ConfigFile>`) keeps its existing
`deny_unknown_fields` behavior, so a file containing an *unknown* key is still
`Rejected` by the loader (pre-existing CLI behavior, unchanged). The frontend
writes only the nine known keys, so it can never introduce that condition, and
it will not delete a pre-existing unknown key from disk.

**Atomic write (`atomic_write`):**

```rust
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    let file_name = path.file_name()
        .and_then(|n| n.to_str()).unwrap_or("config.toml");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, contents)?;
    let file = std::fs::File::open(&tmp)?;
    let _ = file.sync_all(); // best-effort fsync before rename
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => { let _ = std::fs::remove_file(&tmp); Err(e) }
    }
}
```

- Temp file is in the **same directory** as the target, so `rename` is atomic
  on the same filesystem.
- `uuid` is already a runtime dependency (reused for the temp suffix).
- **Missing file:** parent dirs are created and the file is created containing
  only the edited keys (no template comments). `nova-veil-search --init` remains
  the way to scaffold a full commented template; document this in the UI copy.
- **Windows note:** `std::fs::rename` does not replace an existing destination
  on Windows. The production deploy is Linux/Docker (§Caddyfile), where rename
  is atomic-overwrite. If Windows support is required later, fall back to
  `remove_file(dest)` then `rename` (non-atomic). Not required for this feature.

### 3.5 Runtime effect of edits (required companion change — D7)

**Finding:** the HTTP server never reads `config.toml` today. `run_http` builds
its startup operator config with `Config::from_env_map` (env only), and the
per-request `request_config()` (§`src/http.rs`) also uses
`Config::from_env_map`, so persisted `config.toml` values never reach `/mcp`.

**Required change** (one line, in `src/http.rs`):

```rust
fn request_config(base_env: &HashMap<String, String>) -> Config {
    // was: Config::from_env_map(base_env.clone())
    Config::load_from(base_env.clone())   // env > file > defaults
}
```

Effects and caveats (all acceptable / desired):

- Source edits take effect on the **next MCP request** without a restart.
- `state.base_env` (via `request_base_env`) retains `HOME`, `USERPROFILE`, and
  `GROK_SEARCH_CONFIG`, so `load_from` resolves the same path the writer uses.
- Env still wins when present, matching the documented precedence.
- A malformed `config.toml` degrades gracefully (`config_file_state = Rejected`,
  env+defaults used) — same as the CLI, per-request.
- Cost: one small-file parse per request; acceptable behind the existing
  concurrency cap (`MAX_CONCURRENT_REQUESTS = 32`) and low admin-tool QPS.
  A file-mtime cache is possible later but out of scope.
- The startup `operator_cfg` may stay `from_env_map` (cache sizing/timeout are
  not exposed by this UI); switching it is optional and out of scope.
- Update the `http.rs` module doc comment: server-held keys now come from the
  server's environment **or `config.toml`** (environment still wins).

If the evaluator insists on strictly env-only server semantics, drop D7 and
document that edits persist to `config.toml` but only affect the stdio/CLI path
or a restart — this yields a UI that does not affect the live server, so D7 is
the recommended default.

---

## 4. Frontend SPA structure (`web/index.html`)

One self-contained file: `<style>` in `<head>`, `<script>` at end of `<body>`.
No framework, no build, no external requests.

### 4.1 DOM

```html
<body>
  <main id="view-login" hidden>
    <h1>Nova Veil Search — Settings</h1>
    <form id="login-form">
      <label>Username <input id="username" autocomplete="username"></label>
      <label>Password <input id="password" type="password" autocomplete="current-password"></label>
      <button type="submit">Log in</button>
      <p id="login-error" role="alert"></p>
    </form>
  </main>

  <main id="view-settings" hidden>
    <h1>Search sources</h1>
    <div id="sources"></div>            <!-- one card per source -->
    <h2>Source order</h2>
    <ol id="source-order"></ol>         <!-- ordered chain -->
    <div id="order-add"></div>          <!-- add-back buttons for omitted sources -->
    <button id="save">Save</button>
    <p id="save-status" role="status"></p>
    <button id="logout">Log out</button>
  </main>
</body>
```

Each source card (rendered by JS) contains:

- an `<input type="checkbox" class="enabled">` with a sibling label
  (`Tavily`, `Exa`, `TinyFish`, `Firecrawl`);
- an `<input type="password" class="key" placeholder="••••••••" autocomplete="off">`
  (placeholder only when the key is `set`; otherwise empty — the **value** is
  never populated with a real secret, since the API never returns one);
- a `<span class="status">` showing `set` / `unset` / `env` / `clear`;
- a small "Clear key" button per card.

### 4.2 State and data flow (exact JS functions)

```js
const API = { login: 'login', config: 'api/config' };   // relative paths (§4.4)
const TOKEN_KEY = 'nv_token';
const SOURCE_META = [
  { id: 'tavily',    label: 'Tavily' },
  { id: 'exa',       label: 'Exa' },
  { id: 'tinyfish',  label: 'TinyFish' },
  { id: 'firecrawl', label: 'Firecrawl' },
];                                   // fixed render order; driven by server names
let current = null;                  // last GET /api/config payload
```

Functions:

- **`bootstrap()`** — on `DOMContentLoaded`: if
  `sessionStorage.getItem(TOKEN_KEY)` is set → `loadConfig()`, else
  `showLogin()`.
- **`token()`** — returns `sessionStorage.getItem(TOKEN_KEY)` or `null`.
- **`showLogin()`** — hide `#view-settings`, show `#view-login`, clear
  `sessionStorage`, `current = null`.
- **`showSettings()`** — hide `#view-login`, show `#view-settings`.
- **`login(e)`** — form submit handler. `POST login` with
  `{"username": <username>, "password": <password>}` and
  `Content-Type: application/json`. On `200`, store
  `sessionStorage.setItem(TOKEN_KEY, data.token)`, call `loadConfig()`. On
  `401`/`404`, show an inline error; on `404` add "admin login is not enabled".
- **`loadConfig()`** — `GET api/config` with header
  `Authorization: Bearer ${token()}`. On `200`: `current = data`,
  `renderSettings(data)`, `showSettings()`. On `401`: `showLogin()`.
- **`renderSettings(data)`** —
  1. For each `SOURCE_META`, build a card; set the checkbox from
     `data.sources[id].enabled`; set key placeholder to `••••••••` if `set`,
     status text accordingly.
  2. Disable + label `env` the whole card when
     `data.env_overrides[field]` is true:
     - `data.env_overrides[`${id}_enabled`]` disables the checkbox.
     - `data.env_overrides[`${id}_api_key`]` disables the key input and "Clear".
  3. Render `#source-order` from `data.source_providers` (each row: name chip +
     `↑` `↓` `×` buttons). Disable all order controls when
     `data.env_overrides.source_providers`. Render `#order-add` buttons for any
     of the four sources not currently in the order.
- **`save(e)`** — build the PUT body and submit:
  ```js
  const body = {};
  for (const s of SOURCE_META) {
    const enabledField = `${s.id}_enabled`;
    if (!current.env_overrides[enabledField]) {
      body[enabledField] = /* checkbox.checked */;
    }
    const keyInput = /* that card's .key input */;
    const clearPressed = /* that card's clear button pressed flag */;
    if (!current.env_overrides[`${s.id}_api_key`]) {
      if (clearPressed) body[`${s.id}_api_key`] = "";          // clear
      else {
        const v = keyInput.value.trim();
        if (v) body[`${s.id}_api_key`] = v;                    // set
        // else omit -> unchanged (placeholder "••••" means "keep existing")
      }
    }
  }
  if (!current.env_overrides.source_providers) {
    body.source_providers = /* ordered names from #source-order */;
  }
  await fetch(API.config, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json',
               'Authorization': 'Bearer ' + token() },
    body: JSON.stringify(body),
  });
  ```
  - `200` → re-render from response, status "Saved".
  - `400` → list `data.errors[].message`, no navigation.
  - `401` → `showLogin()`.
- **`logout()`** — `sessionStorage.removeItem(TOKEN_KEY)`, `showLogin()`.

### 4.3 Login vs settings view logic

Single file owns both views. The only auth state is the token in
`sessionStorage` (memory-scoped to the browser tab; not a cookie). On any
`401` from `/api/*`, the SPA clears the token and returns to the login view.

### 4.4 URL resolution behind Caddy (critical)

Backend routes are `/`, `/login`, `/api/config`. Caddy's
`handle_path /nova-veil-search/*` strips the `/nova-veil-search` prefix before
proxying, so the SPA is reached at `https://<host>/nova-veil-search/`.

The JS **must use relative URLs** (`'login'` and `'api/config'`, no leading
slash) so they resolve under `/nova-veil-search/…` and proxy to the backend's
`/login` and `/api/config`. Visiting `https://<host>/nova-veil-search/` **with a
trailing slash** is required for relative resolution to stay under the prefix.
Do not use absolute `/login` or `/api/config` (Caddy only routes `/mcp*` and
`/nova-veil-search/*`).

---

## 5. Security specifics

1. **Auth on all `/api/*`:** every `get_config`/`put_config` handler calls
   `authorize(&headers, &state).await` first and returns
   `unauthorized_response()` (401 + `WWW-Authenticate: Bearer`) on false. The
   master token path is constant-time; the session path reuses the existing
   sliding-TTL store. **No new crypto is added.**
2. **Secrets never leave the server.** `SourcesView` emits only `"set"`/`"unset"`.
   Add a unit test asserting the serialized GET response contains no key value.
3. **Origin / DNS-rebinding check on `/api/*`:** replicate `mcp_post`'s existing
   check — when `state.allowed_origins` is `Some` and an `Origin` header is
   present but not in the set, return `403`. Extract the existing block from
   `mcp_post` into `pub(crate) fn origin_allowed(headers, &state.allowed_origins)`
   and call it from `mcp_post`, `get_config`, `put_config`. Absent `Origin`
   (curl, non-browser) remains allowed.
4. **CSRF posture:** no cookies are used (token lives in `sessionStorage` and is
   sent as an `Authorization` header), so a cross-site form cannot forge the
   bearer token; cross-origin JS cannot read our origin's `sessionStorage`. The
   origin check in (3) is the DNS-rebinding backstop. `GET /` (HTML) needs no
   auth/origin check (no secrets). `POST /login` is left exactly as-is.
5. **Path traversal:** the config path is resolved server-side by
   `resolve_config_path(env)` from `$GROK_SEARCH_CONFIG`/`$HOME` only. Client
   input never influences the path — the API exposes no path parameter — so
   there is no traversal vector.
6. **Input validation:** unknown PUT fields are rejected
   (`deny_unknown_fields`); booleans/string/null types are enforced by serde;
   `source_providers` values are allow-listed; key strings are length-capped
   (4096) and control-character-free; body capped at 16 KiB.
7. **Fail-safe write:** an unparseable existing file is **never overwritten**
   (returns an error); the write is atomic (temp + rename) so a crash cannot
   leave a truncated `config.toml`.

---

## 6. Test plan

### 6.1 Unit tests (in `src/config.rs` `#[cfg(test)]`, and `src/web.rs` if needed)

`tempfile` is already a dev-dependency; tests pass an env map with
`GROK_SEARCH_CONFIG` pointing at a `TempDir` file (no process-env mutation).

1. **Secret masking** — env with `TAVILY_API_KEY=tvly-secret` →
   `load_source_config` returns `sources["tavily"].api_key == Set` and the
   serialized JSON does **not** contain `tvly-secret`.
2. **Env-override detection** — with `TAVILY_API_KEY` present,
   `env_overrides["tavily_api_key"] == true` and `sources["tavily"].api_key == Set`;
   absent → `false`/`Unset`. Same for `TAVILY_ENABLED` and
   `GROK_SEARCH_SOURCE_PROVIDERS`.
3. **Read/write round-trip** — seed file with `tavily_enabled = false`, call
   `write_source_config` with `SourceEdits { tavily_enabled: Some(true), exa_api_key: Some("exa-k".into()), ..Default::default() }`,
   then `load_source_config` reflects the new values and the file on disk
   contains `tavily_enabled = true` and `exa_api_key = "exa-k"`.
4. **Unknown-key / comment preservation** — seed file containing an unedited
   known key (`cache_size = 12`) plus a comment; after a write, both persist
   and only the edited key changed.
5. **Clear key** — seed `exa_api_key = "exa-k"`; write
   `SourceEdits { exa_api_key: Some(String::new()), .. }`; re-read shows
   `Unset` and `exa_api_key` is absent from the file.
6. **Missing file creation** — no file at the temp path;
   `write_source_config` creates it (and parent dirs) and re-read succeeds.
7. **Corrupt file refusal** — write `"this is {not toml"` to the path;
   `write_source_config` returns `Err` and the file bytes are unchanged.
8. **Validation rejects bad input** — `validate_edits` flags:
   `source_providers: Some(vec!["bing"])`, an over-4096-char key, and a key
   containing `\n`/`\0`. Empty errors for valid input.
9. **`source_providers` normalization** — `[" Exa ", "tavily"]` normalizes to
   `["exa","tavily"]` (dedup first-wins) in the written file.
10. **Serde shape** — `serde_json::from_str::<SourceEdits>` rejects an unknown
    top-level key and a non-bool `*_enabled` (this locks the PUT contract).

### 6.2 End-to-end curl smoke list

Run:
```bash
cd /workspace/NovaVeilSearch && export PATH="$HOME/.cargo/bin:$PATH"
export GROK_MCP_BIND=127.0.0.1:8080
export GROK_MCP_API_TOKEN=master-secret
export NOVA_ADMIN_PASSWORD=hunter2
export GROK_SEARCH_CONFIG=/tmp/nv-web-test.toml
cargo run --profile release-http --features http -- --http &
```

1. `curl -i http://127.0.0.1:8080/api/config` → **401** (`WWW-Authenticate: Bearer`).
2. `curl -i http://127.0.0.1:8080/` → **200** `Content-Type: text/html` (SPA).
3. `curl -s -X POST http://127.0.0.1:8080/login -H 'Content-Type: application/json' -d '{"username":"admin","password":"hunter2"}'`
   → **200** `{"token":"…","expires_in_seconds":43200}`; capture `TOKEN`.
4. `curl -s http://127.0.0.1:8080/api/config -H "Authorization: Bearer $TOKEN"`
   → **200** masked config (verify no key value appears; `env_overrides`
   keys present). Master token
   (`-H "Authorization: Bearer master-secret"`) works identically.
5. `curl -s -X PUT http://127.0.0.1:8080/api/config -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"tavily_enabled":false,"exa_api_key":"exa-live"}'`
   → **200**; then `cat /tmp/nv-web-test.toml` shows `tavily_enabled = false`
   and `exa_api_key = "exa-live"`.
6. `curl -s -X PUT …/api/config -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"source_providers":["bing"]}'`
   → **400** with `errors[0].field == "source_providers"`.
7. `curl -s -X PUT …/api/config -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"nonsense":true}'`
   → **400** (unknown field).
8. (With D7) after step 5, an authenticated MCP `tools/call web_search` reflects
   the edited sources — verifies live effect.

Build gates: `cargo build --profile release-http --features http` and
`cargo test --profile release-http --features http` must both pass.

---

## 7. Exact visibility / routing edits in `src/http.rs`

Make the following `pub(crate)` (they are currently private to `http.rs`):

- `struct AppState` (also mark fields `base_env` and `allowed_origins`
  `pub(crate)`; the rest stay private).
- `async fn authorize(headers: &HeaderMap, state: &AppState) -> bool`.
- `fn unauthorized_response() -> Response`.
- `fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str>`.
- Add `pub(crate) fn origin_allowed(headers: &HeaderMap, allowed: &Option<HashSet<String>>) -> bool`
  (the extracted origin-check block); call it from `mcp_post`, `get_config`,
  `put_config`.

Router (`src/http.rs`, `run_http`):

```rust
use axum::routing::{get, post};

let app = Router::new()
    .route("/mcp", post(mcp_post))
    .route("/login", post(login))
    .route("/", get(crate::web::serve_index))
    .route("/api/config", get(crate::web::get_config).put(crate::web::put_config))
    .with_state(state);
```

`src/web.rs` handler signatures (all gated):

```rust
pub(crate) async fn serve_index() -> Response;                            // 200 HTML
pub(crate) async fn get_config(State(state): State<AppState>,
                               headers: HeaderMap) -> Response;           // 200 masked
pub(crate) async fn put_config(State(state): State<AppState>,
                               request: axum::extract::Request) -> Response; // login-style body read
```

`put_config` mirrors `login`'s pattern: split `request.into_parts()`, read
`headers`, read body via `axum::body::to_bytes(body, MAX_CONFIG_BODY_BYTES)`
(`MAX_CONFIG_BODY_BYTES = 16 * 1024`), then `authorize` → `origin_allowed` →
`serde_json::from_slice::<SourceEdits>` → `validate_edits` →
`write_source_config(&state.base_env, &edits)` → map result to HTTP.