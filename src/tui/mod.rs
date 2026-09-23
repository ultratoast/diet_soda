//! Terminal lifecycle and a small event loop. Rendering is dirty-driven and
//! capped at 25 FPS, so token bursts never trigger a full redraw per delta.
mod app;
mod commands;
mod input;
mod kitty;
mod picker;
mod render;

pub use crate::workflow::{list_workflows, workflow_path};
pub use input::Input;

use crate::{
    engine::{Engine, Selection},
    hooks,
    model::UiEvent,
};
use anyhow::Result;
use app::App;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    io,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Also used by the panic hook; restoration is best-effort and idempotent.
pub fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Snapshot of the session fields the TUI consumes at startup. Built under
/// the session mutex so the lock can be released before any `App` mutation.
struct StartupSnapshot {
    spend: crate::model::Spend,
    context_tokens: u64,
    status: String,
    display_events: Vec<crate::session::DisplayEvent>,
    legacy_fallback: bool,
    recovered_unmatched: Vec<String>,
}

pub async fn run(
    engine: Engine,
    mut events: mpsc::UnboundedReceiver<UiEvent>,
    config_path: PathBuf,
    initial_workflow: Option<PathBuf>,
    input: String,
    selection: Selection,
) -> Result<()> {
    let config = engine.config.read().await.clone();
    let mut app = App::new(&config, selection);
    // Snapshot the session fields the TUI needs at startup, then drop the
    // mutex before any App mutation. The session is the source of truth
    // for `display_events`, the legacy-fallback decision, and unmatched
    // activity ids — all of which feed the in-memory spine. Holding the
    // mutex across the App mutations would block other engine writers
    // for the entire replay pass and would couple their lifetime to
    // App's borrow checker.
    let snapshot = {
        let session = engine.session.lock().await;
        StartupSnapshot {
            spend: session.spend.clone(),
            context_tokens: session.context_tokens,
            status: format!("Session {} | /help", session.id),
            display_events: session.display_events.clone(),
            // A session predates on-disk activity records iff its retained
            // timeline has no `Activity` entry. `display_events` is the
            // authoritative ordered activity store now that the redundant
            // `Session.activities` slice is gone.
            legacy_fallback: !session
                .display_events
                .iter()
                .any(|event| matches!(event, crate::session::DisplayEvent::Activity(_))),
            recovered_unmatched: session.recovered_unmatched.clone(),
        }
    };
    app.spend = snapshot.spend;
    app.context_tokens = snapshot.context_tokens;
    app.status = snapshot.status;
    let legacy_fallback = snapshot.legacy_fallback;
    for event in snapshot.display_events {
        app.replay_display_event(event, legacy_fallback);
    }
    app.finish_legacy_replay();
    if !snapshot.recovered_unmatched.is_empty() {
        app.apply_unmatched_ids(&snapshot.recovered_unmatched);
    }
    app.refresh_model(&engine).await?;
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    // Tracks the terminal's actual mouse-capture state so `/mouse` toggles can
    // be reconciled against the backend without reinitializing the terminal.
    let mut capture_enabled = true;
    let mut renderer = render::Renderer::default();
    // Pick the launch variant offset once from a UUID so different sessions
    // start on different artwork, then latch it on the renderer before
    // pinning the launch instant. The renderer uses the offset to compute the
    // current variant and to skip a spurious first dirty event.
    renderer.set_variant_offset(kitty::random_offset());
    renderer.set_launch(Instant::now());
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(40));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    if let Some(path) = initial_workflow {
        app.start_workflow(&engine, &path.to_string_lossy(), input)
            .await?;
    }

    let result = async {
        let mut dirty = true;
        while !app.quit {
            tokio::select! {
                Some(event) = events.recv() => {
                    app.event(event);
                    // Bound the batch so a fast provider cannot starve keyboard input.
                    for _ in 0..255 {
                        match events.try_recv() { Ok(event) => app.event(event),Err(_) => break }
                    }
                    dirty = true;
                },
                event = keys.next() => {
                    match event {
                        Some(Ok(Event::Key(key))) => {
                            match app.handle_key(key, &engine).await {
                                Ok(true) => {
                                    if let Err(error) = app.submit(&engine,&config_path).await { app.error(format!("{error:#}")); }
                                }
                                Ok(false) => {}
                                Err(error) => app.error(format!("{error:#}")),
                            }
                        },
                        Some(Ok(Event::Paste(text))) => app.paste(&text),
                        Some(Ok(Event::Mouse(mouse))) => {
                            let target = renderer.activity_at(mouse.column, mouse.row);
                            app.handle_mouse_with_activity_target(mouse, target);
                        },
                        Some(Err(error)) => return Err(error.into()),
                        None => break,
                        _ => {},
                    }
                    dirty = true;
                },
                _ = tick.tick() => {
                    if app.finish_run(&engine,&mut events).await { dirty = true; }
                    if app.picker.as_mut().is_some_and(|picker| picker.poll()) { dirty = true; }
                    if app.approval.as_ref().is_some_and(|a| a.reply.is_closed()) { app.approval = None; dirty = true; }
                    if app.busy.is_some() { dirty = true; }
                    if renderer.variant_dirty(Instant::now()) { dirty = true; }
                    if dirty { terminal.draw(|frame| renderer.draw(frame,&app))?; dirty = false; }
                },
            }
            // Reconcile terminal mouse capture with the app's desired state
            // promptly after `/mouse` (or any other mutation) changes it.
            if app.mouse_enabled != capture_enabled {
                let backend = terminal.backend_mut();
                if app.mouse_enabled {
                    execute!(backend, EnableMouseCapture)?;
                } else {
                    execute!(backend, DisableMouseCapture)?;
                }
                backend.flush()?;
                capture_enabled = app.mouse_enabled;
            }
        }
        Ok::<_,anyhow::Error>(())
    }.await;

    app.cancel_and_join().await;
    engine.mcp.shutdown().await;
    engine.session.lock().await.checkpoint()?;
    let config = engine.config.read().await.clone();
    let _ = hooks::emit(
        &config,
        "shutdown",
        serde_json::json!({}),
        &CancellationToken::new(),
    )
    .await;
    let session_id = engine.session.lock().await.id.clone();
    drop(_guard);
    println!("Resume this session with: diet_soda --session {session_id}");
    result
}
