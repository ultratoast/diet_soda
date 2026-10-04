//! Terminal lifecycle and a small event loop. Rendering is dirty-driven and
//! capped at 25 FPS, so token bursts never trigger a full redraw per delta.
mod app;
mod commands;
mod input;
mod kitty;
mod picker;
mod render;
mod selection;

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

/// Backend that sizes itself from the pty we render to rather than the
/// controlling terminal.
#[cfg(unix)]
mod stdout_sized {
    use std::io;

    use ratatui::{
        backend::{Backend, ClearType, CrosstermBackend, WindowSize},
        buffer::Cell,
        layout::{Position, Size},
    };

    /// A [`CrosstermBackend`] that measures the terminal it actually renders
    /// to: fd 1 (stdout), the pty every frame of this app is drawn to.
    /// `crossterm::terminal::size()` opens `/dev/tty` first and only falls
    /// back to `STDOUT_FILENO`, so a differently sized controlling terminal
    /// would otherwise win over the window we are drawing in. `size()` is
    /// therefore answered with `ioctl(TIOCGWINSZ)` on fd 1, falling back to
    /// the inner crossterm backend when the ioctl fails or reports a 0x0
    /// grid. Every other method delegates unchanged (`window_size()` keeps
    /// crossterm's `/dev/tty`-based answer for now).
    pub(super) struct StdoutSizedBackend(pub(super) CrosstermBackend<io::Stdout>);

    impl Backend for StdoutSizedBackend {
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.0.draw(content)
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.0.hide_cursor()
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.0.show_cursor()
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            self.0.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.0.set_cursor_position(position)
        }

        fn clear(&mut self) -> io::Result<()> {
            self.0.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            self.0.clear_region(clear_type)
        }

        fn append_lines(&mut self, n: u16) -> io::Result<()> {
            self.0.append_lines(n)
        }

        fn size(&self) -> io::Result<Size> {
            match stdout_winsize() {
                Some(size) => Ok(size),
                None => self.0.size(),
            }
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            self.0.window_size()
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    impl io::Write for StdoutSizedBackend {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            io::Write::write(&mut self.0, buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            io::Write::flush(&mut self.0)
        }
    }

    /// The winsize of fd 1, or `None` when the ioctl fails or reports an
    /// empty (0x0) grid — e.g. when stdout is a pipe rather than a pty.
    #[allow(unsafe_code)] // `ioctl(2)` is the only way to ask a tty for its winsize.
    #[allow(clippy::useless_conversion)] // The constant already matches libc's request type.
    fn stdout_winsize() -> Option<Size> {
        let mut winsize = nix::libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `winsize` is a valid out-pointer of exactly the type
        // TIOCGWINSZ expects and the kernel writes at most its size; a bad or
        // non-tty fd just makes ioctl return an error, which is checked below.
        let status = unsafe {
            nix::libc::ioctl(
                nix::libc::STDOUT_FILENO,
                nix::libc::TIOCGWINSZ.into(),
                &mut winsize,
            )
        };
        if status != 0 || winsize.ws_row == 0 || winsize.ws_col == 0 {
            return None;
        }
        Some(Size {
            width: winsize.ws_col,
            height: winsize.ws_row,
        })
    }
}

#[cfg(unix)]
use self::stdout_sized::StdoutSizedBackend;

/// Build the backend the TUI draws through: on Unix, one that measures fd 1
/// (see [`StdoutSizedBackend`]); elsewhere, stock crossterm sizing.
#[cfg(unix)]
fn tui_backend() -> StdoutSizedBackend {
    StdoutSizedBackend(CrosstermBackend::new(io::stdout()))
}

#[cfg(not(unix))]
fn tui_backend() -> CrosstermBackend<io::Stdout> {
    CrosstermBackend::new(io::stdout())
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
    let mut terminal = Terminal::new(tui_backend())?;
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
                            app.text_selection = None;
                            app.selection_dragging = false;
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
                            if let Some(mouse) = selection::handle_mouse(&mut app, renderer.sel_regions(), mouse) {
                                let target = renderer.activity_at(mouse.column, mouse.row);
                                app.handle_mouse_with_activity_target(mouse, target);
                            }
                            if let Some(text) = app.pending_copy.take() {
                                let mut out = io::stdout();
                                let _ = out.write_all(selection::osc52(&text).as_bytes());
                                let _ = out.flush();
                            }
                        },
                        Some(Ok(Event::Resize(_, _))) => {
                            // Popup and region geometry changes on resize; a live selection would highlight stale cells.
                            app.text_selection = None;
                            app.selection_dragging = false;
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
