//! UI state and user intent. Provider/tool orchestration stays in Engine.
use super::{
    picker::{Picker, PickerAction, PickerKind},
    Input,
};
use crate::{
    config::{Config, Theme},
    engine::{Engine, Selection},
    model::{
        ActivityEvent, ActivityKind, ActivityPhase, ActivityStatus, Decision, Message, Spend,
        UiEvent,
    },
    session::DisplayEvent,
    tools,
    workflow::{self, Workflow},
};
use anyhow::{bail, Context, Result};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    iter::FromIterator,
    path::Path,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub(super) struct Entry {
    pub role: String,
    pub context: String,
    pub text: String,
    pub revision: u64,
    /// True while the entry is still being fed by provider deltas. Streaming
    /// entries take the lightweight renderer (bounded plain-text tail, no
    /// Markdown/syntect). The final/completing `Message` replacement clears
    /// this and bumps `revision` so the next draw pays the rich render once.
    pub streaming: bool,
    /// Activity owner of this entry, when the producer attached the entry to a
    /// running activity (e.g. a tool call summary bound to its tool id). `None`
    /// keeps the existing chat-transcript behavior. The renderer/commands
    /// layers that read this field ship in the follow-up waves; the field is
    /// already wired through the push helper and exercised by the spine tests.
    #[allow(dead_code)]
    pub activity_id: Option<String>,
}

/// One ordered slot in the transcript timeline. The arrival order is the only
/// ordering the renderer should rely on: chat entries (`Entry(index)` into
/// `App::entries`) and activity events (`Activity(id)`) interleave by the
/// order events were applied. The enum is deliberately tiny; richer join
/// metadata belongs on the referenced entry/node.
#[derive(Debug, Clone)]
#[allow(dead_code)] // consumed by the renderer/commands layers that follow Wave 1.
pub(super) enum TimelineItem {
    Entry(usize),
    Activity(String),
}

/// In-memory activity record. Mirrors a `Start` event and follows up to one
/// `End`. `revision` bumps on End or on user toggle so dependents can
/// invalidate caches without comparing deep state. `expanded` defaults to
/// false so collapsed sections are the safe default.
#[derive(Debug, Clone)]
#[allow(dead_code)] // consumed by the renderer/commands layers that follow Wave 1.
pub(super) struct ActivityNode {
    pub start: ActivityEvent,
    pub status: Option<ActivityStatus>,
    pub expanded: bool,
    pub revision: u64,
}

/// Per-render layout for the activity spine. Built once per draw from the
/// indexes the spine already maintains so the renderer can avoid
/// `O(visible activities × timeline × depth)` walks and per-item
/// `BTreeSet`/`String` clones. All fields are pure lookups: a value is
/// precomputed exactly once for the current activity state, then read by
/// index while the timeline is walked once to assemble visible descriptors.
///
/// Indices line up with `App::activities`: `depths[id_index]`, etc. The
/// snapshot is allocation-bounded: the only growing fields are the activity
/// vectors whose size is `activities.len()`, and `timeline_rows` whose
/// size is `timeline.len()`.
pub(super) struct LayoutSnapshot {
    /// Depth per activity (root = 0). `u8` caps the realistic depth well
    /// below `usize`; orphan parents report 0 like a root, and a cycle
    /// counts each distinct ancestor once before terminating at the
    /// revisit.
    depths: Vec<u8>,
    /// `true` when every ancestor of the activity is expanded. Roots and
    /// orphan parents count as expanded by definition.
    ancestor_visible: Vec<bool>,
    /// Number of timeline items (activity rows plus owned entry details)
    /// hidden behind this activity. Used by the `( +N )` badge so the
    /// collapsed root reports how much is tucked under it without
    /// scanning the timeline per summary.
    descendant_counts: Vec<u32>,
}

/// Identifies the activity whose one-line summary occupies a chat
/// viewport row. `Renderer.hit_map` holds one `Option<ActivitySummary>`
/// per rendered viewport line: `Some` for an activity summary row (the
/// only toggle target) and `None` for every entry/body row. The struct
/// stores only the id, so the viewport hit map never clones the entry
/// lines the renderer already drew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ActivitySummary {
    pub id: String,
}

pub(super) struct Approval {
    pub title: String,
    pub detail: String,
    pub workflow: bool,
    pub reply: oneshot::Sender<Decision>,
}
pub(super) struct Busy {
    pub(super) task: JoinHandle<Result<String>>,
    pub(super) cancel: CancellationToken,
}

/// Surfaced when the front queued message failed to start (workflow file
/// deleted, configured model no longer resolves, etc). The drain loop is
/// suspended so the next finish_run tick does not busy-loop: the user
/// must explicitly retry by pressing Enter with an empty input. A typed
/// draft is appended behind the parked front instead of starting a new
/// run, so FIFO order and any pending items are preserved. While the
/// queue is blocked, `retry_queued` refuses to clobber an in-flight run
/// and explicit workflow exits clear the parking flag.
#[derive(Clone)]
pub(super) struct QueueBlocked {
    pub(super) error: String,
    pub(super) pending: usize,
}

/// Which pane owns keyboard focus. The composer is the default; F6 moves
/// focus into the activity spine and back. Kept as an explicit enum so
/// renderer/commands code can read the state without inferring it from
/// `focused_activity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)] // read by the renderer wave that paints the focused row.
pub(super) enum Focus {
    #[default]
    Input,
    Activity,
}

pub(super) struct App {
    pub entries: Vec<Entry>,
    pub streams: BTreeMap<String, usize>,
    pub input: Input,
    pub input_history: Vec<String>,
    pub history_index: usize,
    pub spend: Spend,
    pub context_limit: u32,
    pub context_tokens: u64,
    pub workspace: String,
    pub queued_inputs: VecDeque<String>,
    pub theme: Theme,
    pub selection: Selection,
    pub mode: Option<String>,
    /// Session-only mouse capture state. The terminal stays in mouse-capture
    /// mode regardless; when false the app ignores wheel and activity-click
    /// events. Defaults to true, is never persisted, and is preserved across
    /// view/session resets (`reset_view`, `/clear`, `/new`, `/reload`).
    pub mouse_enabled: bool,
    pub workflow_mode: Option<String>,
    pub workflow_complete: bool,
    pub last_workflow_input: Option<String>,
    pub status: String,
    pub model_label: String,
    pub effort_label: String,
    pub scroll: usize,
    pub overlay_scroll: usize,
    pub help: bool,
    pub picker: Option<Picker>,
    pub approval: Option<Approval>,
    pub busy: Option<Busy>,
    /// Set when the front of the queue failed to start. While `Some`, the
    /// drain loop is suspended; Enter on the input line retries the front.
    pub queue_blocked: Option<QueueBlocked>,
    pub quit: bool,
    /// Reset invalidates cached entries even if the new conversation has the same length.
    pub history_generation: u64,
    /// In-memory activity records. Insertion order matches `Activity` events;
    /// the engine never mutates this list directly. Index 0 is the oldest.
    ///
    /// `#[allow(dead_code)]` is scoped to the whole activity-spine block: the
    /// renderer/commands layers that will consume these fields ship in the
    /// follow-up waves. The data model and pure helpers are exercised by
    /// `src/tui/app.rs`'s `#[cfg(test)]` block today.
    #[allow(dead_code)]
    pub activities: Vec<ActivityNode>,
    /// id -> index into `activities`. Lets Start/End lookups stay O(log n).
    #[allow(dead_code)]
    pub activity_index: BTreeMap<String, usize>,
    /// Status arriving before its Start. Drained when the matching Start
    /// arrives. End-without-Start is the only producer.
    #[allow(dead_code)]
    pub pending_activity_ends: BTreeMap<String, ActivityStatus>,
    /// Subagent/WorkflowStep records: at most one running activity per
    /// `context`. Tool records are keyed under `activity_by_external` instead.
    #[allow(dead_code)]
    pub activity_by_context: BTreeMap<String, String>,
    /// Tool records keyed by `(context, external_id)`. Multiple Tool
    /// activities with the same external id collapse to the latest; missing
    /// external_id is not stored.
    #[allow(dead_code)]
    pub activity_by_external: BTreeMap<(String, String), String>,
    /// Arrival-ordered interleaving of chat entries and activity events.
    /// `TimelineItem::Entry(idx)` points into `entries`; `TimelineItem::Activity`
    /// carries the activity id.
    #[allow(dead_code)]
    pub timeline: Vec<TimelineItem>,
    /// Keyboard focus owner. Defaults to the composer and resets there.
    pub focus: Focus,
    /// Activity id under keyboard focus while `focus == Focus::Activity`.
    /// Retained across a return to input focus so re-entering restores the
    /// same row when it is still visible. `None` when nothing is focused.
    pub focused_activity: Option<String>,
}

impl App {
    pub fn new(config: &Config, selection: Selection) -> Self {
        let mut selection = selection;
        if selection.agent.is_none() {
            selection.agent = config.default_agent_name();
        }
        Self {
            entries: vec![],
            streams: BTreeMap::new(),
            input: Input::default(),
            input_history: vec![],
            history_index: 0,
            spend: Spend::default(),
            context_limit: 0,
            context_tokens: 0,
            workspace: config.workspace.display().to_string(),
            queued_inputs: VecDeque::new(),
            theme: config.theme.clone(),
            selection,
            mode: None,
            mouse_enabled: true,
            workflow_mode: None,
            workflow_complete: false,
            last_workflow_input: None,
            status: "Ready".into(),
            model_label: format!("{}:{}", config.model.provider, config.model.model),
            effort_label: "default".into(),
            scroll: 0,
            overlay_scroll: 0,
            help: false,
            picker: None,
            approval: None,
            busy: None,
            queue_blocked: None,
            quit: false,
            history_generation: 0,
            activities: Vec::new(),
            activity_index: BTreeMap::new(),
            pending_activity_ends: BTreeMap::new(),
            activity_by_context: BTreeMap::new(),
            activity_by_external: BTreeMap::new(),
            timeline: Vec::new(),
            focus: Focus::Input,
            focused_activity: None,
        }
    }
    pub fn note(&mut self, text: impl Into<String>) {
        self.push("status", "main", text.into());
    }
    pub fn error(&mut self, text: impl Into<String>) {
        self.push("error", "main", text.into());
    }
    fn push(&mut self, role: &str, context: &str, text: String) -> usize {
        self.push_owned(role, context, text, None)
    }
    /// Append a chat entry and its timeline slot. The optional owner binds the
    /// entry to an activity id for future grouping. Existing streaming
    /// replacement mutates `entries[index]` in place and never re-appends a
    /// timeline row, so this helper is the only writer of `TimelineItem::Entry`.
    fn push_owned(
        &mut self,
        role: &str,
        context: &str,
        text: String,
        activity_id: Option<String>,
    ) -> usize {
        let index = self.entries.len();
        self.entries.push(Entry {
            role: role.into(),
            context: context.into(),
            text,
            revision: 0,
            streaming: false,
            activity_id,
        });
        self.timeline.push(TimelineItem::Entry(index));
        index
    }
    /// Live-mode entry point: messages arrive from `UiEvent::Message` and from
    /// the engine's normal turn path. Tool calls are no longer inlined as
    /// `Tool: ...` summaries here — the real Activity Start events produced
    /// by tool dispatch represent them. `message_inner` carries the legacy
    /// fallback flag for the replay path.
    pub fn message(&mut self, context: String, message: Message) {
        self.message_inner(context, message, false);
    }
    fn message_inner(&mut self, context: String, mut message: Message, legacy_fallback: bool) {
        // Legacy fallback: on the first non-main context message, synthesize
        // a collapsed top-level Subagent/WorkflowStep Activity so the context
        // has an owner and any later tool synthesis has a parent. Main never
        // needs a synthesized owner; tool-result messages get their owner
        // from the synthetic Tool by external id.
        if legacy_fallback && context != "main" && !self.activity_by_context.contains_key(&context)
        {
            self.synthesize_legacy_context_owner(&context);
        }
        // Legacy fallback: capture tool_calls before moving `message` so we
        // can synthesize Tool Start nodes after the assistant content is
        // emitted. In live mode this stays empty and the field is dropped.
        let legacy_tool_calls =
            if legacy_fallback && message.role == "assistant" && !message.tool_calls.is_empty() {
                Some(std::mem::take(&mut message.tool_calls))
            } else {
                None
            };
        // Ownership of the entry produced for this message. tool-result
        // messages look up the Tool by (context, tool_call_id); non-main
        // messages attach to the context owner; main ordinary entries stay
        // unowned. The owner is recorded once when the entry is created and
        // preserved on any subsequent in-place replacement.
        let owner = self.owner_for_message(&context, &message);
        // Apply any tool-result status update now while `message` is still
        // whole — the helper only needs `role` and `tool_call_id` / `content`,
        // both of which are still available before we move the content into
        // the chat text.
        self.apply_legacy_tool_result_status(&context, &message);
        let mut text = message.content;
        if let Some(incomplete) = &message.incomplete {
            // The provider stream ended before its completion event. Replace
            // the live stream entry (if any) with the partial text the
            // engine persisted, then surface the marker and reason so the
            // user can see why the assistant stopped mid-turn. Tool calls
            // are already dropped by the engine, but a defensive guard
            // here keeps the marker stable if the contract ever changes.
            text.push_str(&format!(
                "\n\n[incomplete response: {}]",
                if incomplete.reason.is_empty() {
                    "stream ended before completion".to_string()
                } else {
                    incomplete.reason.clone()
                }
            ));
            if message.role == "assistant" {
                if let Some(index) = self.streams.remove(&context) {
                    self.entries[index].text = text;
                    self.entries[index].streaming = false;
                    self.entries[index].revision += 1;
                    return;
                }
            }
            self.push_owned(&message.role, &context, text, owner);
            return;
        }
        if message.role == "assistant" {
            if let Some(index) = self.streams.remove(&context) {
                self.entries[index].text = text;
                self.entries[index].streaming = false;
                self.entries[index].revision += 1;
                return;
            }
        }
        if message.role == "assistant" {
            self.scroll = 0;
        }
        self.push_owned(&message.role, &context, text, owner);
        if let Some(tool_calls) = legacy_tool_calls {
            let parent = self
                .activity_by_context(&context)
                .map(|n| n.start.id.clone());
            for call in &tool_calls {
                self.synthesize_legacy_tool_start(&context, parent.clone(), call);
            }
        }
    }

    /// Replay one DisplayEvent through the existing handlers. `legacy_fallback`
    /// is the session-wide decision (`activities.is_empty()` on the
    /// Session::display_events vector the caller is iterating): when true,
    /// the message path synthesizes collapsed top-level context and Tool
    /// activities so older sessions without on-disk Activity records still
    /// render grouped. Real Activity events flow through
    /// `handle_activity_event` unchanged; the existing fail-safety covers any
    /// out-of-order arrivals.
    pub fn replay_display_event(&mut self, event: DisplayEvent, legacy_fallback: bool) {
        match event {
            DisplayEvent::Message(entry) => {
                self.message_inner(entry.context, entry.message, legacy_fallback)
            }
            DisplayEvent::Activity(event) => self.handle_activity_event(event),
        }
    }

    /// Resolve the activity id that should own this message's chat entry.
    /// Tool-result messages look up the Tool by external id; everything else
    /// attaches to the running context owner (Subagent/WorkflowStep). Main
    /// ordinary entries stay unowned.
    fn owner_for_message(&self, context: &str, message: &Message) -> Option<String> {
        if message.role == "tool" {
            if let Some(tcid) = message.tool_call_id.as_ref() {
                if let Some(node) = self.activity_by_external(context, tcid) {
                    return Some(node.start.id.clone());
                }
            }
            return None;
        }
        if context == "main" {
            return None;
        }
        self.activity_by_context(context)
            .map(|node| node.start.id.clone())
    }

    /// Streaming entries attach to the running context owner (Subagent /
    /// WorkflowStep) when one is registered. Main stays unowned. Used only
    /// when a new stream opens; subsequent deltas mutate the entry in place
    /// and never re-resolve ownership.
    fn owner_for_stream(&self, context: &str) -> Option<String> {
        if context == "main" {
            return None;
        }
        self.activity_by_context(context)
            .map(|node| node.start.id.clone())
    }

    /// Legacy fallback: synthesize a collapsed top-level Subagent or
    /// WorkflowStep activity for the given context. Idempotent — if the
    /// context already has an owner, nothing is inserted.
    fn synthesize_legacy_context_owner(&mut self, context: &str) {
        if self.activity_by_context.contains_key(context) {
            return;
        }
        let kind = if context.starts_with("subagent:") {
            ActivityKind::Subagent
        } else {
            ActivityKind::WorkflowStep
        };
        let id = self.legacy_unique_id(&format!("legacy:context:{context}"));
        self.handle_activity_event(ActivityEvent {
            id,
            parent_id: None,
            context: context.to_owned(),
            kind,
            phase: ActivityPhase::Start,
            title: context.to_owned(),
            external_id: None,
            status: None,
        });
    }

