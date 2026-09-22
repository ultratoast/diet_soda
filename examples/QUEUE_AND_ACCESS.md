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
| Shell command the allowlist does not recognize as read-only | Every call, inside or outside the workspace |
| Destructive shell command | Every call, even with a standing grant |
| Tool listed in `approval_tools` or custom tool with `hitl` | Every call |

Shell commands are classified by a positive heuristic allowlist: recognized
read-only forms (`cat`, `ls`, `grep`, `git status`, and similar) run without
approval, and everything else — unknown commands, interpreters, wrappers,
script-driven bodies, mutating or network-reaching commands, inline redirects —
asks. The classification is best-effort and not a sandbox. The unified bash
policy deny list runs unconditionally before every shell, `gh`, and command-tool
execution, regardless of classification.

### Directory access before files

Access is requested at the highest useful level. Approving an outside read
grants its containing directory for the session, so a task that needs several
files in one directory asks once instead of once per file. Prefer a
directory-scoped request when the work covers a whole directory; request
individual files only when the directory also holds unrelated data.

Shell commands and custom command tools are approved per call because a command
can do more than read: one approved call does not authorize later calls.

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
