//! The `respan-gateway` command line.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::config::ClientKey;
use crate::init::Chosen;
use crate::logging::{Color, Level, LogFormat};
use crate::{App, Config, ProxyVendor, Source, VERSION};
use clap::{ArgGroup, Parser, Subcommand};
use serde_json::{Map, json};

/// An LLM gateway: one endpoint for OpenAI, Anthropic, Gemini and more,
/// translating between their APIs.
///
/// With no config file it needs only a provider key in the environment:
///
///   export OPENAI_API_KEY=sk-...   # and/or ANTHROPIC_API_KEY, GEMINI_API_KEY, ...
///   respan-gateway
///
/// It reads ./gateway.toml if there is one; `respan-gateway init` writes a
/// starter. The nearest .env file in the current directory or its parents is read first, so keys and
/// the settings below may live there.
#[derive(Debug, Parser)]
#[command(name = "respan-gateway", version = VERSION, verbatim_doc_comment, after_help = EXAMPLES)]
struct Cli {
    /// The config file [default: gateway.toml,
    /// then providers from the environment]
    #[arg(short, long, global = true, value_name = "FILE", env = "RESPAN_CONFIG")]
    config: Option<PathBuf>,

    /// Where to listen, such as 127.0.0.1:8080, localhost:8080 or 8080, in
    /// place of the config's. Beyond loopback, client keys are required
    #[arg(long, global = true, value_name = "ADDR", env = "RESPAN_LISTEN")]
    listen: Option<String>,

    /// How requests are logged to stderr, in place of the config's
    #[arg(long, global = true, value_name = "FORMAT", env = "RESPAN_LOG")]
    log: Option<LogFormat>,

    /// Color in terminal output; auto respects NO_COLOR and redirected output
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "auto",
        env = "RESPAN_COLOR"
    )]
    color: Color,

    /// While serving, reload the config file when it changes (checked every
    /// 2 s). SIGHUP and `POST /ui/api/reload` reload it either way
    #[arg(long, global = true)]
    watch: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the gateway (the default)
    Serve,
    /// Check the configuration, print what it serves and exit
    Check {
        /// Also make one call to each provider with its key (listing its
        /// models) and say whether it is reachable, refuses the key or is
        /// unreachable; the exit status is 1 unless all are reachable
        #[arg(long)]
        online: bool,
    },
    /// List the models of every provider, as `/v1/models` does, and exit
    Models,
    /// Write a commented starter config from the keys in the environment
    Init {
        /// Where to write it
        #[arg(short, long, value_name = "FILE", default_value = crate::DEFAULT_FILE)]
        output: PathBuf,
        /// Replace the file if it exists
        #[arg(long)]
        force: bool,
        /// Enable a vendor too, such as `groq`, its key read from its
        /// variable (`env:GROQ_API_KEY`); `VENDOR=env:NAME` reads another,
        /// and `VENDOR=KEY` writes the key itself into the file
        #[arg(short, long, value_name = "VENDOR[=KEY]")]
        provider: Vec<String>,
        /// Generate a client key (`sk-gw-...`), write it into `client_keys`
        /// and print it once: what a gateway beyond localhost needs
        #[arg(long)]
        client_key: bool,
    },
    /// MCP servers: list them and whether the gateway can authenticate to
    /// each, or log in to one a person must authorize
    Mcp {
        #[command(subcommand)]
        command: Option<McpCommand>,
    },
    /// Relay one vendor's whole API, with each client's own credentials
    ///
    /// No config file and no keys of its own: chat and every other endpoint
    /// of the vendor, byte for byte. For Claude Code
    /// (ANTHROPIC_BASE_URL=http://127.0.0.1:8080) or Codex.
    #[command(group(ArgGroup::new("vendor").required(true).args(["anthropic", "openai"])))]
    Proxy {
        /// Relay to Anthropic
        #[arg(long)]
        anthropic: bool,
        /// Relay to OpenAI
        #[arg(long)]
        openai: bool,
        /// The vendor's base URL, in place of its public one
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Check the proxy's configuration, print it and exit
        #[arg(long)]
        check: bool,
    },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Log in to an HTTP MCP server, as a person approving it once (a device
    /// code, or in a browser), and save the login beside the config, in
    /// `.respan/mcp/<name>.json`, for `auth = "login"`
    Login {
        /// The server, as `[tools.<name>]` names it
        name: String,
    },
}

const EXAMPLES: &str = "Examples:
  respan-gateway                    Serve ./gateway.toml, or the providers in the environment
  respan-gateway init               Write a starter ./gateway.toml
  respan-gateway init --client-key  ... with a generated client key, printed once
  respan-gateway init -p groq       Write a starter that also enables groq, key env:GROQ_API_KEY
  respan-gateway check -c my.toml   Check a config and print what it serves
  respan-gateway check --online     ... and call each provider with its key, and list the tool loop's tools
  respan-gateway models             List the models the providers offer
  respan-gateway proxy --anthropic  Sit in front of Claude Code
  respan-gateway mcp                List the MCP servers and whether each is authenticated
  respan-gateway mcp login linear   Log in to the MCP server [tools.linear], once

