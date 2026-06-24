# AXP RFCs

Substantial changes to AXP go through an **RFC (Request for Comments)** process, so the reasoning
behind the protocol stays visible and reviewable. This keeps the design honest and lets the community
weigh in before something becomes part of the spec.

## When an RFC is needed

Use an RFC for anything that affects the **protocol surface or its guarantees**, such as:

- New or changed messages, methods, or capabilities
- Changes to the discovery, execution, session, or sandbox model
- Security-model changes (capabilities, tiers, egress, control/data-plane separation)
- New transports or wire-format options
- MCP interop behavior
- Anything that affects backward compatibility

You do **not** need an RFC for: typo and documentation fixes, small bug fixes in tooling, examples,
or internal refactors that don't change observable behavior. Send those as a normal PR.

## Process

1. **Open an issue first** to float the idea and check scope/interest before investing in a full write-up.
2. **Copy `0000-template.md`** to `rfcs/0000-my-feature.md` (keep `0000` until a number is assigned)
   and open a pull request.
3. **Discussion happens on the PR.** Expect questions and revisions. Decisions favor clear technical
   reasoning and the project's design principles (see `docs/AXP-ARCHITECTURE.md` and `CONTRIBUTING.md`).
4. **On acceptance**, the RFC is assigned its number, merged with status `Accepted`, and implementation
   can begin. Rejected RFCs are still merged (with status `Rejected` and a short rationale) so the
   history of decisions — including the roads not taken — is preserved.

## Statuses

| Status       | Meaning                                         |
| ------------ | ----------------------------------------------- |
| `Draft`      | Under discussion on a PR                        |
| `Accepted`   | Approved; may be implemented                    |
| `Rejected`   | Not adopted; kept for the record with rationale |
| `Superseded` | Replaced by a later RFC (linked)                |

## Index

_(none yet — this is an early-stage project; the first RFCs are welcome)_
