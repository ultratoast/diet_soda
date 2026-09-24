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
    {"name":"plan","default":true,"model":"openrouter:openai/gpt-6-luna","can_edit":false,"prompt":"./prompts/plan.md"},
    {"name":"researcher","model":"fast","can_edit":false,"prompt":"./prompts/research.md","tools":["web_fetch","read_file","delegate_parallel"]}
  ],
  "tools": [
    {"name":"run_tests","type":"command","description":"Run tests.","command":"cargo","args":["test","--locked"],"hitl":true,"destructive":false,"network_access":false}
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
compatibility. Tab and the `/agent` picker offer non-hidden configured agents, not
legacy modes. Agents marked `hidden: true` remain available to workflows and
delegation and can still be selected with an explicit `/agent name` command.

## Providers And Authentication

Providers may use OpenRouter, LiteLLM, OpenAI, Anthropic, or another compatible
HTTP endpoint. Authentication defaults are selected from `kind`, while `headers`
adds or overrides request headers for both chat and model-catalog requests. Header
values may reference environment variables with `${VAR}`; values are resolved only
when a request is sent.

```json
{
  "providers": {
    "company": {
      "kind": "openai",
      "base_url": "https://llm.example.com/v1",
      "api_key_env": "COMPANY_API_KEY",
      "headers": {
        "X-Tenant": "${COMPANY_TENANT}",
        "Authorization": "Token ${COMPANY_API_KEY}"
      },
      "allow_private_networks": false
    }
  }
}
```

An explicit `Authorization` header overrides the built-in bearer header. Provider
and model-catalog connections are DNS-validated and pinned; redirects and proxies
are disabled. Private/local provider addresses (for example `127.0.0.1`) require
`allow_private_networks: true` on that provider. HTTP MCP servers have the same
per-server option.

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

Shell approval uses a shared positive heuristic allowlist: recognized read-only
forms (`cat`, `ls`, `grep`, read-only `find`, and Git/AWS/GitHub/package queries)
run without approval. Mutating and unknown operations ask, including scripts,
builds, package changes, and `make` targets. The shared tooling set includes
`python`/`python3`, `cargo`, `yarn`, `pip`/`pip3`, `npm`, `make`, `aws`/`awscli`,
`pup`, `gh`, and `gws`. The classification is best-effort and not a sandbox.
AWS/GitHub credential or secret retrieval and commands that download to local
files also ask because they disclose credentials or write local state.
`shell` stays within each agent's tool scope: root/main agents have it, while a
child needs it in its explicit `tools` list. The `write_file`,
destructive-custom-tool, and HITL-MCP gates are unchanged. A standing
`allow_outside_workspace` grant suppresses only the outside-path reason, and
only for forms the allowlist recognizes as safe. Explicit `approval_tools` and
custom-tool `hitl` settings still require approval.

Eligible command-family approvals offer `y` once, `p` for the same command family
for the rest of the current session, `n` to reject, and `a` to abort. Grants are
in-memory, shared with subagents, and cleared by `/clear` and `/new`. They do not
bypass explicit deny rules, outside-workspace checks, or separate HITL gates.

Outside `read_file` is approved once per directory: the approval covers every file
in that directory for the session. Destructive commands always ask, even with the
standing outside-workspace grant, unless an explicit bash deny rule blocks them.
See `QUEUE_AND_ACCESS.md` for the full queueing and access reference.

## Subprocess Environment

Subprocesses never inherit the harness's ambient environment. Each child receives
an explicit baseline (`PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_*`,
`TERM`, `TMPDIR`, `XDG_CONFIG_HOME`)
plus the configured `env` overlay of the tool, hook, or MCP server. `${VAR}`
references inside `env` values resolve against the harness environment at
execution time. The `gh` builtin forwards GitHub token variables (`GH_TOKEN`,
`GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN`, `GH_HOST`). Pass variables such as
`SSH_AUTH_SOCK`, proxy settings, or cloud credentials explicitly through `env`
when a tool needs them.

## Network isolation

Model-invoked shell commands, configured command tools, hooks, and stdio MCP
servers run with network access denied by default. Linux uses an unprivileged
user/network namespace (`unshare`); macOS uses the system `sandbox-exec` network
deny profile. If sandbox setup fails, the requested child process is not run.
This restricts IP networking only; it is not filesystem isolation and does not
block access to local IPC sockets.

Grant unrestricted host-network access to a child only when needed:

- `shell_network_access: true` at the top level grants it to the built-in shell.
- `network_access: true` on a command tool, hook, or stdio MCP definition grants
  it to that process.
- The `gh` builtin is explicitly network-enabled because remote GitHub access is
  its purpose.

A child-process network grant is unrestricted egress, not a domain allowlist.
App-owned HTTP requests use separate policy: web fetch/custom HTTP validate and
pin each destination; provider and HTTP-MCP endpoints are fixed in config and
pinned after validation. Private provider/MCP endpoints require their own
`allow_private_networks: true` setting; that does not change the public-only
checks for user/model-selected URLs.

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
