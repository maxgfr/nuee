//! `nuee`: share a link, get a swarm.

mod cmd {
    pub mod completions;
    pub mod connect;
    pub mod doctor;
    pub mod embed;
    pub mod expose;
    pub mod get;
    pub mod id;
    pub mod join;
    pub mod pin;
    pub mod relay;
    pub mod run;
    pub mod selftest;
    pub mod serve;
    pub mod share;
    pub mod site;
    pub mod sweep;
    pub mod train;
}
mod config;
mod qr;
mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use nuee_net::{Net, NetConfig, NetError, RelayChoice, RelayUrl};
use nuee_proto::SecretKey;
use nuee_store::StoreError;

use crate::ui::Ui;

/// Stable exit codes.
pub mod exit {
    /// Success.
    pub const OK: u8 = 0;
    /// Any other error.
    pub const ERROR: u8 = 1;
    /// Bad command line or unreadable link.
    pub const USAGE: u8 = 2;
    /// Network or relay unreachable.
    pub const NETWORK: u8 = 3;
    /// Verification mismatch or banned.
    pub const MISMATCH: u8 = 4;
    /// Admission refused.
    pub const REFUSED: u8 = 5;
    /// Integrity failure (hash or decryption).
    pub const INTEGRITY: u8 = 6;
    /// Cancelled or interrupted.
    pub const CANCELLED: u8 = 7;
    /// The swarm was ended by its coordinator.
    pub const ENDED: u8 = 8;
}

const EXIT_CODES: &str = "\
Exit codes:
  0  ok
  1  error
  2  usage (bad arguments, truncated or wrong link)
  3  network or relay unreachable
  4  verification mismatch or banned
  5  admission refused
  6  integrity failure (hash or decryption)
  7  cancelled or interrupted
  8  swarm ended by the coordinator

Environment:
  NUEE_CONFIG_DIR         identity key directory (default ~/.config/nuee)
  NUEE_DATA_DIR           data directory (default ~/.local/share/nuee)
  NUEE_ONLINE_TIMEOUT_MS  how long to wait for the relay (default 10000)
  NUEE_DURABLE_ABSENT_MS  coordinators: replace a copy whose holder is gone this long (default 600000)
  NO_COLOR                disable colors";

#[derive(Debug, Parser)]
#[command(
    name = "nuee",
    version,
    about = "Share a link, get a swarm: distributed compute, files and hosting",
    long_about = "Share a link, get a swarm.\n\nEvery verb prints or reads a link. Whoever opens \
                  the link (in a browser or with `nuee join` / `nuee get`) joins the swarm.",
    after_long_help = EXIT_CODES
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Command,
}

/// Flags shared by every verb.
#[derive(Debug, Clone, clap::Args)]
pub struct Global {
    /// Print NDJSON events on stdout (human text goes to stderr); `run`'s final `done`
    /// event lists `peers: [{id, browser, done, verified, banned}]`
    #[arg(long, global = true)]
    pub json: bool,
    /// Use this relay instead of the public ones
    #[arg(long, global = true, value_name = "URL")]
    pub relay: Option<String>,
    /// Only talk through the relay (no direct connection)
    #[arg(long, global = true)]
    pub relay_only: bool,
    /// Base URL of the web page links point to
    #[arg(long, global = true, value_name = "URL", default_value = nuee_proto::link::DEFAULT_BASE)]
    pub web_base: String,
    /// Directory of the identity key
    #[arg(long, global = true, value_name = "DIR", env = "NUEE_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
    /// Directory of stored blobs and pins
    #[arg(long, global = true, value_name = "DIR", env = "NUEE_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// Use a throwaway in-memory identity and write nothing
    #[arg(long, global = true)]
    pub ephemeral: bool,
    /// More logs (repeat for more)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Less output
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a job and print the link workers join with
    Run(cmd::run::Args),
    /// Join a compute swarm as a worker
    Join(cmd::join::Args),
    /// Share files or folders, encrypted, until Ctrl-C
    Share(cmd::share::Args),
    /// Download a shared link
    Get(cmd::get::Args),
    /// Keep a copy of a share (ciphertext only) and serve it
    Pin(cmd::pin::Args),
    /// Serve every pinned item until Ctrl-C
    Serve(cmd::serve::Args),
    /// Train a model across the swarm
    Train(cmd::train::Args),
    /// Run a parameter sweep
    Sweep(cmd::sweep::Args),
    /// Compute embeddings across the swarm
    Embed(cmd::embed::Args),
    /// Expose a local TCP service through a link
    Expose(cmd::expose::Args),
    /// Connect to an exposed service
    Connect(cmd::connect::Args),
    /// Publish a static site through a link
    Site(cmd::site::Args),
    /// Run a relay server
    Relay(cmd::relay::Args),
    /// Check this machine's setup
    Doctor(cmd::doctor::Args),
    /// Offline self-check (no key, no network, no files)
    Selftest(cmd::selftest::Args),
    /// Print this node's identity
    Id(cmd::id::Args),
    /// Print shell completions
    Completions(cmd::completions::Args),
}

