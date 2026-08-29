# Repository Guidelines

## Project Overview

Symphony is an implemented Rust service for running Codex app-server work on eligible GitHub Issues in configured GitHub Projects v2. `SPEC.md` is the normative contract; `src/` contains the runtime, `tests/` contains integration and conformance-focused coverage, and Cargo is the build tool. `WORKFLOW.md` is the checked-in local runtime policy; `WORKFLOW.example.md` is the safe template for new local configurations.

## Architecture & Data Flow

Keep implementation boundaries aligned with `SPEC.md` and the existing modules:

1. `workflow.rs` loads `WORKFLOW.md`, parses optional YAML front matter, and supplies the prompt template.
2. `config.rs` resolves defaults and explicit `$VAR` references, validates typed configuration, and reloads workflows while retaining the last known-good config.
3. `tracker/github.rs` queries GitHub Project v2 issues and their configured Status field through the configured GraphQL endpoint.
4. `orchestrator/` owns scheduling, eligibility, retries, reconciliation, and runtime state.
5. `workspace.rs` and `hooks.rs` create/reuse contained workspaces and run lifecycle hooks.
6. `agent/codex.rs` and `agent/runner.rs` run the Codex app-server protocol and turn loop.
7. `service.rs` owns the process-level loop, worker tasks, reloads, refresh requests, shutdown, and status publication.
8. `observability/http.rs` provides the optional local dashboard and read-only HTTP API.

Main flow: startup validation -> terminal workspace cleanup -> workflow reload check -> poll/reconcile -> candidate sorting and dispatch -> workspace/hook lifecycle -> strict prompt rendering -> Codex turns -> retry, release, or cleanup according to issue state.

Each dispatched issue has one registered worker task. The service owns, cancels, and awaits those tasks during shutdown. Worker events and outcomes carry a dispatch generation and are applied only when that generation still matches the registered task. Keep that ownership and fencing intact when changing lifecycle code.

Each worker launches one Codex app-server process and initializes one session/thread for its workspace. It reuses that session for the configured sequence of turns, sends continuation prompts after the first turn, then shuts the session down before reporting its outcome. Do not turn continuations into independently spawned sessions.

## Repository Layout

- `src/agent/`: Codex JSONL protocol client and worker runner.
- `src/orchestrator/`: scheduler, retry, and runtime-state helpers.
- `src/tracker/`: GitHub Project v2 adapter.
- `src/observability/`: local HTTP dashboard and API.
- `src/config.rs`: typed configuration, validation, and reload support.
- `src/workflow.rs`: workflow parser and loader.
- `src/workspace.rs` / `src/hooks.rs`: path-safety checks, workspace lifecycle, and shell hooks.
- `src/service.rs`: service loop and worker lifecycle ownership.
- `src/main.rs`: CLI entry point; `src/lib.rs`: library exports.
- `tests/`: integration tests for CLI, workflow configuration, prompts, workspaces/hooks, tracker behavior, Codex protocol, workers, completion, orchestration, and HTTP UI/API.
- `Cargo.toml` / `Cargo.lock`: Rust package and locked dependencies.
- `WORKFLOW.md`: local runtime configuration and prompt template; `WORKFLOW.example.md`: committed template.
- `Dockerfile`, `docker-compose.example.yml`, and `.github/workflows/`: container and CI definitions.

## Development Commands

Use Cargo from the repository root:

```sh
cargo run -- path/to/WORKFLOW.md
cargo run -- --check path/to/WORKFLOW.md
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Run a focused integration test with `cargo test --test <test-target>`, for example `cargo test --test web_ui`. If Cargo incremental locking is problematic on the host filesystem, use `CARGO_INCREMENTAL=0 cargo test`.

Codex is launched through `bash -lc <codex.command>` with its working directory set to the per-issue workspace on non-Windows hosts. Hooks run trusted shell configuration through `sh -lc <script>` with their configured timeout (default `60000 ms`).

## Code Conventions & Common Patterns

- Treat `SPEC.md` as normative. Preserve RFC 2119 semantics when implementing behavior.
- Keep scheduler-state mutations in `orchestrator/` and service-side message application. Workers report events and outcomes; they do not mutate scheduling state directly.
- Use issue `id` for tracker/internal keys and `identifier` for logs and workspace naming.
- Normalize issue states for comparisons, labels to lowercase, and workspace keys by replacing non-`[A-Za-z0-9._-]` characters with `_`.
- Configuration resolution is: select workflow path, parse front matter, apply defaults, resolve explicit `$VAR` references, then coerce and validate types. Environment variables do not globally override YAML.
- Prompt rendering is strict: unknown variables or filters are errors.
- Keep tracker writes out of the orchestrator. Ticket state changes, comments, and PR metadata belong to the configured completion/workflow toolchain.
- Log stable `key=value` context, including `issue_id`/`issue_identifier` for issue logs and `session_id` for agent lifecycle logs. Never log API tokens or secret environment values.
- Preserve service availability: validation failures skip dispatch, tracker fetch failures skip a tick, reconciliation failures leave workers running, and worker failures become bounded retries.
- Security invariants are load-bearing: Codex runs only in its per-issue workspace, workspace paths remain under `workspace.root`, and hooks are trusted shell configuration with required timeouts.

## Runtime and HTTP Conventions

- The GitHub tracker defaults to `https://api.github.com/graphql`; `tracker.endpoint` may point at a compatible custom or GitHub Enterprise GraphQL endpoint.
- GitHub configuration requires repository owner/name and Project v2 owner/login/number; `project.owner_type` defaults to `organization`, and its configured Status field is the canonical issue state.
- Relative `workspace.root` values resolve relative to `WORKFLOW.md`; the default root is `<system-temp>/symphony_workspaces`.
- The local HTTP listener is optional and binds to loopback by default. It exposes `GET` state, sources, repositories, and issue-detail routes plus `POST /api/v1/refresh`.
- HTTP repository listing is read-only. `POST /api/v1/refresh`, which queues a poll and reconciliation pass, is the only HTTP mutation. Do not reintroduce workflow-writing API routes.
- Status responses omit tracker secrets, environment values, hook scripts, and prompt text. Hook stdout and stderr are inherited by the service process and visible in its runtime logs; hook output is not included in API responses.
