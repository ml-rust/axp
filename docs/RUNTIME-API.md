# Runtime API reference

This page lists the methods that `axp serve` exposes. For the design, read [AXP-ARCHITECTURE.md](AXP-ARCHITECTURE.md).

## Transport

| Endpoint | Purpose |
| -------- | ------- |
| `POST /` | JSON-RPC 2.0 requests and responses. |
| `GET /job/attach` | Server-Sent Events stream of one job's log. |

The default address is `127.0.0.1:7300`. Set another address with `axp serve --addr <socket-addr>`.

Every JSON-RPC request needs `"jsonrpc": "2.0"`, a `method`, and `params`.

## Authentication

`session.open` returns a `cap_token`. Every other method requires `session_id` and `cap_token` in `params`. The `GET /job/attach` endpoint requires the same two values as query parameters.

An unknown session and a wrong token return the same `UNAUTHORIZED` error. The error does not reveal whether a session exists.

## Methods

| Method | Params | Result |
| ------ | ------ | ------ |
| `session.open` | `workspace` (absolute path), `sandbox_tier`, `capabilities` (optional string array) | `session_id`, `granted_tier`, `cap_token` |
| `session.close` | `session_id`, `cap_token` | `ok` |
| `session.audit` | `session_id`, `cap_token` | `events` (each with `ts_millis` and `kind`) |
| `axp.index` | `session_id`, `cap_token` | `entries` (each with `name` and `desc`) |
| `axp.describe` | `session_id`, `cap_token`, `name` | `signature`, `schema` |
| `job.start` | `session_id`, `cap_token`, a payload (see [Job payloads](#job-payloads)), `cwd` (optional), `capabilities` (optional) | `job_id` |
| `job.status` | `session_id`, `cap_token`, `job_id` | `job_id`, `status`, `seq` |
| `job.cancel` | `session_id`, `cap_token`, `job_id` | Cancel response from `axp-proto` |

`session.open` accepts `sandbox_tier` values `kernel-lsm`, `container`, `process-token`, and `dev-none`. See [Sandbox tiers](../README.md#sandbox-tiers) in the README for what each tier enforces.

`job.attach` is not a JSON-RPC method. Calling it through `POST /` returns `INVALID_REQUEST` with a pointer to `GET /job/attach`.

### Example: open a session

```bash
curl -s http://127.0.0.1:7300/ \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"workspace":"/absolute/path/to/project","sandbox_tier":"dev-none","capabilities":["proc.spawn"]}}'
```

The response has this shape:

```json
{"jsonrpc":"2.0","id":1,"result":{"session_id":"<session_id>","granted_tier":"dev-none","cap_token":"<cap_token>"}}
```

## Job payloads

`job.start` reads the payload fields at the top level of `params`. The `kind` field selects the payload type.

| `kind` | Fields | Grant that the check requires |
| ------ | ------ | ----------------------------- |
| `command` | `command`: shell string, run with `sh -c` | `proc.spawn` |
| `code` | `code`: base64 WebAssembly component. `lang`: optional, must be `wasm-component` when present | None beyond the job capabilities check |
| `capability` | `name`: registry capability name. `params`: JSON object passed to the resolver | `tool:<name>` or `proc.spawn` |

The `cwd` field is relative to the workspace. The `capabilities` field lists job-level grants. Each job grant must fit inside the session grants.

### Example: start a command job

```bash
curl -s http://127.0.0.1:7300/ \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"job.start","params":{"session_id":"<session_id>","cap_token":"<cap_token>","kind":"command","command":"echo hello"}}'
```

The response contains `{"job_id":"<job_id>"}` inside `result`.

## Discovery

`axp.index` returns only the capabilities that the session can use. A session can use a capability when it holds `proc.spawn` or a matching `tool:<name>` grant.

The built-in capabilities are:

| Name | Params | Runs |
| ---- | ------ | ---- |
| `git_diff` | None | `git diff` |
| `git_log` | None | `git log --oneline -20` |
| `read_file` | `path` (string) | `cat <path>` |

A capability description must:

- Contain 10 to 120 bytes.
- Contain no line break.
- Not only repeat the capability name.

Registration fails with a description-quality error when a description breaks a rule.

## Streaming logs

`GET /job/attach` takes these query parameters:

| Parameter | Required | Meaning |
| --------- | -------- | ------- |
| `session_id` | Yes | Session that owns the job. |
| `cap_token` | Yes | Session token from `session.open`. |
| `job_id` | Yes | Job to attach to. |
| `from_offset` | No | Sequence number to resume from. Default `0`. |

When the request has a `Last-Event-ID` header that parses as a number, that value replaces `from_offset`.

Each SSE event has these fields:

| Field | Content |
| ----- | ------- |
| `id` | Sequence number of the frame. |
| `data` | JSON object with `job_id`, `seq`, `stream` (`stdout` or `stderr`), `data` (raw bytes as a JSON number array), and `ts_millis`. |

Jobs keep running when the client disconnects. Reattach with the job id and the last `id` value received.

## Audit events

`session.audit` returns events for four kinds: session opened, session closed, job started, and job finished. Job events carry the `job_id`. Job finished events also carry the terminal status.

The audit log does not record file writes, file reads, or network attempts.

## Error codes

| Code | Name | Meaning |
| ---- | ---- | ------- |
| -32700 | `PARSE_ERROR` | The request body is not valid JSON. |
| -32600 | `INVALID_REQUEST` | The request is not valid JSON-RPC, or it calls `job.attach` through `POST /`. |
| -32601 | `METHOD_NOT_FOUND` | The method name is unknown. |
| -32602 | `INVALID_PARAMS` | The params do not match the method, or the workspace path is invalid. |
| -32603 | `INTERNAL_ERROR` | The server failed to process a valid request. |
| -32001 | `NOT_FOUND` | The capability or job does not exist. |
| -32002 | `DENIED` | The session lacks the grant that the call requires. |
| -32003 | `NOT_IMPLEMENTED` | The runtime does not implement the requested feature. |
| -32004 | `UNAUTHORIZED` | The session is unknown, or the `cap_token` is wrong. |