    /// Legacy fallback: synthesize a collapsed Tool Start node for a call.
    /// The id is derived from `context + tool_call_id`; collisions are
    /// resolved deterministically by appending `#2`, `#3`, ... so the
    /// original call is never overwritten.
    fn synthesize_legacy_tool_start(
        &mut self,
        context: &str,
        parent: Option<String>,
        call: &crate::model::ToolCall,
    ) {
        let base = format!("legacy:tool:{context}:{}", call.id);
        let id = self.legacy_unique_id(&base);
        let arguments = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
        let description = tools::describe_call(&call.name, &arguments);
        // Bound to a single short line so the transcript never shows raw
        // arguments or unbounded wrap.
        let one_line: String = description.split_whitespace().collect::<Vec<_>>().join(" ");
        let bounded = if one_line.chars().count() > 120 {
            let mut out: String = one_line.chars().take(117).collect();
            out.push_str("...");
            out
        } else {
            one_line
        };
        let title = format!("tool {}: {}", call.name, bounded);
        self.handle_activity_event(ActivityEvent {
            id,
            parent_id: parent,
            context: context.to_owned(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::Start,
            title,
            external_id: Some(call.id.clone()),
            status: None,
        });
    }

    /// Pick a deterministic collision-safe id by probing `activity_index`.
    /// Replay order is the only ordering, so the first free form wins.
    fn legacy_unique_id(&mut self, base: &str) -> String {
        if !self.activity_index.contains_key(base) {
            return base.to_owned();
        }
        let mut suffix: usize = 2;
        loop {
            let candidate = format!("{base}#{suffix}");
            if !self.activity_index.contains_key(&candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    /// Update the synthetic Tool node's status based on a tool result
    /// message. Classification walks structured JSON up to 3 levels deep so
    /// double-encoded strings do not hide the error; anything not classified
    /// as an error is treated as success. The status is only ever set by
    /// this path — replay's `finish_legacy_replay` then sweeps any still
    /// running synthetic nodes to Success unless an error result already
    /// marked them.
    ///
    /// Authoritative live activity statuses (set by `activity_end` from a
    /// real `End` event, or by `apply_recovered_unmatched` /
    /// `apply_unmatched_ids` from a recovered unmatched start) take
    /// precedence. The legacy classifier only patches the synthetic Tool
    /// node when no authoritative status has been recorded yet; legacy
    /// synthesized nodes start at `None` and stay there until this helper
    /// (or the sweep) classifies them.
    fn apply_legacy_tool_result_status(&mut self, context: &str, message: &Message) {
        if message.role != "tool" {
            return;
        }
        let Some(tcid) = message.tool_call_id.as_ref() else {
            return;
        };
        let Some(activity_id) = self
            .activity_by_external
            .get(&(context.to_owned(), tcid.clone()))
            .cloned()
        else {
            return;
        };
        let Some(&index) = self.activity_index.get(&activity_id) else {
            return;
        };
        // Authoritative live status wins. The legacy path runs only
        // during session replay of pre-Activity sessions, so live
        // `activity_end` / recovered-unmatched patches will have
        // already set the field when a real End was recorded before the
        // replay started. Touching the field here would overwrite the
        // engine's decision.
        if self.activities[index].status.is_some() {
            return;
        }
        let status = classify_tool_result_status(&message.content);
        let node = &mut self.activities[index];
        if node.status == Some(status) {
            return;
        }
        node.status = Some(status);
        node.revision = node.revision.saturating_add(1);
    }

    /// Mark every still-running synthesized node (ids starting with
    /// `legacy:`) as Success unless an error result already marked it Error.
    /// Real Activity ids (those produced by the live engine) are left alone.
    pub fn finish_legacy_replay(&mut self) {
        for node in &mut self.activities {
            if !node.start.id.starts_with("legacy:") {
                continue;
            }
            if node.status.is_some() {
                continue;
            }
            node.status = Some(ActivityStatus::Success);
            node.revision = node.revision.saturating_add(1);
        }
    }

    /// Mark every node whose id appears in `unmatched_ids` as Cancelled
    /// unless it already has a final status. This is the in-memory mirror
    /// of `Session::recovered_unmatched` for resumed sessions: the on-disk
    /// log is append-only, so cancelled statuses are recorded as a status
    /// patch on existing nodes rather than as a new End event. No End
    /// timeline row is appended; matches the existing
    /// `apply_recovered_unmatched` event-list helper and the
    /// `recovered_unmatched_marks_cancelled_without_emitting_end_row`
    /// invariant test.
    pub fn apply_unmatched_ids(&mut self, unmatched_ids: &[String]) {
        for id in unmatched_ids {
            let Some(&index) = self.activity_index.get(id) else {
                continue;
            };
            if self.activities[index].status.is_some() {
                continue;
            }
            self.activities[index].status = Some(ActivityStatus::Cancelled);
            self.activities[index].revision = self.activities[index].revision.saturating_add(1);
        }
    }

    /// Reset only the in-memory view spine (chat entries, activity
    /// records, indexes, timeline, scroll and history-view state). The
    /// helper is the single source of truth for view-state reset so
    /// `/clear` / `/new` and any future caller stay in lock-step. Runtime
    /// model/agent/theme/mouse settings, the input editor, queue state,
    /// workflow pointers, spend/context tokens, and old session files are
    /// intentionally left alone — those are session state, not view
    /// state, and the existing `/clear` / `/new` handlers manage them
    /// alongside this call.
    ///
    /// `history_generation` is bumped so cached entries that survived the
    /// reset are still invalidated, matching the existing
    /// `reset_session` behavior. There is no separate legacy-replay
    /// counter today; the sweep is a per-replay pass with no retained
    /// state, so resetting the spine is sufficient.
    pub fn reset_view(&mut self) {
        self.finish_streaming_entries();
        self.entries.clear();
        self.streams.clear();
        self.activities.clear();
        self.activity_index.clear();
        self.pending_activity_ends.clear();
        self.activity_by_context.clear();
        self.activity_by_external.clear();
        self.timeline.clear();
        self.scroll = 0;
        self.history_index = 0;
        self.history_generation = self.history_generation.saturating_add(1);
        // A fresh view has no activity rows to point at; drop focus back to
        // the composer so the next keystroke edits input again.
        self.focus = Focus::Input;
        self.focused_activity = None;
    }

    pub fn event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Model {
                context,
                provider,
                model,
                effort,
            } => {
                // Only main-context model events drive the global header.
                // Child/workflow model changes surface through their
                // lifecycle activity records instead.
                if context == "main" {
                    self.model_label = format!("{provider}:{model}");
                    self.effort_label = effort
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "default".into());
                    self.status = format!("Generating | {context}");
                }
            }
            UiEvent::Delta { context, text } => {
                let index = match self.streams.get(&context) {
                    Some(index) => *index,
                    None => {
                        self.scroll = 0;
                        // Streaming entries attach to the running context
                        // owner (Subagent/WorkflowStep) when one is
                        // registered; main stays unowned. The owner is set
                        // once at stream open and preserved by every
                        // subsequent in-place replacement.
                        let owner = self.owner_for_stream(&context);
                        let index = self.push_owned("assistant", &context, String::new(), owner);
                        self.entries[index].streaming = true;
                        self.streams.insert(context, index);
                        index
                    }
                };
                self.entries[index].text.push_str(&text);
                self.entries[index].revision += 1;
            }
            UiEvent::Message { context, message } => self.message(context, message),
            UiEvent::Status { context, text } => {
                // The global status line is main-only; child and workflow
                // status text belongs to their activity rows.
                if context == "main" {
                    self.status = text;
                }
            }
            UiEvent::Spend(spend) => self.spend = spend,
            UiEvent::Context { context, tokens } => {
                if context == "main" {
                    self.context_tokens = tokens;
                }
            }
            UiEvent::Approval {
                title,
                detail,
                workflow,
                reply,
            } => {
                self.overlay_scroll = 0;
                if !reply.is_closed() {
                    self.approval = Some(Approval {
                        title,
                        detail,
                        workflow,
                        reply,
                    });
                }
            }
            UiEvent::Activity(event) => self.handle_activity_event(event),
        }
    }
    pub async fn refresh_model(&mut self, engine: &Engine) -> Result<()> {
        let scope = engine.scope(&self.selection, "main", None).await?;
        self.model_label = format!("{}:{}", scope.model.provider, scope.model.model);
        self.effort_label = scope
            .model
            .reasoning
            .and_then(|r| r.effort)
            .map(|e| e.to_string())
            .unwrap_or_else(|| "default".into());
        self.context_limit = scope.model.max_tokens;
        self.workspace = engine.config.read().await.workspace.display().to_string();
        Ok(())
    }
    pub fn require_idle(&self) -> Result<()> {
        if self.busy.is_some() {
            bail!("Wait for the active run or cancel it with Ctrl+C");
        }
        Ok(())
    }

    pub async fn submit(&mut self, engine: &Engine, config_path: &Path) -> Result<()> {
        // While the queue is blocked the front item must be retried
        // explicitly so the user keeps their draft and FIFO order. A typed
        // draft does NOT start a new run; it joins the queue behind the
        // parked front. The existing draft is taken only after the new
        // item is safely appended.
        if self.queue_blocked.is_some() {
            if self.input.text.trim().is_empty() {
                if self.busy.is_some() {
                    // Wait for the in-flight run before retrying; doing
                    // nothing here avoids accidentally clearing the draft
                    // on a busy run. Surface the reason so the user is
                    // not left staring at a stale "Queued ... blocked"
                    // status.
                    self.status = "Queue blocked; wait for the active run before retrying".into();
                    return Ok(());
                }
                return self.retry_queued(engine).await;
            }
            // Non-empty draft while blocked: queue behind the parked front.
            let text = self.input.take();
            let text = text.trim().to_owned();
            if text.is_empty() {
                return Ok(());
            }
            self.input_history.push(text.clone());
            self.history_index = self.input_history.len();
            self.scroll = 0;
            // Slash commands still run; /reload, /clear, /new will clear the
            // block via their handlers, and /help, /cost etc. work mid-run.
            if text.starts_with('/') || text == ":q" {
                return self.command(&text, engine, config_path).await;
            }
            self.queued_inputs.push_back(text);
            self.status = format!(
                "Queued {} message(s) | blocked at front; press Enter to retry",
                self.queued_inputs.len()
            );
            return Ok(());
        }
        let text = self.input.take();
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Ok(());
        }
        self.input_history.push(text.clone());
        self.history_index = self.input_history.len();
        self.scroll = 0;
        if text.starts_with('/') || text == ":q" {
            return self.command(&text, engine, config_path).await;
        }
        if self.busy.is_some() {
            self.queued_inputs.push_back(text);
            self.status = format!("Queued {} message(s)", self.queued_inputs.len());
            return Ok(());
        }
        self.start_input(engine, text).await?;
        self.status = "Running | Ctrl+C to cancel".into();
        Ok(())
    }

    async fn start_input(&mut self, engine: &Engine, text: String) -> Result<()> {
        if let Some(path) = self.workflow_mode.clone() {
            self.start_workflow(engine, &path, text).await?;
        } else {
            let engine = engine.clone();
            let selection = self.selection.clone();
            let cancel = CancellationToken::new();
            let token = cancel.clone();
            self.busy = Some(Busy {
                cancel,
                task: tokio::spawn(async move { engine.turn(text, selection, token).await }),
            });
        }
        Ok(())
    }
    pub async fn start_workflow(
        &mut self,
        engine: &Engine,
        name: &str,
        input: String,
    ) -> Result<()> {
        self.require_idle()?;
        let config = engine.config.read().await.clone();
        let workflow = Workflow::load(&workflow::workflow_path(name, &config), &config)?;
        let engine = engine.clone();
        let selection = self.selection.clone();
        let workflow_input = input.clone();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        self.busy = Some(Busy {
            cancel,
            task: tokio::spawn(async move {
                workflow::run(&engine, workflow, input, selection, token).await
            }),
        });
        self.last_workflow_input = Some(workflow_input);
        self.workflow_mode = Some(name.to_owned());
        self.workflow_complete = false;
        Ok(())
    }
    /// Clear the `streaming` flag on every entry the live delta map still
    /// tracks. Called on every run teardown path (success, error, join
    /// failure/cancel) and on view reset before `streams` is dropped, so a
    /// stream whose final `Message` never arrived stops taking the
    /// lightweight renderer.
    fn finish_streaming_entries(&mut self) {
        let indexes: Vec<usize> = self.streams.values().copied().collect();
        for index in indexes {
            if let Some(entry) = self.entries.get_mut(index) {
                if entry.streaming {
                    entry.streaming = false;
                    entry.revision = entry.revision.saturating_add(1);
                }
            }
        }
    }

    pub async fn finish_run(
        &mut self,
        engine: &Engine,
        events: &mut mpsc::UnboundedReceiver<UiEvent>,
    ) -> bool {
        if !self.busy.as_ref().is_some_and(|b| b.task.is_finished()) {
            return false;
        }
        let busy = self.busy.take().unwrap();
        while let Ok(event) = events.try_recv() {
            self.event(event);
        }
        match busy.task.await {
            Ok(Ok(_)) => {
                if self.workflow_mode.is_some() {
                    self.enter_workflow_complete();
                    self.status = "Workflow complete | n new | r repeat | q exit workflow".into();
                } else {
                    self.status = "Ready".into();
                }
            }
            Ok(Err(e)) => {
                self.error(format!("{e:#}"));
                self.status = "Run ended".into();
            }
            // JoinError means the spawned task was cancelled, panicked, or
            // aborted. The previous "Running | Ctrl+C to cancel" status was
            // left stale; set a clear follow-up so the footer does not lie
            // about an active run.
            Err(e) => {
                self.status = format!("Run failed: {e}");
                self.error(format!("Run failed: {e}"));
            }
        }
        self.finish_streaming_entries();
        self.streams.clear();
        self.approval = None;
        if let Err(e) = self.refresh_model(engine).await {
            self.error(e.to_string());
        }
        // Drain the next queued input on a clean finish. On start failure we
        // park the popped item back at the front and expose the error via
        // `queue_blocked` so the next finish_run tick does not busy-loop.
        if self.queue_blocked.is_none() {
            if let Some(input) = self.queued_inputs.pop_front() {
                match self.start_input(engine, input.clone()).await {
                    Ok(()) => {
                        self.status = format!(
                            "Running | {} message(s) queued | Ctrl+C to cancel",
                            self.queued_inputs.len()
                        );
                    }
                    Err(error) => {
                        self.queued_inputs.push_front(input);
                        self.mark_queue_blocked(error);
                    }
                }
            }
        }
        true
    }

    /// Raise the workflow-complete overlay. Every false→true transition
    /// resets the overlay body scroll so a freshly raised overlay starts at
    /// the top rather than inheriting a stale offset from a prior overlay.
    fn enter_workflow_complete(&mut self) {
        if !self.workflow_complete {
            self.overlay_scroll = 0;
        }
        self.workflow_complete = true;
    }

    /// Mark the front of the queue as blocked so the drain loop pauses and
    /// the user can retry with Enter. The transcript already holds the
    /// error from `start_input`; this just sets state and the status line.
    /// While in workflow mode the workflow-complete overlay is also raised
    /// so the error is visible alongside the standard n/r/q choices.
    fn mark_queue_blocked(&mut self, error: anyhow::Error) {
        let pending = self.queued_inputs.len();
        let message = format!("{error:#}");
        self.error(message.clone());
        self.queue_blocked = Some(QueueBlocked {
            error: message,
            pending,
        });
        if self.workflow_mode.is_some() {
            self.enter_workflow_complete();
        }
        self.status =
            format!("Queued {pending} message(s) blocked | start failed; press Enter to retry");
    }

    /// Retry the front queued message. Called when the user explicitly
    /// presses Enter while the queue is blocked. On success the block is
    /// cleared and the run continues; on failure the block remains and the
    /// user keeps their draft/FIFO order. Refuses to clobber an in-flight
    /// run: the parked front is left in place and the caller is told why.
    async fn retry_queued(&mut self, engine: &Engine) -> Result<()> {
        if self.busy.is_some() {
            self.status = "Queue blocked; wait for the active run before retrying".into();
            return Ok(());
        }
        let Some(input) = self.queued_inputs.pop_front() else {
            self.queue_blocked = None;
            return Ok(());
        };
        match self.start_input(engine, input.clone()).await {
            Ok(()) => {
                self.queue_blocked = None;
                self.status = format!(
                    "Running | {} message(s) queued | Ctrl+C to cancel",
                    self.queued_inputs.len()
                );
            }
            Err(error) => {
                self.queued_inputs.push_front(input);
                self.mark_queue_blocked(error);
            }
        }
        Ok(())
    }
    pub async fn cancel_and_join(&mut self) {
        if let Some(mut busy) = self.busy.take() {
            busy.cancel.cancel();
            self.approval = None;
            if tokio::time::timeout(Duration::from_secs(5), &mut busy.task)
                .await
                .is_err()
            {
                busy.task.abort();
                let _ = busy.task.await;
            }
        }
    }

    /// Modal input takes priority over chat editing and application shortcuts.
    /// Return true only when the chat input should be submitted by the loop.
    ///
    /// Routing order matches the visible overlays so a hidden picker never
    /// eats keys that should reach a higher-priority modal:
    /// approval → help → workflow-complete → picker → activity Esc → bare
    /// cancel → Tab → F6 focus toggle → activity focus → input editing.
    pub async fn handle_key(&mut self, key: KeyEvent, engine: &Engine) -> Result<bool> {
        // Drop release events unconditionally: terminals on Linux/X11 and
        // over SSH report key-up, and Shift+Release must not act as a key
        // press (otherwise Tab release would cycle the agent and Shift+letter
        // release would still feed the picker search field).
        if key.kind == KeyEventKind::Release {
            return Ok(false);
        }
        // Approvals always win: y/n/r/s/q/Esc must reach the approval
        // dialog and not get hijacked by the bare run-cancel shortcut.
        if self.approval.is_some() {
            return Ok(self.edit_key(key));
        }
        // Help and the workflow-complete overlay sit above any open picker
        // so Esc/q/Enter/n/r/q route to them, not to the picker behind.
        if self.help {
            return Ok(self.edit_key(key));
        }
        if self.workflow_complete {
            // PageUp/PageDown/Home/End scroll the overlay body; the long
            // blocked-error text in particular can outgrow the viewport
            // and the transcript behind stays put.
            match key.code {
                KeyCode::PageDown => {
                    self.overlay_scroll = self.overlay_scroll.saturating_add(10);
                    return Ok(false);
                }
                KeyCode::PageUp => {
                    self.overlay_scroll = self.overlay_scroll.saturating_sub(10);
                    return Ok(false);
                }
                KeyCode::Home => {
                    self.overlay_scroll = 0;
                    return Ok(false);
                }
                KeyCode::End => {
                    self.overlay_scroll = usize::MAX;
                    return Ok(false);
                }
                _ => {}
            }
            // Letter controls require no modifiers so Ctrl/Alt combinations
            // cannot be mistaken for n/r/q decisions. Esc closes without
            // regard to modifiers.
            let unmodified = key.modifiers.is_empty();
            match key.code {
                KeyCode::Char('n') if unmodified => {
                    self.workflow_complete = false;
                    // Clear the parked-front block before the user enters a
                    // fresh run's input. Otherwise submit would append the
                    // draft behind the old blocked front instead of starting
                    // the new workflow. `queued_inputs` is preserved so any
                    // earlier queued messages still drain FIFO later.
                    self.queue_blocked = None;
                    // Preserve the user's current draft; the next workflow
                    // input will be whatever they have in the input box
                    // when they press Enter again.
                    self.status = "Enter input for a new workflow run".into();
                }
                KeyCode::Char('r') if unmodified => {
                    let workflow = self
                        .workflow_mode
                        .clone()
                        .context("Workflow is not selected")?;
                    let input = self.last_workflow_input.clone().unwrap_or_default();
                    // The parked block belongs to the previous run. Clear it
                    // so the repeated run starts immediately and queued
                    // messages drain afterward; the queue itself is preserved.
                    self.queue_blocked = None;
                    self.start_workflow(engine, &workflow, input).await?;
                }
                KeyCode::Char('q') | KeyCode::Esc if key.code == KeyCode::Esc || unmodified => {
                    // Explicit workflow exit also clears the queue-block
                    // parking: the workflow that parked the front is
                    // gone, so leaving the front stuck would be an orphan.
                    // `queued_inputs` is preserved so any queued items the
                    // user typed still drain on the next start_input.
                    self.workflow_complete = false;
                    self.workflow_mode = None;
                    self.last_workflow_input = None;
                    self.queue_blocked = None;
                    self.status = "Exited workflow mode".into();
                }
                KeyCode::Enter
                    if self.queue_blocked.is_some() && self.input.text.trim().is_empty() =>
                {
                    // Empty Enter retries the blocked front item; the
                    // event loop will call submit, which checks the queue
                    // block and routes to retry_queued (which itself
                    // refuses if a run is in flight).
                    return Ok(true);
                }
                _ => {}
            }
            return Ok(false);
        }
        // Open pickers close on Esc/Ctrl+C without touching an in-flight run.
        // We route keys to the picker directly; `edit_key` still owns help.
        if let Some(mut picker) = self.picker.take() {
            let action = picker.key(key);
            match action {
                PickerAction::Close => {
                    if let PickerKind::Themes { original, .. } = &picker.kind {
                        self.theme = *original.clone();
                    }
                    // picker is dropped here.
                }
                PickerAction::Select(reference) => {
                    self.apply_picker_selection(picker, reference, engine)
                        .await?;
                }
                PickerAction::None => {
                    if let Some(theme) = picker.preview_theme() {
                        self.theme = theme.clone();
                    }
                    self.picker = Some(picker); // stay open, no decision yet
                }
            }
            return Ok(false);
        }
        // While the activity spine owns focus, bare Esc returns to the
        // composer instead of cancelling an in-flight run. It sits after every
        // higher-priority modal/picker handler above so those still own their
        // keys, and before the bare cancel branch so Ctrl+C remains the only
        // run-cancel shortcut while activity focus is active.
        if self.focus == Focus::Activity && key.code == KeyCode::Esc {
            self.focus = Focus::Input;
            return Ok(false);
        }
        // Bare Esc/Ctrl+C with no modal may still cancel the run.
        if self.busy.is_some()
            && matches!(key.code, KeyCode::Esc | KeyCode::Char('c'))
            && (key.code == KeyCode::Esc || key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.cancel_active_run();
            return Ok(false);
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            // Tab/BackTab must never touch workflow mode, regardless of
            // whether the run is active, has failed/finished, the queue is
            // blocked, or the workflow-complete overlay is up. Higher-priority
            // modals (approval/help/workflow-complete/picker) already routed
            // away above, so reaching here means no modal claims the key.
            // Cycling the agent in that state would silently drop the
            // workflow pointer, leaving the user with no clear path out.
            if self.workflow_mode.is_some() {
                self.status = "Agent cycling disabled in workflow mode".into();
                return Ok(false);
            }
            let reverse =
                key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
            self.cycle_agent(engine, reverse).await?;
            return Ok(false);
        }
        // F6 toggles focus between the composer and the activity spine. It
        // sits after every modal/picker/workflow handler above so those
        // still own their keys, and before `edit_key` so it can never be
        // typed into the composer.
        if key.code == KeyCode::F(6) {
            self.toggle_focus();
            return Ok(false);
        }
        // While the activity spine owns focus, its movement/expansion keys
        // are consumed here; only transcript scrolling falls through to
        // `edit_key`. This keeps the composer and input history untouched.
        if self.focus == Focus::Activity {
            return Ok(self.activity_key(key));
        }
        Ok(self.edit_key(key))
    }

