# diet_soda

Rust terminal agent harness, implemented from the agreed handoff plan and
stabilized through the Wave 0–5 MVP hardening rounds. The MSRV is Rust 1.84;
Cargo.lock is committed for reproducibility. CI checks Rust 1.84.1 and stable;
the local toolchain used for the latest verification is 1.98.0.

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
MVP stabilization is complete. The hardening waves added structured activity
lifecycles, bounded rendering with measured performance, provider streaming
fixes, SSRF/DNS pinning, MCP concurrency hardening, terminal/filesystem/
redaction hardening, Windows process-tree containment, and acceptance tests
for the documented feature surface. README.md is the user/developer reference;
examples/config.json exercises the main configuration shapes.

## Current priorities and architecture
- The application, Cargo package, Rust crate, and executable are named `diet_soda`.
  Release archives and runtime network identifiers use the same name. Existing
  config/session files are not migrated or rewritten when renaming the executable.
- Default CLI config is `~/.config/diet_soda/config.json` on every platform.
  Startup always reads current disk contents; edits require no rebuild. The
  first launch with no existing config and no `--config` flag automatically
  creates the full default tree (`config.json`, `AGENTS.md`, `theme.json`,
  `bash-permissions.json`, `CONFIGURATION.md`, `QUEUE_AND_ACCESS.md`, the
  `workflows/`, `skills/`, `prompts/`, `sessions/`, and `exports/` directories,
  the default prompt templates, and the default workflow). The announcement
  goes to stderr so scripted `--prompt` output is unaffected. `--init` writes
  `Created <path>` to stdout and remains a strict no-overwrite command. An
  explicit missing `--config` is still an error. `--init` short-circuits before
  auto-init. Auto-init uses a lock file beside the config to serialize
  concurrent first-run launches; companion files are written with
  `create_new(true)` so no existing file is ever overwritten. `--config`
  remains an explicit override; there is no project-local fallback.
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
- Agents have `can_edit` (false by default). Child scopes use their own agent's
  tools, MCPs, `can_edit`, and `allow_outside_workspace` (defaults apply when
  omitted); parent scopes no longer narrow these settings. `can_edit` gates
  write_file, destructive custom command tools, and MCP tools classified as
  edit-capable; read-only-classified MCP tools are offered to every agent.
  `write_file` remains workspace-bound; the unified bash policy and
  outside-workspace approval still gate shell/file access. `bash-permissions:
  unified` loads the shared `bash-permissions.json` deny policy before execution.
- User config includes `AGENTS.md`, `theme.json`, and `bash-permissions.json`.
  `diet_soda --init` ships the same templates beside a new config.
- Editable `models`, `agents`, and `tools` sections serialize as arrays of
  named objects. Runtime code retains maps for lookup; legacy object-shaped input
  remains accepted for transition.
- `examples/CONFIGURATION.md` is the user/developer reference for named arrays and
  field shapes. `diet_soda --init` copies it beside the active config.
- Modes are deprecated; Tab and `/agent` picker switch only among non-hidden
  agents, while workflows and delegation may select hidden agents. Legacy mode
  settings remain tolerated while old configs are migrated.
- Two visible agents: `chat` (default, GLM 5.3 Flash) for conversation/research/creativity
  and `make` (Claude Sonnet 5.5) for technical planning and execution. Hidden
  subagents available via delegation: `plan`, `elephant`, `build`, `code-review`,
  `plan-review`, `debug`, `researcher`, `explorer`, `test-runner`, `test-writer`,
  `doc-writer`. `max_parallel_subagents` defaults to 24.
- Workflow steps may set an optional `agent`. The default
  `elephants_and_goldfish` workflow coordinates plan, review, implementation,
  testing, code review, and debugging stages with post-step HITL gates.
- Built-ins include `web_search` for bounded public search and `gh` for authenticated
  GitHub CLI commands. `gh` checks installation and `gh auth status` before running
  and remains approval-gated.
- Performance and simplicity are the highest priorities. Keep one library + CLI
  package; prefer focused modules and comments explaining invariants/tradeoffs.
- Engine is split into conversation orchestration, scope composition, tool
  dispatch, and pause-aware budgets (`src/engine/`). TUI is split into
  lifecycle, state, commands, input, picker, kitty, and cached rendering
  (`src/tui/`).
- Share the provider HTTP connection pool. Batch UI events; draw only dirty state
  at up to 25 FPS. Cache completed highlighted/wrapped entries, cloning visible
  lines only. Syntax grammars/palettes are loaded lazily once.
- Session events use a single append write. fsync occurs at completed conversation
  and workflow-step checkpoints, exports, and clean shutdown, not per event.
- Rust 2021 preserves the agreed MSRV. Cargo.lock is present; idna_adapter is pinned
  to 1.2.0 because newer ICU derives misreport compatibility. CI is configured for
  Rust 1.84.1 and stable plus a Windows job; the latest local checks ran on
  Rust 1.98.0.

## New behavior agreed and implemented
- Subagents use the ordinary `agents` JSON definitions. `delegate_parallel` accepts
  a tasks array; consecutive `delegate` calls can also run concurrently.
- `max_parallel_subagents` defaults to 24 (1–32). Child model/tool work shares a
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
  preserving old files and selected model/agent/runtime settings. `:q` aliases `/quit`.
- Themes include syntax palettes, semantic status colors, and CTA colors. The TUI
  inherits the terminal's selected system font; `theme.ascii` enables ASCII borders.
- Tab/Shift+Tab cycle only the non-hidden (visible) configured agents
  alphabetically regardless of run state (only workflow mode disables
  cycling), wrapping and preserving the draft; the bare `default` sentinel
  is omitted when an agent is marked `default`, and hidden agents remain
  reachable by name via `/agent <name>`. Modal input takes priority.
- Bare `/model` opens a fuzzy-search picker; explicit references and `add` retain
  their command syntax. Aliases/default/current choices appear immediately, with
  provider `/models` catalogs loaded in cancellable background tasks sharing the
  HTTP pool. Catalog failures leave configured choices usable. Alias selections
  preserve model settings; picking a model is an ephemeral override.
- Picker search matches case-insensitive subsequences and unordered words. Up/Down,
  PgUp/PgDn and Ctrl+Home/End browse; Enter selects; Esc/Ctrl+C cancel. Paste goes to
  the focused search field. The outer TUI margin is one cell on the left,
  right, and bottom; the top margin row may carry animated kitty artwork (see
  the kitty geometry section), not pixel-based.
- `src/tui/picker.rs` shares that UI for `/model`, `/mcp`, `/agent`, `/theme`, and
  `/sessions`. MCP Enter
  toggles enabled state immediately and keeps the dialog open; closing does not
  undo toggles. It shows enablement, not health, and never eagerly starts a server.
- `/sessions` browses previous sessions in the picker, filtered to the launch
  directory recorded as `cwd` in each new session's start event (additive; legacy
  sessions lack it and appear only under `/sessions all`). Enter resumes: fallible
  steps (exists-check, open, checkpoint) complete before any state change, grants
  reset to the new session id, and failures reopen the picker. Listing is a bounded
  256 KiB/file read-only scan (`src/session/list.rs`) that never locks files.
- Theme picker previews on navigation/search, Enter applies for the session,
  Esc/Ctrl+C restores the prior theme. Approvals take priority over all pickers.
- Built-ins: haxx0r, BnP, solarized (dark), mama_j, diet_soda, blue. Syntax colors
  follow the palette, including monochrome code in mama_j. ASCII/highlighting
  toggles survive palette selection. `"theme": "diet_soda"` sets a persistent
  startup preset; existing custom theme objects still work. `/theme configured`
  restores the loaded config theme. `/reload` reloads it from disk.

## MVP stabilization: current state

### Structured activity lifecycle, accordions, and replay
- Tool, subagent, and workflow-step work emit `ActivityEvent` records
  (`src/model.rs`): `kind` (Tool/Subagent/WorkflowStep), `phase` (Start/End),
  sanitized title, optional `parent_id` for nesting, optional `external_id`,
  and a final `status` (success/error/cancelled/denied). Titles reuse the
  `tools::describe_call` approval summary and are sanitized and capped at 160
  scalars before persistence; tool errors record `{error, tool, call}` so
  spawn failures still name the call.
- The TUI renders one-line summaries collapsed by default: `[+]`, status
  (`[ok]`/`[error]`), and a `(+N)` hidden-line count; errors stay visible while
  collapsed. Expanding reveals detail including nested child activities; a
  child renders only while every ancestor above it is expanded.
- Resume replays every recorded activity collapsed regardless of how it was
  expanded when interrupted. Sessions recorded before activity records existed
  synthesize collapsed tool/subagent summaries from message history, so old
  sessions remain readable.

### Keyboard and mouse controls
- Enter sends; Alt+Enter/Ctrl+J insert newlines (Shift+Enter where the terminal
  reports it distinctly). Arrows edit and navigate input history; PageUp/PageDown
  and Ctrl+Home/End scroll the conversation, help, and approval dialogs. Ctrl+C
  cancels the active run; Ctrl+D quits with empty input; bracketed paste is
  supported. Tab/Shift+Tab cycle agents regardless of run state.
- Mouse capture is enabled at startup: a left click toggles a visible activity
  row and moves focus there; the wheel scrolls the transcript, an open overlay,
  or an open picker. `/mouse off|on|toggle` disables/enables capture for the
  session (never persisted, survives `/clear`, `/new`, `/reload`) so native
  terminal selection works. Clicks are ignored while an approval, help,
  workflow-complete overlay, or picker owns the input.
- Enter during an active run queues FIFO; each queued message starts its own
  turn when the run ends. `/clear` and `/new` drop the queue; slash commands
  never queue, and `/tools`, `/mcp`, `/theme`, `/help`, `/cost` work mid-run.

### Bounded stream/final rendering and measured performance
- Display truncation only: a streaming entry shows its most recent 8 KiB; a
  completed entry shows at most 128 KiB or 4,000 lines with a
  `[display truncated: ...]` marker naming the true size and pointing at
  `/export`. The session log always keeps full text, and `/export` writes it.
- Completed messages retain highlighted/wrapped render caches; streaming
  invalidates only the changed entry; frames copy visible lines; redraws touch
  dirty state only at up to 25 FPS; syntax grammars/palettes load lazily once;
  SSE parsing consumes a chunk before shifting its buffer. Wrapping uses a
  single forward-index pass (the 1 MiB unbroken-line harness is its regression
  target) with an ASCII fast path for long unbroken runs.
