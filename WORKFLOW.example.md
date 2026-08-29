---
tracker:
  kind: github
  api_key: $GITHUB_TOKEN
  repositories:
    - owner: your-org-or-user
      name: your-repo
  project:
    owner_type: user # user or organization
    owner_login: your-org-or-user
    # From https://github.com/users/<owner>/projects/<number> or /orgs/<owner>/projects/<number>
    number: 1
    status_field: Status
    priority_field: Priority
  active_states:
    - Ready
    - In progress
  terminal_states:
    - Done
  priority_labels:
    P0: 100
    P1: 75
    P2: 50

server:
  host: 127.0.0.1
  port: 8080

polling:
  interval_ms: 60000

workspace:
  root: ./.symphony-workspaces
  cleanup:
    after_success: committed
  # Omit population (or use kind: none) to retain empty-directory behavior.
  # Population runs before lifecycle hooks. `git` clones only newly created workspaces.
  population:
    kind: git
    repository_url: https://github.com/your-org-or-user/your-repo.git
    branch: main # Or use `ref` for a specific commit or tag; do not set both.
    depth: 1
    # On reuse, fetch and merge only when the current checkout can fast-forward.
    # Use `skip` (the default) to leave reused workspaces entirely unchanged.
    reuse: fetch_ff_only

hooks:
  timeout_ms: 120000
  # Hooks run in the workspace directory with the host's trusted shell.
  # Symphony provides SYMPHONY_HOOK_NAME, SYMPHONY_WORKSPACE_PATH,
  # SYMPHONY_WORKSPACE_KEY, SYMPHONY_SOURCE_ID, SYMPHONY_SOURCE_KEY,
  # SYMPHONY_ISSUE_IDENTIFIER, and SYMPHONY_ISSUE_KEY. The *_KEY values are
  # sanitized identifiers suitable for paths. Failure diagnostics retain bounded,
  # redacted stdout/stderr excerpts.
  after_create: |
    printf 'preparing %s for %s\n' "$SYMPHONY_WORKSPACE_PATH" "$SYMPHONY_ISSUE_IDENTIFIER"
agent:
  max_concurrent_agents: 1
  max_turns: 1
  max_retry_backoff_ms: 300000
  max_concurrent_agents_by_state:
    In progress: 1

codex:
  command: codex app-server
  approval_policy: never
  thread_sandbox: danger-full-access
  turn_sandbox_policy:
    type: dangerFullAccess
  turn_timeout_ms: 3600000
  read_timeout_ms: 5000
  stall_timeout_ms: 300000

completion:
  direct_commit:
    enabled: true
    base_branch: main
    started_state: In progress
    high_review_state: In review
    auto_approved_state: Done
    commit_author_name: Symphony
    commit_author_email: symphony@users.noreply.github.com
---
You are working in an isolated Symphony workspace for GitHub issue {{ issue.identifier }}.

Repository: your-org-or-user/your-repo
Issue URL: {{ issue.url }}
Issue title: {{ issue.title }}
Issue state: {{ issue.state }}
Attempt: {{ attempt }}

Issue description:
{{ issue.description }}

Instructions:
- Treat repository guidance and specifications as authoritative.
- Implement only the requested issue; do not broaden scope.
- Preserve workspace safety: do not write outside the current workspace.
- Run focused verification for the behavior you change.
- The issue title starts with `[Low]`, `[Medium]`, or `[High]`; treat that prefix as the severity for review policy.
- Do not create branches, open pull requests, push, or mutate GitHub tracker state from this run; Symphony will commit verified changes to main and move the project item according to severity.
- Stop after one complete implementation-and-verification pass and report what changed, what was verified, and any remaining blocker.
