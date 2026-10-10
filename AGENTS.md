# Yield engineering workflow

## Continuous integration and merging

- Develop features and fixes on a `codex/` branch in a pull request. Preserve
  unrelated working-tree changes and user libraries; use an isolated worktree
  when needed.
- Run CI locally on this Mac. Do not create, enable or trigger hosted CI.
  Use the repository's documented local check commands and pinned dependencies.
- Choose a small, meaningful validation set from the actual change and its risks.
  Check formatting, compilation and relevant existing tests. Run broader suites
  when subsystem boundaries, failures or unresolved concerns justify them.
- When the relevant automated checks pass on the exact PR head and review has no
  unresolved correctness findings, the agent is authorized to merge the PR
  without another user confirmation. Record the commands, results and source
  identity in the PR. Revalidate after code changes; do not infer success from a
  started job, stale run or skipped required check.
- Manual GUI, listening, hardware and display checks provide feedback and are
  not an additional merge gate for an otherwise green PR. Record unperformed or
  failed manual checks honestly in Pulse. Automated success does not establish
  physical acceptance, signing, notarization or public release readiness.
- When the user finds a bug that automation missed, fix it in a PR and add or
  adjust focused regression coverage for the underlying behavior. Prefer an
  existing test or shared fixture over a duplicate test family. If a physical
  behavior cannot be reproduced automatically, cover the closest meaningful
  dispatch or state boundary and state the remaining limitation.
- Keep tests lean, deterministic and fast: avoid duplicated scenarios, broad
  combinatorial matrices, unnecessary large fixtures and tests that merely
  mirror implementation details. Use expensive native, stress and performance
  workloads only when relevant, in separate explicit lanes. Do not weaken
  assertions, hide failures or increase deadlines merely to make CI green.
- Complete one acceptance pass after the chosen checks pass. Repeat checks only
  after a change, failure or unresolved concern warrants another run.

## Planning and machine state

- Use the installed Pulse tools against the live authority at
  `https://pulse.iocore.dev`. Discover and reuse projects, versions and tickets;
  read each ticket before updating it with its current revision. Preserve source
  context and track unrelated discoveries separately from the active PR.
- Do not create a local planner database, restore a stale planning snapshot or
  expose credentials in code, instructions or logs.
- Use the documented local-CI normal/clean/restore commands for profile testing.
  Preserve backups and ownership evidence; restore the user's normal state and
  verify it after a clean session. Preserve active DAW sessions and libraries.
- Do not check usage quotas, buy credits, change billing or use API-billed
  fallback inference as part of this workflow.
