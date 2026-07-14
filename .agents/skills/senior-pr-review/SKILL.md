---
name: senior-pr-review
description: Review pull requests, patches, diffs, commits, or working-tree changes as a strict senior software engineer. Use for evidence-driven code review focused on correctness, security and data exposure, backward compatibility, concurrency and state consistency, error handling, performance regressions, and test coverage rather than formatting or naming.
---

# Senior PR Review

Review the change as production code. Find concrete defects that the author should fix before merge. Do not edit code unless the user separately asks for fixes.

## Establish the review scope

1. Read repository instructions, relevant manifests, and CI configuration.
2. Identify the requested comparison range. If none is stated, inspect the working-tree and staged diff against `HEAD`.
3. Read the complete diff, then inspect affected callers, callees, types, tests, configuration, and public contracts in the surrounding code.
4. Understand the intended behavior from the request, tests, documentation, issue context, and existing behavior. State any scope assumption that materially limits the review.
5. Run narrow, non-mutating checks or tests when they can confirm or reject a suspected defect. Report commands only when their result matters to a finding or review limitation.

Review changes introduced by the patch. Do not report unrelated pre-existing defects unless the patch makes them reachable, more severe, or directly relies on the broken behavior.

## Prioritize risks

Review in this order:

1. Correctness and behavioral bugs.
2. Security, privacy, secrets, authorization, and data exposure.
3. Backward compatibility of APIs, protocols, schemas, persisted data, configuration, CLI behavior, and supported platforms.
4. Race conditions, ordering, atomicity, retries, idempotency, cancellation, and state consistency.
5. Missing, swallowed, misleading, or destructive error handling.
6. Performance or resource regressions on realistic paths.
7. Missing or weak tests for behavior changed by the patch.

Treat formatting, naming, and subjective style as non-findings unless they create a concrete correctness or maintenance hazard. Do not let cosmetic observations dominate the review.

## Require evidence

Report a finding only when the diff and repository context support a reproducible or logically necessary failure. Trace inputs and control flow far enough to establish that the scenario is reachable under supported use.

Before reporting, verify all of the following:

- The patch introduces or exposes the problem.
- The cited location is the smallest changed region that causes or should prevent it.
- A specific supported input, state, interleaving, deployment, or caller triggers the failure.
- The impact is meaningful enough for the author to act on.
- The recommended correction addresses the cause without assuming an undocumented redesign.

Do not report vague possibilities such as “could race,” “might be insecure,” or “may be slow” without showing the conflicting operations, trust boundary, hot path, data volume, or other concrete evidence. When evidence is insufficient, omit the finding or state it as a review limitation rather than an actionable defect.

For missing tests, identify the exact changed behavior and failure mode that existing tests do not cover. Do not request tests merely because a file changed or coverage could be higher.

## Assign severity

Use the lowest severity consistent with demonstrated impact:

- **Critical**: Enables severe compromise, irreversible widespread data loss, or systemic outage under realistic conditions; blocks merge.
- **High**: Causes security exposure, data corruption, major user-visible failure, or a breaking compatibility regression on a supported path; normally blocks merge.
- **Medium**: Produces incorrect behavior, inconsistent state, unhandled failure, or material degradation in a plausible but limited scenario; should be fixed before merge.
- **Low**: Causes a narrow, recoverable defect or leaves a specific regression-prone behavior untested; fix when the correction is proportionate.

Do not inflate severity to make a finding more prominent.

## Write actionable findings

Present findings first, ordered by severity and then by likelihood or blast radius. Keep each finding focused on one defect and include:

- **Severity**: Critical, High, Medium, or Low.
- **Location**: Repository-relative file path and the tightest relevant line or line range.
- **Failure scenario**: Concrete preconditions, execution path, and observable impact.
- **Recommended correction**: A specific implementation direction and, when useful, the regression test that proves it.

Use this format:

```markdown
### [High] Preserve authorization when using the cached result

- Location: `path/to/file.rs:120`
- Failure scenario: When ..., the new branch ..., allowing/causing ... .
- Recommended correction: Validate ... before ... and add a test that ... .
```

Tie the explanation directly to the cited code. Avoid long restatements of the diff and avoid bundling several independent problems into one finding.

## Conclude the review

If findings exist, add only a brief note about material review limitations or unverified platform behavior after them. If no actionable findings exist, say so explicitly and mention any meaningful validation gap. Never invent a finding to fill the response.
