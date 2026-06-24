# RFC 0000 — <Title>

|                    |                                      |
| ------------------ | ------------------------------------ |
| **Status**         | Draft                                |
| **Author(s)**      | <your name / handle>                 |
| **Created**        | <YYYY-MM-DD>                         |
| **Tracking issue** | <link to the issue opened in step 1> |
| **Supersedes**     | <RFC number, or —>                   |

## Summary

One paragraph: what is being proposed, in plain language.

## Motivation

What problem does this solve? Who hits it, and how? What is broken, missing, or awkward today?
Be concrete — examples and real scenarios are more persuasive than abstractions.

## Guide-level explanation

Explain the proposal as if teaching it to a developer using AXP. Use examples, message sketches, and
the protocol's existing terminology (session, workspace, capability, job, provider, sandbox tier).
Someone should be able to read just this section and understand what changes for them.

```jsonc
// illustrative message(s), if relevant
```

## Reference-level explanation

The precise, detailed design. Cover:

- Exact message/field/capability definitions and their semantics
- How it interacts with existing parts of the protocol
- Edge cases and error behavior
- Versioning / negotiation, if it adds or changes wire surface

## Security implications

AXP is security-sensitive, so this section is required. Address:

- Does this change the capability model, sandbox boundary, or egress controls?
- Could it weaken least-privilege or the control/data-plane separation? If so, why is that acceptable,
  and what mitigates it?
- What are the honest limits of the guarantee being added?

## Backward compatibility

Is this a breaking change to the wire contract? How do older clients/servers behave? Is it
feature-negotiated? Describe any migration path.

## Alternatives considered

What other designs were weighed, and why was this one chosen? "Do nothing" is a valid alternative to
discuss. Recording the roads not taken is part of the point of an RFC.

## Unresolved questions

What is intentionally left open for discussion on the PR or for follow-up RFCs?