    /// Move keyboard focus between the composer and the activity spine.
    /// Entering activity focus keeps the previously focused id when it is
    /// still visible, otherwise selects the newest visible activity. With no
    /// visible activity the composer keeps focus and the status line says so.
    fn toggle_focus(&mut self) {
        match self.focus {
            Focus::Activity => self.focus = Focus::Input,
            Focus::Input => {
                let visible = self.visible_activity_ids();
                if visible.is_empty() {
                    self.status = "No activity to focus".into();
                    return;
                }
                let retained = self
                    .focused_activity
                    .clone()
                    .filter(|id| visible.iter().any(|visible_id| visible_id == id));
                self.focused_activity =
                    Some(retained.unwrap_or_else(|| visible.last().unwrap().clone()));
                self.focus = Focus::Activity;
            }
        }
    }

    /// Keyboard handling while the activity spine owns focus. Movement and
    /// expansion keys are consumed; PgUp/PgDn and Ctrl+Home/End keep the
    /// transcript's existing bottom-distance scrolling. Every other key is
    /// absorbed so the composer and input history cannot be mutated. Returns
    /// `true` only when the loop should submit, which never happens here.
    fn activity_key(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up => self.move_activity_focus(-1),
            KeyCode::Down => self.move_activity_focus(1),
            KeyCode::Left => {
                if let Some(id) = self.focused_activity.clone() {
                    self.set_activity_expanded(&id, Some(false));
                }
            }
            KeyCode::Right => {
                if let Some(id) = self.focused_activity.clone() {
                    self.set_activity_expanded(&id, Some(true));
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(id) = self.focused_activity.clone() {
                    self.toggle_activity_expanded(&id);
                }
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Home if control => self.scroll = usize::MAX,
            KeyCode::End if control => self.scroll = 0,
            _ => {}
        }
        false
    }

    /// Move the focused activity by `delta` rows through the currently
    /// visible ids, clamped at both ends. A missing/absent focus starts from
    /// the newest row when moving forward and the oldest when moving back.
    fn move_activity_focus(&mut self, delta: isize) {
        let visible = self.visible_activity_ids();
        if visible.is_empty() {
            self.focus = Focus::Input;
            return;
        }
        let position = self
            .focused_activity
            .as_ref()
            .and_then(|id| visible.iter().position(|visible_id| visible_id == id));
        let next = match position {
            Some(index) => (index as isize + delta).clamp(0, visible.len() as isize - 1) as usize,
            None if delta < 0 => 0,
            None => visible.len() - 1,
        };
        self.focused_activity = Some(visible[next].clone());
    }

    /// Keep `focused_activity` pointing at a visible row. When the focused
    /// id is hidden by a collapse, move to the nearest collapsed ancestor
    /// that is itself visible (the parent the user just closed); otherwise
    /// fall back to the newest visible row, or the composer when nothing is
    /// visible. Cheap no-op unless the spine actually owns focus.
    fn normalize_activity_focus(&mut self) {
        if self.focus != Focus::Activity {
            return;
        }
        let visible = self.visible_activity_ids();
        if visible.is_empty() {
            self.focus = Focus::Input;
            return;
        }
        let Some(current) = self.focused_activity.clone() else {
            self.focused_activity = Some(visible.last().unwrap().clone());
            return;
        };
        if visible.iter().any(|visible_id| visible_id == &current) {
            return;
        }
        if let Some(ancestor) = self.nearest_visible_collapsed_ancestor(&current) {
            self.focused_activity = Some(ancestor);
            return;
        }
        self.focused_activity = Some(visible.last().unwrap().clone());
    }

    /// Nearest ancestor of `id` that is collapsed and itself visible. That
    /// is the node whose collapse hid `id`, so focus can land on it. Returns
    /// `None` for roots, orphans, and cycles.
    fn nearest_visible_collapsed_ancestor(&self, id: &str) -> Option<String> {
        let mut visited = BTreeSet::from_iter([id.to_owned()]);
        let mut cursor = self
            .activity(id)
            .and_then(|node| node.start.parent_id.clone());
        while let Some(parent_id) = cursor {
            if !visited.insert(parent_id.clone()) {
                return None;
            }
            let parent = self.activity(&parent_id)?;
            if !parent.expanded && self.all_ancestors_expanded(&parent_id) {
                return Some(parent_id);
            }
            cursor = parent.start.parent_id.clone();
        }
        None
    }

    /// Renderer-readable focus helpers. Kept minimal: the renderer only
    /// needs to know whether the spine owns focus and which row is focused.
    #[allow(dead_code)]
    pub fn activity_focused(&self) -> bool {
        self.focus == Focus::Activity
    }

    #[allow(dead_code)]
    pub fn focused_activity_id(&self) -> Option<&str> {
        self.focused_activity.as_deref()
    }

    /// Apply the chosen picker row. On fallible kinds (Models, Agents) we
    /// preserve/reopen the picker so a transient scope error does not strand
    /// the user behind a closed dialog and a lost selection.
    async fn apply_picker_selection(
        &mut self,
        mut picker: Picker,
        reference: String,
        engine: &Engine,
    ) -> Result<()> {
        match &picker.kind {
            PickerKind::Models => {
                match self.select_model(&reference, engine).await {
                    Ok(()) => {
                        if let Err(error) = self.refresh_model(engine).await {
                            self.error(error.to_string());
                        }
                        self.status = format!("Selected {}", self.model_label);
                    }
                    Err(error) => {
                        self.error(format!("{error:#}"));
                        self.status = format!("Model not selected: {reference}");
                        self.picker = Some(picker); // reopen so the user can retry
                    }
                }
            }
            PickerKind::Agents => {
                let config = engine.config.read().await;
                let default_agent = config.default_agent_name();
                drop(config);
                // Pass the literal reference through; `engine.scope` resolves
                // it to the configured default when one exists. Only collapse
                // to `None` when "default" refers to the synthetic scope.
                let selection = Selection {
                    agent: (reference != "default" || default_agent.is_some())
                        .then(|| reference.clone()),
                    agent_mode: None,
                    ..Selection::default()
                };
                match engine.scope(&selection, "main", None).await {
                    Ok(_) => {
                        self.selection = selection;
                        self.mode = None;
                        // Selecting from the picker is explicit consent to
                        // leave the workflow, even mid-flight. The parking
                        // flag points at a workflow/model that no longer
                        // applies to the chosen agent, so clear it too.
                        self.workflow_mode = None;
                        self.workflow_complete = false;
                        self.last_workflow_input = None;
                        self.queue_blocked = None;
                        if let Err(error) = self.refresh_model(engine).await {
                            self.error(error.to_string());
                        }
                        self.note(format!("Switched agent to {reference}"));
                        self.status = format!("Selected agent {reference}");
                    }
                    Err(error) => {
                        self.error(format!("{error:#}"));
                        self.status = format!("Agent not selected: {reference}");
                        self.picker = Some(picker); // reopen so the user can retry
                    }
                }
            }
            PickerKind::Mcps => {
                let config = engine.config.read().await;
                let mut switches = engine.switches.write().await;
                let enabled = !switches.mcp_enabled(&reference, &config);
                switches.mcps.insert(reference.clone(), enabled);
                if let Some(choice) = picker.choices.iter_mut().find(|c| c.reference == reference) {
                    choice.enabled = Some(enabled);
                }
                self.status = format!("MCP {reference}: {}", if enabled { "on" } else { "off" });
                self.picker = Some(picker); // stay open for further toggles
            }
            PickerKind::Themes { .. } => {
                if let Some(theme) = picker.preview_theme() {
                    self.theme = theme.clone();
                }
                self.status = format!("Theme: {reference}");
                // picker is dropped here.
            }
        }
        Ok(())
    }

    /// Compatibility wrapper for callers that have no hit-tested activity
    /// id. Wheel routing and click handling live in
    /// `handle_mouse_with_activity_target`; passing `None` makes every
    /// click a no-op.
    #[cfg(test)]
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !self.mouse_enabled {
            return;
        }
        self.handle_mouse_with_activity_target(mouse, None);
    }

    /// Mouse handling with an optional hit-tested activity id. Existing
    /// callers use `handle_mouse`, which delegates here with `None`.
    ///
    /// Wheel routing is unchanged. A left click on a visible activity
    /// summary row moves keyboard focus to that activity and toggles it.
    /// Clicks are ignored while a modal overlay or picker owns input, and a
    /// `None`, unknown, or hidden `activity_target` is a no-op. Clicks never
    /// touch the composer, queue, scroll offset, or session history.
    pub fn handle_mouse_with_activity_target(
        &mut self,
        mouse: MouseEvent,
        activity_target: Option<String>,
    ) {
        // Session-only kill switch: when capture is disabled the app ignores
        // every mouse event, so wheel and activity clicks are inert.
        if !self.mouse_enabled {
            return;
        }
        let delta = match mouse.kind {
            MouseEventKind::ScrollUp => 3,
            MouseEventKind::ScrollDown => -3,
            MouseEventKind::Down(MouseButton::Left) => {
                // Approval, help, workflow-complete, and pickers all consume
                // input; a click behind them must not move focus or toggle.
                if self.approval.is_some()
                    || self.help
                    || self.workflow_complete
                    || self.picker.is_some()
                {
                    return;
                }
                // Missing target means the click missed every toggle row.
                let Some(id) = activity_target else {
                    return;
                };
                // Unknown ids and activities tucked behind a collapsed
                // ancestor are not clickable: only rows the renderer drew.
                if self.activity(&id).is_none() || !self.all_ancestors_expanded(&id) {
                    return;
                }
                self.focus = Focus::Activity;
                self.focused_activity = Some(id.clone());
                self.toggle_activity_expanded(&id);
                return;
            }
            _ => return,
        };
        // Approval, help, and the workflow-complete overlay (which often
        // shows long blocked-error text) share `overlay_scroll`. The
        // hidden transcript behind the overlay must not move while any of
        // these modals is open.
        if self.approval.is_some() || self.help || self.workflow_complete {
            if delta > 0 {
                self.overlay_scroll = self.overlay_scroll.saturating_add(delta as usize);
            } else {
                self.overlay_scroll = self.overlay_scroll.saturating_sub((-delta) as usize);
            }
        } else if let Some(picker) = self.picker.as_mut() {
            // Route wheel events to an open picker so scrolling the choices
            // list does not jump the hidden transcript. The transcript
            // scroll convention inverts the wheel direction (ScrollDown
            // means "show newer content"); for the picker, ScrollDown moves
            // the selection forward.
            picker.scroll(-delta);
        } else if delta > 0 {
            self.scroll = self.scroll.saturating_add(delta as usize);
        } else {
            self.scroll = self.scroll.saturating_sub((-delta) as usize);
        }
    }

    fn cancel_active_run(&mut self) {
        if let Some(busy) = &self.busy {
            busy.cancel.cancel();
            self.status = "Cancelling...".into();
        }
        if let Some(approval) = self.approval.take() {
            let _ = approval.reply.send(Decision::Abort);
        }
    }

    pub fn paste(&mut self, text: &str) {
        // Approval, help, and the workflow-complete overlay own all input,
        // and the activity spine's focused keys never reach the composer.
        // Paste must not bypass that key isolation and edit the draft behind
        // an owning surface.
        if self.approval.is_some()
            || self.help
            || self.workflow_complete
            || self.focus == Focus::Activity
        {
            return;
        }
        if let Some(picker) = &mut self.picker {
            picker.paste(text);
            if let Some(theme) = picker.preview_theme() {
                self.theme = theme.clone();
            }
        } else {
            self.input.insert(&text.replace('\r', "\n"));
        }
    }

