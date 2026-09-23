//! CLI setup and a headless consumer for the same events used by the TUI.
use anyhow::{bail, Context, Result};
use clap::Parser;
use diet_soda::{
    config::Config,
    engine::{Engine, Selection},
    fsutil, hooks, init,
    model::{Decision, UiEvent},
    session::Session,
    skills,
    text::sanitize_terminal_text,
    tui,
    workflow::{self, Workflow},
};
use std::{
    borrow::Cow,
    io::{self, IsTerminal, Write},
    path::PathBuf,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "A small, extensible terminal agent harness")]
struct Cli {
    /// Config file (default: ~/.config/diet_soda/config.json).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Create editable config and workflow/skill directories; never overwrite.
    #[arg(long)]
    init: bool,
    #[arg(long)]
    validate_config: bool,
    #[arg(long)]
    validate_workflow: Option<PathBuf>,
    #[arg(long)]
    workflow: Option<PathBuf>,
    #[arg(long, default_value = "")]
    input: String,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    list_workflows: bool,
    #[arg(long)]
    list_skills: bool,
    #[arg(long)]
    install_skill: Option<String>,
    /// Run a single prompt without the TUI. Approvals fail closed when stdin is not a terminal.
    #[arg(long)]
    prompt: Option<String>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    effort: Option<diet_soda::config::Effort>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    let explicit = args.config.is_some();
    let path = match args.config {
        Some(path) => diet_soda::config::resolve_path(&std::env::current_dir()?, &path),
        None => Config::default_path()?,
    };
    if args.init {
        // Short-circuit so `--init` runs even when a previous config exists,
        // surfacing the explicit no-overwrite error rather than silently
        // accepting an already-present file. The inner error already names
        // the path, so no extra context is added here.
        init::initialize(&path)?;
        println!("Created {}", path.display());
        return Ok(());
    }
    if !explicit && !path.exists() {
        // Implicit default path with no existing config: initialize once on
        // the user's behalf. Status goes to stderr so scripted `--prompt`
        // output stays clean. A racing process that published the config
        // first is fine; we then proceed to load.
        let mut sink = std::io::stderr().lock();
        init::auto_initialize(&path, &mut sink)?;
    }
    let config = Config::load(&path).with_context(|| {
        if explicit {
            format!(
                "Use --init --config {} to create a configuration",
                path.display()
            )
        } else {
            format!(
                "Automatic initialization did not produce {}; pass --config <path> or run --init",
                path.display()
            )
        }
    })?;
    if args.validate_config {
        println!("Configuration is valid");
        return Ok(());
    }
    if let Some(path) = args.validate_workflow {
        let path = workflow::workflow_path(&path.to_string_lossy(), &config);
        let w = Workflow::load(&path, &config)?;
        println!("Workflow is valid: {} ({} steps)", w.title, w.steps.len());
        return Ok(());
    }
    if args.list_workflows {
        for path in tui::list_workflows(&config)? {
            println!("{path}");
        }
        return Ok(());
    }
    if args.list_skills {
        for skill in skills::discover(&config)? {
            println!("{} — {}", skill.name, skill.description);
        }
        return Ok(());
    }
    if let Some(source) = args.install_skill {
        println!(
            "Installed {}",
            skills::install(&source, &config.skills_dir)
                .await?
                .display()
        );
        return Ok(());
    }
    fsutil::create_dir_all_private(&config.sessions_dir)?;
    let log = fsutil::private_open_options()
        .create(true)
        .append(true)
        .open(config.sessions_dir.join("diet_soda.log"))?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::sync::Mutex::new(log))
        .init();
    let session = Session::open(&config.sessions_dir, args.session.as_deref())?;
    hooks::emit(
        &config,
        "session_start",
        serde_json::json!({"session_id":session.id}),
        &CancellationToken::new(),
    )
    .await?;
    let (tx, rx) = mpsc::unbounded_channel();
    let engine = Engine::new(config, session, tx);
    if args.prompt.is_some() || !io::stdout().is_terminal() {
        let selection = Selection {
            agent: args.agent,
            agent_mode: None,
            model: args.model,
            effort: args.effort,
        };
        return headless(
            engine,
            rx,
            args.prompt,
            args.workflow,
            args.input,
            selection,
        )
        .await;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tui::restore_terminal();
        previous(info);
    }));
    tui::run(
        engine,
        rx,
        path,
        args.workflow,
        args.input,
        Selection {
            agent: args.agent,
            agent_mode: None,
            model: args.model,
            effort: args.effort,
        },
    )
    .await
}

