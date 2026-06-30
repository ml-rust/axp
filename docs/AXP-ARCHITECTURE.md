# AXP — Agent Execution Protocol

## Technical Architecture (v0 draft)

> **What AXP is.** An open protocol that gives AI agents a secure, streaming place to run code
> and use tools — without exposing the host. AXP defines a **sandbox-and-capability contract**:
> an isolated session, explicit least-privilege capabilities, OS-level sandboxing, and structured
> streaming execution. It is **complementary to MCP** and currently ships a static MCP tool mount
> in `axp serve`.
>
> **Relationship to MCP.** MCP standardizes how agents discover and call tools. AXP focuses on a
> different concern: giving an agent a safe, isolated environment to _execute_ in. The two compose,
> with current shipped support limited to a static MCP tool mount (see §8).

This document describes the protocol's object model, layers, and security design. It is a working
draft and will evolve through an open RFC process.

---

## 1. Design principles

AXP is organized around five ideas:

1. **Session-first** — every agent gets an isolated, stateful, resumable execution session.
2. **Workspace-first** — each session is bound to an isolated filesystem workspace.
3. **Stream-first** — execution is a structured async job with streaming logs, not atomic call/return.
4. **Capability-first** — the agent holds explicit, least-privilege, attenuable capabilities, never blanket access.
5. **Sandbox-first** — a kernel-enforced boundary is established by a trusted supervisor _before_ the session starts.

**In one line:** _AXP gives agents a safe place to operate — secure, streaming, and lean on context._

---

## 2. The object model

```
Client (agent host)
   │  opens
   ▼
Session ── bound to ──► Workspace        (isolated filesystem root)
   │                └─► SandboxTier      (declared, enforced before first job)
   │                └─► CapabilitySet    (explicit, attenuable grants)
   │
   ├── Discovery ──► CapabilityIndex (name + 1-line desc)  →  describe(name) on demand
   ├── Execution ──► Job (async, streaming logs, durable, reattachable)
   │                └─► optional PTY (interactive programs only)
   └── Providers ──► Native | CodeMode | MCP-Bridge | Skill
```

| Primitive           | Definition                                                                                                                                                                                                                       |
| ------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Session**         | The unit of stateful execution context. Resumable by `session_id`. Carries the workspace, sandbox tier, capability set, and audit stream.                                                                                        |
| **Workspace**       | An isolated filesystem root the session may touch. Default-deny outside it.                                                                                                                                                      |
| **Capability**      | An explicit, **unforgeable, attenuable** permission token: `fs.read(path)`, `fs.write(path)`, `net.connect(domain)`, `proc.spawn`, or a named tool/skill. A holder can derive a _weaker_ child capability, never a stronger one. |
| **SandboxTier**     | The enforcement guarantee in effect: `kernel-lsm` \| `container` \| `process-token` \| `dev-none`. Declared per session; cannot be self-downgraded.                                                                              |
| **Provider**        | A backend that supplies capabilities. Native (syscalls/CLIs), CodeMode (sandboxed code runtime), MCP-Bridge (mounts MCP servers), Skill (procedure bundles).                                                                     |
| **Job**             | A structured async execution (a command or a code submission) with a reliable, ordered, **resumable** log stream. Decoupled from connection lifetime.                                                                            |
| **CapabilityIndex** | The discovery surface: every capability as `name — one-line description`. Full typed schema fetched only on demand.                                                                                                              |

---

## 3. Layered protocol stack

| Layer            | Responsibility                                                    | Approach                                                                                                                                          |
| ---------------- | ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| **L0 Transport** | Move framed messages                                              | Streamable-HTTP baseline (single endpoint, POST → resumable SSE, `Last-Event-ID`); stdio for local; optional gRPC / Cap'n Proto binary fast-path. |
| **L1 Control**   | Handshake, auth, capability negotiation, sandbox-tier declaration | Stateful session; OAuth/OIDC supported where the identity provider differs from the server.                                                       |
| **L2 Discovery** | Lazy capability discovery                                         | Compact index for breadth + full schema on demand (§6). Registration includes a description-quality check.                                        |
| **L3 Execution** | Run jobs, stream logs, reattach                                   | Structured async jobs + durable replay buffer; opt-in PTY capability.                                                                             |
| **L4 Providers** | Supply capabilities                                               | Native / CodeMode / MCP-Bridge / Skill behind one `Provider` interface.                                                                           |
| **L5 Security**  | Enforce the boundary                                              | Object-capability tokens + kernel sandbox + egress proxy + audit + control/data-plane separation.                                                 |