    fn edit_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.approval.is_some() || self.help {
            match key.code {
                KeyCode::PageDown => {
                    self.overlay_scroll = self.overlay_scroll.saturating_add(10);
                    return false;
                }
                KeyCode::PageUp => {
                    self.overlay_scroll = self.overlay_scroll.saturating_sub(10);
                    return false;
                }
                KeyCode::Home => {
                    self.overlay_scroll = 0;
                    return false;
                }
                KeyCode::End => {
                    self.overlay_scroll = usize::MAX;
                    return false;
                }
                _ => {}
            }
        }
        if let Some(approval) = &self.approval {
            // Ctrl+C aborts and cancels according to existing Abort
            // semantics. The bare-letter decisions (y/n/q/r/s) require no
            // modifiers so Ctrl/Alt combinations cannot trigger them. Esc
            // still rejects.
            let unmodified = key.modifiers.is_empty();
            let decision = match key.code {
                KeyCode::Char('c') if control => Some(Decision::Abort),
                KeyCode::Char('y') if unmodified => Some(Decision::Approve),
                KeyCode::Char('n') if unmodified => Some(Decision::Reject),
                KeyCode::Esc => Some(Decision::Reject),
                KeyCode::Char('q') if unmodified => Some(Decision::Abort),
                KeyCode::Char('r') if unmodified && approval.workflow => Some(Decision::Retry),
                KeyCode::Char('s') if unmodified && approval.workflow => Some(Decision::Skip),
                _ => None,
            };
            if let Some(decision) = decision {
                if decision == Decision::Abort {
                    if let Some(busy) = &self.busy {
                        busy.cancel.cancel();
                    }
                }
                let _ = self.approval.take().unwrap().reply.send(decision);
            }
            return false;
        }
        if self.help {
            // Help is a modal: Esc/q/F(1) close it; Ctrl+C likewise, leaving
            // any in-flight run alone. Other keys are absorbed so they do
            // not leak into the chat input or the input history.
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') | KeyCode::F(1)
            ) && (key.code == KeyCode::Esc
                || key.code == KeyCode::Char('q')
                || control
                || key.code == KeyCode::F(1))
            {
                self.help = false;
            }
            return false;
        }
        if control && key.code == KeyCode::Char('c') {
            self.cancel_active_run();
            return false;
        }
        match key.code {
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.input.insert("\n")
            }
            KeyCode::Enter => return true,
            KeyCode::Char('j') if control => self.input.insert("\n"),
            KeyCode::Char('d') if control && self.input.text.is_empty() => self.quit = true,
            KeyCode::Char('u') if control => {
                self.input.take();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.insert(&c.to_string())
            }
            KeyCode::Backspace if control => self.input.delete_word_backward(),
            KeyCode::Char('h') if control => self.input.delete_word_backward(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left if control => self.input.word_left(),
            KeyCode::Right if control => self.input.word_right(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home if control => self.scroll = usize::MAX,
            KeyCode::End if control => self.scroll = 0,
            KeyCode::Home => {
                self.input.cursor = self.input.text[..self.input.cursor]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0)
            }
            KeyCode::End => {
                self.input.cursor += self.input.text[self.input.cursor..]
                    .find('\n')
                    .unwrap_or(self.input.text.len() - self.input.cursor)
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up if self.history_index > 0 => {
                self.history_index -= 1;
                self.input
                    .set(self.input_history[self.history_index].clone());
            }
            KeyCode::Down => {
                self.history_index = (self.history_index + 1).min(self.input_history.len());
                self.input.set(
                    self.input_history
                        .get(self.history_index)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            KeyCode::F(1) => {
                self.help = true;
                self.overlay_scroll = 0;
            }
            _ => {}
        }
        false
    }

    // -----------------------------------------------------------------
    // Activity spine
    //
    // The renderer/commands layers are not yet wired to activities; this
    // scope only owns the data model. Methods on this block are pure: they
    // read from the spine without touching the terminal, and they accept
    // the input they need explicitly. The only mutating entry point is
    // `handle_activity_event`, which is what `event()` routes
    // `UiEvent::Activity` to. Every pub fn in this section carries
    // `#[allow(dead_code)]` until the renderer and commands layers consume
    // the spine in the follow-up waves; the tests in this file's
    // `#[cfg(test)] mod tests` already exercise the surface end-to-end.
    // -----------------------------------------------------------------

    /// Insert a Start, record an End, or fold a pending End into a Start.
    /// Unknown/misordered events are fail-safe and never panic.
    #[allow(dead_code)]
    fn handle_activity_event(&mut self, event: ActivityEvent) {
        match event.phase {
            ActivityPhase::Start => self.activity_start(event),
            ActivityPhase::End => self.activity_end(event),
        }
        // Events can only add rows today, but a future eviction (or an
        // out-of-order replay) must not leave focus pointing at a vanished
        // id. No-op unless the spine owns focus.
        self.normalize_activity_focus();
    }

    #[allow(dead_code)]
    fn activity_start(&mut self, event: ActivityEvent) {
        let id = event.id.clone();
        // Duplicate Start: the producer either retried or replayed. Skip the
        // duplicate insert (no second timeline row), but still drain any
        // pending End that arrived earlier.
        if self.activity_index.contains_key(&id) {
            self.apply_pending_end(&id);
            return;
        }
        let index = self.activities.len();
        self.activities.push(ActivityNode {
            start: event.clone(),
            status: None,
            expanded: false,
            revision: 0,
        });
        self.activity_index.insert(id.clone(), index);
        self.register_activity_owner(&event);
        self.timeline.push(TimelineItem::Activity(id.clone()));
        self.apply_pending_end(&id);
    }

    #[allow(dead_code)]
    fn activity_end(&mut self, event: ActivityEvent) {
        let id = event.id.clone();
        let status = event.status.unwrap_or(ActivityStatus::Error);
        let Some(&index) = self.activity_index.get(&id) else {
            // End arrived before its Start. Park it so the eventual Start
            // can apply the final status. Nothing else changes: no node, no
            // timeline row, no indexes.
            self.pending_activity_ends.insert(id, status);
            return;
        };
        self.activities[index].status = Some(status);
        self.activities[index].revision = self.activities[index].revision.saturating_add(1);
    }

    #[allow(dead_code)]
    fn apply_pending_end(&mut self, id: &str) {
        if let Some(status) = self.pending_activity_ends.remove(id) {
            if let Some(&index) = self.activity_index.get(id) {
                self.activities[index].status = Some(status);
                self.activities[index].revision = self.activities[index].revision.saturating_add(1);
            }
        }
    }

    /// Bookkeeping for Subagent/WorkflowStep (one running record per
    /// `context`) and Tool (keyed by `(context, external_id)`). Records
    /// without an external_id and without a `context` kind that needs
    /// tracking are simply not registered.
    #[allow(dead_code)]
    fn register_activity_owner(&mut self, event: &ActivityEvent) {
        match event.kind {
            ActivityKind::Subagent | ActivityKind::WorkflowStep => {
                self.activity_by_context
                    .insert(event.context.clone(), event.id.clone());
            }
            ActivityKind::Tool => {
                if let Some(external) = event.external_id.as_ref() {
                    self.activity_by_external
                        .insert((event.context.clone(), external.clone()), event.id.clone());
                }
            }
        }
    }

    /// Pure lookup: id -> node. Returns `None` for unknown ids so callers can
    /// decide whether missing means "never seen" or "already evicted".
    #[allow(dead_code)]
    pub fn activity(&self, id: &str) -> Option<&ActivityNode> {
        self.activity_index.get(id).map(|&i| &self.activities[i])
    }

    /// Pure lookup: Subagent/WorkflowStep owner of a `context`. Tool
    /// activities are not registered here.
    #[allow(dead_code)]
    pub fn activity_by_context(&self, context: &str) -> Option<&ActivityNode> {
        self.activity_by_context
            .get(context)
            .and_then(|id| self.activity(id))
    }

    /// Pure lookup: Tool activity keyed by `(context, external_id)`.
    #[allow(dead_code)]
    pub fn activity_by_external(&self, context: &str, external_id: &str) -> Option<&ActivityNode> {
        self.activity_by_external
            .get(&(context.to_owned(), external_id.to_owned()))
            .and_then(|id| self.activity(id))
    }

    /// Toggle or set the expansion flag and bump `revision` so any cached
    /// renderer invalidates. `expanded = None` flips the current value;
    /// `Some(v)` forces it. Missing ids are no-ops.
    #[allow(dead_code)]
    pub fn set_activity_expanded(&mut self, id: &str, expanded: Option<bool>) {
        let Some(&index) = self.activity_index.get(id) else {
            return;
        };
        let next = match expanded {
            Some(value) => value,
            None => !self.activities[index].expanded,
        };
        if self.activities[index].expanded != next {
            self.activities[index].expanded = next;
            self.activities[index].revision = self.activities[index].revision.saturating_add(1);
        }
        // Collapsing can hide the focused descendant; re-home focus onto the
        // node that was just closed. Expansion never moves the transcript or
        // rebuilds the entry cache (neither `scroll` nor `history_generation`
        // is touched here).
        self.normalize_activity_focus();
    }

    /// Convenience wrapper around `set_activity_expanded(id, None)`.
    #[allow(dead_code)]
    pub fn toggle_activity_expanded(&mut self, id: &str) {
        self.set_activity_expanded(id, None);
    }

    /// Number of ancestors before reaching a root or a cycle. Roots and
    /// orphans (unknown parent) return 0; cycle members stop counting when
    /// they revisit themselves, which makes the depth finite and the
    /// visibility check below safe.
    #[allow(dead_code)]
    pub fn activity_depth(&self, id: &str) -> usize {
        let Some(node) = self.activity(id) else {
            return 0;
        };
        let mut visited = BTreeSet::from_iter([id.to_owned()]);
        let mut depth = 0;
        let mut cursor = node.start.parent_id.clone();
        while let Some(parent_id) = cursor {
            // Cycle detected: stop without counting the revisit. An unknown
            // parent behaves like a root from the depth standpoint.
            if !visited.insert(parent_id.clone()) {
                break;
            }
            let Some(parent) = self.activity(&parent_id) else {
                break;
            };
            depth += 1;
            cursor = parent.start.parent_id.clone();
        }
        depth
    }

    /// `true` when every ancestor along the parent chain is expanded. Roots
    /// return `true` (they have no ancestor to be hidden behind). Unknown
    /// parents stop the walk — an orphan behaves like a root from the
    /// visibility standpoint so the node still shows. Cycles are
    /// deterministic: when a parent has already been seen on this walk the
    /// cycle is treated as "already accounted for" and the function returns
    /// `true` (the whole cycle is visible unless one of its members was
    /// collapsed *before* the revisit, which would have short-circuited the
    /// walk earlier). This guarantees finiteness without infinite loops.
    #[allow(dead_code)]
    pub fn all_ancestors_expanded(&self, id: &str) -> bool {
        let Some(node) = self.activity(id) else {
            return true;
        };
        let mut visited = BTreeSet::from_iter([id.to_owned()]);
        let mut cursor = node.start.parent_id.clone();
        while let Some(parent_id) = cursor {
            if !visited.insert(parent_id.clone()) {
                return true;
            }
            match self.activity(&parent_id) {
                Some(parent) if !parent.expanded => return false,
                Some(parent) => cursor = parent.start.parent_id.clone(),
                None => return true, // orphan parent: visible like a root
            }
        }
        true
    }

    /// Ids of every activity whose node is currently visible, in timeline
    /// arrival order. Roots appear unconditionally; nested activities only
    /// when every ancestor is expanded.
    #[allow(dead_code)]
    pub fn visible_activity_ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        for item in &self.timeline {
            let TimelineItem::Activity(id) = item else {
                continue;
            };
            if self
                .activity(id)
                .is_some_and(|_| self.all_ancestors_expanded(id))
            {
                out.push(id.clone());
            }
        }
        out
    }

    /// `true` when the timeline item at `index` is currently shown: unowned
    /// entries always show; entries owned by an activity show only when the
    /// owner is expanded and every ancestor of the owner is expanded. The
    /// owner reference is captured when the entry is created and survives
    /// streaming replacements, so the same `activity_id` continues to gate
    /// the entry's visibility for the life of the transcript.
    #[allow(dead_code)]
    pub fn entry_is_visible(&self, index: usize) -> bool {
        let Some(owner_id) = self
            .entries
            .get(index)
            .and_then(|e| e.activity_id.as_deref())
        else {
            return true;
        };
        let Some(node) = self.activity(owner_id) else {
            // Owner recorded but no node: treat as orphaned detail, hidden.
            return false;
        };
        node.expanded && self.all_ancestors_expanded(owner_id)
    }

    /// Count of timeline items (activities + owned entries) that have `id`
    /// somewhere in their ancestor chain. Used by the summary line's
    /// `(+N)` trailing badge so a collapsed root reports how many items are
    /// hidden behind it. The node itself is excluded; unknown descendants
    /// stop the walk (orphan parents can't see into a phantom tree).
    #[allow(dead_code)]
    pub fn activity_descendant_count(&self, id: &str) -> usize {
        let mut count = 0usize;
        for item in &self.timeline {
            match item {
                TimelineItem::Activity(other) => {
                    if other.as_str() != id && self.is_ancestor(id, other) {
                        count += 1;
                    }
                }
                TimelineItem::Entry(index) => {
                    if let Some(owner) = self
                        .entries
                        .get(*index)
                        .and_then(|e| e.activity_id.as_deref())
                    {
                        if owner != id && self.is_ancestor(id, owner) {
                            count += 1;
                        }
                    }
                }
            }
        }
        count
    }

    /// `true` when `ancestor` is an ancestor of `descendant` (or the same
    /// node). Cycle-safe via a `BTreeSet`; an unknown parent stops the walk
    /// so phantom chains never report false positives.
    #[allow(dead_code)]
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        if ancestor == descendant {
            return true;
        }
        let mut visited = BTreeSet::from_iter([descendant.to_owned()]);
        let mut cursor = self
            .activity(descendant)
            .and_then(|node| node.start.parent_id.clone());
        while let Some(parent_id) = cursor {
            if !visited.insert(parent_id.clone()) {
                return false;
            }
            if parent_id == ancestor {
                return true;
            }
            match self.activity(&parent_id) {
                Some(parent) => cursor = parent.start.parent_id.clone(),
                None => return false,
            }
        }
        false
    }

    /// Apply Cancelled to every node whose Start has no matching End in the
    /// provided event list. Mirrors `session::unmatched_activity_starts` so
    /// resumed sessions show the user that a tool/subagent/workflow never
    /// finished. End timeline rows are never emitted: cancellation is a
    /// status patch, not a new event.
    #[allow(dead_code)]
    pub fn apply_recovered_unmatched(&mut self, events: &[ActivityEvent]) {
        let mut end_ids: BTreeSet<&str> = BTreeSet::new();
        let mut unmatched: Vec<&str> = Vec::new();
        // Single linear pass: track every End we've seen and any Start whose
        // id never produced a matching End. Borrowing events for both sets
        // keeps the function allocation-light and avoids cloning ids.
        for event in events {
            match event.phase {
                ActivityPhase::End => {
                    end_ids.insert(event.id.as_str());
                }
                ActivityPhase::Start => {
                    unmatched.push(event.id.as_str());
                }
            }
        }
        unmatched.retain(|id| !end_ids.contains(id));
        for id in unmatched {
            let Some(&index) = self.activity_index.get(id) else {
                continue;
            };
            if self.activities[index].status.is_some() {
                continue;
            }
            self.activities[index].status = Some(ActivityStatus::Cancelled);
            self.activities[index].revision = self.activities[index].revision.saturating_add(1);
        }
    }

    /// Build the per-render layout snapshot. Allocation-bounded: every
    /// vector is sized to `self.activities.len()` and the work walks the
    /// parent chain by index (no `String` clones, no per-activity
    /// `BTreeSet`). The depth, ancestor-visibility, and descendant walks
    /// all share one `visited` stamp buffer sized to the activity count,
    /// so each walk makes at most `activities.len()` index hops and a
    /// malformed cycle or orphan parent cannot spin forever.
    pub fn layout_snapshot(&self) -> LayoutSnapshot {
        let n = self.activities.len();
        let mut depths = vec![0u8; n];
        let mut ancestor_visible = vec![true; n];
        // One stamp buffer reused by every walk: a node counts as "seen
        // on this walk" when its slot equals the current stamp. This
        // replaces the previous per-activity `vec![false; n]` / BTreeSet
        // allocation with a single `O(n)` buffer and no heap traffic
        // inside the loops.
        let mut visited = vec![0usize; n];
        let mut stamp = 0usize;
        for index in 0..n {
            stamp += 1;
            depths[index] = self.compute_activity_depth(index, &mut visited, stamp);
            stamp += 1;
            ancestor_visible[index] = self.compute_ancestor_visible(index, &mut visited, stamp);
        }
        // Descendant counts: walk the timeline once. For each row, walk
        // the parent chain from the row's owning activity (or the row's
        // own activity id) and tally one on every ancestor visited.
        // Self is excluded. Cycles terminate at the first revisit, and
        // each tally makes at most `activities.len()` index hops because
        // `visited` is sized to the activity count.
        let mut descendant_counts = vec![0u32; n];
        for item in &self.timeline {
            let owner_id: Option<&str> = match item {
                TimelineItem::Activity(id) => Some(id.as_str()),
                TimelineItem::Entry(entry_index) => self
                    .entries
                    .get(*entry_index)
                    .and_then(|e| e.activity_id.as_deref()),
            };
            let Some(owner_id) = owner_id else {
                continue;
            };
            let Some(&start) = self.activity_index.get(owner_id) else {
                continue;
            };
            stamp += 1;
            self.tally_descendants(start, &mut descendant_counts, &mut visited, stamp);
        }
        LayoutSnapshot {
            depths,
            ancestor_visible,
            descendant_counts,
        }
    }

    /// Walk the parent chain from `start` and bump every ancestor's
    /// descendant counter by one. `start` itself is excluded so the
    /// count is strictly descendants (matches the contract pinned by
    /// `activity_descendant_count`'s tests). Cycles short-circuit at the
    /// first revisit; orphan parents break the walk. Bound: at most
    /// `activities.len()` hops per call because every hop marks a new
    /// slot in `visited`.
    fn tally_descendants(
        &self,
        start: usize,
        counts: &mut [u32],
        visited: &mut [usize],
        stamp: usize,
    ) {
        let mut cursor = start;
        visited[cursor] = stamp;
        loop {
            let parent_id = match self.activities[cursor].start.parent_id.as_deref() {
                Some(id) => id,
                None => return,
            };
            let parent_idx = match self.activity_index.get(parent_id).copied() {
                Some(idx) => idx,
                None => return,
            };
            // Detect a revisit before bumping, so a self-loop (`a` -> `a`)
            // never bumps `a` for itself and a longer cycle stops at the
            // first repeated member.
            if visited[parent_idx] == stamp {
                return;
            }
            visited[parent_idx] = stamp;
            counts[parent_idx] = counts[parent_idx].saturating_add(1);
            cursor = parent_idx;
        }
    }

    /// Walk the parent chain from `index` and count the distinct
    /// ancestors visited before a root, an orphan parent, or a revisit.
    /// Roots and orphans return 0; a 2-cycle reports 1 for each member
    /// (the other node is a real ancestor, counted once). Bound: at most
    /// `activities.len()` hops because every hop marks a new slot in
    /// `visited`.
    fn compute_activity_depth(&self, index: usize, visited: &mut [usize], stamp: usize) -> u8 {
        let mut cursor = index;
        visited[cursor] = stamp;
        let mut depth = 0u8;
        loop {
            let parent_id = match self.activities[cursor].start.parent_id.as_deref() {
                Some(id) => id,
                None => return depth, // root
            };
            let parent_idx = match self.activity_index.get(parent_id).copied() {
                Some(idx) => idx,
                None => return depth, // orphan parent behaves like a root
            };
            if visited[parent_idx] == stamp {
                // Cycle: the revisit is not counted, matching the
                // `activity_depth` BTreeSet walk.
                return depth;
            }
            visited[parent_idx] = stamp;
            depth = depth.saturating_add(1);
            cursor = parent_idx;
        }
    }

    /// Returns `true` when every parent in the chain is expanded.
    /// Cycles terminate at the first revisit (index-based); orphan
    /// parents stop the walk and the activity is treated as a root
    /// (visible). Bound: at most `activities.len()` hops because every
    /// hop marks a new slot in `visited`.
    fn compute_ancestor_visible(&self, index: usize, visited: &mut [usize], stamp: usize) -> bool {
        let mut cursor = index;
        // Mark the activity itself first: a self-loop (`a` -> `a`) is a
        // degenerate cycle that must not loop forever.
        visited[cursor] = stamp;
        loop {
            let parent_id = match self.activities[cursor].start.parent_id.as_deref() {
                Some(id) => id,
                None => return true, // ran off the top: chain is visible
            };
            let parent_idx = match self.activity_index.get(parent_id).copied() {
                Some(idx) => idx,
                None => return true, // orphan parent: visible
            };
            if visited[parent_idx] == stamp {
                // Cycle: every parent on the visited walk was expanded
                // (we got here) or the chain was hidden earlier. Treat
                // the cycle as visible, matching `all_ancestors_expanded`.
                return true;
            }
            if !self.activities[parent_idx].expanded {
                return false;
            }
            visited[parent_idx] = stamp;
            cursor = parent_idx;
        }
    }

    /// Depth lookup by id, backed by the snapshot. Returns 0 when the
    /// snapshot has no entry for the id (unknown / never inserted).
    pub fn depth_for(&self, snapshot: &LayoutSnapshot, id: &str) -> u8 {
        self.activity_index
            .get(id)
            .map(|&i| snapshot.depths.get(i).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    /// Ancestor-visibility lookup by id, backed by the snapshot.
    /// Returns `true` when the snapshot has no entry for the id, which
    /// matches the root convention used everywhere else.
    pub fn ancestor_visible_for(&self, snapshot: &LayoutSnapshot, id: &str) -> bool {
        self.activity_index
            .get(id)
            .map(|&i| snapshot.ancestor_visible.get(i).copied().unwrap_or(true))
            .unwrap_or(true)
    }

    /// Descendant-count lookup by id, backed by the snapshot.
    /// Returns 0 when the snapshot has no entry for the id.
    pub fn descendant_count_for(&self, snapshot: &LayoutSnapshot, id: &str) -> u32 {
        self.activity_index
            .get(id)
            .map(|&i| snapshot.descendant_counts.get(i).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    /// Sanitize a malformed activity id coming from a producer.
    /// Control characters and bare CR/LF are folded to `-`; runs of
    /// whitespace collapse; trailing whitespace is dropped. Length is
    /// capped at 120 characters (the same cap the legacy tool title
    /// uses) and an empty id becomes `"activity"`. The original id is
    /// never mutated in place — a fresh `String` is returned so the
    /// caller can keep the malformed one in logs or error context.
    pub fn sanitize_activity_id(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        let mut pending_dash = false;
        for character in raw.chars() {
            if crate::text::is_unsafe_terminal_char(character) || character == ' ' {
                pending_dash = true;
                continue;
            }
            if pending_dash && !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
            pending_dash = false;
            out.push(character);
        }
        while out.ends_with('-') {
            out.pop();
        }
        if out.is_empty() {
            return "activity".to_owned();
        }
        if out.chars().count() > 120 {
            let mut truncated: String = out.chars().take(117).collect();
            truncated.push_str("...");
            return truncated;
        }
        out
    }
}

/// Decide which `ActivityStatus` a tool-result payload represents. The
/// provider surface often double-encodes content (raw JSON, then a string
/// holding that JSON, then the model-history wrapper), so we unwrap
/// string-encoded JSON up to 3 levels before looking at structure.
/// Top-level `status: "denied" | "cancelled"` (or the same fields nested
/// under `error` / `content` / `stdout`) maps to the matching activity
/// status; an `error` field with no explicit status maps to `Error`;
/// anything else is `Success`. Display-only — never feeds back into model
/// history.
fn classify_tool_result_status(content: &str) -> ActivityStatus {
    let mut value: serde_json::Value = serde_json::from_str(content)
        .unwrap_or_else(|_| serde_json::Value::String(content.to_owned()));
    for _ in 0..3 {
        match value {
            serde_json::Value::String(ref text) => {
                match serde_json::from_str::<serde_json::Value>(text) {
                    Ok(parsed) => value = parsed,
                    Err(_) => break,
                }
            }
            _ => break,
        }
    }
    if let Some(status) = classify_value_status(&value) {
        return status;
    }
    if classify_value_is_error(&value) {
        ActivityStatus::Error
    } else {
        ActivityStatus::Success
    }
}

/// Walk the JSON for an explicit `status` field on any object, including
/// the `denied` / `cancelled` keywords. Returns `None` when nothing
/// explicit shows up so the caller can fall back to the
/// error-or-success heuristic.
fn classify_value_status(value: &serde_json::Value) -> Option<ActivityStatus> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(status) = map
                .get("status")
                .and_then(|v| v.as_str())
                .and_then(status_from_str)
            {
                return Some(status);
            }
            for key in ["error", "content", "stdout"] {
                if let Some(inner) = map.get(key) {
                    if let Some(status) = classify_value_status(inner) {
                        return Some(status);
                    }
                }
            }
            None
        }
        serde_json::Value::Array(items) => items.iter().find_map(classify_value_status),
        _ => None,
    }
}

fn status_from_str(raw: &str) -> Option<ActivityStatus> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "denied" | "rejected" => Some(ActivityStatus::Denied),
        "cancelled" | "canceled" | "aborted" => Some(ActivityStatus::Cancelled),
        "error" | "failed" | "failure" => Some(ActivityStatus::Error),
        "success" | "ok" | "succeeded" => Some(ActivityStatus::Success),
        _ => None,
    }
}

