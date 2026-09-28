# HTTP search tasks and polling

Once accepted, `web_search` on `POST /mcp` and searches on `POST /messages`
continue when the caller disconnects. They keep their concurrency slot until
the search finishes, fails, or reaches the configured search deadline. A
disconnect does not grant the worker a new timeout budget.

The server retains the **complete HTTP reply**, including the AI answer and
the source list, under a task ID allocated before searching. This is separate
from the supplemental query cache and the source cache used by `get_sources`.
`get_sources` alone cannot report running tasks or recover the full answer.

## Existing clients

Without extra headers, MCP clients receive the usual JSON-RPC response or
SSE `message` event, and Messages clients receive the usual JSON response.
Successful task submission adds `X-Nova-Task-Id` and a relative `Location`
header. SSE sends these headers before search completion. The SSE connection
still emits heartbeats while waiting; its disconnect only removes that
subscriber.

Polling is an HTTP extension for clients that explicitly implement it. It is
not an advertised MCP Tasks capability, and existing MCP/DSH clients will not
automatically start polling. Other MCP tools and stdio keep their existing
request/response behavior.

## Submit and poll

Add `Prefer: respond-async` to return immediately after task acceptance:

```bash
curl -i 'https://<host>/nova-veil-search/mcp' \
  -H 'Authorization: Bearer <token>' \
  -H 'Content-Type: application/json' \
  -H 'MCP-Protocol-Version: 2025-11-25' \
  -H 'Prefer: respond-async' \
  -H 'Idempotency-Key: search-example-1' \
  --data '{"jsonrpc":"2.0","id":"search-1","method":"tools/call","params":{"name":"web_search","arguments":{"query":"Rust async cancellation","response_format":"concise"}}}'
```

A pending task returns:

```http
HTTP/1.1 202 Accepted
Preference-Applied: respond-async
Location: mcp/tasks/<task-id>
X-Nova-Task-Id: <task-id>
X-Nova-Task-Status: running
Retry-After: 2
Cache-Control: no-store
Content-Type: application/json

{"task_id":"<task-id>","status":"running"}
```

Resolve `Location` **against the original POST URL**. For the example above,
it resolves to `https://<host>/nova-veil-search/mcp/tasks/<task-id>`. The relative
URL preserves the reverse-proxy prefix and also works for bare `/mcp`.

```bash
curl -i 'https://<host>/nova-veil-search/mcp/tasks/<task-id>' \
  -H 'Authorization: Bearer <same-token>'
```

| Poll response | Client action |
|---|---|
| `202`, `X-Nova-Task-Status: running` | Wait at least the `Retry-After` interval (2 seconds), then poll again. |
| `X-Nova-Task-Status: completed` | Consume the original JSON response body. A successful search has HTTP `200`. |
| `X-Nova-Task-Status: failed` | Consume the original error body and stop polling. A JSON-RPC error may still use HTTP `200`; inspect the body/header. |
| `404` | The task is unknown, expired, evicted, or belongs to another token. |
| `401` | Authenticate again; fetching a retained task requires its original, still-valid token. |

Completed polls return the original response directly, without a task wrapper.
Once the server first finishes streaming the response body, it retains the
reply and its task/idempotency record for another **5 minutes**. During that
window, repeated polls and matching POST retries return the same response bytes;
they do not extend the deadline. After expiry, polls return `404`. This also
applies to the original JSON/SSE POST response and to a completed retry. Running
status (`202`) responses never start this window, even if the search finishes
while that status response is being sent. An unread or unfinished response body
leaves the deadline unchanged. Body completion is a server-side signal, not proof
that the client application received or persisted the reply. Subscribers already
attached may still finish receiving their shared reply.

If a task already completed before an async POST returns, that POST may return
the completed response immediately instead of `202`. Every task response is marked
`Cache-Control: no-store`; retention is inside the server, not an HTTP proxy.

`POST /messages` accepts the same async and idempotency headers; its completed
poll returns an Anthropic-compatible Messages response. A master token supplied
as `x-api-key` also works on the polling endpoint. MCP login-session tokens work
with `Authorization: Bearer` and are scoped to that exact session token.

## Safe retries after a disconnect

Choose a new `Idempotency-Key` for each intended search (1–128 visible ASCII
characters). Retry an interrupted POST with the **same token, key, endpoint,
and JSON body**, including the original MCP JSON-RPC `id`. A retry attaches to
the running task or replays its retained result, without another upstream
search. JSON object field order and insignificant JSON whitespace do not
affect matching. Arrays and string contents remain significant.

Reusing a retained key for different input returns `409`. Omitting the key
starts an independent task on each POST. A new intended search or a deliberate
retry after a completed failure should use a new key. Retries still pass the
normal POST admission checks and can receive `429` at capacity; known task IDs
can be polled even when every search slot is busy.

Persist the key before submitting. It lets a client recover when the network
fails before the task-ID headers arrive. The key stays valid during the
five-minute replay window. After expiration or eviction, the same key can start
new work again; this is bounded retry deduplication, not permanent exactly-once
execution.

## Retention and configuration

- Unclaimed results expire 30 minutes after search completion.
- The first fully streamed reply starts a fixed 5-minute retention period for
  its result, task record, and idempotency key. Repeated delivery does not renew
  it. Both successful and failed replies follow this rule. A first delivery at
  minute 29 therefore retains the reply until minute 34 after search completion.
- A background sweeper runs every 30 seconds even with no incoming requests, so
  expired idle entries are physically removed within the next sweep. Lookups
  also remove expired entries before returning results.
- The in-memory store holds at most 128 task records and 32 MiB of serialized
  replies. Old completed results are evicted first under pressure. A single
  response larger than that budget is not retained for polling. Capacity pressure
  can evict results before either retention deadline.
  These bounds cover the retained task store, not total process memory: active
  searches, response buffers, provider/query caches, and `get_sources` have
  separate allocations and lifetimes. Releasing cached objects also does not
  guarantee that the allocator immediately returns their pages to the OS.
- Running tasks are not evicted. They continue to count against the existing
  32-request limit even after every client disconnects.
- Errors are retained too; worker panics become terminal errors rather than
  permanently running records in the HTTP build using `panic=unwind`.
- Tasks and idempotency records disappear on process restart. With multiple
  replicas, submission, retries, and polling must reach the same process.
- A task uses the service/config snapshot captured when it was submitted.
  Settings reloads do not cancel it or erase its retained reply.
- The task store works with `NOVA_CONFIG_UI=false` and is independent of
  `GROK_SEARCH_RESULT_CACHE_TTL_SECONDS` (which controls supplemental results).

The repository's Caddy rules already forward both
`/nova-veil-search/mcp/tasks/*` and `/mcp/tasks/*`. Custom proxies must forward
this route along with the search POST endpoints.
