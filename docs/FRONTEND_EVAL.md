# Frontend + Config-Write Endpoints — Evaluation & Recommendation

Status: analysis only. No code was changed. All file/line references are against the
current tree (`v0.1.26`).

Purpose: add a login-guarded **web frontend for configuring search sources**
(source on/off toggles + API-key add/edit) and HTTP endpoints to **read/write
`config.toml`**, while keeping server-side memory near zero on a small aarch64
box (~14 GiB host, but the service container is already `mem_limit: 256m` per
`docker-compose.yml:42`, so the *stray memory* budget is really ~1–5 MB on top
of the current ~4.5 MB idle RSS, not 14 GiB).

---

## 0. A blocking architectural fact to resolve first

The current HTTP server **never reads `config.toml`**.

- `run_http()` builds its operator config with `Config::from_env_map(base_env)`
  (`src/http.rs:164`), i.e. **env-only**.
- `main.rs` passes `base_env = std::env::vars()` (`src/main.rs:71`), and
  `run_http` is invoked directly without going through `Config::load()`
  (`src/main.rs:64–73`).
- `Config::load_from()` — the *only* path that reads `config.toml` with the
  documented `env > file > defaults` precedence — lives in `src/config.rs:316–335`
  and is what the **stdio/CLI** path uses (`src/main.rs:76` `Config::load()`), not
  the HTTP path.

Consequence: writing `config.toml` from a web UI changes **nothing** for a running
or restarted HTTP server until one of these also happens:

1. `run_http` switches to `Config::load_from(base_env)` (so the file is merged
   under env at startup), **and/or**
2. a reload mechanism re-reads the file into the operator config (and rebuilds
   the source slots / cache), **and/or**
3. the deployment maps `config.toml` into the container env (e.g. the
   `docker-compose.yml` env block already sets `GROK_SEARCH_API_KEY`,
   `TAVILY_API_KEY` etc. directly — those env vars win and will mask any file
   value the UI writes).

This must be stated explicitly in the build plan; the "write config.toml"
feature is otherwise a no-op in the shipped HTTP build. The rest of this
document assumes the recommended answer is **(1) at minimum** (`run_http` →
`load_from`) **plus a restart**(with restart-as-acceptable documented in the UI,
and file-watch reload deferred as a follow-up).

---

## 1. Frontend serving approach

Compared against the shared constraints: single small binary, behind Caddy,
`/nova-veil-search/*` reachable today, no Node infrastructure on the target box,
near-zero server memory.

### Option (a) — `include_str!` single-page app (RECOMMENDED)

Embed `index.html` + `app.js` + `app.css` with `include_str!(...)` in a new
`web` module (gated on `http`, declared in `src/lib.rs` like `http` is at
`src/lib.rs:6–7`), served by axum `GET` handlers from `&'static str` bodies.

