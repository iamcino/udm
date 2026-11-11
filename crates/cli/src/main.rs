//! # udm-cli: standalone command-line downloads with Usagi
//!
//! Uses the same `usagi` engine as Universal Download Manager's desktop
//! app. Commands download/probe URLs, import lists, inspect or change tasks,
//! update supported settings and report local diagnostics. No telemetry,
//! administrator rights, service or update check is involved. `update pin`
//! only prints the compiled publisher fingerprint; it downloads, stages
//! and installs nothing.
//!
//! This process opens its own engine/store; it does not control a running
//! GUI through IPC. Exit the GUI from the tray before using its database,
//! or choose a separate `--data-dir`. `watch` resumes eligible work; it is
//! not a read-only status command. `get --no-wait` does not leave a daemon.
//!
//! ```text
//! udm-cli probe https://example.com/file.iso --h3
//! udm-cli get https://example.com/file.iso --segments 8
//! udm-cli list
//! udm-cli settings --list
//! udm-cli settings segments_per_task=12
//! udm-cli doctor
//! udm-cli update pin
//! ```
//!
//! Replace example URLs with real file links. Segment limits describe
//! concurrent requests, not measured physical connections. Removal can
//! delete owned partials under the default policy even without `--files`.
//! See command `--help` and the repository's `docs/OPERATIONS.md` before
//! destructive task operations or moving an existing database.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use usagi::config::{Settings, TransportPref};
use usagi::engine::{watch_resumes, Engine, EngineBuilder, Event, LogLevel, TaskSnapshot};
use usagi::error::Result;
use usagi::hash::ExpectedHash;
use usagi::net::stack::NetStack;
use usagi::net::{tls, MemAltSvc};
use usagi::paths::Paths;
use usagi::store::{Store, TaskState};
use usagi::util;

#[derive(Parser)]
#[command(
    name = "udm-cli",
    version = usagi::VERSION,
    about = "Download manager engine: HTTP/1.1, HTTP/2 and HTTP/3 (QUIC). No telemetry, no administrator rights.",
    long_about = None
)]
#[derive(Clone)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Use this data directory instead of %LOCALAPPDATA%\udm.
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Log level: off, error, warn, info, debug, trace.
    #[arg(long, global = true, default_value = "warn")]
    log: String,

    /// Force portable mode (application data in <exe directory>/data).
    #[arg(long, global = true)]
    portable: bool,

    /// Never touch the network except for the URLs you pass (disables
    /// Alt-Svc learning).
    #[arg(long, global = true)]
    no_learning: bool,
}

#[derive(Subcommand, Clone)]
enum Command {
    /// Download one or more URLs and wait for them to finish.
    Get(GetArgs),
    /// Ask a server what it offers (size, Range support, HTTP/3) without downloading.
    Probe(ProbeArgs),
    /// Import URLs from a text file (one per line, `#` starts a comment).
    Import(ImportArgs),
    /// List the tasks stored in the database.
    List,
    /// Resume non-terminal tasks and wait; skip Completed, Failed and Canceled.
    Watch,
    /// Pause tasks.
    Pause { ids: Vec<u64> },
    /// Resume tasks.
    Resume { ids: Vec<u64> },
    /// Stop tasks (keeps the partial file).
    Cancel { ids: Vec<u64> },
    /// Clear the error and try again.
    Retry { ids: Vec<u64> },
    /// Remove tasks from the list.
    Remove {
        ids: Vec<u64>,
        /// Also delete the downloaded / partial files.
        #[arg(long)]
        files: bool,
    },
    /// Show or change settings.
    Settings(SettingsArgs),
    /// Update trust: print the pinned publisher fingerprint (first-trust
    /// check). No network activity; the fingerprint is a compiled constant.
    Update(UpdateArgs),
    /// Report paths, TLS trust store, QUIC availability and telemetry status.
    Doctor,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum TransportArg {
    Auto,
    #[value(name = "h3", alias = "prefer-h3")]
    PreferH3,
    #[value(name = "h3-only")]
    H3Only,
    #[value(name = "no-h3")]
    NoH3,
}

impl From<TransportArg> for TransportPref {
    fn from(value: TransportArg) -> Self {
        match value {
            TransportArg::Auto => TransportPref::Auto,
            TransportArg::PreferH3 => TransportPref::PreferHttp3,
            TransportArg::H3Only => TransportPref::Http3Only,
            TransportArg::NoH3 => TransportPref::NoHttp3,
        }
    }
}

#[derive(Args, Clone)]
struct GetArgs {
    /// One or more http(s) URLs.
    #[arg(required = true)]
    urls: Vec<String>,

