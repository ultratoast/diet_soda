# Queued Messages And Outside Access

Reference for two runtime behaviors: sending messages while a run is active, and
approving work outside the workspace.

## Queued messages

- Pressing Enter during an active run queues the message instead of failing.
- The queue is FIFO. When the active run finishes, the oldest queued message
  starts a new turn.
- Each queued message is its own turn; queued text is not merged into the active
  conversation.
- The status line shows `Queued N message(s)` while messages are waiting.
- Slash commands are handled immediately and are not queued. Commands that need
  an idle agent (`/model`, `/agent`, `/effort`, `/workflow`, `/export`, and
  similar) report the active run instead. `/tools`, `/mcp`, `/theme`, `/help`,
  and `/cost` work during a run.
- Ctrl+C or Esc cancels the active run. Queued messages remain queued and start
  after the run stops.
- `/clear` and `/new` drop the queue along with the conversation history.
- `/quit` and `:q` cancel the active run and discard the queue.

## Outside access

The configured workspace is the default boundary. Work outside it is approved
explicitly.

### What asks for approval

| Action | Approval scope |
| --- | --- |
| `read_file` outside the workspace | Once per directory, then every file in that directory for the rest of the session |
| `shell` argument outside the workspace | Per call, granted only for that call |
| Custom command tool whose cwd is outside | Per call, granted only for that call |
| Shell command the allowlist does not recognize as read-only | Every call unless its command family has a session grant |
| Destructive or updating command | Every call unless its command family has a session grant |
| Tool listed in `approval_tools` or custom tool with `hitl` | Every call |

Shell commands use a positive heuristic allowlist: recognized read-only forms
(`cat`, `ls`, `grep`, read-only `find`, and read-only Git/AWS/GitHub/package
queries) run without approval. Mutating and unknown operations ask, including
scripts, builds, package changes, and `make` targets. This applies to the shared
tooling list (`python`/`python3`, `cargo`, `yarn`, `pip`/`pip3`, `npm`, `make`,
`aws`/`awscli`, `pup`, `gh`, and `gws`) for any agent whose scope includes
`shell`. Classification is best-effort and not a sandbox. The unified bash
policy deny list still runs before shell, `gh`, and command-tool execution;
approval or a session grant cannot bypass an explicit deny rule.
AWS/GitHub credential or secret retrieval and commands that download to local
files also ask because they disclose credentials or write local state.

For eligible command-family prompts, `y` approves once, `p` approves that
command family for the rest of the current session, `n` rejects, and `a` aborts.
Grants are in-memory, shared with subagents, and cleared by `/clear` and `/new`.
They do not apply to outside-workspace approvals, explicit `approval_tools`,
custom-tool HITL, or workflow gates. The older `q` key remains an abort alias.

### Directory access before files

Access is requested at the highest useful level. Approving an outside read
grants its containing directory for the session, so a task that needs several
files in one directory asks once instead of once per file. Prefer a
directory-scoped request when the work covers a whole directory; request
individual files only when the directory also holds unrelated data.

Outside-workspace shell commands and custom command tools remain approved per
call because a command can do more than read. A persistent command-family grant
does not widen outside-workspace access.

### Standing grant

Set `"allow_outside_workspace": true` on an agent to skip the outside-path
approval for non-destructive work outside the workspace — but only for shell
forms the allowlist recognizes as read-only. An unrecognized command with outside
arguments still asks, and destructive commands always ask. Child agents receive
only the intersection of the parent's permissions and cannot widen them.

### What approval does not do

- It does not grant writes outside the workspace; `write_file` stays
  workspace-bound.
- It does not bypass the unified bash policy; blocked commands still fail even
  after approval.
- Directory and per-call grants last for the running session only and are not
  persisted to disk.

### Approval dialog

Dialogs show a human-readable summary instead of raw JSON, for example:

```text
Allow shell?
This call targets something outside the configured workspace.

Run `cat /etc/hosts`
```

Directory reads name both the file and the directory being granted.
