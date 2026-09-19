//! CLI setup and a headless consumer for the same events used by the TUI.
use anyhow::{bail, Context, Result};
use clap::Parser;
use diet_soda::{
    config::Config,
    engine::{Engine, Selection},
    hooks,
    model::{Decision, UiEvent},
    session::Session,
    skills, tools, tui,
    workflow::{self, Workflow},
};
use std::{
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
    let path = match args.config {
        Some(path) => diet_soda::config::resolve_path(&std::env::current_dir()?, &path),
        None => Config::default_path()?,
    };
    if args.init {
        let directory = path
            .parent()
            .context("Config path has no parent directory")?;
        std::fs::create_dir_all(directory)
            .with_context(|| format!("Creating config directory {}", directory.display()))?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "Creating {}; existing files are never overwritten",
                    path.display()
                )
            })?;
        let mut document = serde_json::to_value(Config::default())?;
        document["agents"] = diet_soda::config::default_agent_entries();
        document["system_prompt"] = serde_json::json!("./AGENTS.md");
        document["theme"] = serde_json::json!("./theme.json");
        serde_json::to_writer_pretty(file, &document)?;
        for name in ["workflows", "skills"] {
            std::fs::create_dir_all(directory.join(name))?;
        }
        for (name, contents) in [
            ("AGENTS.md", include_str!("../examples/AGENTS.md")),
            ("theme.json", include_str!("../examples/theme.json")),
            ("bash-permissions.json", tools::DEFAULT_BASH_PERMISSIONS),
            (
                "CONFIGURATION.md",
                include_str!("../examples/CONFIGURATION.md"),
            ),
            (
                "QUEUE_AND_ACCESS.md",
                include_str!("../examples/QUEUE_AND_ACCESS.md"),
            ),
        ] {
            let target = directory.join(name);
            if !target.exists() {
                std::fs::write(target, contents)?;
            }
        }
        std::fs::create_dir_all(directory.join("prompts"))?;
        for (name, contents) in [
            ("plan.md", include_str!("../examples/prompts/plan.md")),
            ("build.md", include_str!("../examples/prompts/build.md")),
            (
                "code-review.md",
                include_str!("../examples/prompts/code-review.md"),
            ),
            (
                "plan-review.md",
                include_str!("../examples/prompts/plan-review.md"),
            ),
            ("debug.md", include_str!("../examples/prompts/debug.md")),
            (
                "research.md",
                include_str!("../examples/prompts/research.md"),
            ),
            ("explore.md", include_str!("../examples/prompts/explore.md")),
            (
                "test-runner.md",
                include_str!("../examples/prompts/test-runner.md"),
            ),
            (
                "test-writer.md",
                include_str!("../examples/prompts/test-writer.md"),
            ),
            (
                "general-purpose.md",
                include_str!("../examples/prompts/general-purpose.md"),
            ),
            (
                "converse.md",
                include_str!("../examples/prompts/converse.md"),
            ),
            (
                "elephant.md",
                include_str!("../examples/prompts/elephant.md"),
            ),
        ] {
            let target = directory.join("prompts").join(name);
            if !target.exists() {
                std::fs::write(target, contents)?;
            }
        }
        println!("Created {}", path.display());
        return Ok(());
    }
    let config = Config::load(&path).with_context(|| {
        format!(
            "Use --init --config {} to create a configuration",
            path.display()
        )
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
    std::fs::create_dir_all(&config.sessions_dir)?;
    let log = std::fs::OpenOptions::new()
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
                UiEvent::Delta { text,.. } => { print!("{text}"); io::stdout().flush()?; },
                UiEvent::Status(text) => eprintln!("{text}"),
                UiEvent::Spend(spend) => eprintln!("\nSpend: {}",spend.display()),
                UiEvent::Approval { title,detail,workflow,reply } => {
                    if !io::stdin().is_terminal() { let _ = reply.send(Decision::Abort); cancel.cancel(); continue; }
                    eprintln!("\n{title}\n{detail}\n{}",if workflow { "[y] continue [r] retry [s] skip [q] abort" } else { "[y] approve [n] reject [q] abort" });
                    let decision = headless_approval(workflow).await?;
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

async fn headless_approval(workflow: bool) -> Result<Decision> {
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
            match key.code {
                KeyCode::Char('y') => return Ok(Decision::Approve),
                KeyCode::Char('r') if workflow => return Ok(Decision::Retry),
                KeyCode::Char('s') if workflow => return Ok(Decision::Skip),
                KeyCode::Char('q') => return Ok(Decision::Abort),
                KeyCode::Char('n') | KeyCode::Esc => return Ok(Decision::Reject),
                _ => {}
            }
        }
    }
    Ok(Decision::Abort)
}