async fn headless(
    engine: Engine,
    mut events: mpsc::UnboundedReceiver<UiEvent>,
    prompt: Option<String>,
    workflow_path: Option<PathBuf>,
    input: String,
    selection: Selection,
) -> Result<()> {
    let cancel = CancellationToken::new();
    // Sanitize terminal-bound output only when a human is watching; a redirected
    // stream is machine input for scripts and must pass through byte-for-byte.
    let stdout_terminal = io::stdout().is_terminal();
    let stderr_terminal = io::stderr().is_terminal();
    let worker = engine.clone();
    let token = cancel.clone();
    let workflow = if let Some(path) = workflow_path {
        let config = engine.config.read().await;
        let path = workflow::workflow_path(&path.to_string_lossy(), &config);
        Some(Workflow::load(&path, &config)?)
    } else {
        None
    };
    if workflow.is_none() && prompt.is_none() {
        bail!(
            "A terminal is required for the TUI; use --prompt or --workflow for headless execution"
        );
    }
    let mut task = tokio::spawn(async move {
        if let Some(workflow) = workflow {
            workflow::run(&worker, workflow, prompt.unwrap_or(input), selection, token).await
        } else {
            worker.turn(prompt.unwrap(), selection, token).await
        }
    });
    let result = loop {
        tokio::select! {
            biased;
            Some(event) = events.recv() => match event {
                UiEvent::Delta { text,.. } => {
                    if stdout_terminal {
                        print!("{}", sanitize_terminal_text(&text, true));
                    } else {
                        print!("{text}");
                    }
                    io::stdout().flush()?;
                },
                UiEvent::Status { context, text } => {
                    let text = if stderr_terminal {
                        sanitize_terminal_text(&text, true)
                    } else {
                        Cow::Borrowed(text.as_str())
                    };
                    if context == "main" {
                        eprintln!("{text}");
                    } else {
                        eprintln!("[{context}] {text}");
                    }
                },
                UiEvent::Spend(spend) => eprintln!("\nSpend: {}",spend.display()),
                UiEvent::Approval { title,detail,workflow,persist_allowed,reply } => {
                    if !io::stdin().is_terminal() { let _ = reply.send(Decision::Abort); cancel.cancel(); continue; }
                    let title = if stderr_terminal {
                        sanitize_terminal_text(&title, false)
                    } else {
                        Cow::Borrowed(title.as_str())
                    };
                    let detail = if stderr_terminal {
                        sanitize_terminal_text(&detail, true)
                    } else {
                        Cow::Borrowed(detail.as_str())
                    };
                    let choices = if workflow {
                        "[y] continue [r] retry [s] skip [a] abort"
                    } else if persist_allowed {
                        "[y] yes [p] yes-persist [n] no [a] abort"
                    } else {
                        "[y] yes [n] no [a] abort"
                    };
                    eprintln!("\n{title}\n{detail}\n{choices}");
                    let decision = headless_approval(workflow, persist_allowed).await?;
                    if decision == Decision::Abort { cancel.cancel(); }
                    let _ = reply.send(decision);
                },
                _ => {},
            },
            result = &mut task => break result?,
            _ = tokio::signal::ctrl_c() => cancel.cancel(),
        }
    };
    println!();
    engine.mcp.shutdown().await;
    let config = engine.config.read().await.clone();
    let _ = hooks::emit(
        &config,
        "shutdown",
        serde_json::json!({}),
        &CancellationToken::new(),
    )
    .await;
    result.map(|_| ())
}

async fn headless_approval(workflow: bool, persist_allowed: bool) -> Result<Decision> {
    use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
    use futures_util::StreamExt;
    struct RawGuard;
    impl Drop for RawGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    crossterm::terminal::enable_raw_mode()?;
    let _guard = RawGuard;
    let mut events = EventStream::new();
    while let Some(event) = events.next().await {
        if let Event::Key(key) = event? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(Decision::Abort);
            }
            let unmodified = key.modifiers.is_empty();
            match key.code {
                KeyCode::Char('y') if unmodified => return Ok(Decision::Approve),
                KeyCode::Char('p') if unmodified && !workflow && persist_allowed => {
                    return Ok(Decision::ApprovePersist)
                }
                KeyCode::Char('r') if unmodified && workflow => return Ok(Decision::Retry),
                KeyCode::Char('s') if unmodified && workflow => return Ok(Decision::Skip),
                KeyCode::Char('a' | 'q') if unmodified => return Ok(Decision::Abort),
                KeyCode::Char('n') if unmodified => return Ok(Decision::Reject),
                KeyCode::Esc => return Ok(Decision::Reject),
                _ => {}
            }
        }
    }
    Ok(Decision::Abort)
}
