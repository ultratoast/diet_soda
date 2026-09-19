# diet_soda Configuration

The active configuration is `~/.config/diet_soda/config.json`. `diet_soda --init`
creates it and sibling `AGENTS.md`, `theme.json`, `bash-permissions.json`,
`workflows/`, `skills/`, `sessions/`, and `exports/` directories. The binary reads
these files at startup; editing them never requires recompilation.

## Named Arrays

`models`, `agents`, and `tools` are arrays of objects. Every object needs
a unique `name`:

```json
{
  "models": [
    {"name":"fast","provider":"openrouter","model":"openai/gpt-4.1-mini","max_tokens":4096}
  ],
  "agents": [
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

Modes are no longer needed. Use named agents and workflows instead. Older `modes`
settings are tolerated for compatibility but are not included by `--init` and Tab
does not cycle them.

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
