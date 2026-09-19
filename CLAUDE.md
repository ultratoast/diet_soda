# diet_soda

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
- The application, Cargo package, Rust crate, and executable are named `diet_soda`.
  Release archives and runtime network identifiers use the same name. Existing
  config/session files are not migrated or rewritten when renaming the executable.
- Default CLI config is `~/.config/diet_soda/config.json` on every platform.
  Startup always reads current disk contents; edits require no rebuild. `--init`
  creates parents and workflow/skill directories without overwriting files.
  `--config` remains an explicit override; there is no project-local fallback.
- Omitted/empty `workspace` means launch CWD; explicit workspace paths remain
  config-relative. Default workflows, skills, sessions, and exports are sibling
  directories beside config.json. Existing explicit paths are preserved; no
  automatic migration of old project configs or sessions is performed.
 - Models, providers, agents, MCPs, tools, hooks, prompts and theme settings
  stay in the main editable JSON. Workflow/skill files are read from disk. CLI
  workflow names resolve like TUI names through workflows_dir. `/reload` resets
  selection/runtime overrides without resetting the session; storage changes
  apply fully after restart.
- Prompt fields accept `./relative/path.md` references resolved beside config.json:
  system_prompt, agent prompt/system_prompt, and agent-mode prompt. Inline strings
  remain inline; referenced files are UTF-8 and capped at 1 MB.
- Agents have `can_edit` (false by default). Scope narrowing prevents children from
  gaining edit permission. `can_edit` gates write_file, shell, and destructive custom
  command tools. `bash-permissions: unified` loads the shared
  `bash-permissions.json` deny policy before command execution.
- User config now includes `AGENTS.md`, `theme.json`, and `bash-permissions.json`.
  `diet_soda --init` ships the same templates beside a new config.
 - Editable `models`, `agents`, and `tools` sections serialize as arrays of
  named objects. Runtime code retains maps for lookup; legacy object-shaped input
  remains accepted for transition.
- `examples/CONFIGURATION.md` is the user/developer reference for named arrays and
   field shapes. `diet_soda --init` copies it beside the active config.
 - Modes are deprecated; Tab cycles visible agents, and workflows select agents per
   step. Legacy mode settings remain tolerated while old configs are migrated.
 - Workflow steps may set an optional `agent`. The default
   `elephants_and_goldfish` workflow coordinates plan, review, implementation,
   testing, code review, and debugging stages with post-step HITL gates.
 - Built-ins include `web_search` for bounded public search and `gh` for authenticated
   GitHub CLI commands. `gh` checks installation and `gh auth status` before running
   and remains approval-gated.
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
- `src/tui/picker.rs` shares that UI for `/model`, `/mcp`, and `/theme`. MCP Enter
  toggles enabled state immediately and keeps the dialog open; closing does not
  undo toggles. It shows enablement, not health, and never eagerly starts a server.
- Theme picker previews on navigation/search, Enter applies for the session,
  Esc/Ctrl+C restores the prior theme. Approvals take priority over all pickers.
- Built-ins: haxx0r, BnP, solarized (dark), mama_j, diet_soda, blue. Syntax colors
  follow the palette, including monochrome code in mama_j. ASCII/highlighting
  toggles survive palette selection. `"theme": "diet_soda"` sets a persistent
  startup preset; existing custom theme objects still work. `/theme configured`
  restores the loaded config theme. `/reload` reloads it from disk.

## Session continuity
Restored project context from `~/Code/session-ses_f492.md` (the earlier temporary
workspace name was `diet_harness`; the application and current workspace are
`diet_soda`). Follow-up work adds keyboard mode
cycling, the model picker with provider discovery, and cell-based outer padding.

## Release distribution
- `.github/workflows/release.yml` (Binary Builds and Releases) builds on every push
  to main, including merges, plus pushed `v*` tags or manual dispatch with an
  existing tag. Tags must exactly match `v{package.version}` in Cargo.toml.
- Main builds only upload Actions artifacts; they do not publish GitHub Releases.
  Combined archives/checksums are retained 30 days, named with version and short
  commit SHA. `package_release.py --snapshot <full-sha>` packages these snapshots.