| Dimension | Assessment |
|---|---|
| Server memory | ~0 heap beyond the response bytes. Assets live in the binary's `.rodata` once; `Body::from(&'static str)` copies only on send. Browser executes everything. |
| Binary footprint | Binary grows by exactly the size of the committed assets (a hand-written admin UI is 5–20 KB; trivial vs the current 4.4 MB). |
| New dependencies | **None.** |
| Deployment complexity | Assets bake into the binary at compile time → single artifact, nothing to mount/version alongside. Cost: asset edits require a rebuild + redeploy (fine for a rarely-changed admin page). |
| Caddyfile changes | **None.** The existing `handle_path /nova-veil-search/*` (`Caddyfile:13–15`) proxies backend `/` → `GET /` returns the shell; the SPA's own `fetch()` calls go to same-origin routes through the same prefix. |
| Path-routing interaction | Clean: SPA and JSON API are same-origin (see §4), so no CORS preflight. Sub-asset URLs should be **relative** (`./app.js`) or absolute under `/nova-veil-search/…`; a single `GET /` (or `GET /settings`) handler returns `index.html` and the JS re-writes for a prefix-less deployment. |

Implementation shape: add `GET /` (or `/settings`) serving the shell, plus
`GET /app.js`, `GET /app.css`, and `GET /api/config` / `POST /api/config`
(or `PUT`) to the router at `src/http.rs:224–227`. Keep bodies tiny; reuse the
existing body-size-cap style (`MAX_BODY_BYTES` at `src/http.rs:48`).

### Option (b) — `tower-http` `ServeDir` from a `web/` directory

| Dimension | Assessment |
|---|---|
| Server memory | Slight per-request file I/O; no large heap. |
| Binary footprint | Smaller than (a) (assets stay on disk). |
| New dependencies | `tower-http` with `fs` feature. The crate is **already compiled** as a transitive dep of `reqwest` (verified in `Cargo.lock`: `reqwest` → `tower-http` 0.6.10), but the `fs` feature is not currently enabled — enabling it pulls `http-range` + `tokio-util` io features. Marginal, but non-zero. |
| Deployment | A `web/` dir must be `COPY`ed into the image (or mounted); i.e. **two** artifacts to version and keep in sync. ServeDir's `PrecompressedStaticFiles`/MIME handling covers gzip via Caddy anyway. |
| Caddyfile | None. `ServeDir` nests under the existing `/` route. |
| Risk | Traversal-safe by construction (tower-http normalizes and rejects `..`), but it's more surface + more deps for assets that could just be embedded. |

### Option (c) — separate static host + Caddy `file_server`

| Dimension | Assessment |
|---|---|
| Server memory | A second process (nginx or a re-used caddy) = MBs of RSS + a config to manage. Worst of the server-local options. |
| Deployment | Third container/volume; `docker-compose.yml` gains a service. |
| Caddyfile | **Yes** — add a `file_server` `handle`; must not collide with `handle_path /nova-veil-search/*`. |
| Verdict | Unjustified for a two-form admin page; only sensible if the frontend grows into a large shared static site. |

### Option (d) — Node SSR

Rejected outright. Adds a long-lived Node process (tens of MB+ RSS), a build
toolchain, and SSR complexity with **zero benefit** for a config form. Violates
the "memory near-zero server-side; frontend runs in the browser" requirement in
the brief.

**Recommendation: (a).** Rationale summarized in §6.

---

## 2. Config write design

### 2.1 Current state

- `Config` (public) is the runtime struct (`src/config.rs:20–69`); it has **no
  `Serialize`** and there is **no Config→TOML writer anywhere**.
- `ConfigFile` is the TOML mirror, private, `Deserialize`-only, and
  `#[serde(deny_unknown_fields, default)]` (`src/config.rs:165–204`). Its
  `into_env_map` maps snake_case TOML keys → env keys (`src/config.rs:209–301`).
- The only writer is `write_template()` (`src/config.rs:564–573`), which writes
  a **fully-commented** template (`CONFIG_TEMPLATE`, `src/config.rs:577–642`) and
  refuses to overwrite an existing file.
- Load precedence is implemented in `load_from` (`src/config.rs:316–335`) via
  `merge_env_over_file` (`src/config.rs:672–680`): **env overlays file**.
- `read_config_file` (`src/config.rs:649–670`) uses `toml::from_str::<ConfigFile>`
  and **rejects the whole file on any unknown key** (by `deny_unknown_fields`).

### 2.2 Round-trip options

**O1 — plain `toml::Table` round-trip.** Parse into a generic `toml::Table`,
overlay the edited keys, `toml::to_string_pretty`. Requires only the `display`
feature. **Loses** all comments, key ordering, and any not-yet-known keys if you
round-trip through `ConfigFile`; if you round-trip through a generic `toml::Table`
you keep unknown keys but still lose comments/formatting.

**O2 — `toml_edit::DocumentMut` targeted edit (RECOMMENDED).** Load the file as a
`DocumentMut`, set/remove only the managed keys, write it back. Preserves
comments, ordering, and unrelated keys verbatim. This matters because operators
hand-edit the annotated template (`CONFIG_TEMPLATE` is entirely comments) and the
file may contain keys `ConfigFile` doesn't yet know about (which currently
*reject* the file — see §2.4). `toml_edit` is **already compiled** transitively
(via `toml`'s `parse` feature → `toml_edit` 0.22.27 in `Cargo.lock`), so adding
it as a direct dependency is nearly free.

**Verdict:** O2 (`toml_edit`), because the config file is a hand-edited,
comment-heavy artifact and a full rewrite would destroy that. A plain
`toml::Table` round-trip is the fallback if we refuse `toml_edit` as a direct
dep — but it is already in the build graph, so there is no real cost argument
against it.

### 2.3 Full rewrite vs merge — why merge, and what it must NOT touch

- Never emit a fresh file from a **subset** of known keys: that silently erases
  every key outside the managed set (`enrich_*`, `response_max_chars`,
  `openai_compatible_*`, future keys) plus all comments.
- Merge = mutate only the managed keys in the parsed document and serialize the
  whole document back. `toml_edit` makes this a few `document["key"] = value(...)`
  assignments and preserves everything else.
- Key deletes: when a user clears a key, **remove** the key rather than writing
  an empty string. `Config::from_env_map` already treats blank API keys as
  absent (`src/config.rs:376–389` etc.), but leaving `tavily_api_key = ""` in the
  file is sloppy and, if a future reader stops trimming, could rebuild a
  provider that only 401s (the exact bug the trim comments at
  `src/config.rs:370–375` guard against).

### 2.4 Env-vs-file precedence surfaced in the UI

Env wins (`merge_env_over_file`). The read endpoint must therefore report, per
managed field, a `source` of `env | file | default` and an `editable` flag:

- `env`: the key is present in the process env / `base_env` → **read-only** in
  the UI, with the notice *"overridden by an environment variable (e.g.
  `TAVILY_API_KEY`) — edit that variable instead"*.
- `file`: value comes from `config.toml` → editable (write goes to the file).
- `default`: neither set → editable; a write adds the key to the file.

The web module should compute this by (1) checking `std::env::vars()`/the stored
`base_env` for each managed env key, and (2) parsing the file independently (via
`toml` or `toml_edit`, not `ConfigFile`) to read its raw keys. `ConfigFile`'s
`deny_unknown_fields` means `read_config_file` returns `Rejected` for files with
future/unknown keys — the write path (toml_edit, generic) has no such problem,
but the read path should also tolerate unknown keys (parse generically) so the UI
can still render a file the server currently rejects.

### 2.5 Atomic write

Write to a temp file in the **same directory** (same filesystem) then
`rename()` over the target:

1. `resolve_config_path` server-side (`src/config.rs:480–490`) — the *only*
   path source; never from the request (see §4 traversal).
2. `std::fs::write(tmp_path, new_bytes)` (or write + `sync_all`).
3. `std::fs::rename(tmp_path, config_path)` — atomic on the same FS.
4. Best-effort preserve mode 0o600 (secrets live here), create parent dirs if
   the file is absent (mirror `write_template`'s `create_dir_all`,
   `src/config.rs:568–570`).

Because the running server doesn't re-read the file (§0), the atomicity only
protects against **torn reads at next start** and against a future file-watch
reloader — not the current process. Still do it; it's cheap and future-proof.

### 2.6 Which fields the "sources" editor manages

Recommended managed set (TOML key ← env key). All are `Option`-typed in
`ConfigFile` and map 1:1 (`src/config.rs:209–301`):

| Field | TOML key | Env key | UI control |
|---|---|---|---|
| Grok key | `grok_api_key` | `GROK_SEARCH_API_KEY` | secret add/edit |
| Web search | `web_search_enabled` | `GROK_SEARCH_WEB_SEARCH` | toggle |
| X search | `x_search_enabled` | `GROK_SEARCH_X_SEARCH` | toggle |
| Tavily | `tavily_api_key` / `tavily_enabled` | `TAVILY_API_KEY` / `TAVILY_ENABLED` | key + toggle |
| Firecrawl | `firecrawl_api_key` / `firecrawl_enabled` | `FIRECRAWL_API_KEY` / `FIRECRAWL_ENABLED` | key + toggle |
| Tinyfish | `tinyfish_api_key` / `tinyfish_enabled` | `TINYFISH_API_KEY` / `TINYFISH_ENABLED` | key + toggle |
| Exa | `exa_api_key` / `exa_enabled` | `EXA_API_KEY` / `EXA_ENABLED` | key + toggle |
| Chain order | `source_providers` | `GROK_SEARCH_SOURCE_PROVIDERS` | ordered multi-select |
| GitHub | `github_token` | `GITHUB_TOKEN` | secret add/edit |

Notes:

- `source_providers` must be restricted to the valid names
  `tavily, exa, tinyfish, duckduckgo, bing, firecrawl`
  (`validate_source_providers`, `src/service.rs`; `CANONICAL_SOURCE_ORDER`).
  The editable key toggles cover only the four key'd sources
  (`tavily`, `exa`, `tinyfish`, `firecrawl`); the keyless engines
  (`duckduckgo`, `bing`) have no key to manage. Validate server-side on write
  using the same function rather than trusting the UI.
- Endpoint URLs (`grok_api_url`, `*_api_url`) are deliberately excluded from the
  default managed set. They are advanced/self-host-gateway knobs; if added, they
  are *not* secrets but the server currently masks them (`redact_url` → `******`,
  see §3), so showing/editing them needs an explicit policy decision. Keep them
  read-only/masked in v1.

### 2.7 Cargo.toml feature change required

Current (`Cargo.toml:30`): `toml = { version = "0.8", default-features = false,
features = ["parse"] }` — deserialize only.

- **O1 (Table round-trip):** add `display` → `["parse", "display"]`. This
  enables `toml::to_string`/`to_string_pretty`. No `serde` feature needed if you
  construct the `toml::Value`/`Table` manually.
- **O1 via derived struct:** also add `serde` and `#[derive(Serialize)]` a
  write-mirror (separate from `ConfigFile`; never serialize `Config`, whose
  secret `Option`s would leak into the file and whose `timeout: Duration` is not
  a `Duration` in the file mirror).
- **O2 (toml_edit, RECOMMENDED):** add `toml_edit = "0.22"` as a direct
  dependency (already in the build graph via `toml`), and leave `toml` at
  `["parse"]`. `toml_edit`'s default features (parse + display) are sufficient;
  `serde` feature only if doing typed maps.

---

## 3. Secret handling

The read API must **never** return secret values. Return only:

- `"set"` / `"unset"` (preferred, matches repo convention), **or**
- a short masked prefix (e.g. `xai-a1…`, `tvly-…`, `sk-…`) capped at ~4 chars +
  a fixed `…` mask, and **only** for keys with a recognizable safe prefix —
  never for `github_token` (a `ghp_*` prefix is itself sensitive).

Existing helpers to reuse (`src/config.rs`):

- Hand-written `Debug for Config` (`src/config.rs:107–161`) already renders every
  secret `Option` as `"set"`/`"unset"` via `mask()`.
- `redact()` (`src/config.rs:809–814`) → `"set"`/`"unset"`, **no fragment**
  (comment at `806–808` explicitly argues against prefix/suffix leak).
- `github_token_status()` (`src/config.rs:447–453`) → public two-state signal.
- `redact_url()` (`src/config.rs:820–826`) → `"******"`; `redact_urls()`
  (`832–878`); `redact_path()` (`888–893`).

Two implementation notes:

1. `redact()` is currently **private** (`fn redact`, `src/config.rs:809`) while
   `redact_url`/`redact_urls`/`redact_path` are `pub(crate)` and
   `github_token_status` is `pub`. The web module lives in the crate so it can
   use the `pub(crate)` items directly, but `redact` either needs to become
   `pub(crate)` or the module should call `Config::github_token_status` plus the
   masked-`Debug` form (or add one small `pub(crate) fn secret_status(&Option<String>)
   -> &'static str` shared helper).
2. The **write** API accepts new secret values in the request body (they travel
   over TLS — the same trust model as `POST /login`, which returns the session
   token as JSON today, `src/http.rs:658–664`). Its response must be
   `{"ok": true, "<key>": "set"}` — **never echo the written value back**.

Consistency callout: the brief permits a "short masked prefix", but the repo's
existing stance (`redact()`'s comment) is *no fragment at all*. Recommend
matching the repo: return `set`/`unset` for the read API, and only add prefixes
if the operator specifically wants to distinguish multiple configured keys
(e.g. rotated Tavily keys `tvly-a,tvly-b`), in which case show a fixed count of
configured keys plus last-4 of each, still never the full or first-N-secret.

---

## 4. Authn/authz & web security

- **Reuse `authorize()`.** Config endpoints must accept the **master token or a
  login-issued session token** exactly like `/mcp` does (`authorize`,
  `src/http.rs:615–627`; `mcp_post` calls it at `262–264`). Add the same guard to
  the new `GET/POST /api/config` (and the SPA shell routes) before any body read
  or config-file I/O, mirroring the "authenticate first" ordering at
  `src/http.rs:258–264`.
- **CSRF posture is inherently good.** Same-origin serving means no CORS
  preflight, and because auth is carried in an `Authorization: Bearer <token>`
  header — not a cookie — a cross-site form POST or `<script>` from another origin
  cannot attach the token. Structural CSRF protection without a CSRF token.
  Do **not** switch to cookie-based session storage, which would reopen this.
- **Don't require CORS.** Served from the backend's own origin, the SPA's
  `fetch()` is same-origin. `GROK_MCP_ALLOWED_ORIGINS` (parsed at
  `src/http.rs:715–728`) is an **origin allowlist enforced inside `mcp_post`
  (`275–281`)**, not CORS response headers. If the operator sets that allowlist,
  it must include the site's own origin or the UI's own calls to `/mcp` (if the
  UI ever makes them) — and the config endpoints' origin check, if added, must
  reuse `state.allowed_origins` the same way (`Absent Origin` ⇒ allowed, present
  ⇒ must be on the list). Keep parity: apply the same origin check to
  `/api/config` so DNS-rebinding can't drive a config write from a hostile
  origin.
- **Path traversal.** The write path must resolve the config path **server-side
  only** (`resolve_config_path` ∈ `src/config.rs:480–490`, honoring
  `$GROK_SEARCH_CONFIG` else `$HOME/.config/nova-veil-search/config.toml`).
  Never accept a path in the request body; the API should accept a **set of
  (field, value) edits**, not a filename. `resolve_config_path` is private
  today — expose it (or reuse `config_path()`/`config_path_for()`,
  `src/config.rs:513–552`) to the web module rather than adding a client path
  parameter.
- **Rate-limit / brute-force `/login`.** Currently `/login`
  (`src/http.rs:633–666`) has only a 300 ms failure sleep
  (`src/http.rs:652–654`) — no per-IP/account lockout, and the concurrency
  semaphore that guards `/mcp` (`src/http.rs:246–253`) does **not** wrap
  `/login`. Recommend: (1) an in-memory per-IP failure counter (fits naturally in
  the existing `SessionStore`-style `Arc<Mutex<…>>`), e.g. 5 failures → 15s
  cooldown; (2) wrap `/login` in a slower path or its own small semaphore so a
  bot can't saturate the 2-worker runtime (`build_runtime`, `src/main.rs:12`);
  (3) optionally increase the fixed sleep. Sessions are in-memory and die on
  restart — acceptable and even desirable for a single-admin box.
- **Master token vs admin creds for the settings page.** Recommend the UI log in
  with **`NOVA_ADMIN_USER`/`NOVA_ADMIN_PASSWORD`** via `POST /login` and hold the
  returned session token **in JS memory** (not `localStorage`). Fall back to
  prompting for the **master token** only when `NOVA_ADMIN_PASSWORD` is unset
  (the endpoint returns 404 — `src/http.rs:634–636`) — the master token is the
  MCP callers' bearer and should not be cached in a browser. The endpoints accept
  either, so the SPA can use whichever it obtained; the UI should just render a
  "login disabled (set NOVA_ADMIN_PASSWORD)" page when 404.

---

## 5. Dependency / additive footprint

| Change | New crates? | Marginal binary | Marginal idle memory |
|---|---|---|---|
| `include_str!` SPA (option a) | none | = asset size (≈ KBs) | ~0 |
| `toml` `display` (+ optional `serde`) | none (toml/toml_edit/serde already in tree) | a few KB (`to_string` path) | ~0 |
| `toml_edit` as direct dep | none (*already compiled* via `toml`'s `parse`) | ~0–few KB (enable `display`) | ~0 |
| `tower-http` `fs` (`ServeDir`) | `http-range` + `tokio-util` io (crate itself already in tree via `reqwest`) | tens of KB | small |
| Node SSR | Node runtime + toolchain | n/a (separate process) | tens–hundreds of MB |

Key facts behind the table (verified in `Cargo.lock`):

- `toml` → `toml_edit` 0.22.27 (so `toml_edit` is already built; promoting it to
  a direct dep costs essentially nothing).
- `reqwest` → `tower-http` 0.6.10 (so the crate is already built; `ServeDir`
  additionally needs its `fs` feature, which reqwest does not currently enable).
- `axum` → `mime` (so `ServeDir`'s MIME detection wouldn't add a new MIME crate).

Net binary cost of the recommended design (option a + `toml_edit`) is in the
single-digit KB range; idle RSS impact is negligible — consistent with the
"memory near-zero server-side" constraint.

---

## 6. Conclusion

### Recommended architecture (one paragraph)

Embed a small, framework-free single-page app with `include_str!` in a new
`web` module gated on `http`, served from the existing axum router as `GET /`
(shell) + `GET /app.js` + `GET /app.css` — no new dependencies, no Caddyfile
change, no disk assets, browser runs the UI so server memory stays ~flat. Add
`GET /api/config` (field-by-field, with `source: env|file|default` and secrets
as `set`/`unset` only) and `POST /api/config` (a server-validated list of
(field,value) edits), both guarded by the existing `authorize()` and the same
origin check as `/mcp`. Writes go through an atomic temp+`rename` using
`toml_edit` to edit only the managed keys (grok key, `web_search_enabled` /
`x_search_enabled`, tavily/firecrawl/tinyfish/exa keys+enabled,
`source_providers`, `github_token`) while preserving comments and unknown keys,
with the target path resolved strictly server-side. Pair this with switching
`run_http` from `Config::from_env_map` to `Config::load_from` so the file
actually takes effect, a session-token login for the UI, and a light per-IP
failure throttle on `/login`.

### Ordered build list

1. Add `toml_edit` (direct) to `Cargo.toml`; expose a server-side
   `config_path()`/`resolve_config_path` accessor and an `edit_config_file(path,
   edits)` write function in `src/config.rs` (atomic temp+rename, key add/remove,
   0o600).
2. Add a new `web` module (`src/web.rs`, gated on `http`, registered in
   `src/lib.rs`) embedding the SPA and exposing the read/write DTOs (secret
   fields as `set`/`unset`, per-field `source`/`editable`).
3. Add router entries in `src/http.rs` (`GET /`, `GET /app.{js,css}`,
   `GET/POST /api/config`), each with `authorize()` + origin check + body-size
   cap, read-only surfacing of env-overridden fields.
4. Switch `run_http` to seed operator config via `Config::load_from(base_env)`
   so `config.toml` is merged under env (with `config_file_state` surfaced to
   the UI).
5. Harden `/login`: per-IP failure counter + cooldown, and route it through the
   concurrency limiter (or a dedicated, stricter one).
6. Validate `source_providers` on write via `validate_source_providers`
   (`src/service.rs:311`); confirm `grok_api_url`/`*_api_url` stay masked/read-only
   in v1.
7. Update `README.md` / `docs/CONFIGURATION.md` and the compose env comments to
   document the new UI, the env-overrides-file precedence in the editor, and the
   "restart required" behavior; add tests for the write path (round-trip,
   comment preservation, atomic rename, unknown-key tolerance, secret redaction
   in the read DTO).