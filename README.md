# diet_soda

A small Rust terminal agent harness with streaming conversations, tools, MCP,
subagents, workflows, skills, and process-based plugin hooks.

The TUI keeps the active model, reasoning effort, session spend, conversation, and
multiline input visible. Fenced code and JSON tool output have syntax highlighting;
colors distinguish speakers, status, errors, and action buttons. The orchestration
library also runs without a terminal.

**Design priorities: performance and simplicity.** One Rust package, a small set of
focused modules, explicit configuration, and no background daemon or database.

## Quick start

### Download a binary

Versioned binaries are published to [GitHub Releases](https://github.com/ultratoast/diet_soda/releases).
Download the archive for your computer and extract it. Rust is not needed to run it.

| Platform | Archive suffix |
|---|---|
| macOS, Apple Silicon | `aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `x86_64-apple-darwin.tar.gz` |
| Linux, x86-64 (glibc 2.35+, such as Ubuntu 22.04+) | `x86_64-unknown-linux-gnu.tar.gz` |
| Windows, x86-64 | `x86_64-pc-windows-msvc.zip` |

Each archive contains `diet_soda` (`diet_soda.exe` on Windows), this README,
the license, and the example configuration/workflows/skills. Put the executable in
a directory on your `PATH`, then run these commands from the project you want to use:

```sh
export OPENROUTER_API_KEY='your-key'
diet_soda
```

The first launch with no existing `~/.config/diet_soda/config.json` automatically
creates the default configuration tree (`config.json`, `AGENTS.md`, `theme.json`,
`bash-permissions.json`, `CONFIGURATION.md`, `QUEUE_AND_ACCESS.md`, the
`workflows/`, `skills/`, `prompts/`, `sessions/`, and `exports/` directories,
and the default prompt templates). A status line is written to **stderr**; stdout
stays clean so scripts that capture `--prompt` output see only model text. Run
`diet_soda --init` instead if you want to print the success line on stdout
(`Created <path>`) and to refuse the operation when a config already exists.

On Windows, use `$env:OPENROUTER_API_KEY = 'your-key'` in PowerShell, and
`.\diet_soda.exe` if running the executable from the current directory.
Keys already exported in your shell environment are inherited automatically.

Releases include `SHA256SUMS`. Compare your download's SHA-256 against its entry
using `shasum -a 256 <archive>` on macOS, `sha256sum <archive>` on Linux, or
`Get-FileHash <archive> -Algorithm SHA256` in PowerShell.

### Build from source

Building requires **Rust 1.84+**. Python 3 is needed only for the example MCP server,
example plugins, and their integration tests.

```sh
cargo build --locked
export OPENROUTER_API_KEY='your-key'
cargo run --locked
```

The first launch with no existing `~/.config/diet_soda/config.json` auto-creates
the default configuration tree. Use `cargo run --locked -- --init` to create it
explicitly; `--init` writes its success line to stdout and refuses to overwrite
an existing config. Set the model to one available to your OpenRouter account. By
default the harness uses `openai/gpt-4.1-mini`; model availability and prices are
controlled by the provider.

To try the richer example configuration directly:

```sh
cargo run --locked -- --config examples/config.json --validate-config
cargo run --locked -- --config examples/config.json
```

To build and install the binary locally:

```sh
make release
diet_soda --config /path/to/config.json
```

`make release` performs a locked release build and installs `diet_soda` to
`~/.cargo/bin/diet_soda`.

To build and test the optimized binary locally without installing it:

```sh
cargo build --release --locked --bin diet_soda
./target/release/diet_soda
```

Run these commands from the repository root. The binary is `target/release/diet_soda`
(`target/release/diet_soda.exe` on Windows). If you do not have a config yet,
run `./target/release/diet_soda --init` first, or use `--config examples/config.json`.
It inherits exported API keys from the terminal where you launch it.

## What's implemented

- OpenRouter, LiteLLM, OpenAI Chat Completions, and Anthropic Messages adapters
- Streaming text and tool calls, multi-turn history, cancellation, token usage
- Provider-reported spend or configured price estimates, including parallel child agents
- Append-only JSONL sessions with resume, exclusive locks, and interrupted-tail recovery
- Built-in web/file/process tools and JSON-defined command/HTTP tools
- JSON Schema argument validation, tool approvals, runtime enable/disable controls
- MCP stdio and Streamable HTTP: initialization, paginated tool discovery, calls,
  session headers, shutdown, and restart
- Configurable agent prompts/modes and agent-directed parallel subagent dispatch
- Exact standalone workflow JSON format with **post-step** HITL gates
- Local/HTTPS skill installation, discovery, activation, and on-demand loading
- JSON-over-stdin lifecycle/plugin hooks
- Themed, syntax-highlighted Ratatui interface and headless CLI
- Runtime model/MCP additions, reasoning controls, and timestamped text exports

## Configuration

**Edit local files, not the binary.** Every startup reads the latest configuration
from `~/.config/diet_soda/config.json`, independent of the launch directory. No
recompilation is needed for models/providers, MCP definitions, tools, prompts,
agents, modes, theme settings, workflows, or skills. Models, MCPs, agents, modes,
tools, hooks, and theme settings are sections of the main JSON file; workflows and
skills have their own files beneath the same directory.

Prompt settings support config-relative file references. For example:

```json
{
  "system_prompt": "./AGENTS.md",
  "agents": {
    "reviewer": { "prompt": "./prompts/reviewer.md" }
  }
}
```

Values beginning with `./` are read as UTF-8 files relative to the directory that
contains `config.json`; other strings remain inline prompt text. This applies to
`system_prompt`, agent `prompt`/`system_prompt`, and agent-mode `prompt`. Missing,
non-file, non-UTF-8, or over-1-MB prompt references fail configuration loading.

Each configured agent has `can_edit`, defaulting to `false`. This controls whether
that agent may receive `write_file`, `shell`, or configured destructive command
tools. Subagents cannot widen the parent agent's permission. Set `"can_edit": true`
only on agents that are explicitly trusted to modify files or run update commands.
The workspace is the default filesystem boundary: `write_file` rejects absolute
paths, traversal, and symlink escapes outside it. `read_file` requests approval
before reading an existing file outside the workspace. Configured command tools
must use a working directory inside the workspace unless the agent explicitly sets
`"allow_outside_workspace": true`. That permission is also narrowed for children
and cannot override a parent denial.

Set `"bash-permissions": "unified"` to apply the shared
`~/.config/diet_soda/bash-permissions.json` policy to shell and command tools. The
policy blocks dangerous command names and invocation fragments before execution.
Use `"none"` only when you have intentionally replaced the safety policy elsewhere.
The standard policy file ships with the tool and is created by `diet_soda --init`.

Default layout (also used on macOS rather than `~/Library/Application Support`):

```text
~/.config/diet_soda/
  config.json
  AGENTS.md
  theme.json
  bash-permissions.json
  CONFIGURATION.md
  QUEUE_AND_ACCESS.md
  .diet_soda-init.lock
  workflows/*.json
  skills/<name>/SKILL.md
  prompts/*.md
  sessions/<session-id>.jsonl
  sessions/diet_soda.log
  exports/MM:DD:YYYY-HH:mm:ss.txt
```

Initialize once with `diet_soda --init`, edit the files with any text editor,
and restart the app to use your changes. `/reload` also rereads the active config
while idle, resetting model/agent/mode/effort and tool/MCP overrides and applying
the configured theme. It keeps the current session; changes to session/log storage
locations take effect on restart. Invalid files produce an error rather than
silently reverting to compiled defaults.

The default config path auto-initializes on first use when no `--config` flag
is supplied: the binary creates the full tree above and announces the action on
stderr, then loads and runs. `--config /path/to/config.json` is the explicit
override; if the path you pass does not exist, the binary reports the missing
path and exits with an error. `diet_soda --init` is the explicit, non-overwriting
variant of the same initialization: it writes its success line to stdout and
refuses to overwrite an existing config or any companion file. Auto-init uses a
persistent sentinel file (`.diet_soda-init.lock`) beside the config so two
concurrent first-run launches cannot publish overlapping contents; whichever
process wins, the tree is complete when the loser proceeds to load. The
sentinel file itself stays on disk across launches; only the per-process
`flock` (Unix) / `LockFileEx` (Windows) lock it carries is held while the
publisher is alive.

The default workspace is **the directory you launch from**, not the config directory.
Omit `workspace` or leave it as `""` for this behavior. An explicit `workspace`
path is resolved relative to the config file; use an absolute path to pin a project.
Storage directories, skill directories, workflow directories, and custom-command `cwd` also resolve
relative to the config file, with `~/` expansion. Executable arguments are literal
argv entries; relative paths inside them resolve in the process's working directory.

`--config /path/to/config.json` is still available as an explicit override for
testing or isolated configurations. There is no automatic project-local fallback.
See [`examples/config.json`](examples/config.json) for all supported settings.
See [`examples/CONFIGURATION.md`](examples/CONFIGURATION.md) for the editable array
shapes and field examples. `diet_soda --init` installs a copy as
`~/.config/diet_soda/CONFIGURATION.md`.
See [`examples/QUEUE_AND_ACCESS.md`](examples/QUEUE_AND_ACCESS.md) for queued
messages and the outside-access approval rules; `--init` installs it as
`~/.config/diet_soda/QUEUE_AND_ACCESS.md`.
If migrating an existing config, leave the old file intact until you have copied
your settings to the new location and checked relative paths. In particular, change
`workspace: "."` to `workspace: ""` to keep using the launch directory, and set
`skills_dir`, `workflows_dir`, `sessions_dir`, and `exports_dir` to `"skills"`,
`"workflows"`, `"sessions"`, and `"exports"` to use the new layout. Existing files
and sessions are never moved or deleted automatically.

These files are local. Credentials are supplied through environment variables.
Provider keys use `api_key_env`; a missing key is reported when that provider is
used, so unused providers need no credentials. Headers and explicit subprocess
environment entries support `${VARIABLE}` references, resolved at execution.
Resolved, explicitly configured secret values are redacted from session records.
Do not put credentials in URLs or prompts; unrelated secrets printed by a program
cannot be recognized automatically.

### Providers and model names

Provider definitions contain `kind`, `base_url`, `api_key_env` (or `null` for an
unauthenticated local endpoint), and `timeout_seconds` (default 120).

| Kind | Base URL example | API |
|---|---|---|
| `openrouter` | `https://openrouter.ai/api/v1` | `/chat/completions` |
| `litellm` | `http://localhost:4000/v1` | `/chat/completions` |
| `openai` | `https://api.openai.com/v1` | `/chat/completions` |
| `anthropic` | `https://api.anthropic.com/v1` | `/messages` |

OpenAI-compatible remote services can be configured with `kind: "openai"` and
their own endpoint. Services requiring a different wire protocol need an adapter
implementing `ModelProvider`.

Model references in agents, workflows, and `/model` resolve as:

1. A key in `models`, such as `fast`.
2. `provider-name:model-id`, such as `openrouter:anthropic/claude-sonnet-4`.
3. A plain model ID using the configured default provider.

**Slashes are always part of the model ID.** Do not write
`openrouter/anthropic/claude-sonnet-4` to select a provider.

Model settings include `max_tokens`, optional `temperature`, optional `reasoning`, and optional
`input_usd_per_million` / `output_usd_per_million`. Provider-reported cost takes
priority. Otherwise configured prices estimate cost from reported tokens. The
TUI marks estimates with `~` and adds `+ unknown` for unpriced requests; unavailable
pricing is never represented as a known zero. Estimates do not model cache pricing,
per-request fees, or provider-specific discounts. Interrupted streams may have
incurred charges that were not reported.

### Reasoning effort

Declare a model's supported effort levels explicitly. The harness does not guess
capabilities from a model name or send unsupported settings to every endpoint.

```json
{
  "provider": "openrouter",
  "model": "openai/gpt-5",
  "max_tokens": 16384,
  "reasoning": {
    "supported_efforts": ["minimal", "low", "medium", "high"],
    "effort": "medium"
  }
}
```

Put this under an alias in `models`, then use `/model <alias>` and `/effort low`.
`/effort` shows the current/supported levels; `/effort default` removes the runtime
override. Changes take effect on the next turn and are not persisted. Switching
models clears the override. A model without a `reasoning` declaration uses its
provider default and rejects `/effort` changes with a helpful error.

The wire fields are OpenRouter `reasoning.effort`, OpenAI/LiteLLM
`reasoning_effort`, and Anthropic `output_config.effort`. The complete vocabulary is
`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`; configure only the
levels accepted by your particular model. Direct Anthropic effort uses `low`
through `max`, not `none`/`minimal`, and requires an effort-capable model. Older
Anthropic budget-only thinking controls are not exposed as effort levels.

Provider-returned signed thinking/continuation metadata is preserved across tool
calls and session resume. It is not rendered as conversation text or exported as
a reasoning transcript. No extra API requests are made to discover capabilities.

In a workflow, an effort override must be supported by every selected step model.
Children use their own configured model/effort settings. Headless runs can also
use `--effort low`.

### Themes

Open `/theme` for a searchable picker with **live preview** as you type or browse.
Enter applies the highlighted theme for this session; Esc or Ctrl+C restores the
previous theme. `/theme <name>` selects directly, and `/theme configured` restores
the theme loaded from your config. Theme changes work during a run and do not
modify your configuration file.

| Built-in | Palette |
|---|---|
| `haxx0r` | Black background with phosphor greens |
| `BnP` | Black background with shades of pink |
| `solarized` | Solarized dark with its standard teal background and accents |
| `mama_j` | White and gray text on black |
| `diet_soda` | Near-black background with green and pink text |
| `blue` | Midnight blue background with ice-blue and cobalt accents |

Each preset includes matching syntax colors. To make a preset your startup theme,
set the top-level configuration field, for example `"theme": "diet_soda"`, then
`/reload`. The picker also includes **configured**, preserving your existing custom
palette. Built-in selection keeps your ASCII-border and syntax-highlighting toggles.

Alternatively, the `theme` object contains `#RRGGBB` values for `background`, `foreground`,
`accent`, `user`, `assistant`, `tool`, `error`, `border`, `muted`, `success`,
`warning`, `cta_background`, and `cta_foreground`. Action buttons use the CTA
colors; approval borders use the warning color.

`syntax_highlighting` enables fenced-code and JSON highlighting. `syntax_theme`
selects a bundled palette: `base16-ocean.dark` (default), `base16-eighties.dark`,
`base16-mocha.dark`, `base16-ocean.light`, `InspiredGitHub`, `Solarized (dark)`, or
`Solarized (light)`. The custom syntax palettes `haxx0r`, `BnP`, `mama_j`, `diet_soda`,
and `blue` are also available. Unknown languages fall back to plain text.

Copy [`examples/theme-high-contrast.json`](examples/theme-high-contrast.json) or
[`examples/theme-light.json`](examples/theme-light.json) into the `theme` object.
`/reload` applies edits and invalidates the render cache.

**Fonts come from your terminal emulator.** Choose any installed system monospace
font in its settings; the harness uses it automatically. No bundled font, Nerd Font,
or font download is required. Unicode text is wrapped by terminal cell width. Set
`theme.ascii: true` for simple ASCII borders. A terminal application cannot set the
emulator's font family through portable terminal APIs.

## TUI

| Command | Purpose |
|---|---|
| `/model` | Open the searchable model picker |
| `/model <reference>` | Switch directly to an alias or `provider:model-id` |
| `/model add <alias> <JSON-or-reference>` | Persist a new model alias and select it |
| `/effort [level\|default]` | Show or change supported reasoning effort |
| `/agent [name\|default] [agent-mode]` | Select an agent and optional mode |
| `/mode [name\|default]` | Select an application mode |
| `/workflow <file-or-name> [input]` | Run a workflow; omit arguments to list files |
| `/tools [name on\|off]` | List or toggle tools |
| `/mcp` | Search MCP servers and toggle their enabled state |
| `/mcp <name on\|off\|restart>` | Toggle/restart a server directly |
| `/mcp add <name> <JSON>` | Persist a new MCP server; UUID generated if omitted |
| `/theme [name\|configured]` | Preview/select themes in a picker or select directly |
| `/skills [name on\|off]` | List or activate skills |
| `/install-skill <source>` | Install a skill |
| `/cost` | Show session spend and unpriced request count |
| `/export [directory]` | Dump the current session to timestamped text |
| `/clear` | Fresh session ID, empty history/input, and zero spend |
| `/new` | Alias for `/clear` |
| `/reload` | Reload config and reset runtime overrides |
| `/help`, `/quit`, `:q` | Command explanations or exit |

Example additions (the JSON is entered directly, without shell quoting):

```text
/model add fast openrouter:openai/gpt-4.1-mini
/model add thinker {"provider":"openrouter","model":"openai/gpt-5","max_tokens":16384,"reasoning":{"supported_efforts":["low","medium","high"],"effort":"medium"}}
/mcp add demo {"transport":"stdio","command":"python3","args":["examples/mcp_echo.py"],"enabled":true,"hitl":false}
/mcp demo on
/mcp deactivate demo
```

Additions validate before an atomic write to the active config file, preserve its
relative paths and environment references, and refuse duplicate names. Edit the
JSON and `/reload` to update an existing definition. The provider named in a model
definition must already exist in `providers`. MCP connections start lazily when
needed. `activate`/`deactivate` are accepted as synonyms for `on`/`off`.

Enter sends; **Alt+Enter or Ctrl+J** inserts a newline. Shift+Enter works where the
terminal reports it distinctly. Arrow keys edit/navigate input history;
PageUp/PageDown scroll the conversation. Ctrl+Home/End scroll to the top/bottom.
Help and approval dialogs also support PageUp/PageDown, Home, and End for reviewing
long output before deciding.
Ctrl+C cancels the active run, and Ctrl+D quits with an empty input. Bracketed paste
is supported. **Tab** cycles configured agents; **Shift+Tab** cycles backward. The
order is alphabetical and wraps at either end. Hidden agents are included in the
cycle because a config whose specialists are all hidden would otherwise have
nothing to switch to; the `hidden` flag only keeps them out of pickers. When one
agent is marked `"default": true`, the bare `default` scope is omitted from the
cycle so it cannot duplicate that agent. Cycling works while idle and preserves
your draft prompt.

### Model picker

Enter `/model` to open the dialog. Configured aliases, the default model, and the
current model appear immediately. Model catalogs from configured providers load
in the background using their `/models` endpoints and configured environment-variable
credentials. Unavailable catalogs show a status message; configured choices remain
selectable. Catalog requests do not generate model responses or session spend.

- Type or paste into **Search** to fuzzy-filter by alias, provider, ID, or display
  name, ignoring case. For example, `gpt41m` matches `gpt-4.1-mini`; separate words
  such as `sonnet anthropic` may appear in any order.
- **Up/Down** browse results; **PageUp/PageDown** move ten entries at a time.
  **Ctrl+Home/End** jump to the first/last result.
- **Enter** selects the highlighted model, retaining an alias's configured
  settings. **Esc** or **Ctrl+C** closes the dialog without changing the model.
- Selection is a runtime override; `/model add` still persists a new alias.

The interface has a one-character-cell margin on all four outer edges. Terminal
layout uses cells rather than pixels; its physical size follows your terminal font.

### MCP and theme pickers

`/mcp` and `/theme` use the same search box, fuzzy matching, browsing keys, and
paste handling as the model picker. In the MCP dialog, each configured server has
an `[on ]` or `[off]` marker. **Enter toggles** the selected server immediately,
leaving the dialog open so you can change several servers. **Esc/Ctrl+C closes**
the dialog; completed toggles remain in effect. These markers show enablement,
not connection health. Enabled servers connect lazily when needed.

In the theme dialog, browse to preview, **Enter applies**, and **Esc/Ctrl+C cancels**
the preview. Neither picker sends requests to a model. Approvals take keyboard
priority if they arrive while a picker is open. To cancel a running generation,
close the picker first, then press Ctrl+C.

Tool and MCP switches can change during a run. They are checked again immediately
before execution, including after an approval or plugin hook. Disabling a tool
does not undo an already-running invocation; Ctrl+C cancels that run. Runtime
overrides are not written back to configuration. Config reloads and agent/model
switches, persistent additions, exports, and session resets require an idle run.

## Tools

The default built-ins are:

| Name | Behavior |
|---|---|
| `web_fetch` | HTTP(S), redirects, bounded download, readable HTML or text |
| `web_search` | Bounded public web search returning titles, URLs, and snippets |
| `gh` | Authenticated GitHub CLI commands; fails if `gh` is missing or unauthenticated |
| `read_file` | Read UTF-8 within the configured workspace |
| `write_file` | Write UTF-8 within the workspace; parent directory must exist |
| `shell` | Execute a program and argv, without an implicit shell |
| `delegate` | Run a configured subagent and return its result |
| `delegate_parallel` | Run independent tasks concurrently, with ordered results |
| `load_skill` | Load an installed skill's instructions |

`builtins` selects which are registered. `disabled_tools` supplies initial disabled
states. `approval_tools` forces approval for named tools, including built-ins and
individual namespaced MCP tools. `require_for_destructive_tools` defaults to true,
covering `write_file`, `gh`, and custom tools marked `destructive`. Shell commands
are classified individually: non-destructive commands that stay inside the workspace
run without approval, while destructive commands and commands targeting paths outside
the workspace ask first. Approving an outside call grants that single call; it does
not widen the agent's standing `allow_outside_workspace` setting. The `gh` tool
checks `gh auth status` before execution and fails clearly when the CLI is missing
or unauthenticated.

Tools from an agent/mode/workflow scope are intersected with global/runtime
availability. Subagents cannot widen parent permissions. Arguments are validated
before approval or execution. A rejected or failed tool produces a tool result
that the model can handle. Abort cancels the run.

### Custom commands

```json
{
  "type": "command",
  "description": "Run project tests",
  "command": "cargo",
  "args": ["test", "--locked"],
  "cwd": ".",
  "env": { "SERVICE_TOKEN": "${SERVICE_TOKEN}" },
  "input_schema": { "type": "object", "properties": {}, "additionalProperties": false },
  "enabled": true,
  "hitl": true,
  "destructive": false,
  "timeout_seconds": 120,
  "max_output_bytes": 100000
}
```

Place this object under a name in `tools`. `args` accepts `{{argument_name}}`
substitutions; each resulting string remains one argv element. There is no shell
expansion. A user can explicitly configure a shell executable or invoke one through
the approved `shell` tool. stdout/stderr are capped independently; exit code and
truncation are returned. Processes inherit the harness environment, with configured
`env` overrides. Unix command process groups are killed on cancellation/timeout.

Custom commands, MCP servers, and plugins run with the user's OS permissions.
Workspace confinement applies to built-in file tools, not to external programs;
this version does not provide an OS sandbox.

### Custom HTTP

Use `type: "http"`, `method`, `url`, and optional `headers`, `query`,
`body_template`, `text_body`, and `response_pointer` (RFC 6901 JSON Pointer).
See the `create_issue` example. JSON bodies and text bodies are mutually exclusive.

- URL argument substitutions are percent-encoded.
- Query values are encoded by the HTTP client.
- JSON fields exactly equal to `{{name}}` preserve the input's JSON type.
- Other string substitutions remain strings; there is no expression evaluation.
- Non-success status codes return errors. Custom HTTP redirects are not followed.
- Truncated responses cannot be used with JSON response extraction.

Defaults: enabled, HITL, and destructive are true; timeout 120 seconds; output cap
100 KB. Set both `hitl: false` and `destructive: false` for an automatically executed
read-only custom tool under the default policy.

### Website reading

`web_fetch` uses a 30-second timeout, at most five redirects, a 2 MB download cap,
and a 100 KB extracted-text cap. It excludes scripts/navigation and prefers main or
article content. It does not execute JavaScript or perform browser automation.
Local HTTP services are supported. Use an MCP browser for JavaScript-heavy sites.

## MCP

Servers live in `mcp_servers`; the map key is the server's name. Each has a unique
`uuid`, `enabled`, `hitl`, and `timeout_seconds`.

```json
{
  "transport": "stdio",
  "uuid": "00fd9d42-c49b-4b86-b99a-040df78d730e",
  "command": "python3",
  "args": ["examples/mcp_echo.py"],
  "env": {},
  "enabled": true,
  "hitl": false
}
```

Streamable HTTP uses `transport: "http"`, `url`, and optional `headers` with
environment references. JSON and SSE POST responses and session IDs are supported.
Negotiated protocol versions: `2024-11-05`, `2025-03-26`, and `2025-06-18`.
The client exposes **tools**; MCP resources, prompts, sampling, elicitation, OAuth,
and the older separate-endpoint SSE transport are not implemented.

Servers connect lazily when their tools are needed. A failed server is reported and
omitted from that request. Tool names normally look like `mcp_demo__echo`; long or
incompatible names get a stable index-based name for that connection. Names are
shown in tool activity and can be toggled with `/tools <name> off`.
`/mcp demo on` enables the demo in the example config.
Disabled servers cannot be enabled merely by an agent/workflow reference.
Cancelled or broken calls discard their connection. Local server process groups
are shut down on Unix; remote sessions receive a best-effort DELETE. The next use
reconnects.

## Agents and subagents

Agents can set `model`, `system_prompt`, `prompt`, `tools`, `mcp_servers` (UUIDs),
`skills`, `max_turns`, `timeout_seconds`, `hidden`, and `can_edit`.

Prompt assembly is global system prompt → agent system prompt → agent prompt →
selected skills and available skill/subagent descriptions. Agent prompts supplement
the global instructions. Hidden agents remain available to workflows and delegation
but do not appear in the Tab cycle. Modes are deprecated; use agents and workflows.

The `delegate` tool takes `{ "agent": "name", "prompt": "task" }`.
Subagents are ordinary entries in the same `agents` object—there is no separate
subagent schema. Each child receives a fresh conversation, its configured prompt, and the supplied
task. Parent history is not copied. Child messages are logged under a separate
context, and child spend contributes to the same session. The parent receives
the child's final result. Default subagent permissions, when omitted, are
`web_fetch`, `read_file`, `load_skill`, and no MCPs, further intersected with parent
permissions. Explicitly list broader permissions on the child when needed.

Agents can decide to run independent work concurrently using:

```json
{
  "tasks": [
    { "agent": "researcher", "prompt": "Investigate the API options." },
    { "agent": "reviewer", "prompt": "Review the supplied design." }
  ]
}
```

That is the input to `delegate_parallel`. Multiple consecutive `delegate` calls in
one model response also run concurrently. Other tools retain their original
execution order. The parent receives results in task/call order even if children
finish out of order, and one child's failure is returned individually.

`max_parallel_subagents` defaults to 4 (range 1–32). It bounds each dispatch batch
and active child model requests/tool executions across the entire session. A parent
waiting for its children holds no slot, so nested delegation works even with a
limit of one. Independent tasks fill free slots as they finish. Approvals are
serialized, and cancellation propagates through the child tree. Calls to a shared
MCP connection are serialized to preserve JSON-RPC state.

Add `delegate`/`delegate_parallel` to an agent's `tools` allowlist when it should be
able to dispatch children; the default agent has both. The example `coordinator`
agent demonstrates this setup.

Regular top-level agents have no model-turn limit. Delegated subagents are capped at
25 model turns; an agent's `max_turns` setting may lower that cap. The legacy global
`max_turns` setting remains accepted but does not limit regular agents.
`max_subagent_depth` defaults to 3. Agent runs default to a 30-minute deadline,
including tool use and approval waits; provider and tool timeouts also apply.
Context compaction is not automatic.

## Workflows and HITL

A workflow is **a separate JSON file with exactly this schema**:

```json
{
  "title": "Research and write",
  "author": "you",
  "steps": [
    {
      "agent": "researcher",
      "model": "openrouter:openai/gpt-4.1-mini",
      "prompt": "Research {{input}} and cite sources.",
      "mcps": [],
      "hitl": true
    },
    {
      "model": "openrouter:openai/gpt-4.1-mini",
      "prompt": "Write a report from {{previous_result}}.",
      "mcps": [],
      "hitl": false
    }
  ]
}
```

An MCP reference is exactly `{ "name": "demo", "uuid": "...", "enabled": true }`.
Names and UUIDs must match configured servers. An empty `mcps` array grants no MCP
tools for that step. Built-in/custom tools follow the active agent and runtime scope.
The optional `agent` field selects a configured agent for that isolated step. If it
is omitted, the current selection is used. Prompts can also request configured
subagents via `delegate` or `delegate_parallel`.

`diet_soda --init` installs `workflows/elephants_and_goldfish.json`. It runs the
plan agent, plan review, plan finalization, elephant implementation coordination,
code review, and debugger stages. HITL steps pause after execution. When an
interactive run completes, the TUI offers a dialog to start a new workflow, repeat
the workflow, or exit workflow mode.

Modes are deprecated. Use named agents for reusable behavior and workflows for
multi-stage execution. Legacy mode settings are tolerated when loading older files,
but `--init` does not create them and Tab cycles configured agents instead.

Each step receives workflow input, the previous accepted result, and its resolved
instructions. Templates: `{{input}}`, `{{previous_result}}`, `{{workflow_title}}`,
and one-based `{{step_index}}`. Unknown variables fail validation.

**HITL order:**

1. Execute the current step, including its complete tool loop.
2. Persist and present the output.
3. If `hitl` is true **and another step exists**, pause.
4. Continue (`y`), retry this step (`r`), discard this result and advance (`s`),
   or abort (`q`). Skipping preserves the previous accepted result.

There is **no final-step gate**. Tool approvals are independent and can occur inside
any step. Failed steps pause for retry, skip, or abort even when `hitl` is false.
Retry re-executes the step; previous external side effects are not rolled back.
Every attempt/result/error is persisted. Interrupted workflow runs can be inspected
in JSONL; automatic checkpoint continuation is not implemented.

```sh
cargo run --locked -- --config examples/config.json \
  --validate-workflow examples/workflows/research-report.json
cargo run --locked -- --config examples/config.json \
  --workflow examples/workflows/research-report.json --input 'Rust MCP clients'
```

## Skills

Skills are directories with `SKILL.md` and optional supporting files:

```markdown
---
name: summarize
description: Summarize material with citations.
---
Your skill instructions go here.
```

Install from a local directory, local `SKILL.md`, `.tar.gz`, an HTTPS raw Markdown
URL, or an HTTPS tarball:

```sh
diet_soda --install-skill ./my-skill
diet_soda --list-skills
```

Archives may contain one skill root or one top-level directory. Installation uses
a staging directory, refuses duplicate names, rejects archive links/path traversal,
and enforces download/unpacked size limits. Local directory symlinks are rejected.
Skill scripts are copied but never executed automatically. Use explicitly allowed
tools if a skill needs a script.

`skills.directories` adds search roots alongside `skills_dir`. `skills.enabled`
injects selected instructions at the next turn; agents may supply their own list.
Other discovered skills contribute only descriptions until the agent invokes
`load_skill`. Skills outside the workspace may be loaded via that tool, while
supporting files remain subject to file-tool workspace limits.

## Plugin hooks

Hooks are external programs configured in `hooks`. Supported events:

`session_start`, `before_model`, `after_model`, `before_tool`, `after_tool`,
`workflow_step`, and `shutdown`.

The harness writes one JSON object to stdin and closes it:

```json
{
  "version": 1,
  "event": "before_tool",
  "payload": { "context": "main", "tool": "web_fetch", "arguments": { "url": "https://example.com" } }
}
```

Exit zero with empty stdout or `{}` to continue. Return `{"deny":"reason"}` or a
nonzero exit to fail the hook. stdout must be a single JSON value, not debug logs.
Hook output is capped at 64 KB. `enabled`, `args`, `env`, and `timeout_seconds`
configure each hook. Executable/cwd semantics match custom commands.

Before hooks can prevent an action. After hooks observe an action that already
occurred; they cannot roll it back. An `after_tool` failure is returned alongside
the real tool result so the model knows the operation already ran. Hooks are
observers/gates in v0.1, not message-rewriting or native dynamic-library plugins.
Custom tools provide the mechanism for plugins to expose actions to agents.

## CLI and sessions

```sh
diet_soda --help
diet_soda --validate-config
diet_soda --list-workflows
diet_soda --list-skills
diet_soda --session <session-id>
diet_soda --agent researcher --model fast
diet_soda --prompt 'Explain this project' --agent reviewer
diet_soda --config examples/config.json --model reasoner --effort low --prompt 'Explain Rust ownership'
```

`--prompt` runs headlessly. A non-terminal stdout also selects headless execution
and requires a prompt or workflow. With an interactive stdin, approvals use single
keys. With non-interactive stdin, a required approval aborts the run; it is never
implicitly accepted. Configure autonomous tools/workflows explicitly for unattended
runs.

### Export and reset

`/export` writes to `exports_dir` (default `~/.config/diet_soda/exports`). The title and
filename use the current local timestamp, for example **`09:18:2026-16:05:02.txt`**
(`MM:DD:YYYY-HH:mm:ss`, 24-hour clock). Exports in the same second receive a numeric
suffix rather than overwriting a file. On Windows only, filename colons become
hyphens; the title keeps the requested format. `/export ./reports` overrides the
directory relative to the workspace.

The readable transcript contains parent/child conversations, tool calls/results,
workflow/approval events, and spend. It is generated from the redacted event log.
`/clear` and `/new` start a new session with a new ID and zero spend; they preserve
old JSONL files and exports, and keep the selected model/mode and runtime settings.

### Persistence

Session events are timestamped JSONL envelopes with `type`, `context`, and `data`.
Main conversation, child contexts, model requests, token/cost usage, workflow steps,
and approvals are recorded. Complete messages are written instead of each streamed
token. A malformed interrupted final line is preserved and marked by a recovery
event; unexpected corruption elsewhere is an error. Outstanding tool calls receive
an interruption result on resume. Each event is serialized into one append write;
completed conversations and workflow steps, exports, and clean shutdown establish
fsync checkpoints. Legacy context-only `clear` events are still understood on resume.

## Development and verification

One library + CLI package keeps compilation and navigation straightforward. Rust
2021 preserves the agreed Rust 1.84 MSRV; CI checks both that toolchain and current
stable. `unsafe` code is forbidden in this crate. Cargo dependencies use focused
features, and `.editorconfig`/rustfmt keep formatting consistent.

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

Tests use loopback HTTP fixtures and a local Python-standard-library MCP server.
They require no credentials and make no paid API calls. Provider tests cover wire
formats locally; live-provider compatibility still depends on endpoint capabilities.

### Publishing a GitHub Release

The [Binary Builds and Releases workflow](.github/workflows/release.yml) runs on
**every push to `main`**, including merge commits, squash/rebase merges, and direct
pushes. After all checks/builds pass, download the combined archive bundle from
**Actions > Binary Builds and Releases > the run > Artifacts**. Snapshot names
include the version and commit, such as `diet_soda-v0.1.0-main-abcdef123456`.
The bundle includes all four native archives and `SHA256SUMS`, retained for 30 days.
Main-branch builds do **not** publish or overwrite a versioned GitHub Release.

The same workflow also runs when you push a `v*` tag.
It verifies that the tag matches `package.version` in `Cargo.toml`, runs formatting,
tests and Clippy, then builds optimized binaries on native Linux, macOS and Windows
runners using Rust 1.84.1 and `Cargo.lock`. Each binary gets a CLI/configuration
smoke test before packaging. Once **all builds succeed**, it creates a GitHub Release
with generated notes, four archives, and `SHA256SUMS`.

To publish the first version:

1. Commit and push the workflow and all changes you want in the release. The tagged
   commit must contain `.github/workflows/release.yml` and its packaging script.
2. Confirm `Cargo.toml` has `version = "0.1.0"`. For subsequent versions, update
   `package.version`, run `cargo check` to refresh `Cargo.lock`, and commit both files.
3. Tag the release commit and push the tag:

   ```sh
   git tag -a v0.1.0 -m "Release v0.1.0"
   git push origin v0.1.0
   ```

4. Follow **Actions > Binary Builds and Releases**. When it succeeds, binaries appear on the
   repository's **Releases** page as, for example,
   `diet_soda-v0.1.0-aarch64-apple-darwin.tar.gz`.

Tags such as `v0.2.0-rc.1` must match the manifest version `0.2.0-rc.1` and create
a GitHub prerelease. Once the workflow is on the default branch, **Actions > Binary
Builds and Releases > Run workflow** also accepts an existing tag. Rerunning uploads to the same release
and replaces assets with matching names. Builds always use the tagged source commit.
The workflow uses GitHub's automatic `GITHUB_TOKEN`; no personal token or model API
keys are needed. Only the publishing job has `contents: write` permission.

The portable packaging helper is `.github/scripts/package_release.py` (Python 3.11+).
To build and package locally on an Apple Silicon Mac:

```sh
cargo build --release --locked --target aarch64-apple-darwin --bin diet_soda
python3 .github/scripts/package_release.py --tag v0.1.0 --target aarch64-apple-darwin
```

Artifacts go to the Git-ignored `target/dist/` directory. Use the matching native
target on another platform; the helper runs the binary before creating its archive.

### Performance choices

- One pooled provider HTTP client is shared across turns and children.
- Completed messages retain highlighted, wrapped render caches. Streaming changes
  invalidate only the changed entry; frames copy only visible lines.
- The TUI batches deltas, redraws only dirty state, and caps rendering at 25 FPS.
- Syntax grammars/palettes load once, lazily. Very long code lines use plain text.
- Session records use one append write rather than many serialization writes;
  fsync is amortized at explicit checkpoints.
- Parallel child work is bounded; completed jobs immediately release capacity.
- SSE parsing consumes a chunk before shifting its buffer, rather than shifting
  the remaining bytes for every line.

Tests verify cache reuse, true concurrent dispatch with a barrier (not timing
guesses), nested one-slot delegation, serialized approvals, and exact cost totals.
These are structural performance choices, not claims of benchmarked speedups.

### Code map

| Module | Responsibility |
|---|---|
| `config.rs`, `config/` | Settings, built-in themes, reasoning capabilities, atomic config edits |
| `model`, `template` | Normalized messages/events and single-pass substitution |
| `provider.rs`, `provider/` | Streaming adapters, SSE framing, reasoning continuation |
| `engine/mod.rs` | Conversation loop, shared limits, approvals, accounting |
| `engine/scope.rs`, `engine/dispatch.rs` | Prompts/permissions and tool/subagent dispatch |
| `tools`, `process` | Built-ins and custom execution |
| `mcp` | MCP connections and tool discovery/calls |
| `workflow` | Strict workflow schema and post-step gates |
| `skills`, `hooks` | Extensibility |
| `session.rs`, `session/export.rs` | Append-only persistence, spend, text export |
| `tui/mod.rs`, `tui/app.rs` | Terminal lifecycle/event loop and UI state |
| `tui/commands.rs`, `tui/input.rs` | Slash commands and UTF-8-safe editing |
| `tui/picker.rs` | Shared fuzzy search and navigation for model/MCP/theme dialogs |
| `tui/render.rs` | Cached rendering, syntax highlighting, semantic colors |
| `main` | CLI and headless event consumer |

`Cargo.lock` is part of the project for reproducible builds. A transitive IDNA adapter
is pinned because newer ICU derives misreport compatibility with Rust 1.84.
