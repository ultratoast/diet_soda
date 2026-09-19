# Diet Harness

Rust terminal agent harness, implemented from the agreed handoff plan. Workspace began empty; installed toolchain is Rust 1.84.1. Keep compatibility with Rust 1.84 and commit Cargo.lock for reproducibility.

## Agreed requirements
- Local JSON configuration, append-only JSONL sessions, environment-variable secrets.
- Providers: OpenRouter first, then LiteLLM, OpenAI, Anthropic.
- Simple themed TUI: history, input, model, session USD spend.
- Built-in, custom command and HTTP tools, MCP, runtime enable/disable.
- Agent definitions, modes, isolated subagents, skills, website reading, plugin hooks.
- Standalone workflows have exactly `title`, `author`, `steps`; steps have `model`, `prompt`, `mcps`, `hitl`; MCP references have `name`, `uuid`, `enabled`.
- Workflow HITL pauses AFTER executing a step, BEFORE advancing to the next. Tool approvals remain independent. No final-step gate when there is no next step.

## Implementation conventions
- Keep orchestration independent of the terminal. Use events and oneshot approvals.
- Models in workflows resolve through named model aliases or `provider:model-id`; plain IDs use the default provider. Do not split provider/model IDs on slashes.
- Tools receive schema-validated arguments, subprocess commands use argv directly, secrets are resolved at execution and never persisted.
- Runtime disablement is checked at execution as well as when advertising tools.
- Tests use local/mock services, no credentials or paid API calls.
- Use `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` for verification.

## Progress
Implemented and locally verified on Rust 1.84.1. README.md is the user/developer
reference; examples/config.json exercises the main configuration shapes.

## Current priorities and architecture
- Performance and simplicity are the highest priorities. Keep one library + CLI
  package; prefer focused modules and comments explaining invariants/tradeoffs.
- Engine is split into conversation orchestration, scope composition, and tool
  dispatch (`src/engine/`). TUI is split into lifecycle, state, commands, input,
  and cached rendering (`src/tui/`).
- Share the provider HTTP connection pool. Batch UI events; draw only dirty state
  at up to 25 FPS. Cache completed highlighted/wrapped entries, cloning visible
  lines only. Syntax grammars/palettes are loaded lazily once.
- Session events use a single append write. fsync occurs at completed conversation
  and workflow-step checkpoints, exports, and clean shutdown, not per event.
- Rust 2021 preserves the agreed MSRV. Cargo.lock is present; idna_adapter is pinned
  to 1.2.0 because newer ICU derives misreport compatibility. CI is configured for
  Rust 1.84.1 and stable; only local 1.84.1 checks have been run in this session.

## New behavior agreed and implemented
- Subagents use the ordinary `agents` JSON definitions. `delegate_parallel` accepts
  a tasks array; consecutive `delegate` calls can also run concurrently.
- `max_parallel_subagents` defaults to 4 (1–32). Child model/tool work shares a
  semaphore. Do not hold a permit while waiting for nested delegation, or the
  one-slot case deadlocks. Concurrent results retain input order; approvals are
  serialized. A child's failure must not stop a sibling's MCP connections.
- Model reasoning support is declared explicitly via `reasoning.supported_efforts`
  and optional `reasoning.effort`. `/effort` validates capabilities and uses native
  provider fields. Preserve signed reasoning metadata for tool continuations.
- `/model add <alias> <JSON-or-reference>` and `/mcp add <name> <JSON>` perform
  validated atomic edits, preserving relative paths and environment references.
  Runtime toggles/effort remain ephemeral. Duplicate additions are rejected.
- `/export [directory]` exports the redacted session log, including child contexts,
  with local timestamp title/filename `MM:DD:YYYY-HH:mm:ss.txt`. Same-second exports
  get suffixes. Windows filenames substitute hyphens for colons only.
- `/clear` and `/new` create a new session ID and reset history/input/spend while
  preserving old files and selected model/mode/runtime settings. `:q` aliases `/quit`.
- Themes include syntax palettes, semantic status colors, and CTA colors. The TUI
  inherits the terminal's selected system font; `theme.ascii` enables ASCII borders.
- Tab/Shift+Tab cycle application modes while idle: default, then configured names
  alphabetically, wrapping and preserving the draft. Modal input takes priority.
- Bare `/model` opens a fuzzy-search picker; explicit references and `add` retain
  their command syntax. Aliases/default/current choices appear immediately, with
  provider `/models` catalogs loaded in cancellable background tasks sharing the
  HTTP pool. Catalog failures leave configured choices usable. Alias selections
  preserve model settings; picking a model is an ephemeral override.
- Picker search matches case-insensitive subsequences and unordered words. Up/Down,
  PgUp/PgDn and Ctrl+Home/End browse; Enter selects; Esc/Ctrl+C cancel. Paste goes to
  the focused search field. The outer TUI margin is one cell, not pixel-based.

## Session continuity
Restored project context from `~/Code/session-ses_f492.md` (originally developed in
`diet_harness`; current workspace is `diet_soda`). Follow-up work adds keyboard mode
cycling, the model picker with provider discovery, and cell-based outer padding.

## Release distribution
- `.github/workflows/release.yml` builds on pushed `v*` tags or manual dispatch with
  an existing tag. Tags must exactly match `v{package.version}` in Cargo.toml.
- Release checks run formatting, tests, and Clippy before native Rust 1.84.1 builds:
  Linux x86-64 (Ubuntu 22.04/glibc 2.35+), macOS Intel and Apple Silicon, Windows x86-64.
  All jobs use the source commit resolved by the initial tag verification job.
- `.github/scripts/package_release.py` uses Python 3.11+ standard libraries to
  validate tags, smoke-test each binary, package binary/README/LICENSE/examples,
  and generate SHA256SUMS only when all platform archives exist. Output: target/dist/.
- Only the publishing job has contents:write. GitHub's automatic token creates
  releases with generated notes; hyphenated versions become prereleases. Reruns
  replace matching assets. README documents tagging, manual runs and installation.
- Locally verified with Actionlint 1.7.12 and an optimized Apple Silicon build.
  Extracted archive passed version/init/config/workflow checks outside the checkout;
  invalid tags and incomplete checksum inputs are rejected. Other native targets
  and GitHub publication await a workflow run; no release was published locally.

## Verification
Latest checks passed: `cargo fmt --check`, `cargo test --locked` (54 tests), and
`cargo clippy --all-targets -- -D warnings`. Tests cover mock providers, real local
stdio/HTTP MCP, true concurrent child dispatch using a barrier, nested one-slot
delegation, serialized approvals, reasoning continuation, configuration edits,
exports/resets, render-cache reuse, mode cycling, fuzzy picker input/cancellation,
mock model catalogs/authentication/pagination, and pseudo-terminal model selection
and cleanup. No paid/live model requests were made. Python 3 is needed for
MCP/plugin/pseudo-terminal fixtures.

## Deliberate scope boundaries
No OS sandbox, automatic context compaction, or workflow checkpoint continuation.
MCP supports tools over stdio/Streamable HTTP, not resources/prompts/OAuth/sampling.
Plugin hooks are process-based observers/gates, not native libraries or arbitrary
message transforms. See README for exact behavior and extension points.
