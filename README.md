<div align="center">

# axp

<h3>A protocol for sandboxed agent execution with lean context use.</h3>

<p>
AXP gives AI agents a sandboxed place to run code and tools. On Linux, `kernel-lsm` jobs run under Landlock and seccomp-bpf.
</p>

<p>
  <a href="#protocol"><strong>Protocol</strong></a>
  ·
  <a href="#sandbox-tiers"><strong>Sandbox</strong></a>
  ·
  <a href="#mcp-mounts"><strong>MCP</strong></a>
  ·
  <a href="#quick-start"><strong>Example</strong></a>
  ·
  <a href="CONTRIBUTING.md"><strong>Contributing</strong></a>
</p>

<p>
  <a href="https://discord.gg/jBhFk9kHPg">
    <img src="https://img.shields.io/discord/1453357769720594543?label=Discord&logo=discord&logoColor=white&color=5865F2" alt="Join the Discord">
  </a>
</p>

<p>
  <a href="LICENSE">
    <img src="https://img.shields.io/github/license/ml-rust/axp" alt="License">
  </a>
  <a href="https://github.com/ml-rust/axp/stargazers">
    <img src="https://img.shields.io/github/stars/ml-rust/axp?style=social" alt="GitHub stars">
  </a>
</p>

</div>

AXP (Agent Execution Protocol) gives each agent a session. The session binds a workspace, capability grants, and a streamed job log. The agent reads a compact tool index and loads a tool's schema only when it selects that tool. AXP works alongside MCP.

AXP is an early draft. The protocol and the runtime will change.

## Protocol

An agent opens a session with `session.open`. The session holds a workspace, a sandbox tier, and capability grants.

```text
Agent ──► session.open ──► Session
                             ├─► Workspace     (root directory for file access)
                             ├─► Sandbox tier  (enforced for each job)
                             ├─► Capabilities  (grants, checked per call)
                             ├─► Discovery     (axp.index, axp.describe)
                             ├─► Jobs          (command, code, capability)
                             └─► Audit         (session.audit)
```

