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
- Agents have `can_edit` (false by default). Scope narrowing prevents children from
  gaining edit permission. `can_edit` gates write_file, destructive custom command
  tools, and tools from MCP servers marked `hitl`. Root/main agents keep `shell`
  for recognized safe forms; children omitting `tools` default to `web_fetch`,
  `read_file`, and `load_skill` (no shell), intersected with the parent, and
  explicit child lists may include `shell` subject to parent intersection and
  normal approval policy. `bash-permissions: unified` loads the shared
  `bash-permissions.json` deny policy before command execution.
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
- Generated default agent models: `plan` GPT Luna latest, `build` DeepSeek v4.1
  Flash, `elephant` Qwen 3.8 Max, `code-review` GLM 5.3, and `plan-review`
  Kimi K3. The default plan agent remains visible; all generated subagents are
  hidden from switching.
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
  preserving old files and selected model/agent/runtime settings. `:q` aliases `/quit`.
- Themes include syntax palettes, semantic status colors, and CTA colors. The TUI
  inherits the terminal's selected system font; `theme.ascii` enables ASCII borders.
- Tab/Shift+Tab cycle every configured agent (hidden included) alphabetically while
  idle, wrapping and preserving the draft; the bare `default` sentinel is omitted
  when an agent is marked `default`. Modal input takes priority.
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
  supported. Tab/Shift+Tab cycle agents while idle.
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
  `context X/Y` header readout (Y is the model's `max_tokens`);
  `Session.context_tokens` restores the latest main request size on resume.
  The full workspace path anchors to the footer's bottom-right with a leading
  ellipsis for long paths, and refreshes on `/reload`.

### Pause-aware execution budgets and builtin timeouts
- `src/engine/budget.rs` implements agent execution deadlines (default 30
  minutes via `timeout_seconds`) that freeze while a tool approval waits —
  including approvals inside child agents, which also freeze every ancestor's
  deadline — so a run paused for a human decision does not burn its budget.
  Workflow HITL gates run after a step's conversation completes and each step
  starts a fresh budget. Lock ordering is child→parent only; deadline
  arithmetic is saturating.
- `builtin_timeouts.shell_timeout_seconds` and `gh_timeout_seconds` (default
  120 each, validated positive) replace fixed deadlines for the shell and `gh`
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
  index 0 maps to that inner y. The divider and side border columns lie
  outside `last_history_rect` and are intentionally not activity-selectable.
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
uses it; that import is now `cfg(unix)`. Stable and Rust 1.84.1 formatting, Clippy,
and full tests pass locally except for the known macOS controlling-PTY `ENOTTY`
fixture. The linked PR's Rust 1.84 job was cancelled after stable failed; the
Windows native job still needs a new CI run for confirmation.

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