    /// Destination directory (default: the system Downloads folder).
    #[arg(short = 'd', long, value_name = "DIR")]
    dir: Option<PathBuf>,

    /// File name to save as (only valid with a single URL).
    #[arg(short = 'o', long, value_name = "NAME")]
    out: Option<String>,

    /// Byte-range requests per task (default: from settings, usually 8).
    #[arg(short = 's', long, value_name = "N")]
    segments: Option<u32>,

    /// Expected checksum, e.g. `sha256:9f86d0...` or a bare hex digest.
    #[arg(long, value_name = "ALGO:HEX")]
    hash: Option<String>,

    /// Transport preference for these tasks.
    #[arg(long, value_enum)]
    transport: Option<TransportArg>,

    /// Add to the list without starting the transfer.
    #[arg(long)]
    paused: bool,

    /// Do not draw the live progress display.
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Exit as soon as the tasks are queued (fire and forget).
    #[arg(long)]
    no_wait: bool,
}

#[derive(Args, Clone)]
struct ProbeArgs {
    url: String,

    /// Dial QUIC even without a cached Alt-Svc advertisement.
    #[arg(long)]
    h3: bool,

    /// Never use HTTP/3.
    #[arg(long)]
    no_h3: bool,

    /// Print every response header.
    #[arg(long)]
    headers: bool,
}

#[derive(Args, Clone)]
struct ImportArgs {
    /// Text file with one URL per line.
    file: PathBuf,

    #[arg(short = 'd', long, value_name = "DIR")]
    dir: Option<PathBuf>,

    #[arg(short = 's', long, value_name = "N")]
    segments: Option<u32>,

    #[arg(long, value_enum)]
    transport: Option<TransportArg>,

    #[arg(long)]
    paused: bool,

    #[arg(short = 'q', long)]
    quiet: bool,
}

#[derive(Args, Clone)]
struct SettingsArgs {
    /// `key=value` pairs, e.g. `segments_per_task=16`.
    pairs: Vec<String>,

    /// Print every setting and exit.
    #[arg(long)]
    list: bool,

    /// Restore factory defaults.
    #[arg(long)]
    reset: bool,
}

#[derive(Args, Clone)]
struct UpdateArgs {
    #[command(subcommand)]
    command: UpdateCommand,
}

#[derive(Subcommand, Clone)]
enum UpdateCommand {
    /// Print the pinned publisher fingerprint so it can be compared out of
    /// band against the fingerprint published in the signed release notes.
    /// No network activity; nothing is downloaded, staged or installed.
    Pin,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(&cli.log);
    if cli.portable {
        std::env::set_var(usagi::paths::PORTABLE_ENV, "1");
    }
    if cli.no_learning {
        std::env::set_var("UDM_NO_LEARNING", "1");
    }