/// A failure with a stable exit code.
#[derive(Debug)]
pub struct Failure {
    /// Exit code.
    pub code: u8,
    /// What happened and what to do.
    pub message: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// An error carrying an exit code.
pub fn fail(code: u8, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Failure {
        code,
        message: message.into(),
    })
}

/// The relay could not be reached in time.
pub fn relay_unreachable() -> anyhow::Error {
    fail(
        exit::NETWORK,
        "relay unreachable: check your connection or pass --relay <url> (see `nuee relay`)",
    )
}

/// Maps a store error to an exit code and a hint.
pub fn store_failure(e: StoreError) -> anyhow::Error {
    match e {
        StoreError::Refused => fail(
            exit::REFUSED,
            "access refused by the provider: the link may be wrong or revoked",
        ),
        StoreError::Unreachable(why) => fail(
            exit::NETWORK,
            format!("provider unreachable ({why}): is the sharer still online?"),
        ),
        StoreError::NoProvider => fail(exit::NETWORK, "the link lists no provider"),
        e @ (StoreError::Integrity(_) | StoreError::Decrypt(_) | StoreError::UnsafePath(_)) => {
            fail(exit::INTEGRITY, format!("{e}; nothing was kept"))
        }
        StoreError::Exists(path) => fail(
            exit::ERROR,
            format!(
                "{} already exists (pass --force to overwrite)",
                path.display()
            ),
        ),
        other => anyhow::Error::new(other),
    }
}

/// Exit code of an error: the one it carries, else 1.
fn code_of(e: &anyhow::Error) -> u8 {
    e.downcast_ref::<Failure>().map_or(exit::ERROR, |f| f.code)
}

/// What every verb gets: the global flags, the output and the identity (loaded lazily).
pub struct Ctx {
    /// Global flags.
    pub global: Global,
    /// Output.
    pub ui: Ui,
    key: OnceLock<SecretKey>,
    /// Every endpoint this run bound, closed once the verb returns (whatever its outcome).
    nets: std::sync::Mutex<Vec<Net>>,
}

impl Ctx {
    /// The identity key: in memory with `--ephemeral`, else loaded or created on disk.
    pub fn secret(&self) -> anyhow::Result<SecretKey> {
        if let Some(k) = self.key.get() {
            return Ok(k.clone());
        }
        let key = if self.global.ephemeral {
            config::fresh_key()?
        } else {
            let dir = config::config_dir(self.global.config_dir.as_deref())?;
            config::load_or_create(&dir)?
        };
        Ok(self.key.get_or_init(|| key).clone())
    }

    /// The data directory (`--data-dir`, `NUEE_DATA_DIR`, XDG, `~/.local/share/nuee`).
    pub fn data_dir(&self) -> anyhow::Result<PathBuf> {
        match &self.global.data_dir {
            Some(d) if !d.as_os_str().is_empty() => Ok(d.clone()),
            _ => nuee_store::data_dir().map_err(anyhow::Error::new),
        }
    }