Environment (also read from the nearest .env in this directory or its parents):
  OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY, GROQ_API_KEY, ...
                     Provider keys; with no config file each is a provider
  RESPAN_CONFIG      The config file, as --config
  RESPAN_LISTEN      Where to listen, as --listen
  PORT               The port to listen on, on every address (0.0.0.0), if
                     neither --listen, RESPAN_LISTEN nor the config's `listen` says
  RESPAN_CLIENT_KEYS Client keys, comma separated, added to the config's
                     `client_keys`; a gateway on a non-loopback address needs one
  RESPAN_LOG         text, json or off, as --log
  RESPAN_COLOR       auto, always or never, as --color
  NO_COLOR           Disable automatic colors when nonempty
  RESPAN_<VENDOR>_BASE_URL
                     Moves a vendor's base URL when there is no config file

Reloading the config without a restart: send SIGHUP, POST /ui/api/reload, or
serve with --watch. Open requests finish on the old config; an invalid one is
refused. A changed listen address needs a restart.";

/// Runs the `respan-gateway` command line on `args` (the program name
/// first) and returns its exit status.
pub async fn run<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    run_with(args, None).await
}

/// Runs the command line as the binary does: reads the nearest `.env` if there is
/// one, then runs on a runtime of its own.
pub fn execute<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let dotenv = match load_dotenv() {
        Ok(loaded) => loaded,
        Err(e) => return fail(&e),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build();
    match runtime {
        Ok(runtime) => runtime.block_on(run_with(args, dotenv)),
        Err(e) => fail(&format!("start the runtime: {e}")),
    }
}

/// Reads the nearest `.env` in the working directory or its parents,
/// without replacing variables already set in the environment.
fn load_dotenv() -> Result<Option<PathBuf>, String> {
    match dotenvy::dotenv() {
        Ok(path) => Ok(Some(path)),
        Err(e) if e.not_found() => Ok(None),
        Err(dotenvy::Error::LineParse(..)) => Err(
            ".env: invalid syntax; use KEY=value, with matching quotes around quoted values".into(),
        ),
        Err(e) => Err(format!(".env: {e}")),
    }
}