- Seven `#[ignore]` release-only performance harnesses (six in
  `src/tui/render.rs`, one in `src/session.rs`) exercise the real paths on
  extreme inputs. They are measurement tools, not gating assertions; current
  medians are in Verification.

### Provider timeout and incomplete messages
- `timeout_seconds` is not a total stream duration. It bounds the wait for the
  response header and first bytes, then re-arms as a per-chunk idle gap: a
  provider that keeps streaming never trips it while inter-chunk gaps stay
  under the limit. The outer read loop breaks as soon as the completion
  sentinel (`[DONE]` / `message_stop`) arrives, so a server that holds the
  body open after completion cannot demote a finished response.
- A stream that ends without its completion event — cancellation, header or
  idle timeout, network error, malformed final chunk — persists as an
  incomplete assistant message. The transcript and `/export` show
  `[incomplete response: <reason>]`. The partial text is never re-entered into
  model request history on resume or continuation, and no usage or spend is
  recorded for it (the provider may still have charged).

### Context-aware status
- The global status line is main-context only: child and workflow status text
  belongs to their activity rows, and child model changes never move the
  header. Main shows `Generating | main` while streaming plus provider phase
  text (`Connecting`, `Waiting for first chunk/token`).
- `UiEvent::Context { context, tokens }` after every model response feeds the
  `context X/Y` header readout (Y is the configured context limit: the model's
  `context_window` override, else the global `max_context_tokens`);
  `Session.context_tokens` restores the latest main request size on resume.
  The full workspace path anchors to the footer's bottom-right with a leading
  ellipsis for long paths, and refreshes on `/reload`.

### Pause-aware execution budgets and builtin timeouts
- `src/engine/budget.rs` implements agent execution deadlines (default 2
  hours via `timeout_seconds`) that freeze while a tool approval waits —
  including approvals inside child agents, which also freeze every ancestor's
  deadline — so a run paused for a human decision does not burn its budget.
  Workflow HITL gates run after a step's conversation completes and each step
  starts a fresh budget. Lock ordering is child→parent only; deadline
  arithmetic is saturating.
- `builtin_timeouts.shell_timeout_seconds` and `gh_timeout_seconds` (default
  600 each, validated positive) replace fixed deadlines for the shell and `gh`
  built-ins. Custom command tools keep per-tool `timeout_seconds`; zero-limit
  processes are rejected before spawn.

### Session storage and recovery
- Every event is one serialized append write; fsync is amortized at completed
  conversations and workflow steps, exports, and clean shutdown. Sessions take
  an exclusive advisory lock; a second opener fails until the first handle
  drops.
- Open parses all contexts (main strict, non-main tolerant), pre-filters
  recovery scanning with a byte marker, repairs unmatched tool calls, and marks
  interrupted tails with recovery events instead of failing. Incomplete
  main-context messages are excluded from model history on both the live path
  and reopen. Legacy context-only `clear` events remain understood on resume.

### SSRF pinning, custom HTTP opt-in, and skills
- `web_fetch` and custom HTTP tools resolve each hop in DNS, classify every
  returned address, and dial through a client pinned to exactly those
  addresses (`resolve_to_addrs`), closing the lookup-to-connect rebinding
  window. Pinned clients disable environment/system proxies and automatic
  redirects; the manual redirect loop revalidates and re-pins every hop
  (exactly five followed, then `Too many redirects`).
- Always-blocked ranges — link-local (IMDS), unspecified, broadcast, multicast,
  `0.0.0.0/8`, IPv4-mapped IPv6, NAT64, 6to4, Teredo, IPv4-compatible, and
  site-local — stay blocked even with `allow_private_networks`, which only
  relaxes loopback/RFC1918/CGNAT/ULA. `web_fetch` uses the global
  `web_fetch.allow_private_networks` opt-in; custom HTTP tools carry a per-tool
  `allow_private_networks` (default false) and never follow redirects.
- Skill installation (local directory, standalone `SKILL.md`, `.tar.gz`,
  HTTPS Markdown/tarball) uses a staging directory, refuses duplicate names,
  rejects archive links and path traversal, enforces entry-count and size
  limits, hardens extracted trees to owner-only modes, and downloads through
  the same pinned, proxy-free, public-only transport. Skill scripts are copied,
  never executed automatically.

### MCP hardening
- Per-server connect gates serialize concurrent connects per name without
  global serialization; different servers connect in parallel. `stop` takes the
  same gate so an in-flight connect cannot resurrect a stopped server, and a
  shutdown flag rejects new connects while full shutdown runs.
- Failed connects are negative-cached for five seconds so concurrent callers
  share one error instead of racing to respawn; cancellation during connect is
  never cached, and explicit stop/restart clears the cache. Shutdown stops all
  servers concurrently instead of serializing.
- stdio stderr is drained to a bounded 8 KiB tail attached to connect and call
  errors, so a crashing server explains itself.
- Exposed tool names are `mcp_<server>__<tool>` when valid and ≤64 bytes, else
  a deterministic `mcp_<sanitized-server>_<16-hex>` FNV-1a hash over server and
  original tool names, stable across `tools/list` ordering and pagination;
  collisions fail closed.

### Terminal sanitization, private modes, and redaction
- `src/text.rs` provides the shared sanitizer: Cc controls, bidi
  overrides/isolates, zero-width/tag/soft-hyphen characters, and Zl/Zp
  separators are stripped; ZWNJ/ZWJ are preserved; tabs become one space;
  single-line mode folds newlines to spaces. All TUI render paths, activity
  titles/ids, picker labels and paste, workflow titles, and remote catalog IDs
  go through it. Headless output sanitizes when the target stream is a
  terminal and passes bytes unchanged when piped.
- Newly created config, session, log, export, and skill files are owner-only
  (`0600` files, `0700` directories on Unix) via `src/fsutil.rs`; existing
  files are never chmodded, and atomic config edits preserve the original mode.
- Redaction covers provider keys, every `${VARIABLE}` referenced in the
  configuration, and `GH_TOKEN`/`GITHUB_TOKEN`/`GH_ENTERPRISE_TOKEN`, with an
  eight-byte floor against over-redaction, JSON-escaped forms, and JSON object
  keys (colliding redacted keys get deterministic suffixes). Exact-match only:
  secrets that only appear after encoding or transformation are not recognized.

### Windows Job Objects and CI scope
- `src/winjob.rs` — the single sanctioned `unsafe_code` allow site (crate lint
  is `deny`) — spawns children `CREATE_SUSPENDED`, assigns them to a
  kill-on-close Job Object, resumes threads with a PID-ownership check, and
  fails closed on assignment denial. Cancellation, timeout, and MCP stop tear
  down the whole process tree; Unix uses process groups.
  `tests/process_tree.rs` pins tree teardown on both platforms.
- CI (`.github/workflows/ci.yml`) runs fmt/test/clippy on Rust 1.84.1 and
  stable (Linux) plus a `windows-process-tree` job (check, clippy,
  `process_tree`, and `mcp_stdio` tests). The release workflow runs the same
  Windows smoke before packaging.

### Default workflow installation
- `--init` and first-run auto-init install
  `workflows/elephants_and_goldfish.json` from the compile-time-embedded
  example, owner-only and no-overwrite. Partial-tree healing recreates it when
  absent, and concurrent first-run races produce one valid file.

### Heuristic shell policy and outside access
- Shell classification is a shared positive allowlist: recognized read-only
  forms (`grep`, read-only `find`, Git/AWS/GitHub/package queries, and similar)
  run without approval; mutating or unknown operations ask. It covers
  `python`/`python3`, `cargo`, `yarn`, `pip`/`pip3`, `npm`, `make`, `aws`/`awscli`,
  `pup`, `gh`, and `gws`, but does not trust arbitrary scripts, builds, package
  changes, or `make` targets. It is best-effort and **not a sandbox**; the
  unified bash-permissions deny list runs on every shell, `gh`, and command-tool
  call and cannot be bypassed by approval. Credential/secret retrieval and
  commands that download to local files also ask.
- Command-family approvals offer `y` once, `p` for that family through the
  current session, `n` to reject, and `a` to abort. Grants are in memory, shared
  by subagents, and reset by `/clear` and `/new`; they do not bypass explicit
  deny rules, outside-workspace checks, or separate HITL gates.
- Outside access: `read_file` outside the workspace is approved once per
  directory per session; shell commands and custom command tools with an
  outside cwd are approved per call. The standing `allow_outside_workspace`
  grant suppresses only the outside-path approval reason, and only for forms
  the allowlist recognizes as safe. Children only intersect grants.

### Header and kitty geometry
- The header occupies terminal rows 1-2 (`HEADER_HEIGHT = 2`): the logo column
  spans both rows, and the top-right metadata block composes
  `<model_label> | agent <resolved_agent> | effort <effort_label>` on row 1 and
  the spend/context line on row 2. The old left-aligned below-logo
  `agent: … | model: … | effort: …` row is removed; legacy agent modes are
  intentionally never displayed. Overlong metadata truncates from the left so
  the agent and effort suffixes survive.
- `resolved_agent` is main-context only and resolves exactly like
  `Engine::scope`: explicit selection, then the configured default agent, then
  the `default` sentinel. `refresh_model` computes it without cloning the whole
  `Config`: workspace path and default agent are copied under the read guard,
  which never crosses an await point.
- One horizontal header `Layout` owns the logo/metadata/separator/kitty
  reservation geometry shared by the header and the artwork. The logo column is
  fixed at exact width 6, the metadata column takes all remaining slack, and
  the kitty is suppressed whenever the metadata remainder would fall below 24
  cells.
- The kitty canvas anchors its row 0 to terminal row 0 (the outer top margin
  row); idle padding keeps terminal row 0 empty, so the first visible idle
  glyph sits on terminal row 1. Animated Fly Girl frames may paint terminal
  row 0. The canvas is confined to the header-reserved right column and is
  vertically clamped by the input region's top; it may coincide with header
  metadata, divider, and history rows but never overlaps non-reserved
  columns. Painted glyph spans are recorded each frame and excluded from
  activity hit tests.