    /// Relays selected by `--relay`.
    pub fn relay_choice(&self) -> anyhow::Result<RelayChoice> {
        match &self.global.relay {
            Some(url) => {
                let parsed: RelayUrl = url.parse().map_err(|_| {
                    fail(
                        exit::USAGE,
                        format!("invalid relay URL {url:?} (expected http(s)://host[:port])"),
                    )
                })?;
                Ok(RelayChoice::Custom(vec![parsed]))
            }
            None => Ok(RelayChoice::N0),
        }
    }

    /// Binds an endpoint with this node's identity and waits until the relay accepted it.
    pub async fn online_net(&self) -> anyhow::Result<Net> {
        let cfg = NetConfig {
            secret: self.secret()?,
            relay: self.relay_choice()?,
            relay_only: self.global.relay_only,
            alpns: Vec::new(),
        };
        let net = Net::bind(cfg)
            .await
            .map_err(|e| fail(exit::NETWORK, e.to_string()))?;
        match net.online(online_timeout()).await {
            Ok(()) => {
                self.keep(&net);
                Ok(net)
            }
            Err(NetError::RelayUnreachable) => {
                net.close().await;
                Err(relay_unreachable())
            }
            Err(e) => Err(fail(exit::NETWORK, e.to_string())),
        }
    }

    /// Registers `net` so it is closed when the verb returns: an endpoint dropped without
    /// `close` aborts its connections and logs an error.
    pub fn keep(&self, net: &Net) {
        self.nets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(net.clone());
    }

