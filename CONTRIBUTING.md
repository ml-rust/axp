# Contributing to AXP

Thanks for your interest in AXP. The protocol is being designed in the open, and contributions —
ideas, critiques, prototypes, and implementations — are all welcome. AXP is an early-stage draft, so
this is a good time to help shape it.

## Ways to contribute

- **Discuss the design.** Open an issue to ask a question, point out a gap, or challenge a decision.
  Honest technical pushback is valued.
- **Propose a change via RFC.** Anything that affects the protocol surface (messages, capabilities,
  discovery, security model, transports) goes through a written RFC so the rationale is recorded and
  reviewable. See *RFC process* below.
- **Prototype.** Experimental clients, providers, sandbox backends, or SDKs are great even if rough —
  real implementations surface real problems.
- **Improve docs.** Clarifications, examples, and corrections to `docs/` are appreciated.

## RFC process

1. **Open an issue first** to sketch the idea and gauge interest, so effort isn't wasted on something
   out of scope.
2. **Submit an RFC** by copying [`rfcs/0000-template.md`](rfcs/0000-template.md) and opening a PR.
   A good RFC covers: the problem, the proposed design, alternatives considered, security
   implications, and backward-compatibility. See [`rfcs/README.md`](rfcs/README.md) for details.
3. **Discussion happens on the PR.** Decisions favor clear technical reasoning and the project's
   design principles (see `docs/AXP-ARCHITECTURE.md`).
4. **Acceptance** is recorded in the RFC; implementation can then proceed.

Small, non-protocol changes (typos, doc fixes, bug fixes in tooling) can skip the RFC and go straight
to a PR.

## Design principles to keep in mind

AXP aims to be **safe by default, lean on context, and interoperable**. Proposals are weighed against:

- **Security first** — does it preserve least-privilege, the sandbox boundary, and control/data-plane
  separation? Changes that weaken the default security posture need strong justification.
- **Honest guarantees** — capabilities and sandbox tiers should state what they actually provide across
  platforms, not imply more.
- **Interoperability** — does it stay compatible with MCP interop where relevant?
- **Simplicity** — prefer the smallest design that solves the real problem.

## Development setup

The core runtime is written in **Rust** and targets Linux, macOS, and Windows on x86_64 and arm64.

```bash
# (placeholder — build instructions will land with the initial runtime)
cargo build
cargo test
```

Until the runtime lands, the most useful contributions are design discussion and RFCs.

## Code of conduct

Please be respectful and constructive. We follow the [Contributor Covenant](CODE_OF_CONDUCT.md).
Assume good faith, critique ideas rather than people, and keep discussion technical.

## Licensing

By contributing, you agree that your contributions are licensed under the project's **Apache License
2.0**. A Developer Certificate of Origin (DCO) sign-off may be required on commits (`git commit -s`);
this will be confirmed as the project's governance is set up.

## Questions

Open an issue. If you're not sure whether something is worth raising — raise it. Early input is
especially valuable while the design is still forming.