- A restored session/chat divider occupies terminal row 3, immediately below
  the header, and stops immediately before the kitty reservation (no glyph at
  or beyond its left edge). With the kitty suppressed, the rule spans full
  width and closes with the top-right corner. Unicode (`border::ROUNDED`) and
  ASCII (`theme.ascii`) glyph sets share `border_symbols`.
- History activity content uses the asymmetric LEFT|RIGHT|TOP inner rect
  (`x+1, y+1, w-2, h-1`); at 80x24 this is `Rect::new(2, 4, 76, 12)`. Hit-map
  index 0 maps to that inner y. The divider and side border columns lie outside
  `last_history_rect` and are intentionally not activity-selectable.
- `INPUT_RESERVED_ROWS = 6` protects header (2) + divider (1) + one history
  content row + footer (2); the input band caps at `content.height - 6`. The
  divider row survives down to content height five (at content height 5 the
  history band is divider-only, zero content rows); one history content row is
  guaranteed from content height six upward. Below roughly ten terminal rows
  the input band degrades to border-only or invisible.
- Regression coverage: exact renderer geometry tests in `src/tui/render.rs`
  (two-row header, tail truncation, narrow-terminal kitty suppression,
  terminal-top anchor, divider stopping before the reservation in Unicode and
  ASCII, suppressed-kitty divider closing at the band's right edge, divider/
  borders not being activity targets) and the controlling-PTY resize test
  (`tui_geometry_survives_controlling_pty_resize` in `tests/cli.rs` with
  `tests/fixtures/tui_geometry.py`, asserting the 100×24 → 40×18 → 100×24
  cycle).

### Kitty artwork
- Three glyph variants (Blob, Cbear, Fly Girl) share one six-row canvas whose
  idle rest pose keeps row 0 empty (animated Fly Girl frames may paint row 0;
  see header geometry above). The launch variant is picked once per TUI session
  from a UUID-derived offset and stays fixed for the whole session; rotation
  is disabled at runtime (the pure `ROTATION_SECONDS` helper remains for
  tests). Idle always shows the rest pose; the variant's animated cycle runs
  only while a run is active. Theme colors follow the active palette.

## Session continuity
Project context was restored from `~/Code/session-ses_f492.md` (the earlier
temporary workspace name was `diet_harness`; the application and current
workspace are `diet_soda`). The MVP stabilization waves are complete; see the
stabilization section above for current behavior.

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
- Historical local verification (earlier session, on macOS): Actionlint 1.7.12
  and an optimized Apple Silicon build; the extracted archive passed
  version/init/config/workflow checks outside the checkout. The current session
  has no actionlint or macOS toolchain; other native targets and GitHub
  publication await a workflow run, and no release has been published.

## Verification

Current session (macOS, Rust 1.98.0) added the shared CLI read-only classifier
and session-scoped `p` approvals:

- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass.
- `cargo test --locked --test security`: 63 passed, including same-family
  persistent grants, stale-session grant rejection, and CLI read/mutation cases.
- The complete suite passes with
  `TMPDIR=/private/var/folders/_0/qs6dq2dn3xxg9d2pbyv31t880000gn/T/opencode`
  and `--skip tui_geometry_survives_controlling_pty_resize`. On this host the
  geometry fixture fails independently with Python `termios` `ENOTTY`; without
  the explicit canonical temp path, several existing temp-path assertions also
  compare `/var` and `/private/var` aliases.
- Existing user config/permission files are not rewritten by default changes;
  remove any old AWS/Git/GitHub hard-deny entries from the active
  `bash-permissions.json` if they should now prompt instead of block.

PR #5 CI runs `35903695834` and `35903693127` exposed cancellation precedence and
Windows-only lint issues (Unix-only imports in `tests/cli.rs` and the
`archive_with_symlink` fixture). Those were fixed with cancellation priority and
`cfg(unix)`. Run `35912051702` then exposed that the cancellation test synchronized
on the fixture's server-side header write rather than the client's receipt; the
test now obtains the response before cancelling its stalled body. Windows Clippy
also found `Config` imported in `tests/runtime.rs` although only a Unix-gated test
uses it; that import is now `cfg(unix)`. Follow-up run `35914576869` passed stable
and Rust 1.84.1 but Windows Clippy flagged Unix-only `process` imports, `GH_LOCK`,
and a Unix-only test in `tests/security.rs`; the current worktree gates them with
`cfg(unix)`. Stable and Rust 1.84.1 formatting, Clippy, and full tests pass locally
except for the known macOS controlling-PTY `ENOTTY` fixture. A local Windows MSVC
cross-target check cannot build `ring` because this Mac lacks Windows C headers;
native Windows CI must confirm the final platform-gating fixes.

Latest measured facts (local Linux x86-64, Rust 1.98.0 toolchain), taken after
the corrected TUI geometry (terminal-top kitty anchor, row-3 divider, and
asymmetric history inner rect):

- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` are clean
  (re-verified this session).
- `cargo test --locked`: 633 passed, 0 failed, 7 ignored. The seven ignored
  tests are the release-only performance harnesses, intentionally excluded
  from the default run.
- `cargo test --locked --lib tui::`: 247 passed, 0 failed, 6 ignored.
- The three controlling-PTY tests (`tui_pseudo_terminal_…`,
  `tui_activity_accordion_…`, `tui_geometry_survives_controlling_pty_resize`)
  pass (`cargo test --locked --test cli tui_`); the geometry test stays green
  on repeated runs across all three kitty variants.
- Performance harnesses (`cargo test --release --lib --locked -- --ignored
  --nocapture`), each run five times, medians:
  - ~20 MiB session load/resume: 50.4 ms reopen; post-resume clear 6.7 ms.
  - 2 MiB completed-entry render: five-run medians remain approximately
    3.55–3.59 ms (1,049 lines); the earlier 5.55 ms sample was a
    non-reproducible outlier.
  - 1 MiB unbroken-line wrap at width 100: 4.5 ms.
  - 50 KiB streaming mixed Markdown, 50 chunks plus final draw: 21.7 ms total.
  - 2 MiB active stream, 64 chunks: 29.6 ms total.
  - 2,000 collapsed tool activities, 50 redraws: 1.23 ms/frame.
  - 10,000-entry steady transcript, 50 redraws: 0.28 ms/frame.
- Baseline A/B measurement in a detached worktree at HEAD: the release perf
  drift is environmental, not attributable to the geometry diff. The largest
  positive delta (+14.5%) occurred in
  `oversized_completed_entry_release_render_harness`, whose measured function
  `render_entry` is not touched by the diff, while diff-path harnesses moved
  in mixed directions (+8.5%, +2.1%, -0.5%, -6.4%).
- Two Python packaging tests pass (`python3 .github/scripts/test_package_release.py`).
- Release build and smoke are green: `target/release/diet_soda` reports
  `diet_soda 0.1.0`, `--help` succeeds, and
  `--config examples/config.json --validate-config` passes.

Not available locally this session: the Rust 1.84.1 toolchain (CI checks
1.84.1 and stable), actionlint (workflow YAML validated by parsing only),
macOS and Windows native builds/runtime, and no GitHub Release has been
published. Tests use loopback HTTP fixtures and local Python MCP/plugin
fixtures; no credentials or paid API calls.

Session state, not product behavior: HEAD is the local commit `4f7441e` on
branch `POC-2` with a dirty working tree of 7 modified paths
(`CLAUDE.md`, `README.md`, `src/tui/app.rs`, `src/tui/commands.rs`,
`src/tui/kitty.rs`, `src/tui/render.rs`, `tests/cli.rs`) plus untracked
`tests/fixtures/tui_geometry.py`. Nothing was staged, committed, or pushed
during this documentation pass.

## Deliberate scope boundaries
No OS sandbox, automatic context compaction, or workflow checkpoint continuation.
MCP supports tools over stdio/Streamable HTTP, not resources/prompts/OAuth/sampling.
Plugin hooks are process-based observers/gates, not native libraries or arbitrary
message transforms. The shell classifier is a heuristic, not a sandbox; the
bash-permissions deny list is the enforced boundary. See README for exact
behavior and extension points.

## Session Notes (2026-10-01): Execution limits and timeouts raised

- `fn seconds()` default is now 600 (was 120): feeds provider, custom tool,
  MCP, hook, and `builtin_timeouts` (shell/gh) defaults.
- `fn depth()` default is now 50 (was 3); `validate()` hard cap on
  `max_subagent_depth` is now 100 (was 16).
- Default agent run deadline (`src/engine/scope.rs`) is now 7200s / 2 hours
  (was 1800s).
- Updated: `config.json`, `examples/config.json`, `README.md`,
  `examples/CONFIGURATION.md`, tests (`core`, `cli`, `workflow_reasoning_tools`);
  added `subagent_depth_default_and_cap` in `tests/core.rs`.
- KNOWN OPEN, caused by uncommitted WIP predating this change (NOT regressions
  from it): `src/engine/dispatch.rs` removed default write_file HITL, breaking
  `non_tty_approval_aborts_write_file_without_executing_it` (cli), hanging
  `disabling_a_tool_while_approval_is_pending_prevents_execution` (runtime),
  and failing `write_approval_detail_previews_content_but_activity_error_summary_does_not`
  (tool_lifecycle); `src/engine/scope.rs` can_edit-narrowing removal breaks
  `agent_skills_override_global_skills_without_widening_parent_permissions`
  (config_contract). Environmental on macOS: two `/private` tmpdir
  canonicalization cli tests and the pty `tui_geometry` test.

## Session Notes (2026-10-01): Response truncation removed

- Tool-call and subagent responses are no longer truncated in practice: the
  engine-level 100 KB cap on every tool result (`src/engine/mod.rs`
  `tool_result`) and the builtin caps (shell 100 KB, gh 200 KB, read_file
  100 KB, web_fetch 2 MB download / 100 KB text) now all use the shared
  `MAX_RESPONSE_BYTES = 100_000_000` safety constant in `src/tools.rs`.
- Custom-tool `max_output_bytes` default raised 100 KB -> 100 MB; explicit
  per-tool values are still honored (field kept for config compatibility).
- `truncated` JSON fields are retained; they are false unless an explicit or
  safety cap is actually hit.
- UNCHANGED ceilings (rejections/guards, not response truncation): read_file
  and write_file 2 MB limits, web_search 1 MB reject, MCP 2 MB message
  rejections (`src/mcp.rs`), skills/catalog 10 MB download caps, 400-char
  approval previews, and all TUI display truncation.
- Risk accepted by user: oversized tool results can overflow model context
  (provider API errors mid-run); runaway commands can buffer up to ~100 MB
  per stream.

## Session Notes (2026-10-01): Uniform 1000-turn model budget

- Every scope (top-level agent or subagent) now gets
  `Some(agent.max_turns.unwrap_or(MAX_MODEL_TURNS).min(MAX_MODEL_TURNS))` with
  `pub const MAX_MODEL_TURNS: usize = 1_000` in `src/engine/scope.rs`.
- Behavior change: top-level agents were previously UNLIMITED (`max_turns: None`
  → `usize::MAX` loop) and ignored per-agent `max_turns`; now the per-agent
  setting is honored at every level and can only lower the 1000 budget. The
  example agents coordinator (12), researcher (8), reviewer (6) therefore stop
  at those counts in workflow-step and Tab-selected top-level runs too.
- `src/engine/mod.rs` keeps `unwrap_or(usize::MAX)` at the conversation loop as
  a defensive fallback (None no longer occurs); the turn-exhaustion bail message
  now reports MAX_MODEL_TURNS (message path has no dedicated test — accepted).
- Legacy global `config.max_turns` default raised 20 -> 1000; still inert at
  runtime (validated nonzero only). Per-agent `max_turns: Some(0)` still rejected.

## Session Notes (2026-10-01): WIP policy changes completed

The uncommitted WIP policy workstreams are now fully reconciled (tests + docs):

- **write_file approvals**: built-in `write_file` no longer prompts by default
  for `can_edit` agents; only explicit `approval_tools` entries force a prompt
  (`src/engine/dispatch.rs`). `require_for_destructive_tools` still gates
  custom tools marked `destructive`. Policy consequence accepted: headless
  `can_edit` agents write without approval unless `approval_tools` lists
  `write_file`.
- **can_edit non-narrowing**: a child with `can_edit: true` keeps edit access
  under a read-only parent (`src/engine/scope.rs`); tools/MCPs intersect and
  `allow_outside_workspace` narrowing were REMOVED in the later
  `increased limits, relaxed permissions for subagents` commit — child scopes
  are no longer narrowed (see README).
- **Default access roots** (`src/tools.rs`): canonicalized `/tmp` read+write
  and the config directory read-only need no outside-workspace approval;
  nonexistent roots skipped; `canonicalize_lenient` resolves missing paths
  through existing ancestors; `..` in nonexistent paths fails closed.
- **git global-options guard** (`src/tools.rs` + `examples/bash-permissions.json`):
  `allow` downgrades to `ask` when `-c`/`--config-env`/`--exec-path`/`--git-dir`/
  `--work-tree` precede the subcommand (policy subject strips them); example
  policy adds read-only git allows and denies `--output`/`--ext-diff`/
  `--textconv`/`--filters`/`--open-files-in-pager`.
- **Reconciliations by the user**: tool_lifecycle `write_approval_detail`
  (approval_tools + comments), cli `non_tty_approval` (approval_tools) and new
  `editor_agent_writes_file_without_approval_in_non_tty`, config_contract
  can_edit assertion flip, `tempdir_in(target)` moves in bash_policy_dispatch,
  runtime outside-path tests, and tests/core.rs outside-path tests
  (`outside_reads_are_detected_for_approval_without_widening_writes`,
  `outside_shell_arguments_require_approval_but_inside_ones_do_not`).
- **Reconciliations this session**: runtime
  `disabling_a_tool_while_approval_is_pending_prevents_execution` (approval_tools
  — was hanging forever), cli `list_workflows`/`install_skill` (canonicalized
  expectations for macOS /private), hooks `headless_cancellation` (readiness =
  first provider request instead of fixed 100ms sleep — macOS first-exec
  gating), fixture `tui_geometry.py` (capture terminal modes before child exit —
  macOS pty teardown ENOTTY), README policy docs (five spots).
- Full-suite green expected; confirmed by the final verification run.

## Session Notes (2026-10-01): Shell policy — python3/cargo/sed allows, bash -c decomposition, path-aware rules

- Shipped + installed `bash-permissions.json`: allow rules for `python`/`python3`
  (bare and with args), `python3.*`, `cargo` (bare and with args); ask rules for
  cargo `publish`/`login`/`install`/`yank`/`owner` in direct AND infix
  (`cargo * <verb>*`) forms so global options before the subcommand still prompt.
  Cargo aliases (`--config alias.…`, `.cargo/config.toml`) are NOT covered.
  Force-push denies extended: `git push * --force*`, `git push * -f*` (infix gap
  found in review; `--force-with-lease` intentionally not matched by `-f*`).
- Path-aware allow matching (`is_normalized_command_path`, `policy_program_token`,
  `policy_subject`, rewritten `resolve_bash_policy` in `src/tools.rs`): allow
  rules apply only to bare names or paths whose immediate parent dir is `bin`
  (`/usr/bin/x`, `venv/bin/x`, `./bin/x`); other paths (`./x`, `/tmp/y/x`,
  `..`-containing) are matched by full path and get no basename allows.
  Deny/ask rules and legacy `blocked_*` still match the trailing command for
  ANY path. Combine order: Deny > Ask > strict-only Allow; the git-global
  Allow→Ask downgrade runs once after combining. Residual risks (accepted):
  any agent-writable dir named `bin` qualifies; matching is case-insensitive;
  prefix globs (`ls*`, `pwd*`) also match longer names (`lsof`, `pwdx`) —
  tightening to `pwd` + `pwd *` pairs is a possible follow-up.
- New module `src/sed_script.rs`: fail-closed GNU-sed scanner
  (`scan_sed_args -> SedScan { may_execute, paths, files, backup_suffix }`).
  `e`, `s///e`, `-f`/`--file`, unknown/abbreviated options, `:` with empty
  label, one-line `a/i/c` text ending in backslash, and any parse ambiguity →
  may_execute. Review-fixed: `:` labels terminate at `;` (GNU behavior —
  `sed ':x; e touch /tmp/pwned'` was a real auto-run bypass before the fix).
  BSD/macOS sed differs; scanner stays conservative.
- New module `src/shell_wrapper.rs`: fail-closed parser for
  `bash|sh|zsh|dash -c "<script>"` (exact [flags∈-[ceux]+ containing c, script]
  shape, ≤4096 chars, ≤16 commands). Accepts only simple commands joined by
  `&& || ; |` newline with strict quoting; rejects all substitutions,
  redirects, globs, `~`, assignments, subshells, `cd`/builtins, nested
  wrappers — anything rejected falls back to whole-invocation approval.
  Builtin reject list must grow whenever a policy allow is added for a name
  that is also a shell builtin.
- `editor_policy_override` (tools.rs): sed auto-runs for `can_edit` scopes when
  `!may_execute` AND the sed path is normalized; only a missing rule or the
  catch-all `*` ask is upgraded; explicit operator ask/deny always win.
- `effective_path_args` (tools.rs): argv + sed script filenames (`w`/`r`/
  `s///w`) with parents + `-i` backup compositions `<file><suffix>` with
  parents, feeding outside-path checks in dispatch AND the shell builtin arm.
- `assess_wrapped_commands` + `WrappedAssessment` (tools.rs): per-segment
  policy → editor override → outside gate → heuristic; hard denies fail the
  whole call (`in shell -c script: …`). Dispatch (`invoke_inner`) uses it for
  wrapped shells: outer catch-all ask ignored, explicit outer ask still
  forces, per-segment reasons in the approval detail, NO `p` session grant for
  wrapped calls or non-normalized paths.
- `tools::builtin` shell arm re-checks every parsed inner segment
  (validate/reject-outside/bash-policy) right before spawn — defense in depth
  even if dispatch is bypassed; the parsed `-c` script string is excluded from
  the OUTER path check (it is source text; its modeled commands are checked).
- `classify_safe_command`: `"which" => true` added next to `"pwd" => true`
  (both now safe at heuristic layer too — matters under `bash-permissions:
  none` or unmatched paths).
- Tests: sed_script (5), shell_wrapper, path-aware policy matrix
  (`path_aware_allow_rules`), editor/effective-path/override units,
  `wrapped_script_assessment_matrix` (decision-level, no execution), runtime
  builtin shell tests (4), bash_policy_dispatch engine tests (6 new, 27 cases)
  incl. session-grant and pwd/which pinning. Full suite green except for the
  known USER-WIP failure `subagent_has_isolated_messages_and_keeps_its_own_tool_scope`
  (dispatch.rs write_file advertising filter vs can_edit=false child — owner
  decision pending, unrelated to this change set).
- Risk accepted by user: python3/cargo allows mean arbitrary code execution for
  ALL agents (incl. read-only); bounds are legacy blocks, deny rules,
  outside-path approval, network sandbox.

## Session Notes (2026-10-01): find allows + agent-catalog name fix

- Policy (shipped + installed `bash-permissions.json`): `find` / `find *`
  allow; ask gates in direct and infix forms for `-delete`, `-exec*`
  (covers -execdir), `-ok*` (covers -okdir), `-fprint*` (covers -fprint0
  and -fprintf since `*` matches zero-or-more), `-fls*`. Needed because a
  policy allow suppresses the built-in heuristic; the heuristic
  `find_args_are_read_only` still guards `bash-permissions: none` setups.
  Infix globs can over-ask (`find . -name -delete-logs.txt`) — accepted.
- Agent catalog: `### research` → `### researcher`, `### explore` →
  `### explorer` in examples/AGENTS.md AND ~/.config/diet_soda/AGENTS.md
  (backup at AGENTS.md.bak). The catalog is the default system prompt
  (init sets `system_prompt: ./AGENTS.md`); wrong headings made
  coordinators delegate to nonexistent agents. Other initialized
  workspaces need a manual refresh — `--init` never overwrites.
- Tests: `find_policy_rules_gate_dangerous_actions` (embedded-policy
  resolve checks), assessment-matrix find cases (incl. outside gate),
  three engine tests in bash_policy_dispatch (27 total). Lib 339.

## Session Notes (2026-10-01): Unified command decision table — read-only deny, classifier fallback, globs

