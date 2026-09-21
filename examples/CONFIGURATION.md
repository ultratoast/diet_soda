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
`write_file`, `shell`, and destructive custom command tools. A child cannot widen
the parent scope. The interactive top-level session retains its existing edit
behavior.

Mark one agent with `"default": true` to use it for a new top-level session when
no agent is explicitly selected. Only one agent may be marked as the default.

The workspace is the default filesystem boundary. `write_file` rejects paths
outside it, including traversal and symlink escapes. `read_file` asks for approval
before reading an existing outside path. Command tools must use an in-workspace
working directory unless the agent explicitly sets `allow_outside_workspace: true`;
children cannot widen that permission.

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
config. Its blocked commands and patterns are checked before built-in shell and
configured command tools execute. The shipped policy covers destructive filesystem,
Git, cloud, container, Kubernetes, package/publishing, and pipe-to-shell patterns.

Built-in shell calls do not require approval merely because they use the shell.
Approval is requested when a command targets a path outside the workspace or is
classified as destructive. Approving an outside call grants that single call;
the standing `allow_outside_workspace` agent setting is not required for it.
Explicit `approval_tools` and custom-tool `hitl` settings still require approval.

Outside `read_file` is approved once per directory: the approval covers every file
in that directory for the session. `allow_outside_workspace: true` is the standing
grant for non-destructive outside work, while destructive commands still ask.
See `QUEUE_AND_ACCESS.md` for the full queueing and access reference.
