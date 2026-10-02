## Agent Catalog

These are the configured agents. Hidden agents are still available to workflows
and delegation but are not offered by the Tab agent switcher.

### `chat`

Visible conversational agent (default). Conversation, brainstorming, research, and
creativity. Use `web_fetch`, `web_search`, and delegate to `researcher` for
independent research. Does not edit files.

### `make`

Visible technical planning and execution agent. Breaks work into small parallel
steps dispatched to subagents. Delegates all edits. Can edit files.

### `plan`

Read-only planning agent. Clarifies scope, risks, dependencies, acceptance
criteria. Hidden from Tab cycling; dispatch it via `delegate` or workflow steps.

### `elephant`

Long-running coordination agent that can dispatch its own `build`, `test-writer`,
`test-runner`, `explorer`, `researcher`, `code-review`, `debug`, and `doc-writer`
subagents. Hidden.

### `build`

Implementation agent. Makes the smallest coherent change to one file. Edits files.
Leaf agent — does not dispatch subagents. Hidden.

### `code-review`

Independent code reviewer. Prioritizes correctness, security, data loss,
regressions. Reports findings ordered by severity with file references. Hidden.

### `plan-review`

Independent plan reviewer. Checks completeness, sequencing, assumptions, failure
modes, testability. Hidden.

### `debug`

Failure investigator. Reproduces, root-causes, fixes, and adds regression tests.
Edits files. Hidden.

### `researcher`

Evidence-gathering agent. Uses web sources and reads files. Separates fact from
inference. Hidden.

### `explorer`

Repository mapping agent. Finds relevant files, data flow, interfaces, tests.
Hidden.

### `test-runner`

Verification agent. Runs narrowest checks then project-standard checks. Reports
exact commands and failures. Hidden.

### `test-writer`

Test design agent. Derives deterministic tests from requirements and failure
modes. Edits files. Hidden.

### `doc-writer`

Documentation agent for docs, docstrings, README. Edits files. Hidden.