fn classify_value_is_error(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("error").is_some_and(|v| !v.is_null()) {
                return true;
            }
            for key in ["content", "stdout"] {
                if let Some(inner) = map.get(key) {
                    if classify_value_is_error(inner) {
                        return true;
                    }
                }
            }
            false
        }
        serde_json::Value::Array(items) => items.iter().any(classify_value_is_error),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_ids_remove_terminal_unsafe_format_chars_but_keep_joiners() {
        let id = "left\u{202e}right\u{200b}\u{2028}family: 👨‍👩‍👧‍👦 می\u{200c}رود";

        let sanitized = App::sanitize_activity_id(id);

        assert_eq!(sanitized, "left-right-family:-👨‍👩‍👧‍👦-می\u{200c}رود");
        assert!(!sanitized.chars().any(crate::text::is_unsafe_terminal_char));
    }
    use crate::model::{ActivityEvent, ActivityKind, ActivityPhase, ActivityStatus};
    use std::io::Write;

    fn start(
        id: &str,
        parent: Option<&str>,
        context: &str,
        kind: ActivityKind,
        external: Option<&str>,
    ) -> ActivityEvent {
        ActivityEvent {
            id: id.into(),
            parent_id: parent.map(str::to_owned),
            context: context.into(),
            kind,
            phase: ActivityPhase::Start,
            title: format!("{id} title"),
            external_id: external.map(str::to_owned),
            status: None,
        }
    }

    fn end(id: &str, context: &str, status: Option<ActivityStatus>) -> ActivityEvent {
        ActivityEvent {
            id: id.into(),
            parent_id: None,
            context: context.into(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::End,
            title: format!("{id} end"),
            external_id: None,
            status,
        }
    }

    fn fresh_app() -> App {
        App::new(&Config::default(), Selection::default())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn activity_app(ids: &[&str]) -> App {
        let mut app = fresh_app();
        for id in ids {
            app.event(UiEvent::Activity(start(
                id,
                None,
                "main",
                ActivityKind::Tool,
                None,
            )));
        }
        app
    }

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 7,
            row: 8,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn status_updates_global_header_only_for_main_context() {
        let mut app = fresh_app();

        app.event(UiEvent::Status {
            context: "main".into(),
            text: "main status".into(),
        });
        assert_eq!(app.status, "main status");

        app.event(UiEvent::Status {
            context: "subagent.plan".into(),
            text: "child status".into(),
        });
        assert_eq!(app.status, "main status");

        app.event(UiEvent::Status {
            context: "workflow.review".into(),
            text: "workflow status".into(),
        });
        assert_eq!(app.status, "main status");
    }

    #[test]
    fn model_updates_global_header_only_for_main_context() {
        let mut app = fresh_app();

        app.event(UiEvent::Model {
            context: "main".into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            effort: Some(crate::config::Effort::High),
        });
        assert_eq!(app.model_label, "openai:gpt-5");
        assert_eq!(app.effort_label, "high");
        assert_eq!(app.status, "Generating | main");

        app.event(UiEvent::Model {
            context: "subagent.plan".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet".into(),
            effort: Some(crate::config::Effort::Low),
        });
        assert_eq!(app.model_label, "openai:gpt-5");
        assert_eq!(app.effort_label, "high");
        assert_eq!(app.status, "Generating | main");

        app.event(UiEvent::Model {
            context: "workflow.review".into(),
            provider: "openrouter".into(),
            model: "review-model".into(),
            effort: None,
        });
        assert_eq!(app.model_label, "openai:gpt-5");
        assert_eq!(app.effort_label, "high");
        assert_eq!(app.status, "Generating | main");
    }

    #[test]
    fn activity_events_update_child_rows_without_changing_global_header() {
        let mut app = fresh_app();
        app.event(UiEvent::Status {
            context: "main".into(),
            text: "main status".into(),
        });
        app.event(UiEvent::Model {
            context: "main".into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            effort: Some(crate::config::Effort::Medium),
        });
        let header = (
            app.status.clone(),
            app.model_label.clone(),
            app.effort_label.clone(),
        );

        app.event(UiEvent::Activity(start(
            "plan",
            None,
            "subagent.plan",
            ActivityKind::Subagent,
            None,
        )));
        assert_eq!(
            app.activity_by_context.get("subagent.plan"),
            Some(&"plan".into())
        );
        assert_eq!(app.activity("plan").unwrap().status, None);
        assert_eq!(
            (
                app.status.clone(),
                app.model_label.clone(),
                app.effort_label.clone()
            ),
            header
        );

        app.event(UiEvent::Activity(end(
            "plan",
            "subagent.plan",
            Some(ActivityStatus::Success),
        )));
        assert_eq!(
            app.activity("plan").and_then(|activity| activity.status),
            Some(ActivityStatus::Success)
        );
        assert_eq!((app.status, app.model_label, app.effort_label), header);
    }

    #[test]
    fn left_click_focuses_and_toggles_activity_without_touching_composer_queue_or_scroll() {
        let mut app = activity_app(&["activity"]);
        app.input.insert("draft");
        app.queued_inputs.push_back("queued message".into());
        app.scroll = 17;
        let before = (
            app.input.text.clone(),
            app.queued_inputs.clone(),
            app.scroll,
        );

        app.handle_mouse_with_activity_target(
            mouse(MouseEventKind::Down(MouseButton::Left)),
            Some("activity".into()),
        );
        assert_eq!(app.focus, Focus::Activity);
        assert_eq!(app.focused_activity_id(), Some("activity"));
        assert!(app.activity("activity").unwrap().expanded);
        assert_eq!(
            (
                app.input.text.clone(),
                app.queued_inputs.clone(),
                app.scroll
            ),
            before
        );

        app.handle_mouse_with_activity_target(
            mouse(MouseEventKind::Down(MouseButton::Left)),
            Some("activity".into()),
        );
        assert!(!app.activity("activity").unwrap().expanded);
        assert_eq!((app.input.text, app.queued_inputs, app.scroll), before);
    }

    #[test]
    fn invalid_or_non_left_mouse_activity_targets_are_ignored() {
        let mut app = activity_app(&["parent"]);
        app.event(UiEvent::Activity(start(
            "child",
            Some("parent"),
            "child",
            ActivityKind::Tool,
            None,
        )));
        app.scroll = 9;
        let before = (app.focus, app.focused_activity.clone(), app.scroll);
        for (kind, target) in [
            (MouseEventKind::Down(MouseButton::Left), Some("unknown")),
            (MouseEventKind::Down(MouseButton::Left), Some("child")),
            (MouseEventKind::Down(MouseButton::Left), None),
            (MouseEventKind::Down(MouseButton::Right), Some("parent")),
            (MouseEventKind::Drag(MouseButton::Left), Some("parent")),
            (MouseEventKind::Moved, Some("parent")),
        ] {
            app.handle_mouse_with_activity_target(mouse(kind), target.map(str::to_owned));
        }
        assert_eq!(
            (app.focus, app.focused_activity.clone(), app.scroll),
            before
        );
        assert!(!app.activity("parent").unwrap().expanded);
    }

    #[test]
    fn modal_and_picker_states_suppress_activity_clicks() {
        let mut app = activity_app(&["activity"]);
        for state in 0..4 {
            match state {
                0 => app.help = true,
                1 => app.workflow_complete = true,
                2 => {
                    let (reply, _receiver) = tokio::sync::oneshot::channel();
                    app.approval = Some(Approval {
                        title: "Approval".into(),
                        detail: "Confirm".into(),
                        workflow: false,
                        reply,
                    });
                }
                _ => {
                    app.picker = Some(Picker::models(
                        &Config::default(),
                        &Config::default().model,
                        None,
                    ));
                }
            }
            app.handle_mouse_with_activity_target(
                mouse(MouseEventKind::Down(MouseButton::Left)),
                Some("activity".into()),
            );
            assert!(!app.activity("activity").unwrap().expanded);
            app.help = false;
            app.workflow_complete = false;
            app.approval = None;
            app.picker = None;
        }
    }

    #[test]
    fn mouse_wheel_compatibility_wrapper_and_targeted_method_keep_routing_unchanged() {
        let mut wrapper = fresh_app();
        let mut targeted = fresh_app();
        let wheel_up = mouse(MouseEventKind::ScrollUp);
        let wheel_down = mouse(MouseEventKind::ScrollDown);
        wrapper.handle_mouse(wheel_up);
        targeted.handle_mouse_with_activity_target(wheel_up, Some("missing".into()));
        wrapper.handle_mouse(wheel_down);
        targeted.handle_mouse_with_activity_target(wheel_down, Some("missing".into()));
        assert_eq!(wrapper.scroll, targeted.scroll);
        assert_eq!(wrapper.overlay_scroll, targeted.overlay_scroll);
    }

    #[test]
    fn disabled_mouse_ignores_wheel_and_activity_click_until_reenabled() {
        let mut app = activity_app(&["activity"]);
        let wheel = mouse(MouseEventKind::ScrollUp);
        app.scroll = 4;
        app.mouse_enabled = false;

        app.handle_mouse(wheel);
        app.handle_mouse_with_activity_target(
            mouse(MouseEventKind::Down(MouseButton::Left)),
            Some("activity".into()),
        );
        assert_eq!(app.scroll, 4);
        assert_eq!(app.focus, Focus::Input);
        assert!(!app.activity("activity").unwrap().expanded);

        app.mouse_enabled = true;
        app.handle_mouse(wheel);
        assert_eq!(app.scroll, 7);
        app.handle_mouse_with_activity_target(
            mouse(MouseEventKind::Down(MouseButton::Left)),
            Some("activity".into()),
        );
        assert_eq!(app.focus, Focus::Activity);
        assert!(app.activity("activity").unwrap().expanded);
    }

    #[test]
    fn paste_is_routed_only_to_the_active_input_surface() {
        let mut app = fresh_app();
        app.input.insert("draft");

        app.paste(" input");
        assert_eq!(app.input.text, "draft input");

        app.focus = Focus::Activity;
        app.paste(" ignored");
        assert_eq!(app.input.text, "draft input");

        app.focus = Focus::Input;
        app.workflow_complete = true;
        app.paste(" ignored");
        assert_eq!(app.input.text, "draft input");

        app.workflow_complete = false;
        app.picker = Some(Picker::models(
            &Config::default(),
            &Config::default().model,
            None,
        ));
        app.paste("model");
        assert_eq!(app.picker.as_ref().unwrap().query.text, "model");
        assert_eq!(app.input.text, "draft input");
    }

    #[test]
    fn entering_workflow_complete_resets_scroll_only_on_a_new_transition() {
        let mut app = fresh_app();
        app.overlay_scroll = 23;
        app.enter_workflow_complete();
        assert_eq!(app.overlay_scroll, 0);

        app.overlay_scroll = 17;
        app.enter_workflow_complete();
        assert_eq!(app.overlay_scroll, 17);

        app.workflow_complete = false;
        app.overlay_scroll = 9;
        app.enter_workflow_complete();
        assert_eq!(app.overlay_scroll, 0);
    }

    #[tokio::test]
    async fn approval_ctrl_c_aborts_and_modified_decision_letters_are_ignored() {
        let (_dir, engine) = test_engine().await;
        let mut app = fresh_app();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<_, anyhow::Error>(String::new())
        });
        app.busy = Some(Busy {
            task,
            cancel: cancel.clone(),
        });
        let (reply, mut response) = oneshot::channel();
        app.approval = Some(Approval {
            title: "Approve".into(),
            detail: "Run command".into(),
            workflow: true,
            reply,
        });

        app.handle_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            &engine,
        )
        .await
        .unwrap();
        assert!(response.try_recv().is_err());
        assert!(app.approval.is_some());

        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &engine,
        )
        .await
        .unwrap();
        assert!(cancel.is_cancelled());
        assert_eq!(response.await.unwrap(), Decision::Abort);
        assert!(app.approval.is_none());
        app.cancel_and_join().await;
    }

    #[tokio::test]
    async fn approval_unmodified_letters_decide_and_modified_letters_do_not() {
        let (_dir, engine) = test_engine().await;
        for (letter, expected) in [
            ('y', Decision::Approve),
            ('n', Decision::Reject),
            ('q', Decision::Abort),
            ('r', Decision::Retry),
            ('s', Decision::Skip),
        ] {
            let mut app = fresh_app();
            let (reply, response) = oneshot::channel();
            app.approval = Some(Approval {
                title: "Approve".into(),
                detail: "Run command".into(),
                workflow: true,
                reply,
            });
            app.handle_key(key(KeyCode::Char(letter)), &engine)
                .await
                .unwrap();
            assert_eq!(response.await.unwrap(), expected);
        }

        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            let mut app = fresh_app();
            let (reply, mut response) = oneshot::channel();
            app.approval = Some(Approval {
                title: "Approve".into(),
                detail: "Run command".into(),
                workflow: true,
                reply,
            });
            app.handle_key(KeyEvent::new(KeyCode::Char('n'), modifiers), &engine)
                .await
                .unwrap();
            assert!(response.try_recv().is_err());
            assert!(app.approval.is_some());
        }
    }

    #[tokio::test]
    async fn workflow_complete_controls_require_unmodified_letters_and_esc_exits() {
        let (_dir, engine) = test_engine().await;
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            for letter in ['n', 'r', 'q'] {
                let mut app = fresh_app();
                app.workflow_complete = true;
                app.workflow_mode = Some("missing-workflow".into());
                app.handle_key(KeyEvent::new(KeyCode::Char(letter), modifiers), &engine)
                    .await
                    .unwrap();
                assert!(app.workflow_complete);
            }
        }

        let mut app = fresh_app();
        app.workflow_complete = true;
        app.workflow_mode = Some("workflow".into());
        app.handle_key(key(KeyCode::Char('n')), &engine)
            .await
            .unwrap();
        assert!(!app.workflow_complete);

        app.workflow_complete = true;
        app.workflow_mode = Some("workflow".into());
        app.handle_key(key(KeyCode::Esc), &engine).await.unwrap();
        assert!(!app.workflow_complete);
    }

    async fn test_engine() -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::open(dir.path(), Some("test")).unwrap();
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        (dir, Engine::new(Config::default(), session, events))
    }

    #[tokio::test]
    async fn f6_enters_newest_visible_activity_and_exits_preserving_id() {
        let (_dir, engine) = test_engine().await;
        let mut app = activity_app(&["oldest", "newest"]);

        assert!(!app.handle_key(key(KeyCode::F(6)), &engine).await.unwrap());
        assert_eq!(app.focus, Focus::Activity);
        assert_eq!(app.focused_activity_id(), Some("newest"));

        app.handle_key(key(KeyCode::Up), &engine).await.unwrap();
        assert_eq!(app.focused_activity_id(), Some("oldest"));
        app.handle_key(key(KeyCode::F(6)), &engine).await.unwrap();
        assert_eq!(app.focus, Focus::Input);
        app.handle_key(key(KeyCode::F(6)), &engine).await.unwrap();
        assert_eq!(app.focused_activity_id(), Some("oldest"));
    }

    #[tokio::test]
    async fn f6_reports_no_visible_activity_without_changing_focus() {
        let (_dir, engine) = test_engine().await;
        let mut app = fresh_app();

        app.handle_key(key(KeyCode::F(6)), &engine).await.unwrap();

        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.status, "No activity to focus");
    }

    #[test]
    fn activity_focus_movement_is_clamped_and_normalizes_missing_ids() {
        let mut app = activity_app(&["first", "second", "third"]);
        app.focus = Focus::Activity;
        app.focused_activity = Some("second".into());

        app.activity_key(key(KeyCode::Up));
        app.activity_key(key(KeyCode::Up));
        assert_eq!(app.focused_activity_id(), Some("first"));
        app.activity_key(key(KeyCode::Down));
        app.activity_key(key(KeyCode::Down));
        app.activity_key(key(KeyCode::Down));
        assert_eq!(app.focused_activity_id(), Some("third"));

        app.focused_activity = Some("missing".into());
        app.activity_key(key(KeyCode::Up));
        assert_eq!(app.focused_activity_id(), Some("first"));
        app.focused_activity = Some("deleted".into());
        app.normalize_activity_focus();
        assert_eq!(app.focused_activity_id(), Some("third"));

        app.activities.clear();
        app.activity_index.clear();
        app.timeline.clear();
        app.normalize_activity_focus();
        assert_eq!(app.focus, Focus::Input);
    }

    #[test]
    fn activity_focus_expansion_keys_and_parent_collapse_preserve_entry_view_state() {
        let mut app = fresh_app();
        app.event(UiEvent::Activity(start(
            "parent",
            None,
            "main",
            ActivityKind::Subagent,
            None,
        )));
        app.event(UiEvent::Activity(start(
            "child",
            Some("parent"),
            "child-context",
            ActivityKind::Tool,
            None,
        )));
        app.message("main".into(), Message::new("user", "draft context"));
        let entry_revision = app.entries[0].revision;
        let generation = app.history_generation;
        app.scroll = 23;
        app.focus = Focus::Activity;
        app.focused_activity = Some("parent".into());

        app.activity_key(key(KeyCode::Right));
        assert!(app.activity("parent").unwrap().expanded);
        app.activity_key(key(KeyCode::Enter));
        assert!(!app.activity("parent").unwrap().expanded);
        app.activity_key(key(KeyCode::Char(' ')));
        assert!(app.activity("parent").unwrap().expanded);
        app.activity_key(key(KeyCode::Left));
        assert!(!app.activity("parent").unwrap().expanded);
        assert_eq!(app.entries[0].revision, entry_revision);
        assert_eq!(app.history_generation, generation);
        assert_eq!(app.scroll, 23);

        app.set_activity_expanded("parent", Some(true));
        app.focused_activity = Some("child".into());
        app.set_activity_expanded("parent", Some(false));
        assert_eq!(app.focused_activity_id(), Some("parent"));
    }

    #[test]
    fn activity_focus_page_and_control_home_end_scroll_transcript() {
        let mut app = activity_app(&["activity"]);
        app.focus = Focus::Activity;
        app.focused_activity = Some("activity".into());
        app.scroll = 20;

        app.activity_key(key(KeyCode::PageUp));
        assert_eq!(app.scroll, 30);
        app.activity_key(key(KeyCode::PageDown));
        assert_eq!(app.scroll, 20);
        app.activity_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        assert_eq!(app.scroll, usize::MAX);
        app.activity_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 0);
    }

    #[tokio::test]
    async fn activity_focus_absorbs_composer_and_history_keys() {
        let (_dir, engine) = test_engine().await;
        let mut app = activity_app(&["activity"]);
        app.focus = Focus::Activity;
        app.focused_activity = Some("activity".into());
        app.input.insert("draft");
        app.input_history = vec!["previous".into()];
        app.history_index = 1;
        let before = (app.input.text.clone(), app.input.cursor, app.history_index);

        for event in [
            key(KeyCode::Char('x')),
            key(KeyCode::Backspace),
            key(KeyCode::Left),
            key(KeyCode::Right),
            key(KeyCode::Up),
            key(KeyCode::Down),
            key(KeyCode::PageUp),
            key(KeyCode::PageDown),
            key(KeyCode::Home),
            key(KeyCode::End),
        ] {
            app.handle_key(event, &engine).await.unwrap();
        }
        assert_eq!(
            (app.input.text, app.input.cursor, app.history_index),
            before
        );
    }

    #[tokio::test]
    async fn modals_and_workflow_tab_guard_have_priority_over_activity_focus() {
        let (_dir, engine) = test_engine().await;
        let mut app = activity_app(&["activity"]);
        app.focus = Focus::Activity;
        app.focused_activity = Some("activity".into());

        app.help = true;
        app.handle_key(key(KeyCode::F(6)), &engine).await.unwrap();
        assert_eq!(app.focus, Focus::Activity);
        app.help = false;
        app.workflow_complete = true;
        app.handle_key(key(KeyCode::F(6)), &engine).await.unwrap();
        assert_eq!(app.focus, Focus::Activity);
        app.workflow_complete = false;
        app.picker = Some(Picker::models(
            &Config::default(),
            &Config::default().model,
            None,
        ));
        app.handle_key(key(KeyCode::F(6)), &engine).await.unwrap();
        assert_eq!(app.focus, Focus::Activity);
        assert!(app.picker.is_some());

        app.picker = None;
        app.workflow_mode = Some("workflow".into());
        app.handle_key(key(KeyCode::Tab), &engine).await.unwrap();
        assert_eq!(app.focused_activity_id(), Some("activity"));
        assert!(app.status.contains("Agent cycling disabled"));
    }

    #[tokio::test]
    async fn esc_returns_from_activity_focus_without_cancelling_active_run() {
        let (_dir, engine) = test_engine().await;
        let mut app = activity_app(&["activity"]);
        app.focus = Focus::Activity;
        app.focused_activity = Some("activity".into());
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(async { Ok::<_, anyhow::Error>(String::new()) });
        app.busy = Some(Busy {
            task,
            cancel: cancel.clone(),
        });

        app.handle_key(key(KeyCode::Esc), &engine).await.unwrap();

        assert_eq!(app.focus, Focus::Input);
        assert!(!cancel.is_cancelled());
    }

    #[tokio::test]
    async fn ctrl_c_while_activity_focused_keeps_existing_run_cancel_policy() {
        let (_dir, engine) = test_engine().await;
        let mut app = activity_app(&["activity"]);
        app.focus = Focus::Activity;
        app.focused_activity = Some("activity".into());
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok::<_, anyhow::Error>(String::new())
        });
        app.busy = Some(Busy {
            task,
            cancel: cancel.clone(),
        });

        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &engine,
        )
        .await
        .unwrap();

        assert!(cancel.is_cancelled());
        assert_eq!(app.status, "Cancelling...");
        app.cancel_and_join().await;
    }

    #[test]
    fn activity_start_then_end_records_status_and_revision() {
        let mut app = fresh_app();
        app.event(UiEvent::Activity(start(
            "tool-1",
            None,
            "main",
            ActivityKind::Tool,
            Some("req-9"),
        )));
        let node = app.activity("tool-1").expect("node inserted");
        assert_eq!(node.status, None);
        assert!(!node.expanded);
        assert_eq!(node.revision, 0);
        // Exactly one timeline row, of the Activity variant, in arrival order.
        assert_eq!(app.timeline.len(), 1);
        assert!(matches!(&app.timeline[0], TimelineItem::Activity(id) if id == "tool-1"));

        app.event(UiEvent::Activity(end(
            "tool-1",
            "main",
            Some(ActivityStatus::Success),
        )));
        let node = app.activity("tool-1").unwrap();
        assert_eq!(node.status, Some(ActivityStatus::Success));
        // Revision bumps exactly once on End; the in-place update must not
        // append a second timeline row.
        assert_eq!(node.revision, 1);
        assert_eq!(app.timeline.len(), 1);
    }

    #[test]
    fn new_activity_node_is_collapsed_by_default() {
        let mut app = fresh_app();
        app.event(UiEvent::Activity(start(
            "tool-1",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        assert!(!app.activity("tool-1").unwrap().expanded);
        assert_eq!(
            app.visible_activity_ids(),
            vec!["tool-1".to_string()],
            "collapsed roots remain visible",
        );
    }

    #[test]
    fn end_before_start_queues_status_and_applies_on_start() {
        let mut app = fresh_app();
        // End arrives first: nothing should panic, no node, no timeline row.
        app.event(UiEvent::Activity(end(
            "tool-2",
            "main",
            Some(ActivityStatus::Denied),
        )));
        assert!(app.activity("tool-2").is_none());
        assert!(app.timeline.is_empty());
        assert_eq!(
            app.pending_activity_ends.get("tool-2"),
            Some(&ActivityStatus::Denied),
        );

        // The Start now arrives and immediately folds the pending End.
        app.event(UiEvent::Activity(start(
            "tool-2",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        let node = app.activity("tool-2").unwrap();
        assert_eq!(node.status, Some(ActivityStatus::Denied));
        assert_eq!(node.revision, 1);
        assert!(app.pending_activity_ends.is_empty());
        // Single timeline row for the Start; the End never produced one.
        assert_eq!(app.timeline.len(), 1);
        assert!(matches!(&app.timeline[0], TimelineItem::Activity(id) if id == "tool-2"));
    }

    #[test]
    fn duplicate_start_is_idempotent_and_does_not_duplicate_timeline() {
        let mut app = fresh_app();
        app.event(UiEvent::Activity(start(
            "tool-3",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        let revision_after_first = app.activity("tool-3").unwrap().revision;
        let timeline_len_after_first = app.timeline.len();

        // Second arrival: same id, no pending End. Fail-safe — must not
        // panic, must not push a duplicate timeline row, must not bump
        // revision (no End happened, no toggle happened).
        app.event(UiEvent::Activity(start(
            "tool-3",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        assert_eq!(
            app.activity("tool-3").unwrap().revision,
            revision_after_first
        );
        assert_eq!(app.timeline.len(), timeline_len_after_first);
        assert_eq!(app.activities.len(), 1);

        // An End that arrives after the duplicate Start finds the existing
        // node and updates it normally; the End does not push a timeline row.
        app.event(UiEvent::Activity(end(
            "tool-3",
            "main",
            Some(ActivityStatus::Success),
        )));
        assert_eq!(
            app.activity("tool-3").unwrap().status,
            Some(ActivityStatus::Success),
        );
        assert!(app.pending_activity_ends.is_empty());
        // Still exactly one node and exactly one Activity timeline row.
        assert_eq!(app.activities.len(), 1);
        let activity_rows = app
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::Activity(_)))
            .count();
        assert_eq!(activity_rows, 1);

        // Pending End paired with the matching Start folds in cleanly.
        // Park an End for an unknown id, then issue the Start: the pending
        // End must apply on insert.
        app.event(UiEvent::Activity(end(
            "tool-4",
            "main",
            Some(ActivityStatus::Denied),
        )));
        assert_eq!(
            app.pending_activity_ends.get("tool-4"),
            Some(&ActivityStatus::Denied),
        );
        app.event(UiEvent::Activity(start(
            "tool-4",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        assert_eq!(
            app.activity("tool-4").unwrap().status,
            Some(ActivityStatus::Denied),
        );
        assert!(app.pending_activity_ends.is_empty());
        assert_eq!(app.activities.len(), 2);

        // Duplicate Start of an id that already has a pending End drains
        // it without inserting a second node or pushing a second row.
        app.event(UiEvent::Activity(end(
            "tool-5",
            "main",
            Some(ActivityStatus::Cancelled),
        )));
        app.event(UiEvent::Activity(start(
            "tool-5",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        let timeline_before = app.timeline.len();
        app.event(UiEvent::Activity(start(
            "tool-5",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        assert_eq!(
            app.activity("tool-5").unwrap().status,
            Some(ActivityStatus::Cancelled),
        );
        assert_eq!(
            app.timeline.len(),
            timeline_before,
            "duplicate Start appended no row",
        );
    }

    #[test]
    fn parent_visibility_toggle_and_depth_obey_expansion() {
        let mut app = fresh_app();
        app.event(UiEvent::Activity(start(
            "sub",
            None,
            "main",
            ActivityKind::Subagent,
            None,
        )));
        app.event(UiEvent::Activity(start(
            "tool-a",
            Some("sub"),
            "main",
            ActivityKind::Tool,
            None,
        )));
        app.event(UiEvent::Activity(start(
            "tool-b",
            Some("sub"),
            "main",
            ActivityKind::Tool,
            None,
        )));
        app.event(UiEvent::Activity(start(
            "grandchild",
            Some("tool-a"),
            "main",
            ActivityKind::Tool,
            None,
        )));

        // Depth counts ancestors before reaching a root.
        assert_eq!(app.activity_depth("sub"), 0);
        assert_eq!(app.activity_depth("tool-a"), 1);
        assert_eq!(app.activity_depth("tool-b"), 1);
        assert_eq!(app.activity_depth("grandchild"), 2);
        // Orphan parent (unknown) returns 0 like a root.
        app.event(UiEvent::Activity(start(
            "orphan",
            Some("never-existed"),
            "main",
            ActivityKind::Tool,
            None,
        )));
        assert_eq!(app.activity_depth("orphan"), 0);
        assert!(app.all_ancestors_expanded("orphan"));

        // Default collapsed -> only roots are visible, in timeline arrival
        // order. `orphan` was inserted after the subagent tree, so it shows
        // up second.
        assert_eq!(
            app.visible_activity_ids(),
            vec!["sub".to_string(), "orphan".to_string()],
        );

        // Expand the subagent root -> all its direct descendants become
        // visible too, in the order they were inserted. grandchild stays
        // hidden because its parent tool-a is collapsed.
        app.set_activity_expanded("sub", Some(true));
        assert!(app.activity("sub").unwrap().expanded);
        assert_eq!(
            app.visible_activity_ids(),
            vec![
                "sub".to_string(),
                "tool-a".to_string(),
                "tool-b".to_string(),
                "orphan".to_string(),
            ],
            "grandchild stays hidden behind collapsed tool-a",
        );

        // Expand tool-a too; grandchild now shows.
        app.set_activity_expanded("tool-a", Some(true));
        assert_eq!(
            app.visible_activity_ids(),
            vec![
                "sub".to_string(),
                "tool-a".to_string(),
                "tool-b".to_string(),
                "grandchild".to_string(),
                "orphan".to_string(),
            ],
        );

        // Toggle collapses tool-a. The tool-a node itself stays visible
        // (its parent sub is expanded); only its children (grandchild) are
        // hidden until it expands again.
        app.toggle_activity_expanded("tool-a");
        assert!(!app.activity("tool-a").unwrap().expanded);
        assert_eq!(
            app.visible_activity_ids(),
            vec![
                "sub".to_string(),
                "tool-a".to_string(),
                "tool-b".to_string(),
                "orphan".to_string(),
            ],
        );

        // Toggle bumps revision so dependent caches can invalidate.
        let before = app.activity("sub").unwrap().revision;
        app.toggle_activity_expanded("sub");
        assert_eq!(app.activity("sub").unwrap().revision, before + 1);
        assert!(!app.activity("sub").unwrap().expanded);

        // Setting the same value is a no-op (no revision bump).
        let after = app.activity("sub").unwrap().revision;
        app.set_activity_expanded("sub", Some(false));
        assert_eq!(app.activity("sub").unwrap().revision, after);
    }

    #[test]
    fn cycle_and_orphan_parent_fail_safe_without_infinite_loop() {
        let mut app = fresh_app();
        // Manual cycle: a's parent is b, b's parent is a. The depth/visibility
        // walkers must terminate with finite values and no panic. The nodes
        // are inserted through `event` so the timeline and indexes reflect
        // the same shape as a real producer.
        app.event(UiEvent::Activity(start(
            "a",
            Some("b"),
            "main",
            ActivityKind::Tool,
            None,
        )));
        app.event(UiEvent::Activity(start(
            "b",
            Some("a"),
            "main",
            ActivityKind::Tool,
            None,
        )));

        // Depth counts real ancestors. In a 2-cycle each node sees the
        // other as its single ancestor, so the depth is 1 — the cycle is
        // detected (no infinite loop) but its members are real ancestors
        // of each other.
        assert_eq!(app.activity_depth("a"), 1);
        assert_eq!(app.activity_depth("b"), 1);

        // With both members collapsed, neither ancestor chain is fully
        // expanded (each has the other as a collapsed parent). The cycle
        // does not loop forever — the first collapsed parent short-circuits.
        assert!(!app.all_ancestors_expanded("a"));
        assert!(!app.all_ancestors_expanded("b"));
        // ...so neither node is currently visible.
        let visible = app.visible_activity_ids();
        assert!(!visible.contains(&"a".into()));
        assert!(!visible.contains(&"b".into()));

        // Expand the cycle as a whole: every ancestor is expanded, so the
        // walk terminates on the revisit and reports visible.
        app.set_activity_expanded("a", Some(true));
        app.set_activity_expanded("b", Some(true));
        assert!(app.all_ancestors_expanded("a"));
        assert!(app.all_ancestors_expanded("b"));
        let visible = app.visible_activity_ids();
        assert!(visible.contains(&"a".into()));
        assert!(visible.contains(&"b".into()));

        // Orphan parent: parent references a node that doesn't exist.
        let before = app.timeline.len();
        app.event(UiEvent::Activity(start(
            "orphan",
            Some("ghost"),
            "main",
            ActivityKind::Tool,
            None,
        )));
        assert_eq!(
            app.timeline.len(),
            before + 1,
            "timeline records the orphan just like any other activity",
        );
        // Orphan parent counts as a root: depth 0, visible by default,
        // appears in the visible list immediately.
        assert_eq!(app.activity_depth("orphan"), 0);
        assert!(app.all_ancestors_expanded("orphan"));
        assert!(app.visible_activity_ids().iter().any(|id| id == "orphan"));
    }

    #[test]
    fn layout_snapshot_terminates_on_two_node_cycle_and_orphan() {
        let mut app = fresh_app();
        // a's parent is b, b's parent is a: a two-node cycle.
        app.event(UiEvent::Activity(start(
            "cyc-a",
            Some("cyc-b"),
            "main",
            ActivityKind::Tool,
            None,
        )));
        app.event(UiEvent::Activity(start(
            "cyc-b",
            Some("cyc-a"),
            "main",
            ActivityKind::Tool,
            None,
        )));
        // Orphan: parent references a node that was never inserted.
        app.event(UiEvent::Activity(start(
            "orphan",
            Some("ghost"),
            "main",
            ActivityKind::Tool,
            None,
        )));

        // The snapshot walk must terminate and match the legacy id-based
        // helpers: each 2-cycle member sees the other as one ancestor, and
        // the orphan behaves like a root.
        let snapshot = app.layout_snapshot();
        assert_eq!(app.depth_for(&snapshot, "cyc-a"), 1);
        assert_eq!(app.depth_for(&snapshot, "cyc-b"), 1);
        assert_eq!(app.depth_for(&snapshot, "orphan"), 0);

        // Both cycle members are collapsed, so the cycle is hidden; the
        // orphan is visible because unknown parents behave like a root.
        assert!(!app.ancestor_visible_for(&snapshot, "cyc-a"));
        assert!(!app.ancestor_visible_for(&snapshot, "cyc-b"));
        assert!(app.ancestor_visible_for(&snapshot, "orphan"));

        // Descendant tallies terminate too; a cycle member counts the other
        // member once and never itself.
        assert_eq!(app.descendant_count_for(&snapshot, "cyc-a"), 1);
        assert_eq!(app.descendant_count_for(&snapshot, "cyc-b"), 1);
        assert_eq!(app.descendant_count_for(&snapshot, "orphan"), 0);

        // Expand the cycle: every ancestor is now expanded, so both members
        // become visible and the snapshot still terminates.
        app.set_activity_expanded("cyc-a", Some(true));
        app.set_activity_expanded("cyc-b", Some(true));
        let snapshot = app.layout_snapshot();
        assert!(app.ancestor_visible_for(&snapshot, "cyc-a"));
        assert!(app.ancestor_visible_for(&snapshot, "cyc-b"));
        assert!(app.visible_activity_ids().contains(&"cyc-a".to_string()));
        assert!(app.visible_activity_ids().contains(&"cyc-b".to_string()));
    }

    #[test]
    fn context_and_external_indexes_round_trip() {
        let mut app = fresh_app();
        app.event(UiEvent::Activity(start(
            "sub-1",
            None,
            "main",
            ActivityKind::Subagent,
            None,
        )));
        app.event(UiEvent::Activity(start(
            "sub-1b",
            None,
            "child",
            ActivityKind::Subagent,
            None,
        )));
        // Latest Subagent per context wins.
        app.event(UiEvent::Activity(start(
            "sub-1c",
            None,
            "main",
            ActivityKind::WorkflowStep,
            None,
        )));
        assert_eq!(app.activity_by_context("main").unwrap().start.id, "sub-1c");
        assert_eq!(app.activity_by_context("child").unwrap().start.id, "sub-1b");
        assert!(app.activity_by_context("missing").is_none());

        // Tool records key on (context, external_id) only when external_id is set.
        app.event(UiEvent::Activity(start(
            "tool-1",
            None,
            "main",
            ActivityKind::Tool,
            Some("req-7"),
        )));
        app.event(UiEvent::Activity(start(
            "tool-2",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        let looked = app
            .activity_by_external("main", "req-7")
            .expect("external lookup works");
        assert_eq!(looked.start.id, "tool-1");
        // Tool without external_id is not registered in either index.
        assert!(app.activity_by_context("main").unwrap().start.id == "sub-1c");
        // Missing context or external id returns None.
        assert!(app.activity_by_external("main", "missing").is_none());
        assert!(app.activity_by_external("missing", "req-7").is_none());
    }

    #[test]
    fn entry_pushes_append_timeline_in_order_with_optional_owner() {
        let mut app = fresh_app();
        let _ = app.push("user", "main", "hi".into());
        let _ = app.push_owned("assistant", "main", "hello".into(), Some("tool-1".into()));
        assert_eq!(app.entries.len(), 2);
        assert_eq!(app.timeline.len(), 2);
        assert!(matches!(app.timeline[0], TimelineItem::Entry(0)));
        assert!(matches!(app.timeline[1], TimelineItem::Entry(1)));
        assert_eq!(app.entries[1].activity_id.as_deref(), Some("tool-1"));
        // Pushing a fresh entry from `note`/`error` records no owner.
        app.note("status text");
        assert_eq!(app.entries[2].activity_id, None);
        assert!(matches!(app.timeline[2], TimelineItem::Entry(2)));
    }

    #[test]
    fn streaming_replacement_does_not_duplicate_timeline() {
        let mut app = fresh_app();
        // Open a stream, then push a delta. Each delta mutates the entry in
        // place; the timeline must contain exactly one Entry row, no matter
        // how many deltas arrive.
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "hel".into(),
        });
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "lo".into(),
        });
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: " world".into(),
        });
        let entry_rows = app
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::Entry(_)))
            .count();
        assert_eq!(entry_rows, 1, "stream replaces in place; no duplicate rows");
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].text, "hello world");
        assert!(app.entries[0].streaming);
        assert_eq!(app.entries[0].revision, 3);
        // Closing the stream with a final Message also does not append.
        app.event(UiEvent::Message {
            context: "main".into(),
            message: Message::new("assistant", "hello world"),
        });
        let entry_rows_after = app
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::Entry(_)))
            .count();
        assert_eq!(entry_rows_after, 1);
        assert!(!app.entries[0].streaming);
        assert_eq!(app.entries[0].revision, 4);
    }

    #[test]
    fn incomplete_message_replacement_clears_streaming_and_bumps_revision() {
        let mut app = fresh_app();
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "partial".into(),
        });
        let index = app.streams["main"];
        assert!(app.entries[index].streaming);
        assert_eq!(app.entries[index].revision, 1);

        app.event(UiEvent::Message {
            context: "main".into(),
            message: Message::incomplete_assistant("partial", "cancelled"),
        });

        assert!(!app.entries[index].streaming);
        assert_eq!(app.entries[index].revision, 2);
        assert_eq!(
            app.entries[index].text,
            "partial\n\n[incomplete response: cancelled]"
        );
    }

    #[test]
    fn finishing_a_stream_clears_flag_and_invalidates_renderer_revision() {
        let mut app = fresh_app();
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "partial".into(),
        });
        let index = app.streams["main"];
        let revision = app.entries[index].revision;

        app.finish_streaming_entries();

        assert!(!app.entries[index].streaming);
        assert_eq!(app.entries[index].revision, revision + 1);
    }

    #[test]
    fn end_without_status_defaults_to_error() {
        let mut app = fresh_app();
        app.event(UiEvent::Activity(start(
            "tool-x",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        app.event(UiEvent::Activity(end("tool-x", "main", None)));
        assert_eq!(
            app.activity("tool-x").unwrap().status,
            Some(ActivityStatus::Error),
        );
    }

    #[test]
    fn unknown_events_never_panic() {
        let mut app = fresh_app();
        // End for an id that never had a Start.
        app.event(UiEvent::Activity(end(
            "ghost",
            "main",
            Some(ActivityStatus::Success),
        )));
        assert!(app.activity("ghost").is_none());
        assert_eq!(
            app.pending_activity_ends.get("ghost"),
            Some(&ActivityStatus::Success),
        );
        // Misordered End again: still pending, never overrides with a panic.
        app.event(UiEvent::Activity(end(
            "ghost",
            "main",
            Some(ActivityStatus::Denied),
        )));
        // Most recent pending status wins (BTreeMap insert is idempotent on
        // the same key; this matches the "store pending status" semantics).
        assert_eq!(
            app.pending_activity_ends.get("ghost"),
            Some(&ActivityStatus::Denied),
        );
        // Setting expansion on an unknown id is a no-op.
        let before_revision = app.activities.len();
        app.set_activity_expanded("ghost", Some(true));
        assert_eq!(app.activities.len(), before_revision);
        // apply_recovered_unmatched with an unknown id list does nothing.
        app.apply_recovered_unmatched(&[end("ghost", "main", None)]);
        assert!(app.activity("ghost").is_none());
    }

    #[test]
    fn recovered_unmatched_marks_cancelled_without_emitting_end_row() {
        let mut app = fresh_app();
        app.event(UiEvent::Activity(start(
            "tool-a",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        app.event(UiEvent::Activity(start(
            "tool-b",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        app.event(UiEvent::Activity(start(
            "tool-c",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        // tool-a and tool-c have matching Ends; tool-b does not.
        app.event(UiEvent::Activity(end(
            "tool-a",
            "main",
            Some(ActivityStatus::Success),
        )));
        app.event(UiEvent::Activity(end(
            "tool-c",
            "main",
            Some(ActivityStatus::Success),
        )));
        let timeline_before = app.timeline.len();

        let events = vec![
            start("tool-a", None, "main", ActivityKind::Tool, None),
            end("tool-a", "main", Some(ActivityStatus::Success)),
            start("tool-b", None, "main", ActivityKind::Tool, None),
            start("tool-c", None, "main", ActivityKind::Tool, None),
            end("tool-c", "main", Some(ActivityStatus::Success)),
        ];
        app.apply_recovered_unmatched(&events);

        // tool-a and tool-c keep their recorded status; only tool-b flips
        // to Cancelled. No new End timeline row is appended.
        assert_eq!(
            app.activity("tool-a").unwrap().status,
            Some(ActivityStatus::Success),
        );
        assert_eq!(
            app.activity("tool-b").unwrap().status,
            Some(ActivityStatus::Cancelled),
        );
        assert_eq!(
            app.activity("tool-c").unwrap().status,
            Some(ActivityStatus::Success),
        );
        assert_eq!(app.timeline.len(), timeline_before);
        assert_eq!(
            app.timeline
                .iter()
                .filter(|item| matches!(item, TimelineItem::Activity(id) if id == "tool-b"))
                .count(),
            1,
            "still exactly one timeline row for tool-b",
        );

        // Idempotent: a second pass does not double-cancel or change a node
        // that already has a final status.
        let tool_b_rev = app.activity("tool-b").unwrap().revision;
        app.apply_recovered_unmatched(&events);
        assert_eq!(app.activity("tool-b").unwrap().revision, tool_b_rev);
    }

    // -----------------------------------------------------------------
    // App message/detail ownership and legacy replay synthesis tests.
    //
    // Live mode never inlines `Tool: ...` prose into assistant entries;
    // tool calls are represented by real Activity Start events. Replay
    // honors a session-wide `legacy_fallback` flag: when the session has no
    // real Activity records, the message path synthesizes collapsed
    // Subagent/WorkflowStep and Tool nodes so the transcript still renders
    // grouped. The tests below cover both branches plus the ownership
    // wiring that ties chat entries to the right activity id.
    // -----------------------------------------------------------------

    fn assistant_with_tool_call(content: &str, call_id: &str, name: &str) -> Message {
        let mut message = Message::new("assistant", content);
        message.tool_calls.push(crate::model::ToolCall {
            id: call_id.into(),
            name: name.into(),
            arguments: "{}".into(),
        });
        message
    }

    fn tool_result(call_id: &str, content: &str) -> Message {
        Message::tool(call_id, content)
    }

    #[test]
    fn live_message_does_not_inline_tool_prose() {
        let mut app = fresh_app();
        app.message(
            "main".into(),
            assistant_with_tool_call("I will look that up.", "call-1", "shell"),
        );
        // Exactly one chat entry, owned by the assistant context (main is
        // unowned). Tool prose must not appear in the entry text.
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].role, "assistant");
        assert_eq!(app.entries[0].activity_id, None);
        assert_eq!(app.entries[0].text, "I will look that up.");
        assert!(
            !app.entries[0].text.contains("Tool:"),
            "live mode must not append tool prose; got {:?}",
            app.entries[0].text
        );
        assert!(!app.entries[0].text.contains("shell"));
    }

    #[test]
    fn live_message_preserves_content_reasoning_and_incomplete_marker() {
        let mut app = fresh_app();
        // Reasoning metadata is carried through unchanged; we never render
        // it into the transcript, but the field is preserved on the
        // message going in.
        let mut message = Message::new("assistant", "first line\nsecond line");
        message.reasoning = Some("thinking trace".into());
        app.message("main".into(), message);
        assert_eq!(app.entries[0].text, "first line\nsecond line");

        // Incomplete marker renders the [incomplete response: ...] suffix
        // exactly as before and still replaces the live stream entry.
        // The Delta opens the assistant stream and the incomplete Message
        // replaces it in place — a single Entry timeline row covers both.
        let entries_before_stream = app.entries.len();
        let rows_before_stream = app
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::Entry(_)))
            .count();
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "partial ".into(),
        });
        let stream_index = app.streams.get("main").copied().expect("stream open");
        let partial = Message::incomplete_assistant("partial answer", "idle timeout after 5s");
        app.message("main".into(), partial);
        assert_eq!(
            app.entries[stream_index].text,
            "partial answer\n\n[incomplete response: idle timeout after 5s]"
        );
        // Stream lifecycle produced exactly one additional Entry row.
        assert_eq!(
            app.timeline
                .iter()
                .filter(|item| matches!(item, TimelineItem::Entry(_)))
                .count(),
            rows_before_stream + 1,
            "stream replacement must not add a second row on top of the open"
        );
        assert_eq!(app.entries.len(), entries_before_stream + 1);

        // Final incomplete marker on a context without a live stream
        // pushes a fresh row carrying the marker.
        let standalone =
            Message::incomplete_assistant("cold partial", "stream ended before completion");
        app.message("main".into(), standalone);
        let entries: Vec<&str> = app.entries.iter().map(|e| e.text.as_str()).collect();
        assert!(entries
            .contains(&"cold partial\n\n[incomplete response: stream ended before completion]"));
    }

    #[test]
    fn tool_result_entry_is_owned_by_synthetic_tool_and_updates_status() {
        let mut app = fresh_app();
        // Assistant emits a tool call (legacy fallback synthesizes the Tool
        // node); tool result message must attach the entry to that Tool
        // and update its status from the JSON content.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: assistant_with_tool_call("checking…", "call-1", "shell"),
            }),
            true,
        );
        // Synthetic tool node should exist with external_id=call-1.
        let tool_id = app
            .activity_by_external("main", "call-1")
            .expect("synthetic tool registered")
            .start
            .id
            .clone();
        // Tool result arrives as a separate DisplayEvent; entry owner must
        // be the synthetic Tool, and the assistant prose must not be
        // duplicated.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: tool_result("call-1", r#"{"ok": true}"#),
            }),
            true,
        );
        assert_eq!(app.entries.len(), 2);
        assert_eq!(app.entries[0].text, "checking…");
        assert_eq!(app.entries[1].text, r#"{"ok": true}"#);
        assert_eq!(
            app.entries[1].activity_id.as_deref(),
            Some(tool_id.as_str()),
            "tool result entry must be owned by the synthetic Tool"
        );
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Success),
            "non-error JSON result sets Success"
        );
    }

    #[test]
    fn non_main_context_messages_attach_to_synthesized_subagent() {
        let mut app = fresh_app();
        // First non-main message in legacy mode synthesizes the context
        // owner BEFORE the message is emitted; subsequent context messages
        // attach to the same id.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "subagent:planner".into(),
                message: Message::new("user", "hi planner"),
            }),
            true,
        );
        let owner = app
            .activity_by_context("subagent:planner")
            .expect("synthesized")
            .start
            .id
            .clone();
        assert!(owner.starts_with("legacy:context:subagent:planner"));
        // User entry is owned.
        assert_eq!(app.entries[0].activity_id.as_deref(), Some(owner.as_str()));

        // A second non-main message (assistant) attaches to the same
        // owner — only ONE synthesized Subagent exists.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "subagent:planner".into(),
                message: Message::new("assistant", "plan"),
            }),
            true,
        );
        assert_eq!(app.entries[1].activity_id.as_deref(), Some(owner.as_str()));
        assert_eq!(
            app.activity_by_context("subagent:planner")
                .unwrap()
                .start
                .id,
            owner
        );
        // Workflow context uses WorkflowStep, not Subagent.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "workflow:review".into(),
                message: Message::new("user", "review please"),
            }),
            true,
        );
        assert!(app
            .activity_by_context("workflow:review")
            .unwrap()
            .start
            .kind
            .eq(&ActivityKind::WorkflowStep));
    }

    #[test]
    fn stream_delta_assigns_owner_on_open_and_preserves_on_replacement() {
        let mut app = fresh_app();
        // Register a context owner up front so the stream picks it up.
        app.event(UiEvent::Activity(start(
            "sub",
            None,
            "subagent:planner",
            ActivityKind::Subagent,
            None,
        )));
        // Open a stream for a non-main context; the assistant entry is
        // owned by the Subagent from the very first delta.
        app.event(UiEvent::Delta {
            context: "subagent:planner".into(),
            text: "first".into(),
        });
        let stream_index = app
            .streams
            .get("subagent:planner")
            .copied()
            .expect("stream open");
        assert_eq!(
            app.entries[stream_index].activity_id.as_deref(),
            Some("sub")
        );

        // Many deltas mutate in place — exactly one timeline Entry row.
        app.event(UiEvent::Delta {
            context: "subagent:planner".into(),
            text: " second".into(),
        });
        app.event(UiEvent::Delta {
            context: "subagent:planner".into(),
            text: " third".into(),
        });
        let entry_rows = app
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::Entry(_)))
            .count();
        assert_eq!(entry_rows, 1, "deltas replace in place; no duplicate rows");
        assert_eq!(app.entries[stream_index].text, "first second third");
        assert_eq!(
            app.entries[stream_index].activity_id.as_deref(),
            Some("sub"),
            "owner survives every delta replacement"
        );

        // Final Message arrives via message_inner; it replaces the stream
        // entry in place without appending a timeline row.
        app.event(UiEvent::Message {
            context: "subagent:planner".into(),
            message: Message::new("assistant", "final"),
        });
        let entry_rows_after = app
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::Entry(_)))
            .count();
        assert_eq!(entry_rows_after, 1);
        assert_eq!(app.entries[stream_index].text, "final");
        assert_eq!(
            app.entries[stream_index].activity_id.as_deref(),
            Some("sub")
        );
    }

    #[test]
    fn main_stream_stays_unowned() {
        let mut app = fresh_app();
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "hello".into(),
        });
        assert_eq!(app.entries[0].activity_id, None);
    }

    #[test]
    fn legacy_replay_orders_context_user_assistant_then_synthetic_tools() {
        let mut app = fresh_app();
        let events = vec![
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "subagent:planner".into(),
                message: Message::new("user", "do the thing"),
            }),
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "subagent:planner".into(),
                message: assistant_with_tool_call("on it", "call-1", "shell"),
            }),
        ];
        let legacy = events.iter().all(|e| matches!(e, DisplayEvent::Message(_)))
            && app.activities.is_empty();
        for event in events {
            app.replay_display_event(event, legacy);
        }

        // Three timeline rows: Subagent Activity, user Entry, assistant
        // Entry, then the synthetic Tool Start row.
        assert_eq!(app.timeline.len(), 4);
        assert!(
            matches!(&app.timeline[0], TimelineItem::Activity(id) if id.starts_with("legacy:context:subagent:planner"))
        );
        assert!(matches!(&app.timeline[1], TimelineItem::Entry(0)));
        assert!(matches!(&app.timeline[2], TimelineItem::Entry(1)));
        assert!(
            matches!(&app.timeline[3], TimelineItem::Activity(id) if id.starts_with("legacy:tool:subagent:planner:call-1"))
        );

        // Synthetic Tool parent is the Subagent, not None.
        let tool_id = app
            .activity_by_external("subagent:planner", "call-1")
            .expect("synthetic tool")
            .start
            .id
            .clone();
        assert_eq!(
            app.activity(&tool_id).unwrap().start.parent_id.as_deref(),
            Some(
                app.activity_by_context("subagent:planner")
                    .unwrap()
                    .start
                    .id
                    .as_str()
            )
        );
    }

    #[test]
    fn legacy_tool_result_marks_tool_error_on_structured_error_json() {
        let mut app = fresh_app();
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: assistant_with_tool_call("trying", "call-err", "shell"),
            }),
            true,
        );
        let tool_id = app
            .activity_by_external("main", "call-err")
            .expect("synthesized")
            .start
            .id
            .clone();
        // Tool result carries a string that holds JSON-encoded error
        // content; the helper must unwrap and mark Error.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: tool_result("call-err", r#""{\"error\":\"boom\"}""#),
            }),
            true,
        );
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Error),
            "string-encoded JSON with error field flips the Tool node to Error"
        );
    }

    #[test]
    fn legacy_tool_result_error_via_nested_content_field() {
        let mut app = fresh_app();
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: assistant_with_tool_call("trying", "call-nested", "shell"),
            }),
            true,
        );
        let tool_id = app
            .activity_by_external("main", "call-nested")
            .expect("synthesized")
            .start
            .id
            .clone();
        // The error is nested inside `content`, not at the top level.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: tool_result("call-nested", r#"{"content":{"error":"nope"}}"#),
            }),
            true,
        );
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Error),
            "error nested under content/stdout still flips the Tool node"
        );
    }

    #[test]
    fn legacy_tool_result_non_error_json_marks_success() {
        let mut app = fresh_app();
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: assistant_with_tool_call("trying", "call-ok", "shell"),
            }),
            true,
        );
        let tool_id = app
            .activity_by_external("main", "call-ok")
            .expect("synthesized")
            .start
            .id
            .clone();
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: tool_result("call-ok", r#"{"stdout":"hello"}"#),
            }),
            true,
        );
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Success)
        );
    }

    #[test]
    fn finish_legacy_replay_marks_running_synthetic_nodes_success() {
        let mut app = fresh_app();
        // Synthesize a context owner and a synthetic Tool with no result.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "subagent:planner".into(),
                message: assistant_with_tool_call("...", "call-pending", "shell"),
            }),
            true,
        );
        let context_id = app
            .activity_by_context("subagent:planner")
            .unwrap()
            .start
            .id
            .clone();
        let tool_id = app
            .activity_by_external("subagent:planner", "call-pending")
            .unwrap()
            .start
            .id
            .clone();
        // Both still None before the sweep.
        assert_eq!(app.activity(&context_id).unwrap().status, None);
        assert_eq!(app.activity(&tool_id).unwrap().status, None);

        app.finish_legacy_replay();
        assert_eq!(
            app.activity(&context_id).unwrap().status,
            Some(ActivityStatus::Success)
        );
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Success)
        );

        // Already-set Error stays Error — sweep does not overwrite.
        app.event(UiEvent::Activity(end(
            &tool_id,
            "subagent:planner",
            Some(ActivityStatus::Error),
        )));
        // The sweep should not touch non-legacy ids anyway, but verify
        // the legacy-prefixed Error survives a second sweep.
        app.finish_legacy_replay();
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Error)
        );
    }

    // -----------------------------------------------------------------
    // Live-mode regressions for `apply_legacy_tool_result_status`. The
    // guard `if self.activities[index].status.is_some() { return; }` is
    // the one-line rule that prevents the legacy classifier from
    // overwriting an authoritative live status. Each test below proves
    // the rule survives: a real Activity Start whose End is delivered
    // first (or its Denied / Cancelled / Success variant is recorded by
    // the engine) must keep its status even when a replayed tool result
    // arrives afterward with a different payload. The same tests also
    // cover the converse case — a replayed result arriving *before* any
    // authoritative End must apply the classifier's verdict.
    // -----------------------------------------------------------------

    fn replay_legacy_tool_result_without_synthesis(
        app: &mut App,
        call_id: &str,
        content: &str,
    ) -> String {
        // Reach into `app` to skip the legacy fallback so the test
        // exercises the `apply_legacy_tool_result_status` short-circuit
        // against an already-authoritative node, not against a freshly
        // synthesized one. Return the activity id the helper bound.
        let tool_id = format!("real-tool:{call_id}");
        app.event(UiEvent::Activity(start(
            &tool_id,
            None,
            "main",
            ActivityKind::Tool,
            Some(call_id),
        )));
        app.message("main".into(), tool_result(call_id, content));
        tool_id
    }

    #[test]
    fn legacy_classifier_does_not_overwrite_authoritative_live_end_success() {
        let mut app = fresh_app();
        // Wire a real Tool Start, then deliver the End with Success so
        // the engine records an authoritative status. A subsequent
        // legacy tool-result message must NOT downgrade it.
        let tool_id = replay_legacy_tool_result_without_synthesis(
            &mut app,
            "call-success",
            r#"{"stdout":"first answer"}"#,
        );
        app.event(UiEvent::Activity(end(
            &tool_id,
            "main",
            Some(ActivityStatus::Success),
        )));
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Success)
        );
        let rev = app.activity(&tool_id).unwrap().revision;

        // Now feed a tool result carrying `status: denied` plus an
        // error field. The legacy classifier would say Denied/Error,
        // but the authoritative live End wins.
        app.message(
            "main".into(),
            tool_result("call-success", r#"{"status":"denied","error":"late"}"#),
        );
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Success),
            "authoritative live Success survives a later legacy Denial"
        );
        assert_eq!(
            app.activity(&tool_id).unwrap().revision,
            rev,
            "no-op rewrite must not bump revision"
        );
    }

    #[test]
    fn legacy_classifier_does_not_overwrite_authoritative_live_end_denied() {
        let mut app = fresh_app();
        let tool_id = replay_legacy_tool_result_without_synthesis(
            &mut app,
            "call-denied",
            r#"{"stdout":"ok"}"#,
        );
        // Live Denied is authoritative.
        app.event(UiEvent::Activity(end(
            &tool_id,
            "main",
            Some(ActivityStatus::Denied),
        )));
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Denied)
        );

        // A replayed tool result carrying an `error` field would
        // normally classify as Error, but the authoritative Denied
        // status is preserved.
        app.message(
            "main".into(),
            tool_result(
                "call-denied",
                r#"{"error":"boom","content":"something broke"}"#,
            ),
        );
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Denied),
            "authoritative live Denial survives a later legacy Error"
        );
    }

    #[test]
    fn legacy_classifier_does_not_overwrite_authoritative_live_end_cancelled() {
        let mut app = fresh_app();
        let tool_id = replay_legacy_tool_result_without_synthesis(
            &mut app,
            "call-cancel",
            r#"{"stdout":"ok"}"#,
        );
        // Live Cancelled arrives via the engine; a later legacy
        // result must NOT downgrade or replace it.
        app.event(UiEvent::Activity(end(
            &tool_id,
            "main",
            Some(ActivityStatus::Cancelled),
        )));
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Cancelled)
        );

        // A late legacy result that says "ok again" must not flip
        // the cancelled node back to Success.
        app.message(
            "main".into(),
            tool_result("call-cancel", r#"{"ok":true,"stdout":"nothing"}"#),
        );
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Cancelled),
            "authoritative live Cancellation survives a later legacy Success"
        );
    }

    #[test]
    fn legacy_classifier_does_apply_when_live_end_is_missing() {
        let mut app = fresh_app();
        // No engine End arrives: the legacy classifier is the only
        // authority and must mark the synthetic Tool node.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: assistant_with_tool_call("trying", "call-late", "shell"),
            }),
            true,
        );
        let tool_id = app
            .activity_by_external("main", "call-late")
            .expect("synthesized")
            .start
            .id
            .clone();
        // Denied via the JSON classifier.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: tool_result("call-late", r#"{"status":"rejected"}"#),
            }),
            true,
        );
        assert_eq!(
            app.activity(&tool_id).unwrap().status,
            Some(ActivityStatus::Denied),
            "missing authoritative End lets the legacy classifier set the status"
        );
    }

    #[test]
    fn finish_legacy_replay_leaves_real_activity_nodes_alone() {
        let mut app = fresh_app();
        // A real Activity node (engine-produced) starts with a non-legacy
        // id and stays None after sweep.
        app.event(UiEvent::Activity(start(
            "real-tool",
            None,
            "main",
            ActivityKind::Tool,
            Some("req-1"),
        )));
        assert_eq!(app.activity("real-tool").unwrap().status, None);
        app.finish_legacy_replay();
        assert_eq!(
            app.activity("real-tool").unwrap().status,
            None,
            "real Activity nodes are not touched by the legacy sweep"
        );
    }

    #[test]
    fn duplicate_legacy_call_ids_get_deterministic_suffix_and_no_overwrite() {
        let mut app = fresh_app();
        // First tool-call id collides on `legacy:tool:main:c1`.
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: assistant_with_tool_call("a", "c1", "shell"),
            }),
            true,
        );
        let first_id = app
            .activity_by_external("main", "c1")
            .unwrap()
            .start
            .id
            .clone();
        // Reset external lookup so we can replay the same call id fresh:
        // first_message already registered c1, but the legacy helper
        // refuses to overwrite, so a second call with the same id falls
        // back to #2. Build the message path through synthesize_legacy_tool_start
        // directly to exercise the collision resolver without double-arming
        // activity_by_external.
        let second_id = app.legacy_unique_id(&format!("legacy:tool:main:{first_id}"));
        assert_ne!(first_id, second_id);
        // The collision suffix is deterministic — re-running yields the same id.
        let again = app.legacy_unique_id(&format!("legacy:tool:main:{first_id}"));
        assert_eq!(second_id, again);

        // Original call stays in place (no overwrite). Insert a real node
        // with the base id manually and confirm the helper bumps to #2
        // without disturbing the original.
        app.event(UiEvent::Activity(start(
            "legacy:tool:main:c1",
            None,
            "main",
            ActivityKind::Tool,
            Some("c1"),
        )));
        let bumped = app.legacy_unique_id("legacy:tool:main:c1");
        assert_eq!(bumped, "legacy:tool:main:c1#2");
        let node1_rev = app.activity("legacy:tool:main:c1").unwrap().revision;
        // Resolving again does not bump the original's revision.
        let _ = app.legacy_unique_id("legacy:tool:main:c1");
        assert_eq!(
            app.activity("legacy:tool:main:c1").unwrap().revision,
            node1_rev
        );
    }

    #[test]
    fn real_activity_replay_does_not_synthesize_anything() {
        let mut app = fresh_app();
        // A session with real Activity records has non-empty activities.
        // replay_display_event with legacy_fallback=false must route
        // activity events through handle_activity_event without adding
        // legacy-prefixed ids, and must not synthesize tools for the
        // assistant message's tool_calls.
        app.replay_display_event(
            DisplayEvent::Activity(start(
                "real-sub",
                None,
                "subagent:planner",
                ActivityKind::Subagent,
                None,
            )),
            false,
        );
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "subagent:planner".into(),
                message: assistant_with_tool_call("hi", "call-x", "shell"),
            }),
            false,
        );
        // Only the real Subagent Activity; no synthetic context node, no
        // synthetic Tool Start.
        assert_eq!(app.activities.len(), 1);
        assert_eq!(
            app.activity_by_context("subagent:planner")
                .unwrap()
                .start
                .id,
            "real-sub"
        );
        assert!(app
            .activity_by_external("subagent:planner", "call-x")
            .is_none());
        // The assistant entry still owns nothing special in this branch
        // because no synthetic Tool exists; context lookup still resolves
        // to the real Subagent.
        assert_eq!(
            app.entries[0].activity_id.as_deref(),
            Some("real-sub"),
            "non-main context message attaches to the real Subagent"
        );
    }

    #[test]
    fn replay_routes_activity_through_existing_handlers_without_synthesizing() {
        let mut app = fresh_app();
        // An End before its Start must still queue in pending_activity_ends
        // — replay routes through handle_activity_event, which is already
        // fail-safe.
        app.replay_display_event(
            DisplayEvent::Activity(end("future-tool", "main", Some(ActivityStatus::Success))),
            false,
        );
        assert_eq!(
            app.pending_activity_ends.get("future-tool"),
            Some(&ActivityStatus::Success)
        );
    }

    #[test]
    fn legacy_title_is_one_line_and_bounded() {
        let mut app = fresh_app();
        // Build a tool call whose arguments produce a long description.
        let long_arg = "x".repeat(500);
        let mut message = Message::new("assistant", "long");
        message.tool_calls.push(crate::model::ToolCall {
            id: "long".into(),
            name: "shell".into(),
            arguments: format!(r#"{{"command":"echo","args":["{long_arg}"]}}"#),
        });
        app.replay_display_event(
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message,
            }),
            true,
        );
        let node = app
            .activity_by_external("main", "long")
            .expect("synthetic tool");
        let title = &node.start.title;
        assert!(!title.contains('\n'), "title must be a single line");
        // Bounded: the suffix is "tool shell: ..." + bounded body, capped at
        // 120 chars + "...". We don't assert the exact cap because the
        // summarize path may shorten the description first; we only assert
        // that the title is bounded and starts with the expected prefix.
        assert!(title.starts_with("tool shell:"));
        assert!(
            title.chars().count() <= 200,
            "title stays bounded, got {}",
            title.chars().count()
        );
    }

    // -----------------------------------------------------------------
    // Startup replay + reset integration.
    //
    // These tests exercise the App-side surface that `tui::run` calls
    // against a fresh `Session` snapshot:
    // - `replay_display_event` interleaves messages and activities in the
    //   exact order they appear in `Session::display_events`.
    // - `finish_legacy_replay` sweeps running legacy nodes to Success.
    // - `apply_unmatched_ids` mirrors `Session::recovered_unmatched` and
    //   marks still-open nodes Cancelled without emitting a timeline row.
    // - `reset_view` is the single source of truth for view-state reset
    //   used by `/clear` and `/new`; `/reload` must NOT call it.
    //
    // The tests keep the spine coverage local — no TTY, no engine — so
    // they remain stable on machines without a real provider or a real
    // session directory.
    // -----------------------------------------------------------------

    #[test]
    fn real_activity_session_resumes_in_interleaved_order() {
        // Build a session-shaped `display_events` vector in the same
        // order `Session::open` produces it: original JSONL arrival order,
        // messages and activities interleaved. `legacy_fallback` is the
        // session-wide decision the TUI computes from whether
        // `display_events` contains an `Activity` entry; with real activity records
        // present the flag is false and no synthesis happens.
        let mut app = fresh_app();
        let events = vec![
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: Message::new("user", "hello"),
            }),
            DisplayEvent::Activity(start(
                "real-tool",
                None,
                "main",
                ActivityKind::Tool,
                Some("req-1"),
            )),
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "main".into(),
                message: Message::new("assistant", "thinking"),
            }),
            DisplayEvent::Activity(end("real-tool", "main", Some(ActivityStatus::Success))),
        ];
        let legacy_fallback = false;
        for event in events {
            app.replay_display_event(event, legacy_fallback);
        }

        // One real Activity node, no synthesis.
        assert_eq!(app.activities.len(), 1);
        assert_eq!(
            app.activity("real-tool").unwrap().status,
            Some(ActivityStatus::Success)
        );
        // Default collapsed: real-tool is still visible (root) but
        // expanded state is false.
        let real = app.activity("real-tool").unwrap();
        assert!(!real.expanded);
        assert_eq!(
            app.visible_activity_ids(),
            vec!["real-tool".to_string()],
            "real Activity is visible as a root when collapsed"
        );

        // Timeline interleaves entries and the activity in arrival order:
        // user Entry, real-tool Activity, assistant Entry, end folds into
        // the existing node (no timeline row for End).
        assert_eq!(app.timeline.len(), 3);
        assert!(matches!(&app.timeline[0], TimelineItem::Entry(0)));
        assert!(matches!(&app.timeline[1], TimelineItem::Activity(id) if id == "real-tool"));
        assert!(matches!(&app.timeline[2], TimelineItem::Entry(1)));

        // Ownership: main entries stay unowned; non-main entries would
        // attach to the context owner.
        assert_eq!(app.entries[0].activity_id, None);
        assert_eq!(app.entries[1].activity_id, None);
    }

    #[test]
    fn recovered_unmatched_ids_mark_cancelled_without_emitting_end_row() {
        // The session surfaces `recovered_unmatched` as a `Vec<String>`
        // of activity ids that never received an End. `apply_unmatched_ids`
        // marks each matching in-memory node Cancelled without pushing
        // a new End timeline row, matching the event-list helper's
        // invariant.
        let mut app = fresh_app();
        app.event(UiEvent::Activity(start(
            "tool-a",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        app.event(UiEvent::Activity(start(
            "tool-b",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        // tool-a has a real End; tool-b never closed.
        app.event(UiEvent::Activity(end(
            "tool-a",
            "main",
            Some(ActivityStatus::Success),
        )));
        let timeline_before = app.timeline.len();
        let unmatched = vec!["tool-b".to_string()];
        app.apply_unmatched_ids(&unmatched);

        assert_eq!(
            app.activity("tool-a").unwrap().status,
            Some(ActivityStatus::Success),
            "real End status survives a recovered-unmatched pass"
        );
        assert_eq!(
            app.activity("tool-b").unwrap().status,
            Some(ActivityStatus::Cancelled),
            "unmatched id flips to Cancelled"
        );
        assert_eq!(
            app.timeline.len(),
            timeline_before,
            "Cancelled status patch must not emit a new timeline row"
        );

        // Unknown ids are no-ops (no panic, no row).
        let timeline_before_unknown = app.timeline.len();
        app.apply_unmatched_ids(&["ghost".to_string()]);
        assert_eq!(app.timeline.len(), timeline_before_unknown);

        // Idempotent: calling twice on the same id does not bump the
        // revision a second time.
        let revision = app.activity("tool-b").unwrap().revision;
        app.apply_unmatched_ids(&unmatched);
        assert_eq!(app.activity("tool-b").unwrap().revision, revision);
    }

    #[test]
    fn legacy_no_activity_session_synthesizes_fallback_nodes_in_place() {
        // A session with no activity records sets `legacy_fallback = true`
        // for every replay event. The first non-main context message
        // synthesizes a Subagent owner; assistant tool calls synthesize
        // Tool Start nodes parented to that Subagent. Real activity
        // events never appear (the session has no activities), so the
        // synthesized nodes are the only thing the user sees.
        let mut app = fresh_app();
        let events = vec![
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "subagent:planner".into(),
                message: Message::new("user", "plan it"),
            }),
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "subagent:planner".into(),
                message: assistant_with_tool_call("on it", "call-1", "shell"),
            }),
            DisplayEvent::Message(crate::session::TranscriptEntry {
                context: "subagent:planner".into(),
                message: tool_result("call-1", r#"{"stdout":"hi"}"#),
            }),
        ];
        let legacy = true;
        for event in events {
            app.replay_display_event(event, legacy);
        }
        app.finish_legacy_replay();

        // One synthesized Subagent, one synthesized Tool — no duplicates.
        assert_eq!(app.activities.len(), 2);
        let context_owner = app
            .activity_by_context("subagent:planner")
            .expect("synthesized subagent owner");
        assert!(context_owner
            .start
            .id
            .starts_with("legacy:context:subagent:planner"));
        assert_eq!(
            context_owner.status,
            Some(ActivityStatus::Success),
            "legacy sweep marks the synthesized Subagent Success"
        );
        let tool = app
            .activity_by_external("subagent:planner", "call-1")
            .expect("synthesized tool");
        assert!(tool
            .start
            .id
            .starts_with("legacy:tool:subagent:planner:call-1"));
        assert_eq!(tool.status, Some(ActivityStatus::Success));
        assert_eq!(
            tool.start.parent_id.as_deref(),
            Some(context_owner.start.id.as_str()),
            "synthesized Tool parent is the synthesized Subagent"
        );

        // Timeline shows the synthesized Subagent at the front, then
        // entries in arrival order, then the synthesized Tool Start.
        assert!(matches!(
            &app.timeline[0],
            TimelineItem::Activity(id) if id.starts_with("legacy:context:subagent:planner")
        ));
        assert!(matches!(&app.timeline[1], TimelineItem::Entry(0)));
        assert!(matches!(&app.timeline[2], TimelineItem::Entry(1)));
        assert!(matches!(
            &app.timeline[3],
            TimelineItem::Activity(id) if id.starts_with("legacy:tool:subagent:planner:call-1")
        ));

        // Non-main entries attach to the synthesized Subagent; the tool
        // result entry attaches to the synthesized Tool.
        assert_eq!(
            app.entries[0].activity_id.as_deref(),
            Some(context_owner.start.id.as_str())
        );
        assert_eq!(
            app.entries[1].activity_id.as_deref(),
            Some(context_owner.start.id.as_str())
        );
        assert_eq!(
            app.entries[2].activity_id.as_deref(),
            Some(tool.start.id.as_str())
        );
    }

    #[test]
    fn incomplete_message_renders_after_replay_and_is_excluded_from_model_history() {
        // Build a session with a user message, an incomplete assistant
        // transcript, and a follow-up user message. The session's
        // `display_events` includes all three (the user can see them in
        // the transcript); `messages` (the provider request history)
        // excludes the incomplete one so it cannot re-execute.
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().to_path_buf();
        let session_id = "incomplete-fixture";
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let write = |file: &mut std::fs::File, value: serde_json::Value| {
            let mut bytes = serde_json::to_vec(&value).unwrap();
            bytes.push(b'\n');
            file.write_all(&bytes).unwrap();
        };
        write(
            &mut file,
            serde_json::json!({
                "type": "session",
                "at": "2026-01-01T00:00:00Z",
                "context": "main",
                "data": {"id": session_id, "version": 1}
            }),
        );
        write(
            &mut file,
            serde_json::json!({
                "type": "message",
                "at": "2026-01-01T00:00:01Z",
                "context": "main",
                "data": Message::new("user", "tell me a story")
            }),
        );
        write(
            &mut file,
            serde_json::json!({
                "type": "message",
                "at": "2026-01-01T00:00:02Z",
                "context": "main",
                "data": Message::incomplete_assistant(
                    "once upon a",
                    "stream ended before completion"
                )
            }),
        );
        write(
            &mut file,
            serde_json::json!({
                "type": "message",
                "at": "2026-01-01T00:00:03Z",
                "context": "main",
                "data": Message::new("user", "continue")
            }),
        );
        drop(file);

        // Reopen through the public `Session::open` path used by the TUI.
        let session = crate::session::Session::open(&sessions_dir, Some(session_id)).unwrap();

        // Incomplete message is NOT in the model request history.
        assert!(
            !session.messages.iter().any(|m| m.incomplete.is_some()),
            "incomplete assistant message must never re-enter provider history; got {:?}",
            session.messages
        );
        assert_eq!(session.messages.len(), 2, "only the two user turns survive");
        assert_eq!(session.messages[0].content, "tell me a story");
        assert_eq!(session.messages[1].content, "continue");

        // But it IS in the unified display timeline the TUI replays.
        let incomplete_visible = session.display_events.iter().any(|event| match event {
            crate::session::DisplayEvent::Message(entry) => {
                entry.message.incomplete.is_some() && entry.message.content == "once upon a"
            }
            _ => false,
        });
        assert!(
            incomplete_visible,
            "incomplete message must surface in display_events for transcript replay"
        );

        // Drive the same startup replay the TUI runs and confirm the
        // incomplete entry lands in the transcript with the marker text.
        let mut app = fresh_app();
        let legacy_fallback = !session
            .display_events
            .iter()
            .any(|event| matches!(event, crate::session::DisplayEvent::Activity(_)));
        for event in session.display_events.iter().cloned() {
            app.replay_display_event(event, legacy_fallback);
        }
        app.finish_legacy_replay();
        if !session.recovered_unmatched.is_empty() {
            app.apply_unmatched_ids(&session.recovered_unmatched);
        }
        let partial_row = app
            .entries
            .iter()
            .find(|entry| entry.text.contains("once upon a"))
            .expect("incomplete assistant row visible after replay");
        assert!(
            partial_row.text.contains("[incomplete response:"),
            "incomplete marker visible in transcript; got {:?}",
            partial_row.text
        );
    }

    #[test]
    fn reset_view_clears_entries_streams_timeline_and_activity_spine() {
        // The reset helper is the single source of truth for view-state
        // reset used by `/clear` and `/new`. It must clear every piece
        // of in-memory view state while preserving runtime settings
        // (selection, theme, mouse). Build a full app, populate
        // everything, then assert each field is empty after the reset.
        let mut app = fresh_app();
        // Populate chat entries (one streaming, one settled), activity
        // spine, scroll, history view.
        app.message("main".into(), Message::new("user", "hi"));
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "partial".into(),
        });
        app.event(UiEvent::Activity(start(
            "tool-1",
            None,
            "main",
            ActivityKind::Tool,
            None,
        )));
        app.event(UiEvent::Activity(end(
            "tool-1",
            "main",
            Some(ActivityStatus::Success),
        )));
        app.scroll = 17;
        app.history_index = 1;
        app.input_history.push("prior".into());
        let prior_generation = app.history_generation;
        let prior_theme = app.theme.clone();
        let prior_selection = app.selection.clone();

        app.reset_view();

        assert!(app.entries.is_empty(), "entries cleared");
        assert!(app.streams.is_empty(), "streams cleared");
        assert!(app.timeline.is_empty(), "timeline cleared");
        assert!(app.activities.is_empty(), "activities cleared");
        assert!(app.activity_index.is_empty(), "activity_index cleared");
        assert!(
            app.pending_activity_ends.is_empty(),
            "pending_activity_ends cleared"
        );
        assert!(
            app.activity_by_context.is_empty(),
            "activity_by_context cleared"
        );
        assert!(
            app.activity_by_external.is_empty(),
            "activity_by_external cleared"
        );
        assert_eq!(app.scroll, 0, "scroll reset to 0");
        assert_eq!(app.history_index, 0, "history view reset to 0");
        assert_eq!(
            app.history_generation,
            prior_generation + 1,
            "history_generation bumped so cached entries invalidate"
        );
        // Runtime settings survive.
        assert_eq!(app.theme, prior_theme, "theme preserved");
        assert_eq!(
            app.selection.agent, prior_selection.agent,
            "selection.agent preserved"
        );
        assert_eq!(
            app.selection.model, prior_selection.model,
            "selection.model preserved"
        );
        // input_history itself is not view state; the helper does not
        // touch it (the `/clear` handler clears it separately).
        assert_eq!(app.input_history, vec!["prior".to_string()]);
    }
}