    /// Closes every endpoint registered with [`Ctx::keep`].
    async fn close_nets(&self) {
        let nets = std::mem::take(
            &mut *self
                .nets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for net in nets {
            net.close().await;
        }
    }

    /// The blob store: on disk under the data directory, in memory with `--ephemeral`.
    pub async fn hub(&self, net: &Net) -> anyhow::Result<nuee_store::StoreHub> {
        let cfg = nuee_store::HubConfig::default();
        if self.global.ephemeral {
            return Ok(nuee_store::StoreHub::memory(net, cfg));
        }
        let dir = self.data_dir()?;
        nuee_store::StoreHub::open(net, &dir, cfg)
            .await
            .with_context(|| format!("opening the store in {}", dir.display()))
    }

    /// The home relay of `net` as written in links.
    pub fn relay_string(net: &Net) -> String {
        net.home_relay().map(|u| u.to_string()).unwrap_or_default()
    }
}

/// How long to wait for the relay (`NUEE_ONLINE_TIMEOUT_MS`, default 10 s).
pub fn online_timeout() -> Duration {
    std::env::var("NUEE_ONLINE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(nuee_net::ONLINE_TIMEOUT, Duration::from_millis)
}

/// Durability timing of a coordinator. `NUEE_DURABLE_ABSENT_MS` shortens the delay after which
/// an unreachable holder's copy is replaced (drills and end-to-end tests); holders are then
/// probed four times within that delay so a present one is never taken for absent.
pub fn durable_config() -> nuee_store::durable::ServiceConfig {
    let mut cfg = nuee_store::durable::ServiceConfig::default();
    let absent = std::env::var("NUEE_DURABLE_ABSENT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0);
    if let Some(ms) = absent {
        cfg.durable.absent_after_ms = ms;
        cfg.durable.probe_every_ms = cfg.durable.probe_every_ms.min((ms / 4).max(1));
        cfg.tick = cfg
            .tick
            .min(Duration::from_millis(cfg.durable.probe_every_ms));
    }
    cfg
}

/// A link argument: the text itself, or one line read from stdin for `-` (keeps secrets out
/// of the shell history).
pub fn link_arg(arg: &str) -> anyhow::Result<String> {
    if arg != "-" {
        return Ok(arg.trim().to_owned());
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("cannot read the link from stdin")?;
    let line = line.trim();
    if line.is_empty() {
        return Err(fail(exit::USAGE, "no link on stdin"));
    }
    Ok(line.to_owned())
}

/// Maps a link parse error to exit code 2 with its message.
pub fn link_failure(e: nuee_proto::LinkError) -> anyhow::Error {
    fail(exit::USAGE, e.to_string())
}

/// Default number of worker slots: half the cores, at least one.
pub fn default_slots() -> u16 {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    u16::try_from((cores / 2).max(1)).unwrap_or(1)
}

/// Waits for Ctrl-C; an unusable signal handler means waiting forever.
pub async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::warn!("cannot listen for Ctrl-C: {e}");
        std::future::pending::<()>().await;
    }
}

async fn dispatch(command: Command, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    match command {
        Command::Run(a) => cmd::run::run(a, ctx).await,
        Command::Join(a) => cmd::join::run(a, ctx).await,
        Command::Share(a) => cmd::share::run(a, ctx).await,
        Command::Get(a) => cmd::get::run(a, ctx).await,
        Command::Pin(a) => cmd::pin::run(a, ctx).await,
        Command::Serve(a) => cmd::serve::run(a, ctx).await,
        Command::Train(a) => cmd::train::run(a, ctx).await,
        Command::Sweep(a) => cmd::sweep::run(a, ctx).await,
        Command::Embed(a) => cmd::embed::run(a, ctx).await,
        Command::Expose(a) => cmd::expose::run(a, ctx).await,
        Command::Connect(a) => cmd::connect::run(a, ctx).await,
        Command::Site(a) => cmd::site::run(a, ctx).await,
        Command::Relay(a) => cmd::relay::run(a, ctx).await,
        Command::Doctor(a) => cmd::doctor::run(a, ctx).await,
        Command::Selftest(a) => cmd::selftest::run(a, ctx).await,
        Command::Id(a) => cmd::id::run(a, ctx).await,
        Command::Completions(a) => cmd::completions::run(a, ctx).await,
    }
}

/// The command tree, for completions.
pub fn command() -> clap::Command {
    <Cli as clap::CommandFactory>::command()
}

/// The log filter for a `-v` count when `NUEE_LOG` is unset. At the default verbosity the DNS
/// resolver's warnings about unparsable system nameservers (link-local addresses on macOS) are
/// hidden: they are harmless and read as errors.
fn default_log_filter(verbose: u8) -> &'static str {
    match verbose {
        0 => "warn,n0_dns_resolver=error",
        1 => "info,nuee=debug",
        2 => "debug",
        _ => "trace",
    }
}

fn init_logs(verbose: u8) {
    let default = default_log_filter(verbose);
    let filter = tracing_subscriber::EnvFilter::try_from_env("NUEE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let res = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none())
        .try_init();
    if let Err(e) = res {
        eprintln!("cannot set up logging: {e}");
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    init_logs(cli.global.verbose);
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("a TLS crypto provider was already installed");
    }
    let ui = Ui::new(cli.global.json, cli.global.quiet);
    let ctx = Ctx {
        global: cli.global,
        ui,
        key: OnceLock::new(),
        nets: std::sync::Mutex::new(Vec::new()),
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            ctx.ui
                .error(exit::ERROR, &format!("cannot start the runtime: {e}"));
            return ExitCode::from(exit::ERROR);
        }
    };
    let res = rt.block_on(async {
        let res = dispatch(cli.command, &ctx).await;
        ctx.close_nets().await;
        res
    });
    // Running tasks (a lease on a blocking thread) must not hold the exit.
    rt.shutdown_timeout(Duration::from_millis(500));
    match res {
        Ok(code) => code,
        Err(e) => {
            let code = code_of(&e);
            ctx.ui.error(code, &format!("{e:#}"));
            ExitCode::from(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::default_log_filter;

    #[test]
    fn default_verbosity_hides_dns_resolver_warnings() {
        assert_eq!(default_log_filter(0), "warn,n0_dns_resolver=error");
        assert!(
            tracing_subscriber::EnvFilter::try_new(default_log_filter(0)).is_ok(),
            "the default filter parses"
        );
        assert_eq!(default_log_filter(1), "info,nuee=debug");
        assert_eq!(default_log_filter(2), "debug");
        assert_eq!(default_log_filter(5), "trace");
    }
}
