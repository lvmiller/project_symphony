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

The optional local HTTP UI/API is implemented as an operator observability surface. Its configured repository listing is read-only; `POST /api/v1/refresh` is its only mutation. Tracker write tools and SSH worker support remain intentionally outside this implementation.

## Requirements

- Rust toolchain with Cargo.
- `codex` CLI with app-server support.
- Docker or another OCI runtime if building/running the container image.
- GitHub token with access to the configured repository and Project v2; when `completion.direct_commit.enabled` is true, the token must be able to push repository contents and update the Project v2 status field.

The GitHub token is usually provided through `GITHUB_TOKEN` and referenced from `WORKFLOW.md` as `$GITHUB_TOKEN`. The GitHub GraphQL endpoint defaults to `https://api.github.com/graphql`; set `tracker.endpoint` for GitHub Enterprise or another compatible endpoint.

## Build and verify

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Container image:

```sh
docker build -t symphony:local .
```

If the filesystem has Cargo incremental-lock issues, use:

```sh
CARGO_INCREMENTAL=0 cargo test
```

## Run

```sh
cargo run -- path/to/WORKFLOW.md
```

Run with the local dashboard/API enabled from the CLI:

```sh
cargo run -- --port 8080 path/to/WORKFLOW.md
```

Or enable it from workflow front matter:

```sh
cargo run -- path/to/WORKFLOW.md
```

```yaml
server:
  port: 8080
```

If no path is provided, Symphony uses `./WORKFLOW.md`.

For local setup, copy the committed example and customize repository/project values:

```sh
cp WORKFLOW.example.md WORKFLOW.md
```

The binary validates startup config before entering the service loop. It exits nonzero on startup failures and logs structured `key=value` events to stderr. `--check` validates config and exits without binding the optional HTTP listener, even when `--port` is supplied.

The dashboard binds to loopback `127.0.0.1` by default when enabled. Use `--host 0.0.0.0 --port 8080` only behind trusted network controls. The service reloads `WORKFLOW.md` on subsequent ticks while preserving its last known-good configuration; `POST /api/v1/refresh` requests an immediate poll/reconcile pass. Repository configuration remains workflow-owned and is not writable over HTTP.

HTTP API routes:

- `GET /` serves the dashboard.
- `GET /api/v1/state` returns `generated_at`, `counts` (`running`, `retrying`, `sources`), running and retrying issue arrays, `codex_totals` (input, output, total tokens and seconds running), and optional `rate_limits`.
- `GET /api/v1/sources` returns `generated_at` and loaded source summaries: source ID, workflow path, configured repositories and Project v2 settings, active/terminal states, polling interval, and workspace root. Secrets, hook scripts, and prompt text are omitted.
- `GET /api/v1/repositories?source_id=<id>` returns the selected source ID, workflow path, and its configured repository `{owner,name}` entries. This endpoint is read-only.
- `GET /api/v1/{issue_identifier}` returns the known issue's source and issue IDs, status, workspace, attempt, running/retry, recent-event, error, and log metadata. Percent-encode identifiers containing `#`.
- `POST /api/v1/refresh` is the sole HTTP mutation. It returns `202 Accepted` with `queued`, `coalesced`, `requested_at`, and `operations` (`poll`, `reconcile`).

API 404 and 405 responses are JSON: `{"error":{"code":"<code>","message":"<message>"}}`. Unknown API routes return `route_not_found`; unsupported methods return `method_not_allowed`. Unknown sources and issues also return JSON 404 errors.

## Run in Docker

Use container paths in `WORKFLOW.md`. The committed `docker-compose.example.yml` bind-mounts `WORKFLOW.md` read-only: startup and reload only read the workflow, and repository configuration is not writable over HTTP. It mounts `./.symphony-workspaces` at the container path expected by the image without auto-creating missing host paths. Because the container workdir is `/app`, the committed example's relative `workspace.root` points at `/app/.symphony-workspaces`.

Create `WORKFLOW.md` and the workspace directory, then run one Compose service for the Codex auth method you use. For API-key auth, export `OPENAI_API_KEY` and run the default service:

```sh
cp -n WORKFLOW.example.md WORKFLOW.md
mkdir -p .symphony-workspaces
docker compose -f docker-compose.example.yml up --build symphony
```

For Codex subscription/login auth, mount your Codex home instead. By default the example uses `$HOME/.codex`; set `CODEX_HOME` if your credentials live elsewhere:

```sh
cp -n WORKFLOW.example.md WORKFLOW.md
mkdir -p .symphony-workspaces
docker compose -f docker-compose.example.yml up --build symphony-codex-home
```

For configuration-only validation, use the same service name with `run --rm --build`:

```sh
docker compose -f docker-compose.example.yml run --rm --build symphony --check
docker compose -f docker-compose.example.yml run --rm --build symphony-codex-home --check
```

