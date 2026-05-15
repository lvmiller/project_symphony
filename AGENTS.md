# Repository Guidelines

## Project Overview

Symphony is specified as a long-running automation service that reads eligible issues from an issue tracker, creates isolated per-issue workspaces, and runs coding-agent sessions for those issues. This repository is currently specification-only: `SPEC.md` is the sole source artifact, and there is no implementation, test suite, build file, or package-manager config yet.

## Architecture & Data Flow

Keep implementation boundaries aligned with `SPEC.md`:

1. `Workflow Loader` reads `WORKFLOW.md`, parses optional YAML front matter, and returns `{ config, prompt_template }`.
2. `Config Layer` applies defaults, resolves explicit `$VAR` indirection, validates typed values, and supports dynamic reload with last-known-good fallback.
3. `Issue Tracker Client` fetches GitHub Project v2 candidate issues, terminal issues, and current states; it normalizes payloads into the stable issue model.
4. `Orchestrator` is the single owner of scheduling state (`running`, `claimed`, `retry_attempts`) and performs poll, reconcile, dispatch, retry, and release transitions.
5. `Workspace Manager` maps sanitized issue identifiers to `<workspace.root>/<workspace_key>`, runs hooks, and enforces workspace-root containment.
6. `Agent Runner` creates/reuses the workspace, renders the prompt, launches `codex app-server`, streams events, and reports outcomes back to the orchestrator.
7. `Logging` and optional status surfaces expose operator-visible state without becoming correctness dependencies.

Main flow: startup validation -> terminal workspace cleanup -> poll tick -> reconcile running issues -> validate config -> fetch active candidates -> sort by priority/age/identifier -> dispatch within concurrency limits -> create workspace -> render prompt strictly -> run Codex turn(s) -> retry, release, or clean up according to issue state.

## Key Directories

- `SPEC.md`: language-agnostic service contract and architecture reference.
- `AGENTS.md`: AI-assistant guidance for this repository.
- `WORKFLOW.md`: expected runtime contract file when an implementation/workflow is present; not currently in the repo.
- No `src/`, `tests/`, `scripts/`, `docs/`, or build/config directories exist yet.

When adding implementation code, prefer directories that mirror the spec layers (`workflow`, `config`, `tracker`, `orchestrator`, `workspace`, `agent_runner`, `observability`) rather than mixing responsibilities.

## Development Commands

No repository build, test, lint, or run commands are currently defined. Do not assume Node, Bun, Go, Rust, Python, or any package manager until tooling files are added.

Commands explicitly referenced by the spec:

```sh
codex app-server
codex app-server generate-json-schema --out <dir>
```

Runtime command execution patterns from the spec:

- Codex subprocess launch: `bash -lc <codex.command>` with `cwd` set to the per-issue workspace.
- Workspace hooks: POSIX `sh -lc <script>` or stricter `bash -lc <script>`; hook timeout defaults to `60000 ms`.

## Code Conventions & Common Patterns

- Treat `SPEC.md` as normative. Preserve RFC 2119 semantics when implementing behavior.
- Keep orchestration state mutations centralized in the orchestrator; workers report outcomes rather than mutating scheduler state directly.
- Use issue `id` for tracker/internal keys and `identifier` for logs/workspace naming.
- Normalize issue states for comparisons, labels to lowercase, and workspace keys by replacing non-`[A-Za-z0-9._-]` characters with `_`.
- Config resolution order: select workflow path, parse front matter, apply defaults, resolve explicit `$VAR` references, then coerce/validate typed values. Environment variables do not globally override YAML.
- Prompt rendering must be strict: unknown variables or filters are errors.
- Keep tracker writes out of the orchestrator. Ticket state changes, comments, and PR metadata belong to the workflow/agent toolchain.
- Log stable `key=value` context, always including `issue_id`/`issue_identifier` for issue logs and `session_id` for agent lifecycle logs. Never log API tokens or secret env values.
- Error handling should preserve service availability: validation failures skip dispatch, tracker fetch failures skip the tick, reconciliation failures keep workers running, and worker failures become bounded retries.
- Security invariants are load-bearing: Codex must run only in the per-issue workspace, workspace paths must remain under `workspace.root`, and hooks are trusted shell configuration with required timeouts.

## Important Files

- `SPEC.md`: primary reference for domain model, workflow schema, state machine, GitHub Projects v2 contract, Codex integration, observability, failure handling, and safety rules.
- `WORKFLOW.md` (expected, absent): repository-owned runtime policy file with YAML front matter plus Markdown prompt template.
- Future implementation should document any implementation-defined choices required by the spec, especially approval/sandbox policy, workspace population, logging sinks, and status surfaces.

## Runtime/Tooling Preferences

- Runtime is intentionally language-agnostic, but the host must support local filesystem workspaces, shell hooks, issue-tracker access, and a Codex app-server-compatible executable.
- Current tracker target is GitHub Projects v2 (`tracker.kind: github`) with default endpoint `https://api.github.com/graphql`; canonical env var is `GITHUB_TOKEN` when referenced from config.
- Required GitHub tracker config includes `repository.owner`, `repository.name`, `project.owner_login`, and `project.number`; `project.owner_type` defaults to `organization`, and the Project v2 Status field is the canonical issue state.
- Default workspace root is `<system-temp>/symphony_workspaces`; relative `workspace.root` values resolve relative to `WORKFLOW.md`.
- Codex protocol schemas and message shapes must come from the targeted Codex app-server version, not from hand-maintained assumptions in this repo.
- Approval, sandbox, operator-confirmation, workspace population, log sinks, and optional HTTP/status surfaces are implementation-defined and must be documented when implemented.

## Testing & QA

There is no current test framework, test command, CI config, or coverage policy.

When implementation begins, add focused tests around spec-critical behavior before declaring features done:

- `WORKFLOW.md` parsing, non-map front matter errors, strict template rendering, and dynamic reload last-known-good behavior.
- Config defaults, `$VAR` resolution, required GitHub repository/project fields, and invalid-value handling.
- Workspace key sanitization, root containment, hook timeout/failure semantics, and non-destructive reuse.
- Orchestrator candidate eligibility, concurrency limits, retry backoff, reconciliation, terminal cleanup, and restart recovery assumptions.
- GitHub Project v2 pagination, payload normalization, error category mapping, and malformed response handling.
- Codex runner launch cwd validation, timeout/stall handling, event/token accounting, unsupported tool calls, and user-input-required policy.
- Logging assertions for required context fields and secret redaction.
