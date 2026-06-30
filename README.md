# AXP — Agent Execution Protocol

**An open protocol that gives AI agents a secure, sandboxed place to run code and use tools — with
lean context usage. AXP works alongside MCP.**

> ⚠️ **Early draft.** AXP is in active design. The protocol and these documents will change. Feedback,
> discussion, and early implementations are very welcome.

---

## Why AXP

When an AI agent needs to do real work — run a command, edit files, call a tool, start a long-running
process — two things matter a lot: **safety** and **context cost**. AXP focuses on exactly that:

- **Safe execution.** Each agent gets an isolated workspace. Every action is capability-scoped,
  sandboxed at the OS level, and audited. The host is not exposed.
- **Lean context.** Tool discovery is lazy: the agent sees a compact index of what's available and
  loads full detail only for the tool it actually uses, so context stays small even with large catalogs.
- **Reliable long-running work.** Jobs stream their output and can be detached from and reattached to,
  so work survives disconnects.
- **Works with what you have.** AXP can mount static MCP tools into `axp serve`.

**MCP connects agents to tools; AXP focuses on giving agents a safe place to run them.** Many setups
will use both.

## How it fits together

```
Agent ──► AXP Session ──► isolated Workspace
                      ├─► Capabilities  (explicit, least-privilege, attenuable)
                      ├─► Sandbox       (OS-level: Landlock/seccomp · Seatbelt · AppContainer)
                      ├─► Jobs          (streaming logs, resumable, reattachable)
                      └─► Providers     (native · code-mode · MCP servers · skills)
```

A quick taste of the flow (illustrative):

```jsonc
session.open  { workspace, sandbox_tier, capabilities[] }   → { session_id }
axp.index     { session_id }                                → [{name, desc}, …]   // cheap, full catalog
axp.describe  { name }                                      → { signature, schema } // detail on demand
job.start     { command | code }                            → { job_id }          // streams logs, resumable
```

See **[docs/AXP-ARCHITECTURE.md](docs/AXP-ARCHITECTURE.md)** for the full design.

## Cross-platform

AXP is implemented in Rust and ships as a single static binary. It targets **Linux, macOS, and
Windows** on **x86_64 and arm64**. Sandbox enforcement uses each platform's native mechanism, and a
session always declares its enforcement tier so clients know the guarantee they're getting.

## Status

| Area                            | State                     |
| ------------------------------- | ------------------------- |
| Architecture draft              | In progress — see `docs/` |
| Runtime (Rust)                  | Early development         |
| MCP bridge                      | Static MCP tool mounts     |
| SDKs (TypeScript / Python / Go) | Planned                   |
| Conformance suite               | Planned                   |

Example:

```bash
axp serve \
  --mcp-provider docs \
  --mcp-tool search \
  --mcp-desc "Search documentation with an external MCP bridge" \
  --mcp-bridge axp-mcp-bridge
```

For one or more static tools from a single provider, `axp serve` also accepts a JSON MCP config:

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

Each JSON config defines one provider. This is a static mount format, not live MCP discovery or a
general AXP configuration file.

## Contributing

AXP is developed in the open and welcomes contributions — see **[CONTRIBUTING.md](CONTRIBUTING.md)**.
Design changes go through a public RFC process so the reasoning stays visible and reviewable.

## License

Licensed under the **Apache License 2.0**. See [LICENSE](LICENSE).