Pass any additional Codex authentication variables or mounted credential files required by the installed `codex` CLI. The image includes `tini` as PID 1 for signal forwarding and child reaping.

Equivalent `docker run` commands use the same Codex auth choice. API-key auth:

```sh
docker run --rm \
  -e GITHUB_TOKEN \
  -e OPENAI_API_KEY \
  -p 127.0.0.1:8080:8080 \
  --mount type=bind,src="$PWD/WORKFLOW.md",dst=/app/WORKFLOW.md,readonly \
  --mount type=bind,src="$PWD/.symphony-workspaces",dst=/app/.symphony-workspaces \
  symphony:local --host 0.0.0.0 --port 8080
```

Codex home auth:

```sh
docker run --rm \
  -e GITHUB_TOKEN \
  -p 127.0.0.1:8080:8080 \
  --mount type=bind,src="$PWD/WORKFLOW.md",dst=/app/WORKFLOW.md,readonly \
  --mount type=bind,src="$PWD/.symphony-workspaces",dst=/app/.symphony-workspaces \
  --mount type=bind,src="${CODEX_HOME:-$HOME/.codex}",dst=/home/symphony/.codex \
  symphony:local --host 0.0.0.0 --port 8080
```

Add `--check` after `symphony:local` for configuration-only validation. Add `--host 0.0.0.0 --port 8080` after `symphony:local` when publishing the container listener through a host-loopback port mapping.

If a workflow uses `workspace.root: $SYMPHONY_WORKSPACE_ROOT`, the image sets that variable to `/app/.symphony-workspaces` by default.

## Minimal `WORKFLOW.md`

```markdown
---
tracker:
  kind: github
  api_key: $GITHUB_TOKEN
  repositories:
    - owner: octo-org
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

server:
  host: 127.0.0.1
  port: 8080

workspace:
  root: ./.symphony-workspaces
  cleanup:
    after_success: committed

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

- Tracker adapter: `tracker.kind: github`; repository-only issues are not dispatched unless they are in the configured Project v2. `tracker.endpoint` defaults to public GitHub's GraphQL endpoint and may be set to a compatible custom endpoint.
- State source: GitHub Project v2 Status field maps to `issue.state`.
- Workspace lifecycle: Symphony creates/reuses directories only; this Rust runtime removes a workspace after successful direct-commit completion by default (`workspace.cleanup.after_success: committed`). Set it to `never` to retain successful-run workspaces. Checkout, sync, bootstrap, and any additional cleanup policy still belong in hooks.
- Existing non-directory workspace path: fail safely; never replace user data.
- Codex default posture: high-trust defaults are used unless overridden in `WORKFLOW.md` with schema-valid Codex values.
- User input required by Codex: treated as a run failure so the worker does not stall indefinitely.
- Container runtime: the image uses `tini` for PID 1 signal forwarding/reaping, handles SIGINT/SIGTERM, and executes hooks plus `codex.command` inside the container namespace.
- Logging: structured logs are emitted to stderr; hook stdout and stderr are inherited by the service process and therefore visible in its runtime logs.
- HTTP UI/API: disabled unless `server.port` or CLI `--port` is set; responses omit tracker secrets, environment values, hook script contents, and prompt text. Hook output is not included in API responses.

## Repository layout

```text
.github/workflows/  CI workflow definitions
src/
  agent/             Codex protocol client and runner composition
  orchestrator/      Scheduler, retry, state, reconciliation helpers
  tracker/           GitHub Project v2 tracker adapter
  observability/     Local HTTP dashboard and API
  config.rs          Typed config and reload handling
  workflow.rs        WORKFLOW.md loader
  workspace.rs       Workspace path safety and lifecycle
  hooks.rs           Shell hook execution
  prompt.rs          Strict prompt rendering
  service.rs         Host service loop
  main.rs            CLI entrypoint

tests/               Conformance-focused integration tests
Cargo.toml           Rust package manifest
Cargo.lock           Locked Rust dependencies
SPEC.md              Normative service specification
AGENTS.md            Repository guidance
WORKFLOW.md          Local runtime policy and prompt template
WORKFLOW.example.md  Committed workflow template
Dockerfile           Container image build for Symphony + Codex CLI
docker-compose.example.yml
                     Container runtime example for Docker Compose
.dockerignore        Container build context exclusions
```

## GitHub upload notes

Commit source, tests, `.github/workflows/`, `Cargo.toml`, `Cargo.lock`, `SPEC.md`, `AGENTS.md`, `.gitignore`, `.dockerignore`, `Dockerfile`, `docker-compose.example.yml`, `WORKFLOW.example.md`, and this README.

Do not upload generated or local-only artifacts:

- `target/`, including generated Codex schemas under `target/codex-schema/`
- `.env` or `.env.*`
- a local `WORKFLOW.md` containing runtime secrets; commit `WORKFLOW.example.md` instead
- logs/temp files
- OS/editor metadata such as `.DS_Store`, `.idea/`, `.vscode/`