- `command_read_status` (src/tools.rs) implements one table for `shell`/`gh`
  builtin calls: policy deny → tool error; explicit allow → run (outside-path
  prompts preserved); explicit ask → prompt (can_edit) / DENY (read-only);
  catch-all `*` ask or no rule → classifier: safe local reads RUN for all
  agents, unsafe → prompt (can_edit) / DENY (read-only). Denials `bail!`
  before hooks/budget-pause/approval events; message contract:
  `read-only agent: "<invocation>" is not a permitted read operation (<cause>);
  use read_file/grep/web_fetch, or delegate to an edit-capable agent`.
- `local_read_is_safe` excludes network/credential CLIs (aws, awscli, gws,
  npm, pip, pip3, yarn, gh, cargo) from the fallback even in read-only forms
  (`aws eks get-token` leaks credentials into context). Explicit operator
  policy rules still override. Asymmetry pinned by tests: the `gh` builtin
  TOOL uses gh_args_are_read_only (gh pr diff runs); `gh` via SHELL is
  excluded (prompts/denies).
- Classifier hardening (src/tools.rs): sort (-o/--o*/--c[!h]* incl. bundled
  clusters + GNU abbreviations), date (set-clock forms: -s/-S clusters, --s*
  longs, bare numeric operands; -d/-f consume values; -Iseconds safe), file
  (-C/-m/-M magic compile), NEW uniq (≤1 positional; `-` counts; post-`--`
  positionals; `uniq IN OUT` writes), NEW du, fd/fdfind (bundled -x/-X),
   rg (--hostname-bin added to --pre), NEW `git grep` arm (raw-case O/f
   clusters, long-prefix rejection for --open-files-in-pager/--no-index/--file
   abbreviations, git_read_only_flag_is_safe pass; -c/-o false positives are
   safe-direction). TIGHTENED: `git remote show` removed from read-only git
   (network + stored credentials) — security.rs vectors moved accordingly.
   classify_safe_command now passes RAW args to git_args_are_read_only
  (uppercase shorts like -C/-O no longer case-folded; harmless widening).
- `git grep --` is a safe pathspec terminator; textconv, ext-diff, and
  external-diff abbreviations are gated.
- `rg -L`/`--follow` are gated because symlink traversal can read outside
  content.
- `shell_wrapper`: unquoted `* ? [ ]` accepted in argument words (glob
  pipelines like `sh -c 'wc -l src/*.rs | sort -rn | head -40'` decompose and
  run). Residual risks accepted + documented: expansion results unchecked at
  approval time (moot under universal python3/cargo allows); expansion can
  inject flag-like words (file named `-o`); quoted vs unquoted `*`
  indistinguishable. Command position still glob-free.
- `validate_shell_program`: multi-word `command` values ("ls -l") rejected
  pre-prompt with schema-steering error unless the token contains `/` and
  exists as a file relative to workspace/absolute, or is a bare spaced name
  matching an existing workspace file; the bare-name exception stays subject
  to downstream prompt/deny gating. Called in dispatch + builtin shell arm.
- `assess_wrapped_commands`: unchanged signature; WrappedAssessment gains
  `deny_reasons`; per-segment decisions now flow through command_read_status;
  hard policy denies keep the "in shell -c script:" prefix contract;
  `npm install /tmp/x` under a no-catch-all policy denies for read-only
  (unsafe dominates outside — fail-closed).
- dispatch.rs: shell/gh approval tangle replaced by the table; wrapped deny
  bails before any approval; persist keys only for Prompt outcomes (never
  wrapped, never non-normalized paths); custom command tools, read_file
  outside grants, approval_tools, custom hitl UNCHANGED (operator surfaces
  still prompt for everyone).
- Known limit (recorded in README): python3/cargo/make allows mean read-only
  agents can still execute arbitrary code — deny-non-reads bounds accidental
  misuse, not an adversarial model. `./ls`/`./pwd` run via the fallback
  (classifier judges basenames); the path rule still blocks basename ALLOWS
  for unsafe commands at non-bin paths.
- Tests: lib 350 (command_read_status table, matrix deny_reasons rows,
  validate_shell_program, classifier arms); bash_policy_dispatch 36 (8 new
  contract tests incl. the user's two production scripts and
  sed-deny-for-read-only); security 64; tool_lifecycle 21; runtime 41.
  Swept flips documented in-test (agent can_edit flips for ask-mechanics
  tests; deny-contract rewrites for read-only tests). Open USER-WIP failure
  unchanged: runtime subagent_has_isolated_messages_and_keeps_its_own_tool_scope.

## Session Notes (2026-10-01): python3/cargo tiered — universal allows reversed

- REVERSAL: the 19 python/python3/cargo/make allow+ask keys were removed from
  both bash-permissions.json files (installed backup:
  bash-permissions.json.bak-20261001). Rationale: defend against arbitrary
  code execution; policy globs cannot gate cargo subcommands safely
  (`cargo * test*` would match `cargo run test-helper`).
- Replacement = code-level tiers consulted on catch-all/no-rule
  (`command_read_status` + `dev_workflow_is_safe` + hardened
  `classify_safe_command` arms in src/tools.rs):
  - QUERY (runs for ALL agents): cargo --version/-V/help(bare or builtin
    name — non-builtins exec cargo-<name> from PATH, so the list is strict;
    vendor/scripts removed)/metadata --no-deps/locate-project/read-manifest/
    pkgid (requires --locked|--frozen|--offline — bare pkgid can rewrite
    Cargo.lock + hit the registry); python* --version/--help; rustfmt with
    --check + flag whitelist (--print-config rejected — it writes).
  - DEV (can_edit ONLY; read-only deny): cargo test/bench/build/check/
    clippy/fetch/add/remove/update/generate-lockfile/tree; python3 -m
    pytest/unittest/py_compile/compileall/venv/ensurepip; -m pip
    install/uninstall/download/wheel. Gates: is_normalized_command_path;
    exact case-sensitive subcommand/module; pre-flag whitelists; -X
    rejected; forbidden cargo flags --config*/-Z*/-C* rejected ANYWHERE
    before `--` (rustc-wrapper injection; -C rejection is deliberate
    fail-closed); post-`--` passthrough unchecked.
  - ELSE (scripts, -c, run/publish/install, make, awk): prompt editors /
    deny read-only. cargo left NETWORK_CREDENTIAL_COMMANDS (aws/awscli/gws/
    npm/pip/pip3/yarn/gh remain excluded).
- Accepted by design: dev tier executes repo/registry code (build scripts,
  proc macros, conftest.py, pip setup.py); editors can reach unprompted
  arbitrary execution anyway (write_file doesn't prompt editors → build.rs +
  cargo test). --config exclusion is consistency, not a boundary. Read-only
  agents can NO LONGER run arbitrary code via shipped policy.
- Wrapper: `-l` accepted in clusters (`bash -lc`; `bash -l -c` stays
  Unparseable) — residuals: profiles sourced (HOME passes through), profile
  functions/cd invisible to per-segment checks. Exact redirect tokens
  `2>&1`, `2>/dev/null`, `>/dev/null` consumed at word start with boundary
  rules; all other redirects keep scripts Unparseable.
