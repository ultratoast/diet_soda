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
    {
        let session = engine.session.lock().await;
        app.spend = session.spend.clone();
        app.context_tokens = session.context_tokens;
        app.status = format!("Session {} | /help", session.id);
        for message in &session.messages {
            app.message("main".into(), message.clone());
        }
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
                        Some(Ok(Event::Mouse(mouse))) => app.handle_mouse(mouse),
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