- Release checks run formatting, tests, and Clippy before native Rust 1.84.1 builds:
  Linux x86-64 (Ubuntu 22.04/glibc 2.35+), macOS Intel and Apple Silicon, Windows x86-64.
  All jobs use the source commit resolved by the initial verification job.
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
Latest checks passed: `cargo fmt --check`, `cargo test --locked` (63 tests), and
`cargo clippy --all-targets -- -D warnings`. Tests cover mock providers, real local
stdio/HTTP MCP, true concurrent child dispatch using a barrier, nested one-slot
delegation, serialized approvals, reasoning continuation, configuration edits,
exports/resets, render-cache reuse, mode cycling, fuzzy picker input/cancellation,
mock model catalogs/authentication/pagination, MCP picker toggles and approval
priority, theme preview/cancellation/config loading, syntax/cache updates, and
pseudo-terminal picker selection and cleanup. The TTY fixture applies cursor
diffs instead of searching raw output, avoiding random session-ID-related flakes.
Config tests cover isolated-home startup, nested init, no overwrite or local
fallback, launch-directory workspace, fresh disk edits across restarts, named CLI
workflows, and reload of changed settings without resetting session history.
Two Python packaging tests and Actionlint 1.7.12 pass. Optimized Apple Silicon
binaries built both at target/release/diet_soda and for snapshot packaging.
The local target/release/diet_soda binary was rebuilt after the config-location
change; its help and example configuration validation passed.
No paid/live model requests were made. Python 3 is needed for
MCP/plugin/pseudo-terminal fixtures.

## Deliberate scope boundaries
No OS sandbox, automatic context compaction, or workflow checkpoint continuation.
MCP supports tools over stdio/Streamable HTTP, not resources/prompts/OAuth/sampling.
Plugin hooks are process-based observers/gates, not native libraries or arbitrary
message transforms. See README for exact behavior and extension points.

## Bug round: kitty, Tab, tool-call text, outside access
- Kitty restored to the 16-frame pixel sampling of the source GIF; all frames share
  a padded 12-row canvas so the body cannot shift while the Z's move. Cells paint as
  solid `█`; body uses `theme.border`, eyes pink, Z's lighter pink.
- Tab cycles every configured agent (hidden included) because a config whose
  specialists are all hidden had nothing to cycle to. The bare `default` sentinel is
  omitted when an agent is marked `default`, which previously made Tab appear stuck
  between `default` and that same agent. The agent picker matches this.
- Tool calls in the transcript use `tools::describe_call`, the same human-readable
  summary as approval dialogs; unknown tools still fall back to pretty JSON.
- Outside access is now granted per approved call: shell args outside the workspace,
  custom command tools whose cwd is outside, and outside `read_file` each request
  approval and, when approved, run without the standing `allow_outside_workspace`
  flag. The flag remains the standing grant and is still narrowed for children.
- The pseudo-terminal fixture now waits on status lines (`agent: plan | Tab`) rather
  than header text and covers Tab then Shift+Tab back to `default`.
- Tool errors record `{error, tool, call}` where `call` is the `describe_call`
  summary, so spawn/transport failures that omit the command still name it for the
  model and the transcript. `tool_result_text` renders that as `[error] ...` plus
  the call line, and parses double-encoded JSON strings (top-level, `content`, or
  `stdout`) before display so escaped JSON never reaches the transcript.
- Verified: 26 lib tests, 3 cli tests (including the TTY fixture), 20 core, 8
  integrations, 2 catalog, 4 parallel-agent, 3 reasoning, 11 runtime tests; clippy
  clean; release binary rebuilt at `target/release/diet_soda`.

## Header context and workspace footer
- `UiEvent::Context { context, tokens }` is sent after every model response. The TUI
  tracks the latest main-context size and shows `context X/Y` beside spend, where Y
  is the model's `max_tokens`. `Session.context_tokens` restores the latest main
  request size on resume; subagent contexts do not change the header.
- The full workspace path is anchored to the bottom-right corner on the footer row,
  with the buttons sharing the left side. Long paths keep their tail with a leading
  ellipsis. `App.workspace` refreshes with `refresh_model`, so `/reload` updates it.
- Header right column widened from 24 to 36 cells to fit the combined spend and
  context line.
- Verified: 29 lib tests (includes header/footer rendering and session restore),
  3 cli, 21 core, 8 integrations, 2 catalog, 4 parallel-agent, 3 reasoning, 11
  runtime; clippy clean; release binary rebuilt.