- Blocked-text scan (`script_text_is_blocked`): Unparsable -c scripts are
  scanned pre-approval (quote/backslash strip, lowercase, split on
  metacharacters with | & ; emitted as tokens; basename compare +
  pattern windows + pipeline fallback matching a pipe pattern's right stage
  by its basename-normalized FIRST token) → hard deny on hit; called in
  dispatch before approval/hooks/pause and in the builtin shell arm.
  Best-effort: `$'\x72m'/${v}rm/eval/base64 evade — read-only denied anyway
  (script-driven gate), editors get the human prompt. Limits: only
  bash|sh|zsh|dash -c at normalized paths (not ksh/fish/python -c/find
  -exec). "2>/dev/", "> /dev/", fork-bomb patterns dead for scan context.
- rustfmt --check query arm (user's `rustfmt --edition 2021 --check` case);
  awk stays interpreter-gated (user decision after the python3 reversal);
  make gated (user decision).
- Tests: lib 362+ (cargo/python/rustfmt tier arms, dev_workflow vectors,
  scan tokenizer vectors incl. r""m/rm$(echo)/pipeline forms, matrix tier
  rows, command_read_status_tier_rows); bash_policy_dispatch 43 (7 new
  engine tests covering the user's four production prompts: rustfmt --check,
  bash -lc cargo check 2>&1, blocked rm-script deny, glob pipeline with
  2>/dev/null; tier pins both agent kinds; legacy python/cargo test renamed
  query_tier_forms_run_without_approval_for_read_only_agents with execution
  evidence). Full sweep 774 passed; open USER-WIP failure unchanged
  (subagent_has_isolated_messages...).

## Session Notes (2026-10-02): built-in tool fixes, cd/rm shell policy, output-cap sizing

Session-log forensics showed that most apparent built-in failures were model
misuse or intended policy, rather than harness defects. The real defects fixed
here were `read_file` line ranges, diagnosis of DuckDuckGo's HTTP 202 response,
misleading missing-binary errors, and unhelpful provider HTTP errors. The leading
`cd` and editor `rm` policy relaxations and context/10 output cap were user
decisions (including automatic sizing from the provider catalog; the context/10
cap is superseded — see the 2026-10-05 globals below).

- **`read_file` ranges:** the built-in accepts optional integer `offset` and
  `limit`, both minimum 1; `offset` is a 1-based starting line and `limit` is the
  maximum number of lines. With neither set, the full-file response remains
  unchanged (`{content, truncated}`). With either set, it returns the requested
  range and adds `start_line`, `end_line`, and `total_lines`; offset past EOF
  yields empty content. This lets read-only agents inspect large files without
  `sed -n` (which they are denied).
- **Stringified array arguments:** for built-in tools only, an array-typed
  argument supplied as a JSON-encoded string (for example shell `args` equal to
  `"[\"-n\",\"x\"]"`) is parsed back into an array before schema validation.
  MCP and custom tools are deliberately unaffected. Strings that do not parse as
  JSON arrays remain unchanged and get the original validation error.
- **Leading `cd` in `bash|sh|zsh|dash -c`:** scripts may begin with one or more
  `cd <dir>` segments joined by exactly `&&` (for example `cd src && ls`). Each
  `cd` must itself be leading (only accepted `cd`s may precede it), have exactly
  one literal directory argument, and use no flags, `~`, `$`, backticks, or glob
  characters. For zsh the argument must start with `./`, `../`, or `/`. At
  approval time the harness verifies every target exists and is a directory,
  models the working directory chain, and checks every later segment's relative
  paths against the workspace and every target in that chain. This prevents a
  runtime `cd` failure or a race from hiding an outside relative path. A target
  outside the workspace requires approval. Login profiles (`-l`) for any of the
  four shells can export `CDPATH` or run their own `cd`; zsh's prefix rule
  mitigates the always-sourced `~/.zshenv`, while non-login bash/sh/dash have
  `CDPATH` stripped.
- **Edit-capable `rm`:** `can_edit` agents can auto-remove plain relative
  workspace files with `rm <files>` or `rm -f <files>`. `rm -rf` and `rm -fr`
  remain hard denies through `blocked_patterns`. Recursive/other-flag forms
  (`-r`, `-R`, `--recursive`), globs, `.`, `..`, any operand containing a `..` or
  `.git` path component, trailing-slash operands, and absolute operands fall
  through to normal approval; `can_edit: false` agents are always denied `rm`.
  This auto-allow is disabled after a preceding `cd` changes the working
  directory, so e.g. `cd .git && rm config` prompts. Removing `rm` from
  `blocked_commands` means the raw-script blocked-token scan no longer hard-denies
  bare `rm`; a backstop still hard-denies obfuscated recursive deletes such as
  `rm$(echo) -rf x`.
- **`web_search`:** endpoint remains DuckDuckGo HTML, with no User-Agent or
  endpoint change. DuckDuckGo intermittently responds with HTTP 202 bot-challenge
  markup lacking result nodes; the parser error now includes the HTTP status, e.g.
  `...did not match the expected DuckDuckGo result markup (HTTP 202 Accepted;
  DuckDuckGo may be rate-limiting or serving a challenge page)`.
- **Missing binary:** a nonexistent program invoked through sandboxed shell now
  reports `Command not found: <name>` instead of misreporting
  `Network sandbox failed closed: sandbox-exec: execvp() ... No such file or
  directory`.
- **Provider HTTP errors:** a non-success provider response includes a bounded
  2000-character response-body snippet, with whitespace collapsed and API keys
  redacted (for example `Provider returned HTTP 400 Bad Request: {...}`). Empty
  or unreadable bodies retain the status-only error. This makes errors such as
  invalid model IDs diagnosable.
- **Output cap = context window / 10:** **SUPERSEDED (2026-10-05) — the
  `window / 10` and `window / 4` derivations are gone; see the current model in
  "Session Notes (2026-10-05): explicit context/output token globals" at the
  end of this file.** Historically, `ModelConfig` had optional
  `context_window` (tokens), validated at 10 or greater and omitted from
  serialization when unset, preserving existing config round-trips. Request
  `max_tokens` / `max_completion_tokens` was `output_cap`: if the window was
  known explicitly or from the provider catalog, it was
  `max(context_window / 10, 1)`, further clamped to a discovered per-model
  max-output limit when advertised. With no known window the cap was exactly
  the configured `max_tokens`. Catalog discovery is lazy and safe: the Engine
  caches limits by `(provider base_url, model id)` whenever the catalog is
  fetched (opening the `/model` picker calls `list_models`); requests only
  read that cache, so no request-path network access or latency is introduced.
  Catalog parsing takes context from `context_length`, `context_window`, or
  `max_input_tokens`, and output limits from `top_provider.max_completion_tokens`,
  `max_output_tokens`, or `max_tokens` (first positive integer wins) — that
  parsing is unchanged, but discovered values now only lower the explicit
  globals. The TUI context status shows explicit `context_window`, else the
  global `max_context_tokens`; catalog-only context is not shown.

### Residuals / accepted gaps

- In `-l` login profiles the four shells may set `CDPATH` or execute their own
  `cd`; this can invalidate modeled cwd assumptions. For `-c` parsing, glob
  expansion remains unchecked at approval time and TOCTOU between approval and
  execution remains possible.
- `rm -r` and `rm --recursive` (recursive without `-f`) now prompt editors rather
  than hard-denying. Exotic `rm$(true;echo) -rf` evades the raw-scan backstop and
  prompts with the full script visible to the human rather than hard-denying.
  Multi-line scripts can occasionally over-block a legitimate `rm` (fail-closed).
- `web_search` still depends on DuckDuckGo not rate-limiting or serving a
  challenge page; the improved error only makes the HTTP status visible.

### Test inventory and known failures

- Test inventory: lib 445, runtime 43 (+1 known skip:
  `subagent_has_isolated_messages_and_keeps_its_own_tool_scope`, pre-existing
  user WIP), `bash_policy_dispatch` 44, security 64, core 35, and
  `config_contract` 7.
- Two known pre-existing unrelated failures: `tests/cli.rs`
  `tui_activity_accordion_expands_and_collapses_with_keyboard_and_sgr_mouse`
  (user's concurrent TUI mouse WIP), and `tests/default_bash_policy.rs`
  `embedded_default_bash_policy_auto_allows_make` (stale; the make allow-rule
  was removed in an earlier session and this fails on HEAD too).
- An errant whole-workspace `cargo fmt` run by one worker reformatted files
  beyond its scope. Those changes are semantics-preserving and rustfmt-clean
  (which CI requires), but the user may wish to review or revert formatting in
  files they did not intend to change.

## Session Notes (2026-10-02): bash permissions flipped to allow-all + blacklist

- **Motivation / decision:** allowlist maintenance had hit diminishing returns;
  the user chose an allow-all baseline with a blacklist, backed by code-level
  gates where ordered globs cannot safely express invocation semantics. This is
  an intentional increase in what edit-capable agents can run unprompted, not a
  claim that the classifier is a sandbox.
- **Why code gates were required:** catch-all allow by itself could have
  escalated read-only agents, bypassed the dedicated `gh` builtin's read-only
  classifier, and allowed destructive `rm` through glob expansion. Five
  attack-review rounds found and fixed additional bypass/friction cases:
  non-normalized `./gh`; `git --no-pager push --force` evasion; executing `sed`
  and `awk`; combined flag clusters (`-ic`, `-ucimport…`, `-0777ne`); `fd -Hx`
  and `--exec=`; Perl `-M` payload splicing; Go `-C`/`-exec` hooks; package-manager
  parity; and `cmd /c` / PowerShell `-Command`.
- **Policy contents:** embedded `examples/bash-permissions.json` now has
  `"*": "allow"` first, about 110 ask rules, then deny rules last. Ordered
  rules are last-match-wins; `*`/`?` globs are anchored full-subject matches
  over normalized invocations. The ask tier covers find side effects, Git push
  and history rewriting, other VCS, package/registry operations, rmdir/chmod/
  chown, service managers/schedulers, OS installers, containers/cloud,
  privilege tools, and network clients. Final denies cover Git output/diff-exec
  flags and force pushes. `blocked_commands` and `blocked_patterns` stay hard
  tiers that always deny: commands include shred/mkfs/fdisk/diskutil/dd,
  shutdown/poweroff/reboot/halt, kill/pkill/killall, mount/umount, iptables/
  pfctl, gcloud/az, terraform/kubectl/helm; patterns include rm -rf/-fr,
  Docker prunes, curl-to-shell, writes to `/dev`, fork bombs, base64-to-shell,
  Terraform destroy, destructive kubectl operations, and helm uninstall. Neither
  policy rules nor approval can override them.
- The installed `~/.config/diet_soda/bash-permissions.json` was migrated to be
  byte-identical to the shipped policy. Backups are
  `bash-permissions.json.bak-20261001` and
  `bash-permissions.json.bak-20261002b`. `rmdir`, `chmod`, and `chown` moved
  from hard-deny to ask.
- **Catch-all code gates:** read-only agents ignore the catch-all and still use
  the strict classifier; the built-in `gh` ignores it too. Edit-capable shell
  invocations prompt on non-plain `rm`, wrappers/launchers, inline code,
  Deno/Bun eval/exec or remote specifiers, executing sed, awk, side-effecting
  find, fd execution flags, rg preprocessor hooks, package managers, Go run/
  install/get/generate/tool and code hooks, and unrecognized Git globals.
  Plain relative file deletes and ordinary editor/build/test commands run.
  Parsed `bash -c` segments remain individually judged with cd-chain cwd
  tracking; unparseable scripts get the hard-block pre-scan and whole-invocation
  gates. Network-denied-by-default subprocesses, scrubbed environment,
  outside-workspace path approval, and hard blocks remain containment.
- **Read-only and normalization details:** shell-invoked `gh` gets the builtin's
  read/write parity for normalized paths; non-normalized paths prompt/deny.
  Benign Git globals are stripped before rule matching, so `git --no-pager
  push --force` still hits deny. Unsafe globals (`-c`, `--config-env`,
  `--exec-path`, `--git-dir`, `--work-tree`) downgrade allows to ask. Read-only
  classifier additions include Git blame/rev-list/describe/shortlog/cat-file/
  show-ref/merge-base/name-rev/stash-list, abbreviation-proof flag rejection,
  and head/tail numeric shorthand and attached-value support. Behavior change:
  read-only `git log -c`, `--no-ext-diff`, and `--no-textconv` are denied now;
  old explicit allow rules ran them.
- **Test repointing:** four `bash_policy_dispatch` tests now supply explicit ask
  policies to preserve their assertions; the shipped-allow-all matrix pins
  roughly 140 decisions. `default_bash_policy` is 3/3 again: its formerly stale
  make-auto-allow test passes under allow-all.
- **Residual risks / friction:** blacklist long tail remains: unlisted launchers
  such as `tar --to-command` run for editors by design. `rg -L`/`--follow` now
  prompts (friction); `at*`/`ip*`/`host*` globs conservatively over-match. The
  `-l` login-shell/CDPATH and approval-to-execution TOCTOU residuals from the
  cd feature still apply.
- **Current suite state:** lib 479; `bash_policy_dispatch` 44;
  `default_bash_policy` 3; security 64; core 35; runtime 43 + 1 known skip.
  Known unrelated failures are the CLI accordion mouse test and
  `config_contract` `default_agents`, both user WIP.

## Session Notes (2026-10-02): silent empty-response failures, startup limit discovery, read-only git -C

- **Symptom / evidence:** write subagents (`build`, `test-writer`) and then the
  main agent appeared to "fail silently" / "crash". Session-log forensics
  (`~/.config/diet_soda/sessions/43d0f5e4-*.jsonl`): 17 of 33 write-subagent runs
  returned an empty or single-space result to the parent while their activity
  status was `success`; every one had `output_tokens == 4096` exactly (the output
  cap), i.e. reasoning models (deepseek-v4.1-flash, minimax-m3) spent the whole
  cap on reasoning and emitted nothing. In the last hour the MAIN agent
  (z-ai/glm-5.3-flash, ~192k-token context) returned four assistant turns with
  empty content, no tool calls and only 58–161 output tokens, which the engine
  completed as `Ok("")` — invisible, so it looked like a crash ("crashed?", "?").
  Empty final turns over the whole session: build 14, make 2, test-writer 2,
  main 5.
- **Root causes:** (1) `provider.rs` ignored `finish_reason == "length"`
  (OpenAI/OpenRouter) and Anthropic `stop_reason == "max_tokens"`; the truncated
  stream returned `Ok` with blank text and no tool calls, which the loop treats as
  a final answer. (2) `conversation_inner` returned `Ok(content)` for ANY
  assistant turn without tool calls, including blank ones. (3) The "output cap =
  context_window/10" feature (earlier today; superseded 2026-10-05 by the
  explicit `max_output_tokens` / `max_context_tokens` globals — the derivation
  no longer exists) only applied when a window was
  known; discovery was lazy (filled only when the `/model` picker called
  `Engine::list_models`), so subagents stayed at the default `model.max_tokens`
  of 4096 in practice.
- **Fixes:**
  - Truncation is now an error: `RemoteProvider::stream` returns an
    `IncompleteStreamError` ("response truncated: the model stopped at its max
    output token limit (N tokens); raise max_tokens, or context_window if the cap
    is derived from it (a model's advertised max output is a hard ceiling)") when
    an OpenAI-style `finish_reason` is `length`/`max_tokens` or Anthropic
    `stop_reason` is `max_tokens`. The check runs before the EOF/`[DONE]`
    acceptance block, so it applies with and without `[DONE]` and with partial
    tool calls. Truncated output is ALWAYS an error, even when some text arrived
    (documented decision). For streams that completed the protocol, the billed
    usage is attached to the error (`IncompleteStreamError.usage`) and the engine
    records it (same price-estimate fallback as success via
    `apply_cost_estimate`), so spend accounting is not lost.
    `IncompleteStreamError`'s Display is now "Provider response incomplete:
    {reason}".
  - Empty final turn is an error: in `conversation_inner`, an assistant turn with
    no tool calls and blank (`trim().is_empty()`) content persists an `incomplete`
    marker (visible in the transcript, excluded from model-visible history so a
    retry does not replay `content: ""`) and returns an error ("model returned an
    empty response (N output tokens, no tool calls) for model M; retry the turn,
    or switch models if it repeats"). Usage/spend stay recorded; the `after_model`
    hook is not emitted for the discarded turn. This covers main turns, subagent
    children (a blank child final is now a delegate TOOL ERROR instead of
    `{"result":""}`), and WORKFLOW steps (a step whose model answers blank now
    fails the run and goes through the existing retry/skip/abort gate). No
    automatic retry was added.
  - Startup limit discovery: new `Config.discover_model_limits` (default true).
    `Engine::prefetch_limits()` calls `list_models` for every configured provider
    concurrently and ignores all errors; headless/`--prompt` runs await it for at
    most 5 seconds before the first turn (adds up to 5 s startup latency when a
    catalog endpoint is slow or unreachable), the TUI runs it in a background task
    (the first TUI turn can race it). `/model` still refreshes the same cache.
    Tests that spawn the real binary against the sequential mock server set the
    flag to false (cli.rs builders, hooks_acceptance, and the three Python pty
    fixtures). Not implemented: refresh on `/reload` (that path lives in
    `src/tui/commands.rs`, user WIP). If discovery fails and no `context_window` is
    set, the cap stays at `max_tokens` (default 4096) — recommend setting
    `context_window` for the reasoning models subagents use. *(Superseded
    2026-10-05: the cap is now the explicit global `max_output_tokens` (default
    128_000) or the model's `max_tokens` override; discovery only lowers it, and
    there is no 4096-era default.)*
  - Read-only `git -C`: `git_args_are_read_only` strips leading benign globals
    (`-C <path>`, `--no-pager`, `--paginate`, `-p`, `-P`, `--no-optional-locks`,
    `--literal-pathspecs`, `--glob-pathspecs`, `--noglob-pathspecs`,
    `--no-replace-objects`, `--bare`) before classifying; `-c`, `--config-env`,
    `--exec-path`, `--git-dir`, `--work-tree`, `--namespace`, `--super-prefix`,
    unknown flags and attached `-C<path>` still deny. A second or later `-C` with
    a relative value is rejected (git chains `-C` relative to the previous one,
    which the per-argument outside-path gate cannot model); the editor catch-all
    helper `git_leading_globals_all_known` applies the same rule (→ Prompt).
    `-p`/`--paginate` are accepted only because every subprocess is spawned with
    piped stdio and a scrubbed env (git launches a pager only on a TTY); if a PTY
    spawn mode is ever added, remove them or pin `GIT_PAGER=cat` first. This fixes
    the explorer's `git -C <path> status/log/branch/diff` denials caused by the
    allowlist removal.
- **Reviewer environment note:** the `code-review` agent's configured model
  `openrouter/qwen/qwen3.8-max` was no longer offered; reviews were run with
  `openrouter/qwen/qwen3.8-max-prime` (user-approved for this session). Update the
  agent config to avoid this.
- **Residual risks / follow-ups:** a model that keeps returning blank turns now
  produces a visible error each time rather than silence (user must retry or
  switch models); `GIT_PAGER=cat` pin in `process.rs` isolated env was suggested as
  defense in depth (not done); `-C <other repo under an allowed access root>`
  extends the already-accepted repo-config hazard class (fsmonitor/textconv in
  that repo's config) to those repos; discovery uses every configured provider (an
  unreachable one costs up to the 5 s bound on headless start); main-agent models
  with ~190k token histories are unusually likely to emit blank turns (consider
  compaction).
- **Verification state:** lib 483, runtime 61 (+1 known failing test
  `subagent_has_isolated_messages_and_keeps_its_own_tool_scope`, pre-existing user
  WIP, run with `--skip`), cli 26 (+1 known failure
  `tui_activity_accordion_expands_and_collapses_with_keyboard_and_sgr_mouse`, user
  TUI WIP), hooks_acceptance 5, security 64, bash_policy_dispatch 44,
  subagent_lifecycle 12, parallel_agents 5, workflow_activity 12, reasoning 6,
  prompt_cache 6, edge_wave2 20; config_contract has the known user-WIP failure
  `default_agents_use_the_requested_models`.

### Corrections (2026-10-02, later): test failures resolved

- Supersedes the "known failing tests" statements in the earlier 2026-10-01/02 sections of this file (roughly lines ~925-930, ~1004-1005 and ~1083-1099) and the "run runtime with `--skip subagent_has_isolated_messages_and_keeps_its_own_tool_scope`" advice. Those lines are left unedited as history.
- `tests/runtime.rs::subagent_has_isolated_messages_and_keeps_its_own_tool_scope` — FIXED. The child `researcher` AgentConfig now sets `can_edit: true`; `AgentConfig::default()` has `can_edit: false` and the advertising filter (`src/engine/dispatch.rs` ~:209) hides `write_file` from non-editing agents, so the test could never see `write_file`. The previously unreachable `session.spend.microusd == 369` assertion now runs and passes. The runtime suite no longer needs any `--skip`.
- `tests/config_contract.rs::default_agents_use_the_requested_models` — FIXED. The test's "example config" expectations were realigned to the user's edited `examples/config.json` (plan, elephant, reviewer, code-review, plan-review). A typo in the config was also fixed: `elephant` model `openrouter:deepseek-v4.1-flash` (missing vendor segment) → `openrouter:deepseek/deepseek-v4.1-flash` (user-confirmed).
- `tests/cli.rs::tui_activity_accordion_expands_and_collapses_with_keyboard_and_sgr_mouse` — FIXED in the fixture (`tests/fixtures/activity_accordion.py`), not in `src/tui/*`: the fixture sent only an SGR mouse PRESS; the mouse-selection feature starts a selection on press and turns a press+release at the same cell into the activity click (`src/tui/selection.rs` `handle_mouse`, synthetic Down on Up-without-movement), so the driver now sends the matching release (`…m`) too.
- opencode `code-review` subagent: `~/.config/opencode/opencode.jsonc` model `openrouter/qwen/qwen3.8-prime` (not in the OpenRouter catalog; `qwen3.8-max` had been removed earlier) → `openrouter/qwen/qwen3.8-max-prime`. Verified working by a real review dispatch.
- Resolved (2026-10-03, superseded 2026-10-05 by the explicit `max_context_tokens` / `max_output_tokens` globals): the output cap now falls back to `max_tokens` clamped by a discovered max output (the built-in default `tokens()` is 128_000, so an unconfigured model no longer sends 1000000 as the cap); `examples/config.json` now sets per-model `context_window` on `fast` (1048576) and `reasoner` (1000000). The user should regenerate their live config to pick this up.
- Current full-suite expectation: `cargo test` with no filters or skips passes on every target.

## Session Notes (2026-10-02): perl is an editor-safe command

- **Request:** writer (`can_edit`) agents use `perl -pi -e 's/a/b/' file` for in-place edits and stalled on prompts. Under the allow-all bash policy, inline-code perl (`-e`/`-E`/`-n`/`-p` clusters, attached bodies, `-M` payloads) was gated as "inline script" (`invocation_is_script_driven`), so only `perl script.pl` ran unprompted.
- **Change (src/tools.rs):** non-executing perl now auto-runs for edit-capable agents, mirroring `sed`. `editor_policy_override` has a perl-family arm (`is_perl_family`: `perl` or `perl5.xx`): when `perl_args_may_execute(args)` is false a missing/catch-all rule becomes `("perl (can_edit)", Allow)`; specific operator `ask`/`deny` rules (e.g. `perl*`) still win. A new gate in the `catch_all_allow` editor chain (after the sed arm, before awk and the wrapper/script-driven arm) returns Prompt("perl can execute commands") for normalized-path perl that the scanner flags — required because unflagged perl now bypasses the script-driven prompt, so every scanner miss would otherwise Run. Read-only agents are unchanged (strict classifier: perl inline/scripts denied). Wrappers (`env perl`, `sudo perl`) and non-normalized paths (`./perl`, `/tmp/y/perl`) are unchanged; the outside-workspace argv gate still applies (`-pi -e … /etc/hosts` → approval). Parsed `bash -c` segments benefit too.
- **Scanner (`perl_args_may_execute`, fail-closed, best-effort):** walks switch clusters (`-e`/`-E` bodies attached or detached, `-M`/`-m` payloads, value-taking `F`/`I`/`i`/`C`/`V`), operands, and `--`. Flags: switches `-x -d -D -S -P`; whitespace/control/punctuation inside switch args outside a body or value (perl keeps parsing after whitespace: `-p -e system(1)` in ONE argv element); unknown long options (except `--version`/`--help`); a missing `-e` body; `-M` payloads outside `[A-Za-z0-9_:=,.-]`; attached `-I` values that start with `/` or contain `..`; `-i` backup suffixes containing `/`; operands that contain `|`, start with `<`/`>`/`+`, have surrounding whitespace or control chars (perl's 2-argument `open` on `<>`/ARGV operands runs `cmd|` and writes `>f` — this also tightens `perl x.pl 'a|b'`, which used to run); bodies and payloads containing backticks, the substrings `CORE::GLOBAL`, `IO::`, `HTTP::Tiny`, `LWP`, `Net::`, `FileHandle`, `Proc::`, `Expect`, `CPAN`, `Win32`, or the identifiers `system exec fork qx readpipe syscall popen rmtree remove_tree Open2 Open3 IPC chmod chown rmdir socket connect eval open ARGV ARGVOUT readline kill`.
- **Review history:** security attack review ran four rounds (FAIL → fix → PASS). Real holes found and closed: in-body `@ARGV="git push|"; <>` (needs identifier `ARGV`/`readline`), `IO::File->new("git push|")` / `IO::Pipe` (substring `IO::`), `-S cpan` and `-MCPAN -e 'CPAN::Shell->install(...)'`, attached `-I../x` bypassing the outside gate, `-pi.bak/x` suffix through a workspace symlink, `kill`, `Win32::Spawn`, and the positional-operand magic-open and one-argument switch-cluster forms.
- **Known false positives (extra prompts):** plain-text uses of the flagged words (`s/system/foo/`, `s/CPAN/cpan/`, `/@ARGV/`), `-Mopen`/`PerlIO::` layers, `-pi~` (`~` is outside the suffix charset), operands like `a|b`, ` f`, `>out`.
- **Accepted residuals (documented in the fn doc comment):** `do FILE`/`require FILE`, `s///ee` string-eval of data, obfuscated symbolic calls (`&{"sys"."tem"}`), `-M` module code beyond the scan, perl reading/writing/deleting anywhere via file ops (`sysopen`, `syswrite`, `rename`, `link`, `symlink`) beyond the argv path gate — the same exposure class as `python3 x.py` which editors already run — and `unlink glob(...)` mass deletes bypassing the plain-`rm` rules.
- **Tests:** `perl_writer_walker_{family,true_side,false_side}`, `perl_writer_{editor_runs,editor_prompts,path_and_wrapper_forms}_both_policies` (each runs under the embedded allow-all policy AND an inline `*: ask` policy), `perl_writer_specific_rule_outside_gate_and_read_only`, `perl_writer_wrapped_*`. Two pre-existing tests were updated deliberately: `catch_all_bypass_editor_prompts_for_executable_scripts_and_wrappers` and `catch_all_bypass2_editor_prompts_for_inline_code_and_find_actions` had pinned BENIGN perl inline (`-eprint 1`, `-lane x`) as editor-Prompt; those cases moved to the editor-Run lists, malicious-body equivalents (`-eprint 1;system('x')`, `-lane system(1)`) were added to the Prompt lists, and every other malicious pin was left untouched. The pure `invocation_is_script_driven` function and its direct asserts are unchanged.
- **Supersedes:** earlier notes in this file that list `perl -e/-E/-n…` among always-prompting forms for edit-capable agents now apply only to bodies the scanner flags.

## Session Notes (2026-10-03): output-cap fallback hardening (SUPERSEDED 2026-10-05)

**Superseded:** the `output_cap` fallbacks and `window`-derived cap described
here were replaced by the explicit `max_context_tokens` / `max_output_tokens`
globals (see "Session Notes (2026-10-05): explicit context/output token
globals" below). Kept as history; only the discovery-failure logging and
debug-log items still stand.

- **Root cause of provider HTTP 400 ("...1000000 in the output"):** the request output cap is `ModelConfig::output_cap`; when no context window was known it fell back to the configured `max_tokens` verbatim. The live config's old `max_tokens: 1000000` was sent as the requested output, exceeding the endpoint's total context and getting rejected. The installed binary was NOT stale — the failing process held a pre-edit config value.
- **Fixes:** (1) `output_cap`'s no-window fallback now clamps `max_tokens` to a discovered per-model max output (`src/config.rs`); (2) built-in default `tokens()` lowered to `128_000`; (3) catalog-discovery failures are logged via `tracing::warn!` and surfaced as a `UiEvent::Status` only for the default/active provider (`src/engine/mod.rs` `prefetch_limits`); (4) `src/provider.rs` logs the derived cap at debug level (`target: "diet_soda::provider"`, "derived output cap") to `<sessions_dir>/diet_soda.log`.
- **Template/docs:** `examples/config.json` now sets `context_window` (1048576 on `fast`, 1000000 on `reasoner`); `examples/CONFIGURATION.md` and `README.md` document the precedence; tests reconciled (`context_window_tests`, two `tests/runtime.rs` default assertions).
- **Live verification:** throwaway config + real OpenRouter call → derived cap 262144 for `z-ai/glm-5.3-flash`, request returned OK, no 400.
- **Handoff:** run `make release` to install the fixed binary; regenerate the live config with a fresh `--init` (or hand-set a sane `max_tokens`/`context_window`); restart any long-running session.
- **Caveat:** the working tree carries unrelated WIP that leaves some committed tests red (`tui::picker` x2, `config_contract::default_agents_use_the_requested_models`, runtime `custom_tool_without_max_output_bytes_uses_huge_default` / `list_models_discovery_caps_output_tokens` / `prefetch_limits_fills_cache_before_turn`); none are caused by this change set.

## Session Notes (2026-10-04): reason-accurate retry notes + transient stream auto-retry

- **Symptom:** diet_soda kept showing "Your previous response was truncated at the output token limit … Re-issue the affected tool call in smaller pieces". It was NOT an output-limit truncation.
- **Root cause:** the injected note was a single `RETRY_NOTE` used for EVERY `IncompleteStreamError` reason. Live failures recorded reason `incomplete tool call from provider` (src/provider.rs:637 — a protocol stream that ended with a degenerate tool call, empty id/name) not a length stop. Driver: a ~429k-token main-context history with no compaction makes the reasoning model intermittently emit malformed calls. The EOF-tolerant path can also reach that branch, so this is not a claim that the protocol always completed.
- **Fixes:** (1) `retry_note(reason)` selects `RETRY_NOTE` for output-limit reasons, else the new `STREAM_FAILURE_NOTE` (names the reason, asks to continue), used at both injection sites (src/engine/mod.rs). (2) One-shot auto-retry per model iteration for an explicit allow-list of transient reasons: "stream ended before completion event", "incomplete tool call from provider", "provider reported a streaming error" (cancellation, header/idle timeout, 16 MB overflow, empty turns are NOT retried); the failed attempt's billed usage is recorded and `attempt` is rolled back so the context-overflow retry stays available. (3) The degenerate-call path (src/provider.rs) now records billed usage under the same `finished && tokens_reported` policy as the truncation path. `before_model` hooks and the `model_request` record fire once per turn (the retry re-issues only the provider call).
- **Tests:** `cargo test --lib retry_note` (2), plus new provider/engine tests in tests/runtime.rs: `provider_degenerate_tool_call_is_incomplete_with_recorded_usage`, `transient_stream_failure_is_retried_once_and_recovers`, `transient_stream_failure_retry_exhausted_records_stream_failure_note`, `output_limit_truncation_is_not_retried`.
- **Open:** context compaction remains the real fix for the giant history (not implemented). Code-review minor follow-ups: reset/clear the TUI live-stream entry on retry (deltas from the failed attempt currently concatenate until the final Message replaces them); the allow-list/reason strings are stringly duplicated across provider.rs and engine/mod.rs.

## Session Notes (2026-10-05): explicit context/output token globals

- Two explicit top-level config settings bound every request:
  `max_context_tokens` (default `1_000_000`) is the maximum session context /
  maximum input tokens per request — history is trimmed to fit the remaining
  input budget after reserving the output cap and the system/tool prompt bytes
  (`src/engine/context_budget.rs`; an irreducible overflow fails with
  `ContextBudgetExceeded` rather than sending an oversized request); and
  `max_output_tokens` (default `128_000`) is the maximum output tokens per
  request, sent as `max_tokens` / OpenAI `max_completion_tokens` / Anthropic
  `max_tokens`. Validation requires both to be positive and
  `max_output_tokens <= max_context_tokens`.
- Per-model optional overrides: a model's `max_tokens` overrides
  `max_output_tokens`; its `context_window` overrides `max_context_tokens`.
  A catalog-discovered context window or max-output limit only LOWERS the
  effective value, never raises it. The old `window / 4` (and earlier
  `window / 10`) derivation and the 131,072 fallback are removed. If the
  effective output cap would meet or exceed the context limit it is clamped
  below it, leaving no input budget; such a request fails with a
  context-budget error rather than being silently shrunk. Config validation
  rejects an explicit `context_window` that does not leave room below the
  output cap. The TUI status bar shows the configured context limit: the model
  `context_window`, else the global `max_context_tokens` (a discovered catalog
  window is not consulted for that readout).
- `resolve_model` now resets a raw `provider:id` model's `max_tokens` /
  `context_window` (alongside `reasoning`) when the ID differs from the
  top-level `model`, so top-level overrides do not leak onto other model IDs.
- `--init` now emits `max_context_tokens` / `max_output_tokens` in the
  generated `config.json` and omits `model.max_tokens` (the globals plus
  optional per-model overrides replace it).
