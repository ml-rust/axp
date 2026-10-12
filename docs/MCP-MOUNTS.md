# Static MCP mounts

`axp serve` mounts MCP tools as static AXP capabilities. AXP does not implement the MCP wire protocol. Each mounted tool runs through a bridge program that you supply. This repo does not ship a bridge program.

Mounts are static. `axp serve` does not discover tools from a running MCP server.

## Mount forms

Use one of two forms. Do not combine them.

| Form | Flags | Tools per mount |
| ---- | ----- | --------------- |
| Flags | `--mcp-provider`, `--mcp-tool`, `--mcp-desc`, `--mcp-bridge` | One. All four flags are required together. |
| JSON config | `--mcp-config <path>` | One or more. Each file defines one provider. |

Passing `--mcp-config` with any flag form exits with an error.

### Flags form

```bash
axp serve \
  --mcp-provider docs \
  --mcp-tool search \
  --mcp-desc "Search documentation with an external MCP bridge" \
  --mcp-bridge axp-mcp-bridge
```

The flags form uses these fixed values:

| Setting | Value |
| ------- | ----- |
| Bridge args | `["call"]` |
| Input schema | `{"type": "object", "additionalProperties": true}` |

### JSON config form

```bash
axp serve --mcp-config mcp.json
```

```json
{
  "provider": "docs",
  "bridge": {
    "program": "axp-mcp-bridge",
    "args": ["call"]
  },
  "tools": [
    {
      "name": "search",
      "desc": "Search documentation with an external MCP bridge",
      "schema": { "type": "object", "additionalProperties": true }
    }
  ]
}
```

| Field | Required | Rule |
| ----- | -------- | ---- |
| `provider` | Yes | Non-empty. Identifies the provider. |
| `bridge.program` | Yes | Non-empty. The program that AXP runs. |
| `bridge.args` | No | Defaults to `["call"]`. |
| `tools` | Yes | At least one entry. |
| `tools[].name` | Yes | Non-empty and unique within the file. |
| `tools[].desc` | Yes | Non-empty. |
| `tools[].schema` | No | JSON Schema for the tool input. Defaults to `{"type": "object", "additionalProperties": true}`. |

A file that breaks a rule makes `axp serve` exit with an error that names the field.

## How a call runs

A mounted tool appears in `axp.index` with its `desc`. `axp.describe` returns its signature and `schema`.

A `job.start` request with `kind` set to `capability` runs the bridge. AXP builds this argv:

```text
<bridge.program> <bridge.args...> --provider <provider> --tool <name> --params <json>
```

The `<json>` value is the JSON object from the job's `params`. AXP passes each argument as a separate argv entry. AXP does not use a shell.

The bridge runs as a job under the session sandbox. Calling a mounted tool needs the `tool:<name>` grant or `proc.spawn`, the same as a built-in capability. See [Runtime API](RUNTIME-API.md#discovery).

## Name collisions

If a mounted tool name matches a name from another provider, AXP exposes both entries as `<provider>:<name>`. For example, provider `docs` with tool `search` exposes `docs:search` next to `native:search`. Use the qualified name in `axp.describe` and `job.start`.
