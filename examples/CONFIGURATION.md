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

Set `"bash-permissions": "unified"` (the default) to load `bash-permissions.json`
beside the config and enforce it on every `shell`, `gh`, and user-defined
command-tool call; for a custom command tool the rules match the
template-rendered command line. `"none"` disables all bash permission
enforcement, legacy and new; the companion file is then ignored without error.

The policy file has two layers.

`blocked_commands` and `blocked_patterns` are legacy hard denies: command names
(matched case-insensitively against the executable basename) and invocation
fragments (matched as contiguous lowercase token sequences against the
normalized invocation). The shipped lists cover destructive filesystem, Git,
cloud, container, Kubernetes, package/publishing, and pipe-to-shell patterns.
Nothing bypasses them — not an approval, not a session grant, not an `allow`
rule — and they stay enforced alongside any `bash` rules. To make a currently
blocked command prompt instead of fail, remove it from the legacy lists and add
an `ask` rule.

The optional `bash` field adds ordered glob rules. It accepts a scalar effect,
which equals a single `"*"` rule:

```json
{ "bash": "ask" }
```

or an object mapping command-line globs to `"allow"`, `"ask"`, or `"deny"`,
matching the shipped file:

```json
{
  "bash": {
    "*": "ask",
    "git rev-parse*": "allow",
    "git push*": "ask",
    "git push --force*": "deny"
  }
}
```

**Order is significant.** Rules are evaluated in document order and the last
matching rule wins; the baseline-first convention exists for that reason. In
the example above `git push --force` is denied even though `git push*` asks,
because the narrower rule comes later — swapping the two lines turns the hard
deny into a prompt. Because the last match wins, a later `allow` *does* override
an earlier `deny`; place deny rules **after** any broader allow rules
(deny-last convention) to keep the block authoritative. The shipped default
follows this: `"git push --force*": "deny"` is the final rule, after
`"git push*": "ask"`. JSON tooling that sorts keys or round-trips the file
through a re-serializer changes policy meaning, and a repeated glob is kept as
two rules with the later occurrence winning.

Matching runs against a canonical subject, not the raw command line. The
executable basename is used, so `/bin/rm` and `rm.exe` match `rm` rules;
everything is lowercased; git global options between `git` and the subcommand
are removed, so `git -C /path push --force` evaluates as `git push --force`;
and the tokens are joined with single spaces, each token POSIX-quoted when it
contains characters outside `A-Za-z0-9` and `-_/.:=`. Quoted arguments keep
their boundaries: `git commit -m "push --force"` has subject
`git commit -m 'push --force'` and does not match `git push --force*`.

Glob syntax: a pattern must cover the whole subject — matching is anchored, not
a substring search. `*` matches zero or more characters, including spaces and
quotes; `?` matches exactly one character; `\` escapes `*`, `?`, and `\` —
inside a JSON string write `\\*` for a literal asterisk and `\\\\` for a
literal backslash. There are no character classes, and matching is
case-insensitive. Invalid escapes — a backslash before anything other than `*`,
`?`, or `\`, or a trailing lone backslash — are rejected when the policy loads,
naming the offending pattern.

Effects (accepted case-insensitively, e.g. `allow` or `ALLOW`; lowercase is
conventional):

| Effect | Behavior |
| --- | --- |
| `deny` | Hard block evaluated before any approval prompt: the call fails as a tool error instead of asking, and the check runs again at execution. An approval cannot bypass it. The error names the matched glob. |
| `ask` | Forces the approval prompt, which names the matched rule. `y` approves once, `p` grants the command family for the current session, `n` rejects, and `a` aborts. |
| `allow` | Suppresses only the ordinary risk heuristic (script, mutating, network, and redirect classification) and the `gh` non-read-only ask gate — so `{"gh *": "allow"}` also auto-runs destructive `gh` forms. It never overrides the legacy `blocked_commands` / `blocked_patterns` lists, tool-level HITL gates (`approval_tools`, custom-tool `hitl`, `destructive`), or outside-workspace path approval. Among `bash` rules the last matching rule wins, so a later `allow` does override an earlier `deny` — follow the deny-last guidance above. |

An invocation that matches no `bash` rule falls back to the ordinary approval
heuristic: recognized read-only forms (`cat`, `ls`, `grep`, read-only `find`,
and Git/AWS/GitHub/package queries) run without approval, while mutating and
unknown operations ask — including scripts, builds, package changes, and
`make` targets. The shared tooling set includes `python`/`python3`, `cargo`,
`yarn`, `pip`/`pip3`, `npm`, `make`, `aws`/`awscli`, `pup`, `gh`, and `gws`.
The classification is best-effort and not a sandbox. AWS/GitHub credential or
secret retrieval and commands that download to local files also ask because
they disclose credentials or write local state. Because the shipped baseline is
`"*": "ask"`, every invocation resolves to a rule; unrecognized read-only forms
prompt unless you add an explicit allow rule or set `"bash-permissions"` to
`"none"`.

The shipped `bash` section allows only forms that cannot mutate state or
execute anything regardless of arguments: `ls`, `cat`, `head`, `tail`, `grep`,
`wc`, `pwd`, `which`, `git rev-parse`, `git ls-files`, and
read-only `gh` list/view forms. Broader allows are
deliberately absent: `"git log*": "allow"` would also admit
`git log --ext-diff`, which executes a repository-configured external diff
driver, and `git log --output=<file>`, which writes a file. `git status` is
likewise excluded: it can execute a repository-configured `core.fsmonitor`
hook and writes `.git/index`. `git status`, `git log`, `git diff`, and
`git show` therefore prompt; add an allow rule if you accept that trade-off.
The `"git push --force*": "deny"` rule also matches `--force-with-lease` and
`--force-if-includes`; that breadth is intentional and conservative.

`shell` stays within each agent's tool scope: root/main agents have it, while a
child needs it in its explicit `tools` list. The `write_file`,
destructive-custom-tool, and HITL-MCP gates are unchanged. A standing
`allow_outside_workspace` grant suppresses only the outside-path approval
reason; the bash policy, including the `"*": "ask"` baseline, still applies in
full. Outside-workspace shell and command-tool calls remain approved per call,
and a policy `allow` rule never overrides the outside-path check. Explicit
`approval_tools` and custom-tool `hitl` settings still require approval.

Eligible command-family approvals offer `y` once, `p` for the same command
family for the rest of the current session, `n` to reject, and `a` to abort.
Policy `ask` prompts use the same keys and grants. Grants are in-memory, shared
with subagents, and cleared by `/clear` and `/new`. They do not bypass explicit
deny rules, outside-workspace checks, or separate HITL gates.

Outside `read_file` is approved once per directory: the approval covers every
file in that directory for the session.

If the policy file is missing, the embedded default policy applies. A
present-but-malformed file — invalid JSON, an unknown field, a non-string
effect, or an effect other than `allow`/`ask`/`deny` — is a hard error, so a
broken edit fails closed instead of silently disabling the policy. Older
diet_soda binaries reject a policy file containing the `bash` field with an
unknown-field error; upgrade the binary before editing the policy. Hook-spawned
processes are not covered by bash permissions. See `QUEUE_AND_ACCESS.md` for
the full queueing and access reference.

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