async fn run_with<I, T>(args: I, dotenv: Option<PathBuf>) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return u8::try_from(e.exit_code()).unwrap_or(FAILURE);
        }
    };
    cli.color.apply();
    let command = cli.command.unwrap_or(Command::Serve);
    let models = matches!(command, Command::Models);
    let online = matches!(command, Command::Check { online: true });
    let (config, check) = match command {
        Command::Init {
            output,
            force,
            provider,
            client_key,
        } => return init(&output, force, &provider, client_key),
        Command::Proxy {
            anthropic,
            url,
            check,
            ..
        } => {
            if cli.config.is_some() {
                return fail(
                    "proxy cannot be used with --config; proxy builds its own configuration",
                );
            }
            let vendor = match anthropic {
                true => ProxyVendor::Anthropic,
                false => ProxyVendor::OpenAi,
            };
            (Ok(Config::proxy(vendor, url.as_deref())), check)
        }
        Command::Mcp { command: None } => {
            return match Config::discover(cli.config.as_deref()) {
                Ok(config) => {
                    println!("{}", crate::mcp_login::list(&config));
                    SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            };
        }
        Command::Mcp {
            command: Some(McpCommand::Login { name }),
        } => {
            let config = match Config::discover(cli.config.as_deref()) {
                Ok(config) => config,
                Err(e) => return fail(&e.to_string()),
            };
            return match crate::mcp_login::login(&config, &name).await {
                Ok(path) => {
                    println!("Logged in to '{name}'; saved to {}.", path.display());
                    0
                }
                Err(e) => fail(&format!("mcp login {name}: {e}")),
            };
        }
        Command::Serve => (Config::discover(cli.config.as_deref()), false),
        Command::Check { .. } | Command::Models => (Config::discover(cli.config.as_deref()), true),
    };
    let overrides = Overrides {
        listen: cli.listen.filter(|l| !l.is_empty()),
        log: cli.log,
    };
    let config_beats_port = overrides.listen.is_none()
        && config.as_ref().is_ok_and(|c| c.listen.is_some())
        && std::env::var("PORT").is_ok_and(|p| !p.is_empty());
    let config = match config
        .map_err(|e| e.to_string())
        .and_then(|c| overrides.apply(c))
    {
        Ok(config) => config,
        Err(e) => return fail(&e),
    };
    let log = config.log;
    let source = config.source().to_string();
    let reload_source = config.source().clone();
    let app = match tokio::task::spawn_blocking(move || {
        App::new(config).map_err(|error| error.to_string())
    })
    .await
    {
        Ok(Ok(app)) => app,
        Ok(Err(error)) => return fail(&format!("{source}: {error}")),
        Err(error) => return fail(&format!("{source}: startup worker failed: {error}")),
    };
    if models {
        return list_models(&app).await;
    }
    if let Some(mut refusal) = app.open_refusal(app.listen()) {
        if config_beats_port {
            refusal.push_str(
                "\n  The config's `listen` wins over PORT, so PORT is ignored; to follow \
                 PORT, remove `listen` from the config.",
            );
        }
        return fail(&refusal);
    }
    if check {
        println!("{}", crate::logging::report(&app.report()));
        println!("{source}: {}", crate::logging::paint("ok", "1;32", false));
        if online {
            let (report, all_reachable) = app.probe().await;
            println!("\n{report}");
            return if all_reachable { SUCCESS } else { FAILURE };
        }
        return SUCCESS;
    }
    if let Some(path) = &dotenv {
        log.say(Level::Info, &format!("loaded {}", path.display()));
    }

    // Reloads read the source the gateway started from, with the same
    // overrides; the proxy has none.
    let app = match reload_source {
        Source::File(path) => app.reload_from(move || overrides.apply_to(Config::load(&path))),
        Source::Environment => app.reload_from(move || overrides.apply_to(Config::from_env())),
        _ => app,
    };
    #[cfg(unix)]
    let hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    // The first calls are priced and routed by the providers' lists.
    app.listed().await;
    let addr = app.listen();
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return fail(&bind_error(addr, &e)),
    };
    let bound = listener.local_addr().unwrap_or(addr);
    match log {
        LogFormat::Json => {
            let fields = json!({ "addr": bound.to_string(), "version": VERSION, "config": source });
            let fields = fields.as_object().cloned().unwrap_or_else(Map::new);
            eprintln!("{}", log.line(Level::Info, "listening", fields));
        }
        _ => println!("{}", crate::logging::report(&app.banner(bound))),
    }
    #[cfg(unix)]
    if let Some(hangup) = hangup {
        tokio::spawn(reload_on_hangup(app.clone(), hangup));
    }
    if let (true, Source::File(path)) = (cli.watch, app.config().source()) {
        tokio::spawn(reload_on_change(app.clone(), path.clone()));
    } else if cli.watch {
        log.say(Level::Warn, "--watch needs a config file; not watching");
    }
    match app.serve(listener, crate::shutdown_signal()).await {
        Ok(()) => SUCCESS,
        Err(e) => fail(&e.to_string()),
    }
}

/// What the command line and the environment set over the config, applied
/// to each config read, so a reload keeps them.
#[derive(Debug, Clone)]
struct Overrides {
    listen: Option<String>,
    log: Option<LogFormat>,
}

impl Overrides {
    fn apply_to(&self, config: Result<Config, crate::Error>) -> Result<Config, crate::Error> {
        let config = config.map_err(|e| e.to_string());
        config
            .and_then(|c| self.apply(c))
            .map_err(crate::Error::new)
    }

    fn apply(&self, mut config: Config) -> Result<Config, String> {
        if let Some(listen) = &self.listen {
            config.listen = Some(
                crate::config::parse_listen(listen).map_err(|e| format!("listen {listen}: {e}"))?,
            );
        } else if config.listen.is_none() {
            // What a PaaS host sets: its router reaches us from outside.
            let port = std::env::var("PORT").ok().filter(|p| !p.is_empty());
            if let Some(port) = port {
                let n = port
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| format!("PORT={port}: not a port number"))?;
                config.listen = Some(SocketAddr::from(([0, 0, 0, 0], n)));
            }
        }
        if let Ok(keys) = std::env::var("RESPAN_CLIENT_KEYS") {
            let keys = keys.split(',').map(str::trim).filter(|k| !k.is_empty());
            config.client_keys.extend(keys.map(ClientKey::from));
        }
        if let Some(log) = self.log {
            config.log = log;
        }
        Ok(config)
    }
}

// Configuration can prepare models and run synchronous warmup. Keep that work
// off async workers while the previous build continues serving during reload.
async fn reload_without_blocking(app: &App) {
    let log = app.config().log;
    let app = app.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || app.reload()).await {
        log.say(Level::Error, &format!("reload worker failed: {error}"));
    }
}

/// Reloads on each SIGHUP.
#[cfg(unix)]
async fn reload_on_hangup(app: App, mut hangup: tokio::signal::unix::Signal) {
    while hangup.recv().await.is_some() {
        app.config()
            .log
            .say(Level::Info, "SIGHUP: reloading the config");
        reload_without_blocking(&app).await;
    }
}