- **Capabilities.** A grant names one permission: `fs.read(<path>)`, `fs.write(<path>)`, `proc.spawn`, `tool:<name>`, `net.connect(<domain>)`, or `pty.open`. A job can request only the grants that its session holds. The [Sandbox tiers](#sandbox-tiers) section lists what each grant enforces.
- **Discovery.** `axp.index` returns each capability with a one-line description. `axp.describe` returns the signature and JSON Schema for one name.
- **Jobs.** `job.start` runs a shell command, a base64 WebAssembly component, or a named capability. Output streams as log frames. Jobs keep running when the client disconnects. `GET /job/attach` replays the log from a sequence offset.
- **Audit.** `session.audit` returns session open and close events and job start and finish events. The audit log does not cover file or network activity.

The methods, parameters, and error codes are listed in [docs/RUNTIME-API.md](docs/RUNTIME-API.md).

The protocol design is in [docs/AXP-ARCHITECTURE.md](docs/AXP-ARCHITECTURE.md). Changes to the protocol go through an RFC in [rfcs/](rfcs/README.md).

## Quick Start

Prerequisites: Rust 1.85 or later. AXP is not on crates.io, so build it from this repo.

```bash
cargo build --release -p axp
./target/release/axp serve
```

The server prints `AXP runtime listening on http://127.0.0.1:7300` and runs until you stop it.

Open a second terminal. Open a session that allows command jobs:

```bash
curl -s http://127.0.0.1:7300/ \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"workspace":"/absolute/path/to/project","sandbox_tier":"dev-none","capabilities":["proc.spawn"]}}'
```

The `result` object holds `session_id` and `cap_token`. Copy both values into the next requests. The `dev-none` tier enforces no sandbox. Use `kernel-lsm` on Linux to enforce one.

Start a command job:

```bash
curl -s http://127.0.0.1:7300/ \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"job.start","params":{"session_id":"<session_id>","cap_token":"<cap_token>","kind":"command","command":"echo hello"}}'
```

The `result` object holds `job_id`. Stream the job log:

```bash
curl -N 'http://127.0.0.1:7300/job/attach?session_id=<session_id>&cap_token=<cap_token>&job_id=<job_id>&from_offset=0'
```

Each `data:` line holds the frame. The `data` field is a byte array. For `echo hello`, it is `[104,101,108,108,111,10]`.

`axp demo runtime` runs one session through index, job, and attach against a running server.

### Serve flags

| Flag | Default | Purpose |
| ---- | ------- | ------- |
| `--addr <socket-addr>` | `127.0.0.1:7300` | Address to bind. |
| `--mcp-provider <id>` | None | Provider id for one static MCP tool. Use with the other three `--mcp-*` flags. |
| `--mcp-tool <name>` | None | Tool name for one static MCP tool. |
| `--mcp-desc <text>` | None | Description for one static MCP tool. |
| `--mcp-bridge <program>` | None | Bridge program that runs the tool. |
| `--mcp-config <path>` | None | JSON file that mounts one or more tools. Do not combine with the other `--mcp-*` flags. |

## Sandbox tiers

Each session declares a sandbox tier. A job fails to start when the host cannot enforce its tier. The runtime never falls back to a weaker tier.

| Tier | Enforcement | Support |
| ---- | ----------- | ------- |
| `kernel-lsm` | Landlock limits file access. A seccomp-bpf filter blocks `ptrace`, `process_vm_readv`, `process_vm_writev`, and io_uring calls. | Linux with Landlock. Other systems fail the job at start. |
| `dev-none` | None | All systems. Development only. |
| `container` | Not implemented | Job start fails. |
| `process-token` | Not implemented | Job start fails. |

Under `kernel-lsm`, a job reads the workspace and the `fs.read` paths. It writes only to `fs.write` paths.

| Grant | Enforced by | Notes |
| ----- | ----------- | ----- |
| `fs.read(<path>)` | Landlock under `kernel-lsm` | The workspace root is always readable. |
| `fs.write(<path>)` | Landlock under `kernel-lsm` | The workspace root is read-only unless a write grant covers it. |
| `proc.spawn` | Engine check | Required for `command` jobs. The seccomp filter does not block `execve`. |
| `tool:<name>` | Engine check | Required for `capability` jobs. Authorizes one named capability. |
| `net.connect(<domain>)` | None | Accepted and parsed. The runtime does not filter or block network access. |
| `pty.open` | None | Accepted and parsed. The runtime does not open a PTY. |

Sandboxing limits what a job can reach. It does not stop prompt injection. The design also covers macOS Seatbelt and Windows AppContainer backends. See [docs/AXP-ARCHITECTURE.md](docs/AXP-ARCHITECTURE.md#72-kernel-sandbox-portable-contract-per-os-backends).

## MCP mounts

AXP does not implement the MCP wire protocol. `axp serve` mounts static tool descriptors. Each call runs a bridge program that you supply. This repo does not ship a bridge program.

```bash
axp serve \
  --mcp-provider docs \
  --mcp-tool search \
  --mcp-desc "Search documentation with an external MCP bridge" \
  --mcp-bridge axp-mcp-bridge
```

To mount one or more tools from one provider, pass a JSON file with `--mcp-config mcp.json`. The file format, validation rules, and call argv are in [docs/MCP-MOUNTS.md](docs/MCP-MOUNTS.md).

## Status

| Area | State |
| ---- | ----- |
| Protocol | Early draft. See [docs/AXP-ARCHITECTURE.md](docs/AXP-ARCHITECTURE.md). |
| Runtime (`axp serve`) | Early development. Sessions and jobs live in memory, so a server restart ends them. |
| Sandbox | `kernel-lsm` on Linux only. |
| Code mode | WebAssembly components only. |
| Static MCP mounts | Available. |
| Rust client | `axp-client` crate in this workspace. |
| SDKs (TypeScript, Python, Go) | Planned. |

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for conventions and checks.

Questions and design discussion happen on [Discord](https://discord.gg/jBhFk9kHPg).

## License

Licensed under Apache-2.0. See [LICENSE](LICENSE).
