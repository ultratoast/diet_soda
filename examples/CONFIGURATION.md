# diet_soda Configuration

The active configuration is `~/.config/diet_soda/config.json`. The first launch
with no existing config and no `--config` flag automatically creates the full
default tree below; the announcement is written to stderr so scripted
`--prompt` output is unaffected. `diet_soda --init` is the explicit,
non-overwriting variant: it prints `Created <path>` to stdout and refuses to
overwrite an existing config or companion file.

Default tree:

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

The binary reads these files at startup; editing them never requires
recompilation. Auto-init serializes concurrent first-run launches through a
persistent sentinel file (`.diet_soda-init.lock`) beside the config;
whichever process publishes first, the rest of the tree is filled in with
`create_new(true)` so no companion file is ever clobbered. The sentinel file
stays on disk across launches; only the per-process lock on it is held while
the publisher is alive.

## Named Arrays

`models`, `agents`, and `tools` are arrays of objects. Every object needs
a unique `name`:

```json
{
  "models": [
    {"name":"fast","provider":"openrouter","model":"openai/gpt-4.1-mini","max_tokens":4096}
  ],
  "agents": [
    {"name":"plan","default":true,"model":"openrouter:deepseek/deepseek-v4-flash","can_edit":false,"prompt":"./prompts/plan.md"},
    {"name":"researcher","model":"fast","can_edit":false,"prompt":"./prompts/research.md","tools":["web_fetch","read_file","delegate_parallel"]}
  ],
  "tools": [
    {"name":"run_tests","type":"command","description":"Run tests.","command":"cargo","args":["test","--locked"],"hitl":true,"destructive":false}
  ]
}
```

`can_edit` defaults to `false` for configured agents and subagents. It gates
`write_file`, destructive custom command tools, and tools from MCP servers
marked `hitl` — not `shell`. Root/main agents keep `shell` for recognized safe
forms; a child agent that omits `tools` defaults to `web_fetch`, `read_file`,
and `load_skill` (no `shell`), intersected with the parent scope. An explicit
child tool list may include `shell`, subject to parent intersection and the
normal approval policy. A child cannot widen
the parent scope. Migration note: children no longer receive `shell` by
default; add `"shell"` to a child agent's explicit `tools` list if it needs it.
The literal agent name `default` is reserved; mark one agent
with `"default": true` instead — only one agent may be marked as the default, and
it is used for a new top-level session when no agent is explicitly selected.

The workspace is the default filesystem boundary. `write_file` rejects paths
outside it, including traversal and symlink escapes. `read_file` asks for approval
before reading an existing outside path. Command tools must use an in-workspace
working directory unless the agent explicitly sets `allow_outside_workspace: true`;
children cannot widen that permission.

On Unix, newly created config-tree files get mode `0600` and directories `0700`
(further restricted by the process umask). Existing files and directories are
never chmodded, so operator-set permissions survive every launch.

Modes are no longer needed. Use named agents and workflows instead. `/mode` accepts
agent names as an alias for `/agent`; older `modes` settings remain tolerated for
compatibility. Tab cycles configured agents, not legacy modes.

## Prompts And Paths

Prompt fields can be inline strings or exact `./relative/path` references resolved
beside `config.json`. Supported fields are `system_prompt`, agent `prompt` and
`system_prompt`, and agent-mode `prompt`. Referenced files are UTF-8 and capped at
1 MB.

Directory settings such as `workspace`, `workflows_dir`, `skills_dir`,
`sessions_dir`, `exports_dir`, and `skills.directories` resolve relative to the
config directory. An empty `workspace` means the directory from which the binary
was launched.

## Bash Policy

Set `"bash-permissions": "unified"` to load `bash-permissions.json` beside the
config. Its blocked commands and patterns are enforced at execution time, before
built-in shell and configured command tools run — unconditionally, so an
approval cannot bypass the check. The shipped policy covers destructive
filesystem, Git, cloud, container,
Kubernetes, package/publishing, and pipe-to-shell patterns. If the policy file is
missing, the embedded default policy applies; a present-but-malformed file is a
loading error. `"none"` disables the policy entirely.

Shell approval uses a positive heuristic allowlist: recognized read-only forms
(`cat`, `ls`, `grep`, `git status`, and similar) run without approval, and
everything else asks — unknown commands, interpreters and shells, wrappers,
script-driven `-c` bodies, mutating or network-reaching commands, and inline
output redirects. The classification is best-effort and not a sandbox. `shell`
stays available to every agent whose scope includes it — root/main agents
always, a child only when its explicit `tools` list contains it — for
recognized safe forms, and routes every classified call through approval; the
`write_file`, destructive-custom-tool, and hitl-MCP gates are unchanged. A standing `allow_outside_workspace` grant
suppresses only the
outside-path reason, and only for forms the allowlist recognizes as safe.
Explicit `approval_tools` and custom-tool `hitl` settings still require approval.

Outside `read_file` is approved once per directory: the approval covers every file
in that directory for the session. Destructive commands always ask, even with the
standing grant. See `QUEUE_AND_ACCESS.md` for the full queueing and access
reference.

## Subprocess Environment

Subprocesses never inherit the harness's ambient environment. Each child receives
an explicit platform baseline (Unix: `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`,
`LC_*`, `TERM`, `TMPDIR`, `XDG_CONFIG_HOME`; Windows: `PATH`, `USERPROFILE`,
`SystemRoot`, `COMSPEC`, `APPDATA`, `LOCALAPPDATA`, and related system paths)
plus the configured `env` overlay of the tool, hook, or MCP server. `${VAR}`
references inside `env` values resolve against the harness environment at
execution time. The built-in `shell` tool has no ambient opt-in; only the `gh`
builtin forwards GitHub token variables (`GH_TOKEN`, `GITHUB_TOKEN`,
`GH_ENTERPRISE_TOKEN`, `GH_HOST`). Pass variables such as `SSH_AUTH_SOCK`,
proxy settings, or cloud credentials explicitly through `env` when a tool needs
them.

## Built-in Tool Timeouts

`builtin_timeouts` configures the two built-ins that shell out:

```json
{
  "builtin_timeouts": {
    "shell_timeout_seconds": 120,
    "gh_timeout_seconds": 120
  }
}
```

Both default to 120 seconds and must be positive. Provider `timeout_seconds`
bounds the response header wait and then re-arms as a per-chunk idle gap; it is
not a total stream duration.