/// Reloads whenever the config file at `path` is modified, checked every 2 s.
async fn reload_on_change(app: App, path: PathBuf) {
    let modified = || std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut seen = modified();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        // A file mid-replacement is missing for a moment: wait for it.
        let now = modified();
        if now.is_some() && now != seen {
            seen = now;
            app.config()
                .log
                .say(Level::Info, "config file changed: reloading");
            reload_without_blocking(&app).await;
        }
    }
}

/// Why `addr` cannot be listened on, and what to do.
fn bind_error(addr: std::net::SocketAddr, e: &std::io::Error) -> String {
    use std::io::ErrorKind::{AddrInUse, AddrNotAvailable, PermissionDenied};
    let hint = match e.kind() {
        AddrInUse => format!(
            "something else is listening there; stop it, or use another port \
             (`--listen {}` or `--listen 127.0.0.1:0` for any free one)",
            addr.port().saturating_add(1)
        ),
        PermissionDenied => {
            "ports below 1024 need privileges; use a higher one, such as 8080".to_owned()
        }
        AddrNotAvailable => "that address is not one of this machine's".to_owned(),
        _ => return format!("listen on {addr}: {e}"),
    };
    format!("listen on {addr}: {e}\n  {hint}")
}

async fn list_models(app: &App) -> u8 {
    let (models, failed) = app.models().await;
    let text = |m: &serde_json::Value, key: &str| m[key].as_str().unwrap_or_default().to_owned();
    let mut rows: Vec<[String; 3]> = models
        .iter()
        .map(|m| [text(m, "owned_by"), text(m, "kind"), text(m, "id")])
        .collect();
    rows.sort();
    let width = |i: usize| rows.iter().map(|r| r[i].len()).max().unwrap_or(0).max(8);
    let (provider, kind) = (width(0), width(1));
    println!("{:<provider$}  {:<kind$}  MODEL", "PROVIDER", "KIND");
    for [owner, kind_of, id] in &rows {
        println!("{owner:<provider$}  {kind_of:<kind$}  {id}");
    }
    for why in &failed {
        eprintln!("warning: could not list {why}");
    }
    match rows.is_empty() && !failed.is_empty() {
        true => FAILURE,
        false => SUCCESS,
    }
}

fn init(output: &Path, force: bool, providers: &[String], client_key: bool) -> u8 {
    let chosen: Result<Vec<Chosen>, String> = providers.iter().map(|p| Chosen::parse(p)).collect();
    let chosen = match chosen {
        Ok(chosen) => chosen,
        Err(e) => return fail(&e),
    };
    if output.exists() && !force {
        let path = output.display();
        return fail(&format!("{path} exists; add --force to replace it"));
    }
    let private = client_key || chosen.iter().any(Chosen::is_literal);
    let key = match client_key {
        true => match crate::init::new_client_key() {
            Ok(key) => Some(key),
            Err(e) => return fail(&format!("generate a client key: {e}")),
        },
        false => None,
    };
    let starter = crate::init::starter(&chosen, key.as_deref());
    if let Err(e) = write_config(output, &starter, private) {
        return fail(&format!("write {}: {e}", output.display()));
    }
    let path = output.display();
    println!("wrote {path}");
    for c in &chosen {
        let id = c.vendor.id;
        match (c.is_literal(), c.variable()) {
            (true, _) => eprintln!(
                "warning: {path} holds {id}'s key in plain text; keep it private, or use \
                 `-p {id}` to read it from {} instead",
                c.vendor.key_env.first().copied().unwrap_or("a variable")
            ),
            (_, Some(var)) if std::env::var(var).is_ok_and(|v| !v.is_empty()) => {}
            (_, Some(var)) => {
                println!("set {var} in .env")
            }
            _ => {}
        }
    }
    if let Some(key) = &key {
        println!(
            "client key (shown once; it is also in {path}):\n  {key}\n\
             Clients send it as `Authorization: Bearer {key}` or `x-api-key: {key}`; the \
             gateway takes it off, no provider sees it.\n"
        );
    }
    if chosen.is_empty() && crate::init::detected_vendors().is_empty() {
        println!("set OPENAI_API_KEY in .env");
    }
    SUCCESS
}

/// Writes `text` to `path`, readable by its owner alone if `private`: it
/// holds a key.
fn write_config(path: &Path, text: &str, private: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    if private {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // `mode` is a new file's alone: an older one may be readable by others.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        return file.write_all(text.as_bytes());
    }
    let _ = private;
    std::fs::write(path, text)
}

const SUCCESS: u8 = 0;
const FAILURE: u8 = 1;

fn fail(message: &str) -> u8 {
    eprintln!(
        "{} {message}",
        crate::logging::paint("error:", "1;31", true)
    );
    FAILURE
}