**A note on token efficiency.** The model never sees L0 framing bytes — they are parsed and
discarded before tokenization. So the wire format is a _latency/bandwidth_ concern, not a _context_
concern. Context efficiency lives entirely at L2: what enters the model's context window.

---

## 4. Wire formats — two independent concerns

1. **Model-facing representation (L2)** — what the model reads:
   - **Discovery index:** `name — one-line description` — compact, and sufficient to choose a tool (§6).
   - **On-demand detail:** **TypeScript-style signatures** + JSON Schema 2020-12 (for validation),
     generated from a single IDL source. TypeScript-style signatures are compact and align with how
     models read code.
   - The model emits **JSON or code** — formats it reads fluently.
2. **Network representation (L0)** — what crosses the socket:
   - **JSON-RPC 2.0** baseline (universal, MCP-compatible).
   - Optional negotiated **Cap'n Proto** (object-capability references) or **Protobuf** binary fast-path
     for latency-sensitive deployments.

---

## 5. Execution model — structured async jobs

Execution is modeled as a structured async **job** with a streaming log, rather than a live interactive
terminal — this matches how agents reason over discrete, finite results.

```
job.start { command | code, capabilities[], cwd }     → { job_id }      # idempotent
   ↳ stream: ordered, reliable, resumable log events   (SSE / gRPC)
job.attach { job_id, from_offset }                     → replays from durable buffer
job.signal { job_id, signal }                          → SIGINT / custom
job.cancel { job_id }
job.status { job_id }                                  → running | exited(code) | killed
```

**Reliability (transport-independent):**

- Command lifetime is **decoupled** from connection lifetime.
- **Reattach-by-job-id** with an **offset/epoch durable replay buffer**: an agent can detach, do other
  work, reconnect, and read the full log — output is never lost.
- **Idempotent** job start; explicit **in-band cancel/signal**.
- The model's log channel is **reliable, ordered, and resumable**.

**Code-mode** (first-class): the agent submits code that calls capabilities as ordinary SDK functions,
executed inside the sandbox. This keeps context lean and lets the agent compose steps without an
inference round-trip per step. The execution runtime is intentionally lightweight (e.g. an embeddable
JS engine, Python, or shell) — no mandatory heavy runtime dependency.

**PTY** is an **opt-in capability** (`pty.open`) for genuinely interactive programs (REPLs, `ssh`,
password prompts, editors). Cross-platform: openpty (Unix) / ConPTY (Windows).

---

## 6. Discovery — index for breadth, schema on demand

The model is shown a **compact index of every capability** (`name — description`) so it retains full
awareness of what is available and can choose correctly. The **full typed schema is fetched only for
the capability it selects**, to fill in arguments.

```
axp.index            → [{name, desc}, …]    # the whole catalog, cheaply
axp.describe(name)   → {signature, schema}  # full detail, only for the chosen tool
```

- **Adaptive:** very small catalogs may inline full detail (the extra round-trip is not worth saving);
  large catalogs stay index-only, optionally with search/ranking over providers.
- **Description quality is the key factor in good selection.** Tool registration therefore includes a
  **conformance check** that flags vague or near-duplicate descriptions before a capability is published.

---

## 7. Security model — zero-trust local execution

### 7.1 Object-capability core

- Capabilities are **unforgeable references**, not forgeable strings — possession is authority.
- **Attenuation:** any holder can mint a strictly weaker child capability (narrower path, fewer verbs,
  shorter TTL), enabling safe delegation to sub-agents or code-mode.

### 7.2 Kernel sandbox (portable contract, per-OS backends)

The agent requests capabilities **abstractly**; a trusted supervisor compiles them to OS-native policy.
The agent **never writes OS-specific policy** — this is both the cross-platform layer and a key defense
against config-based escapes.

| OS / Arch              | Enforcement backend                                             | Tier            |
| ---------------------- | --------------------------------------------------------------- | --------------- |
| Linux (x86_64/arm64)   | Bubblewrap + **Landlock** + **seccomp-bpf** + network namespace | `kernel-lsm`    |
| macOS (x86_64/arm64)   | **Seatbelt** / `sandbox-exec` (SBPL)                            | `kernel-lsm`    |
| Windows (x86_64/arm64) | **AppContainer** + Restricted Token + Job Object + per-user WFP | `process-token` |
| any                    | container / microVM (Firecracker / libkrun) fallback            | `container`     |