    let result = run(cli).await;
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("udm-cli: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    // Clone so the handlers can still borrow the global flags.
    let command = cli.command.clone();
    match command {
        Command::Get(args) => cmd_get(&cli, args).await,
        Command::Probe(args) => cmd_probe(&cli, args).await,
        Command::Import(args) => cmd_import(&cli, args).await,
        Command::List => cmd_list(&cli),
        Command::Watch => cmd_watch(&cli).await,
        Command::Pause { ids } => simple(&cli, |e, id| e.pause(id), &ids, "paused"),
        Command::Resume { ids } => simple(&cli, |e, id| e.resume(id), &ids, "queued"),
        Command::Cancel { ids } => simple(&cli, |e, id| e.cancel(id), &ids, "cancelled"),
        Command::Retry { ids } => simple(&cli, |e, id| e.retry(id), &ids, "retrying"),
        Command::Remove { ids, files } => {
            let engine = build_engine(&cli, true)?;
            for id in &ids {
                engine.remove(*id, files)?;
            }
            drain_settle(&engine);
            println!("removed {} task(s)", ids.len());
            engine.shutdown();
            Ok(ExitCode::SUCCESS)
        }
        Command::Settings(args) => cmd_settings(&cli, args),
        Command::Update(args) => cmd_update(args),
        Command::Doctor => cmd_doctor(&cli),
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

async fn cmd_get(cli: &Cli, args: GetArgs) -> Result<ExitCode> {
    if args.out.is_some() && args.urls.len() != 1 {
        return Err(usagi::Error::Other(
            "--out can only be used with exactly one URL".into(),
        ));
    }
    let expected = match &args.hash {
        Some(spec) => Some(ExpectedHash::parse(spec)?),
        None => None,
    };
    let engine = build_engine(cli, false)?;
    let mut ids = Vec::new();
    for url in &args.urls {
        let mut request = usagi::AddRequest::new(url.clone());
        if let Some(dir) = &args.dir {
            request = request.dest_dir(dir.clone());
        }
        if let Some(name) = &args.out {
            request = request.filename(name.clone());
        }
        if let Some(segments) = args.segments {
            request = request.segments(segments);
        }
        if let Some(hash) = &expected {
            request = request.expected_hash(hash.clone());
        }
        if let Some(transport) = args.transport {
            request = request.transport_pref(transport.into());
        }
        request = request.start_paused(args.paused);
        let id = engine.add(request)?;
        ids.push(id);
        println!("queued #{id}: {url}");
    }
    if args.no_wait || args.paused {
        engine.shutdown();
        return Ok(ExitCode::SUCCESS);
    }
    let code = wait_for(&engine, &ids, !args.quiet).await;
    engine.shutdown();
    Ok(code)
}

async fn cmd_probe(cli: &Cli, args: ProbeArgs) -> Result<ExitCode> {
    let mut settings = load_settings(cli)?;
    settings.learn_alt_svc = !cli.no_learning && settings.learn_alt_svc;
    let pref: TransportPref = if args.no_h3 {
        TransportPref::NoHttp3
    } else if args.h3 {
        TransportPref::PreferHttp3
    } else {
        settings.transport_pref
    };

    let stack = NetStack::new(&settings, Arc::new(MemAltSvc::new()))?;
    println!(
        "probing {} (transport preference: {})",
        args.url,
        pref.as_str()
    );
    println!(
        "  HTTP/3 stack in this process: {}",
        if stack.http3_available() {
            "available"
        } else {
            "unavailable (QUIC endpoint could not be created)"
        }
    );

    let started = Instant::now();
    match stack.probe(&args.url, pref).await {
        Ok(info) => {
            let elapsed = started.elapsed();
            println!("  status:            HTTP {}", info.status);
            println!("  transport:         {}", info.transport.as_str());
            println!("  final URL:         {}", info.final_url);
            println!("  size:              {}", util::human_bytes_opt(info.size));
            println!(
                "  range requests:    {}",
                if info.accept_ranges {
                    "supported"
                } else {
                    "NOT supported"
                }
            );
            println!("  etag:              {}", opt(&info.etag));
            println!("  last-modified:     {}", opt(&info.last_modified));
            println!("  content-type:      {}", opt(&info.content_type));
            println!("  filename hint:     {}", opt(&info.filename_hint));
            println!("  probe took:        {:.0?}", elapsed);
            match &info.learned_alt_svc {
                Some(entry) => println!(
                    "  alt-svc:           HTTP/3 on {}:{} (valid {}s)",
                    entry.host, entry.port, entry.max_age_secs
                ),
                None => println!("  alt-svc:           no HTTP/3 advertised"),
            }
            if args.headers {
                println!("  headers:");
                for (k, v) in info.headers_vec() {
                    println!("    {k}: {v}");
                }
            }
            let resumable = info.accept_ranges && info.size.unwrap_or(0) > 0;
            println!(
                "  segmented resume:  {}",
                if resumable {
                    format!(
                        "yes, up to {} concurrent byte-range requests",
                        settings.segments_per_task
                    )
                } else {
                    "no - a single request will be used".to_string()
                }
            );
            stack.shutdown().await;
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            println!("  probe failed: {e}");
            stack.shutdown().await;
            Ok(ExitCode::from(2))
        }
    }
}

async fn cmd_import(cli: &Cli, args: ImportArgs) -> Result<ExitCode> {
    let content =
        std::fs::read_to_string(&args.file).map_err(|e| usagi::Error::io(args.file.clone(), e))?;
    let urls: Vec<String> = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split_whitespace().next().unwrap_or("").to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if urls.is_empty() {
        return Err(usagi::Error::Other(format!(
            "{} contains no URLs",
            util::display_path(&args.file)
        )));
    }
    let get = GetArgs {
        urls,
        dir: args.dir,
        out: None,
        segments: args.segments,
        hash: None,
        transport: args.transport,
        paused: args.paused,
        quiet: args.quiet,
        no_wait: false,
    };
    println!("importing {} URL(s)", get.urls.len());
    cmd_get(cli, get).await
}

fn cmd_list(cli: &Cli) -> Result<ExitCode> {
    let paths = resolve_paths(cli)?;
    let store = Store::open(&paths.db_file)?;
    store.recover_finalizations()?;
    let tasks = store.list_tasks()?;
    if tasks.is_empty() {
        println!(
            "no tasks (database: {})",
            util::display_path(&paths.db_file)
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:>4}  {:<11}  {:>10}  {:>10}  {:<6}  {}",
        "id", "state", "size", "done", "proto", "file"
    );
    for task in tasks {
        let record = &task.record;
        let mut received: u64 = task.segments.iter().map(|s| s.done).sum();
        if record.state == TaskState::Completed {
            // Segment rows are dropped once a task finishes; the file itself is
            // the ground truth for "done".
            if let Ok(md) = std::fs::metadata(record.final_path()) {
                received = md.len();
            }
        }
        let proto = if matches!(
            record.transport_summary,
            usagi::store::records::TransportSummary::Mixed(_)
        ) {
            "Mixed".to_string()
        } else {
            record
                .transport
                .map(|t| t.short().to_string())
                .unwrap_or_else(|| "-".to_string())
        };
        println!(
            "{:>4}  {:<11}  {:>10}  {:>10}  {:<6}  {}",
            record.id,
            record.state.as_str(),
            util::human_bytes_opt(record.total_size),
            util::human_bytes(received as f64),
            proto,
            util::display_path(&record.final_path())
        );
        if let Some(err) = &record.error {
            println!("      error: {err}");
        }
    }
    let stats = store.stats()?;
    println!(
        "\n{} task(s), {} completed, {} failed, {} on disk, database {}",
        stats.tasks,
        stats.completed,
        stats.failed,
        util::human_bytes(stats.bytes_on_disk as f64),
        util::human_bytes(stats.db_bytes as f64)
    );
    Ok(ExitCode::SUCCESS)
}

async fn cmd_watch(cli: &Cli) -> Result<ExitCode> {
    let engine = build_engine(cli, false)?;
    let ids: Vec<u64> = engine
        .snapshot()
        .iter()
        .filter(|s| watch_resumes(s.state))
        .map(|s| s.id)
        .collect();
    for id in &ids {
        let _ = engine.resume(*id);
    }
    if ids.is_empty() {
        println!("nothing to resume");
        engine.shutdown();
        return Ok(ExitCode::SUCCESS);
    }
    println!("resuming {} task(s)", ids.len());
    let code = wait_for(&engine, &ids, true).await;
    engine.shutdown();
    Ok(code)
}

fn simple(
    cli: &Cli,
    action: impl Fn(&Engine, u64) -> Result<()>,
    ids: &[u64],
    verb: &str,
) -> Result<ExitCode> {
    if ids.is_empty() {
        return Err(usagi::Error::Other("no task ids given".into()));
    }
    let engine = build_engine(cli, true)?;
    for id in ids {
        action(&engine, *id)?;
        println!("#{id} {verb}");
    }
    drain_settle(&engine);
    engine.shutdown();
    Ok(ExitCode::SUCCESS)
}

fn cmd_settings(cli: &Cli, args: SettingsArgs) -> Result<ExitCode> {
    let paths = resolve_paths(cli)?;
    let store = Store::open(&paths.db_file)?;
    let mut settings = store.load_settings()?.unwrap_or_default();
    if args.reset {
        settings = Settings::default();
        store.save_settings(&settings)?;
        println!("settings restored to defaults");
    }
    for pair in &args.pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| usagi::Error::Other(format!("`{pair}` is not a key=value pair")))?;
        settings.apply_pair(key, value)?;
        store.save_settings(&settings)?;
        println!("{key} = {value}");
    }
    if args.list || args.pairs.is_empty() {
        for (key, value) in settings.to_pairs() {
            println!("{key:>24} = {value}");
        }
        println!(
            "\n(download directory resolves to {})",
            util::display_path(&settings.download_dir())
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_update(args: UpdateArgs) -> Result<ExitCode> {
    match args.command {
        UpdateCommand::Pin => cmd_update_pin(),
    }
}

fn cmd_update_pin() -> Result<ExitCode> {
    // A failed self-check means the compiled constants were transcribed
    // wrong; refuse to print a fingerprint that cannot be trusted.
    if !usagi::update_pin::pins_self_consistent() {
        return Err(usagi::Error::Other(
            "update pin self-check failed: the compiled pin does not match the publisher public key".into(),
        ));
    }
    println!(
        "publisher pin (SHA-256): {}",
        usagi::update_pin::publisher_pin()
    );
    println!(
        "publisher public key:    {}",
        usagi::update_pin::publisher_public_key_hex()
    );
    println!(
        "recovery pin (SHA-256):  {}",
        usagi::update_pin::recovery_pin()
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_doctor(cli: &Cli) -> Result<ExitCode> {
    let paths = resolve_paths(cli)?;
    println!("udm {}", usagi::VERSION);
    println!("  built with:        Rust {}", rustc_version());
    println!("  debug build:       {}", usagi::IS_DEBUG);
    println!();
    println!("storage (no administrator rights are used anywhere):");
    for line in paths.describe().lines() {
        println!("  {line}");
    }
    println!("  database exists:   {}", paths.db_file.exists());
    if paths.db_file.exists() {
        match Store::open(&paths.db_file) {
            Ok(store) => match store.stats() {
                Ok(stats) => {
                    println!("  tasks:             {}", stats.tasks);
                    println!("  completed:         {}", stats.completed);
                    println!("  failed:            {}", stats.failed);
                    println!(
                        "  bytes on disk:     {}",
                        util::human_bytes(stats.bytes_on_disk as f64)
                    );
                    println!(
                        "  database size:     {}",
                        util::human_bytes(stats.db_bytes as f64)
                    );
                    println!("  schema version:    {}", stats.schema_version);
                    println!("  HTTP/3 endpoints:  {}", stats.alt_svc_entries);
                    for entry in store.alt_svc_all()? {
                        println!(
                            "    {}:{} (learned {}, {} failure(s))",
                            entry.host,
                            entry.port,
                            util::format_timestamp(entry.learned_at),
                            entry.failures
                        );
                    }
                }
                Err(e) => println!("  stats failed:      {e}"),
            },
            Err(e) => println!("  cannot open:       {e}"),
        }
    }
    println!();
    println!("TLS:");
    match tls::native_roots() {
        Ok((roots, rejected)) => println!(
            "  trust anchors:     {} from the OS trust store ({} unparsable)",
            roots.len(),
            rejected
        ),
        Err(e) => println!("  trust store error: {e}"),
    }
    println!("  provider:          ring (rustls); no OpenSSL, no schannel");
    println!();
    println!("QUIC / HTTP3:");
    let settings = load_settings(cli)?;
    match NetStack::new(&settings, Arc::new(MemAltSvc::new())) {
        Ok(stack) => {
            println!(
                "  quic endpoint:     {}",
                if stack.http3_available() {
                    "created (UDP, ephemeral port, no listening TCP port)"
                } else {
                    "UNAVAILABLE - HTTP/2 and HTTP/1.1 will be used"
                }
            );
            println!("  http3 enabled:     {}", settings.http3_enabled);
            println!("  preference:        {}", settings.transport_pref.as_str());
        }
        Err(e) => println!("  stack error:       {e}"),
    }
    println!();
    println!("privacy:");
    println!("  telemetry:         none (no analytics, no crash reports, no update pings)");
    println!(
        "  outbound hosts:    only the ones you enter{}",
        if usagi::TELEMETRY_ENDPOINTS.is_empty() {
            ""
        } else {
            " (!)"
        }
    );
    println!("  user-agent:        {}", settings.user_agent);
    println!(
        "  system proxy:      {}",
        if settings.use_system_proxy {
            "honoured (can be turned off in settings)"
        } else {
            "ignored"
        }
    );
    println!(
        "  log file:          {}",
        util::display_path(&paths.log_file)
    );
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn resolve_paths(cli: &Cli) -> Result<Paths> {
    let mut paths = Paths::discover()?;
    if let Some(dir) = &cli.data_dir {
        paths.root = dir.clone();
        paths.db_file = dir.join("udm.redb");
        paths.log_file = dir.join("logs").join("udm.log");
        paths.temp_dir = dir.join("tmp");
    }
    paths.ensure_dirs()?;
    Ok(paths)
}

fn load_settings(cli: &Cli) -> Result<Settings> {
    let paths = resolve_paths(cli)?;
    let mut settings = if paths.db_file.exists() {
        let store = Store::open(&paths.db_file)?;
        store.load_settings()?.unwrap_or_default()
    } else {
        Settings::default()
    };
    if cli.no_learning {
        settings.learn_alt_svc = false;
    }
    Ok(settings)
}

fn build_engine(cli: &Cli, idle: bool) -> Result<Engine> {
    let paths = resolve_paths(cli)?;
    EngineBuilder::new().paths(paths).idle(idle).build()
}

fn rustc_version() -> &'static str {
    // Filled in at compile time by build-script-free means.
    option_env!("UDM_RUSTC").unwrap_or("unknown")
}

fn opt(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "-".to_string())
}

/// Give the supervisor a moment to process a command.
fn drain_settle(engine: &Engine) {
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(25));
        let _ = engine.drain_events(64);
    }
}

/// Wait for Ctrl+C - or SIGTERM on Unix - so an interrupt still goes through
/// the graceful path: `wait_for` returns, `main` drops the `Engine`, and the
/// engine's shutdown flushes progress durably before the process exits.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            // No SIGTERM support (should not happen on Unix): fall back to Ctrl+C.
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Render progress until every id reaches a terminal state.
async fn wait_for(engine: &Engine, ids: &[u64], live: bool) -> ExitCode {
    let tty = std::io::stdout().is_terminal() && live;
    let started = Instant::now();
    let mut lines_drawn = 0usize;
    let mut exit = ExitCode::SUCCESS;

    loop {
        tokio::select! {
            _ = shutdown_signal() => {
                clear_lines(lines_drawn);
                println!("\ninterrupted - progress is saved; resume with `udm-cli watch`");
                return ExitCode::from(130);
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }

        // Surface log lines the engine emitted.
        for event in engine.drain_events(32) {
            match event {
                Event::Log { level, message } => {
                    if matches!(level, LogLevel::Error | LogLevel::Warn) {
                        clear_lines(lines_drawn);
                        lines_drawn = 0;
                        eprintln!("[{}] {message}", level.as_str());
                    }
                }
                Event::TaskCompleted { id, path } => {
                    clear_lines(lines_drawn);
                    lines_drawn = 0;
                    println!("#{id} completed -> {path}");
                }
                Event::TaskFailed { id, error } => {
                    clear_lines(lines_drawn);
                    lines_drawn = 0;
                    eprintln!("#{id} failed: {error}");
                    exit = ExitCode::from(3);
                }
                _ => {}
            }
        }

        let snapshot = engine.snapshot();
        let watched: Vec<&TaskSnapshot> = snapshot.iter().filter(|s| ids.contains(&s.id)).collect();
        if tty {
            clear_lines(lines_drawn);
            let mut out = String::new();
            for task in &watched {
                out.push_str(&render_line(task, started.elapsed()));
                out.push('\n');
            }
            let totals = engine.totals();
            out.push_str(&format!(
                "  {} active, {} queued, {} paused, aggregate {}\r\n",
                totals.active,
                totals.queued,
                totals.paused,
                util::human_speed(totals.active_speed)
            ));
            print!("{out}");
            let _ = std::io::stdout().flush();
            lines_drawn = watched.len() + 1;
        } else if !live {
            // quiet
        }

        if watched.iter().all(|t| t.state.is_terminal()) {
            clear_lines(lines_drawn);
            for task in &watched {
                println!(
                    "#{:<3} {:<10} {:>10}  {}",
                    task.id,
                    task.state.as_str(),
                    util::human_bytes(task.received as f64),
                    task.error
                        .clone()
                        .unwrap_or_else(|| util::display_path(&PathBuf::from(&task.final_path)))
                );
            }
            let failed = watched
                .iter()
                .any(|t| t.state == TaskState::Failed || t.state == TaskState::Canceled);
            return if failed { ExitCode::from(3) } else { exit };
        }
    }
}

fn clear_lines(n: usize) {
    if n == 0 || !std::io::stdout().is_terminal() {
        return;
    }
    let mut out = String::new();
    for _ in 0..n {
        out.push_str("\x1b[1A\x1b[2K");
    }
    print!("{out}");
    let _ = std::io::stdout().flush();
}

fn render_line(task: &TaskSnapshot, _elapsed: Duration) -> String {
    let percent = match task.fraction {
        Some(f) => format!("{:>5.1}%", f * 100.0),
        None => "   ?  ".to_string(),
    };
    let bar = match task.fraction {
        Some(f) => {
            let filled = (f * 24.0).round() as usize;
            format!(
                "[{}{}]",
                "#".repeat(filled),
                ".".repeat(24usize.saturating_sub(filled))
            )
        }
        None => format!("[{}]", "?".repeat(24)),
    };
    let size = match task.total_size {
        Some(total) => format!(
            "{} / {}",
            util::human_bytes(task.received as f64),
            util::human_bytes(total as f64)
        ),
        None => format!("{} / ?", util::human_bytes(task.received as f64)),
    };
    let speed = if task.state == TaskState::Verifying {
        format!("verify {}", util::human_speed(task.verify_bps))
    } else {
        util::human_speed(task.speed_bps)
    };
    let eta = if task.state == TaskState::Downloading {
        util::human_eta(task.eta_secs)
    } else {
        task.state.label().to_string()
    };
    let proto = if matches!(
        task.transport_summary,
        usagi::store::records::TransportSummary::Mixed(_)
    ) {
        "Mixed".to_string()
    } else {
        task.transport
            .map(|t| t.short().to_string())
            .unwrap_or_else(|| "-".to_string())
    };
    let name = truncate(&task.filename, 34);
    format!(
        "#{:<3} {name:<34} {bar} {percent}  {size:>22}  {speed:>13}  {eta:<10} {proto:<3} segs {}/{}",
        task.id,
        task.segments_done,
        task.segments_total
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

fn init_logging(level: &str) {
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;

    let filter = match level.to_ascii_lowercase().as_str() {
        "off" => LevelFilter::OFF,
        "error" => LevelFilter::ERROR,
        "warn" => LevelFilter::WARN,
        "info" => LevelFilter::INFO,
        "debug" => LevelFilter::DEBUG,
        "trace" => LevelFilter::TRACE,
        _ => LevelFilter::WARN,
    };
    let file = Paths::discover()
        .ok()
        .map(|p| p.log_file)
        .unwrap_or_else(|| PathBuf::from("udm-cli.log"));
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let appender = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .ok();

    let stderr = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .with_filter(filter);
    let registry = tracing_subscriber::registry().with(stderr);
    match appender {
        Some(file) => {
            let file_layer = fmt::layer()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_filter(filter);
            let _ = registry.with(file_layer).try_init();
        }
        None => {
            let _ = registry.try_init();
        }
    }
}
