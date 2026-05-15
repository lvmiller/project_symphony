# Symphony

Symphony is a Rust implementation of the `SPEC.md` service contract: a long-running automation service that reads eligible work from an issue tracker, creates isolated per-issue workspaces, and runs Codex app-server turns for each issue.

This implementation targets GitHub Issues in GitHub Projects v2. The configured Project v2 Status field is the canonical Symphony issue state.

## What is implemented

- Workflow loading from `WORKFLOW.md` with optional YAML front matter.
- Typed config defaults, validation, `$VAR` indirection, relative path handling, and last-known-good reload behavior.
- GitHub GraphQL tracker client for Project v2 issue candidates, terminal cleanup fetches, and state refresh by node ID.
- Strict Liquid-compatible prompt rendering with `issue` and `attempt` inputs.
- Deterministic per-issue workspace creation/reuse with lifecycle hooks.
- Codex app-server JSONL stdio client for `codex-cli 0.121.0` message shapes.
- Orchestrator state, dispatch eligibility, retries, stall detection, reconciliation decisions, token/rate-limit aggregation, and runtime snapshots.
- CLI startup validation and structured logs to stderr.

Optional `SPEC.md` extensions are intentionally not included: HTTP status server, tracker write tools, and SSH worker support.

## Requirements

- Rust toolchain with Cargo.
- `codex` CLI with app-server support.
- GitHub token with access to the configured repository and Project v2.

The GitHub token is usually provided through `GITHUB_TOKEN` and referenced from `WORKFLOW.md` as `$GITHUB_TOKEN`.

## Build and verify

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

If the filesystem has Cargo incremental-lock issues, use:

```sh
CARGO_INCREMENTAL=0 cargo test
```

## Run

```sh
cargo run -- path/to/WORKFLOW.md
```

If no path is provided, Symphony uses `./WORKFLOW.md`.

For local setup, copy the committed example and customize repository/project values:

```sh
cp WORKFLOW.example.md WORKFLOW.md
```

The binary validates startup config before entering the service loop. It exits nonzero on startup failures and logs structured `key=value` events to stderr.

## Minimal `WORKFLOW.md`

```markdown
---
tracker:
  kind: github
  api_key: $GITHUB_TOKEN
  repository:
    owner: octo-org
    name: octo-repo
  project:
    owner_type: organization
    owner_login: octo-org
    number: 7
    status_field: Status
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Done
    - Closed
    - Canceled

workspace:
  root: ./.symphony-workspaces

agent:
  max_concurrent_agents: 2
  max_turns: 20

codex:
  command: codex app-server
---
You are working on GitHub issue {{ issue.identifier }}.

Title: {{ issue.title }}
State: {{ issue.state }}

Use the repository workflow and stop when the issue reaches the next handoff state.
```

## Configuration notes

Implementation-defined choices are part of the runtime contract:

- Tracker adapter: `tracker.kind: github`; repository-only issues are not dispatched unless they are in the configured Project v2.
- State source: GitHub Project v2 Status field maps to `issue.state`.
- Workspace population: Symphony creates/reuses directories only. Checkout, sync, dependency bootstrap, or cleanup policy belongs in hooks.
- Existing non-directory workspace path: fail safely; never replace user data.
- Codex default posture: high-trust defaults are used unless overridden in `WORKFLOW.md` with schema-valid Codex values.
- User input required by Codex: treated as a run failure so the worker does not stall indefinitely.
- Logging: structured logs are emitted to stderr; secrets must not be logged.

## Repository layout

```text
src/
  agent/          Codex protocol client and runner composition
  orchestrator/   Scheduler, retry, state, reconciliation helpers
  tracker/        GitHub Project v2 tracker adapter
  config.rs       Typed config and reload handling
  workflow.rs     WORKFLOW.md loader
  workspace.rs    Workspace path safety and lifecycle
  hooks.rs        Shell hook execution
  prompt.rs       Strict prompt rendering
  service.rs      Host service loop
  main.rs         CLI entrypoint

tests/            Conformance-focused integration tests
SPEC.md           Normative service specification
AGENTS.md         Repository guidance
```

## GitHub upload notes

Commit source, tests, `Cargo.toml`, `Cargo.lock`, `SPEC.md`, `AGENTS.md`, `.gitignore`, `WORKFLOW.example.md`, and this README.

Do not upload generated or local-only artifacts:

- `target/`, including generated Codex schemas under `target/codex-schema/`
- `.env` or `.env.*`
- `WORKFLOW.md` local runtime policy; commit `WORKFLOW.example.md` instead
- logs/temp files
- OS/editor metadata such as `.DS_Store`, `.idea/`, `.vscode/`