- **The tier is declared per session.** A client can refuse to run sensitive work under a weaker tier.
  A session's tier **cannot be silently downgraded** by agent-local configuration.
- Sandbox guarantees differ by platform; AXP states the tier honestly rather than implying uniform isolation.

### 7.3 Network egress

Operating systems do not provide a kernel primitive for _domain_ allowlisting, so AXP routes network
capabilities through an **out-of-process egress proxy** in a per-process network namespace
(default-deny + allowlist). Residual risks (SNI fronting, ECH, DNS-based exfiltration) are documented
rather than glossed over.

### 7.4 Control/data-plane separation

- Sandbox **policy is control-plane** and lives **outside** the workspace. The agent process cannot
  author or disable its own sandbox.
- Configuration and rules files are treated as **untrusted data**; they (and their parent directories)
  are mounted read-only.
- Policy is compiled by the supervisor **before** the session is initialized.

### 7.5 Honest limits

In-OS sandboxes share the host kernel. Sandboxing contains **blast radius**; it does not by itself stop
prompt injection. Robust deployments combine it with control/data-plane separation, egress filtering,
and human approval for irreversible actions. AXP ships an **append-only audit log** (every command,
file mutation, capability request, and network attempt) as a first-class primitive.

---

## 8. MCP interoperability — current mount mode

AXP is designed to work with the existing MCP ecosystem, not around it.

1. **AXP-alone** — the agent runs against native capabilities / code-mode / CLIs; no MCP involved.
2. **AXP + MCP** — the runtime **mounts MCP servers as a `Provider`**: each MCP tool becomes an AXP
   capability (surfaced via the index + schema-on-demand + code-mode SDK, and sandboxed and audited
   like any other capability).

Current shipped surface: `axp serve` can mount exactly one static MCP tool provider when all four
flags are supplied together: `--mcp-provider`, `--mcp-tool`, `--mcp-desc`, and `--mcp-bridge`. The
bridge process is invoked directly via argv, with a fixed `call` prefix plus provider, tool, and
params arguments.

AXP internals are not coupled to MCP, and MCP is never required — it is one `Provider` implementation
behind a clean interface.

---

## 9. Implementation

- **Core: Rust.** A single static binary, cross-compiled to **Linux / macOS / Windows × x86_64 / arm64**.
  No GC, direct kernel/PTY access, first-class ARM. Runs as a local daemon or a remote server.
- **SDKs:** TypeScript, Python, Go as thin bindings over the Rust core.
- **Single schema source:** one IDL → generates TypeScript signatures (model-facing) + JSON Schema
  (validation) + binary descriptors (transport).

---

## 10. Minimal message sketch (illustrative, JSON-RPC baseline)

```jsonc
// 1. open a sandboxed session
→ {"method":"session.open","params":{
     "workspace":"/proj","sandbox_tier":"kernel-lsm",
     "capabilities":["fs.read(/proj)","fs.write(/proj)","proc.spawn","net.connect(api.github.com)"]}}
← {"result":{"session_id":"s_91","granted_tier":"kernel-lsm","cap_token":"…"}}

// 2. lazy discovery — cheap index of everything
→ {"method":"axp.index","params":{"session_id":"s_91"}}
← {"result":[{"name":"git_diff","desc":"Show uncommitted working-tree changes"}, …]}

// 3. detail only for the chosen tool
→ {"method":"axp.describe","params":{"name":"git_diff"}}
← {"result":{"signature":"git_diff(): string","schema":{ /* JSON Schema */ }}}

// 4. structured async job with streaming logs
→ {"method":"job.start","params":{"session_id":"s_91","command":"git diff"}}
← {"result":{"job_id":"j_5"}}
← (stream) {"job":"j_5","seq":0,"stream":"stdout","data":"diff --git …"}
← (stream) {"job":"j_5","exit":0}

// reattach after disconnect — durable replay from offset
→ {"method":"job.attach","params":{"job_id":"j_5","from_offset":0}}
```

---

## 11. Status & contributing

This is an early draft. The protocol is developed in the open under an Apache-2.0 license, with changes
proposed through a public RFC process. Feedback, implementations, and conformance reports are welcome —
see `CONTRIBUTING.md` (forthcoming) and the RFC repository.
