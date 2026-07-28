//! Command line entry point for the WinXtend agent.
//!
//! With no arguments it runs the daemon in the foreground, which is what an
//! autostart entry, a terminal, and a supervisor all want. The other modes are
//! thin: `--status` and `--pair` talk to an already-running agent over the local
//! control channel rather than duplicating any of its logic, so they cannot
//! disagree with it.

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::Parser;
use wx_agent::config::Config;
use wx_agent::engine::{self, EngineOptions, AGENT_VERSION};
use wx_agent::ipc::{self, ErrorCode, IpcClient, Request, Response};
use wx_agent::{autostart, Event};

#[derive(Debug, Parser)]
#[command(
    name = "wx-agent",
    version = AGENT_VERSION,
    about = "WinXtend agent: share one keyboard and mouse across machines",
    long_about = None
)]
struct Cli {
    /// Register the agent to start with this user's session, then exit.
    ///
    /// Deliberately per-user rather than a system service: a session-0 service
    /// cannot install desktop input hooks, so it would start, look healthy, and
    /// capture nothing. See the `autostart` module for the full reasoning.
    #[arg(long, conflicts_with_all = ["uninstall", "status", "pair"])]
    install: bool,

    /// Remove the autostart registration, then exit.
    #[arg(long, conflicts_with_all = ["status", "pair"])]
    uninstall: bool,

    /// Print what the running agent is doing, then exit.
    #[arg(long, conflicts_with = "pair")]
    status: bool,

    /// Supply the six-digit code shown on the machine that started pairing.
    #[arg(long, value_name = "CODE")]
    pair: Option<String>,

    /// Start pairing with a discovered machine and print the code to read out.
    ///
    /// The counterpart of `--pair`: this side generates the code, the other side
    /// types it. Takes a node id or any unambiguous prefix.
    #[arg(long, value_name = "NODE_ID", conflicts_with_all = ["pair", "status", "install", "uninstall"])]
    pair_with: Option<String>,

    /// Which machine the code is for, when more than one is waiting. A node id, or
    /// any unambiguous prefix of one.
    #[arg(long, value_name = "NODE_ID", requires = "pair")]
    peer: Option<String>,

    /// Directory holding the identity key, trust store, and configuration.
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,

    /// Override the QUIC port for this run without editing the config file.
    #[arg(long)]
    port: Option<u16>,

    /// Override this node's name for this run.
    #[arg(long)]
    name: Option<String>,

    /// Do not announce on, or browse, the local network.
    #[arg(long)]
    no_discovery: bool,

    /// Log level: error, warn, info, debug, trace. `RUST_LOG` wins if set.
    #[arg(long, default_value = "info")]
    log: String,

    /// Print the resolved configuration and exit, for inspection.
    #[arg(long)]
    print_config: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log);

    let config_dir = match &cli.config_dir {
        Some(dir) => dir.clone(),
        None => wx_net::default_config_dir()
            .context("no per-user configuration directory is available")?,
    };
    let config_path = config_dir.join(wx_agent::config::CONFIG_FILE);

    // The modes that talk to a running agent need no config at all, so they are
    // handled before anything is loaded or created.
    if cli.status {
        return runtime()?.block_on(show_status(&config_dir));
    }
    if let Some(code) = cli.pair.clone() {
        return runtime()?.block_on(submit_pairing_code(&config_dir, code, cli.peer.clone()));
    }
    if let Some(target) = cli.pair_with.clone() {
        return runtime()?.block_on(start_pairing(&config_dir, target));
    }

    let mut config = Config::load_or_default(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;
    if let Some(port) = cli.port {
        config.network.port = port;
    }
    if let Some(name) = &cli.name {
        config.node.name = name.clone();
    }
    if cli.no_discovery {
        config.network.discovery = false;
    }

    if cli.install {
        return install(&mut config, &config_path);
    }
    if cli.uninstall {
        return uninstall(&mut config, &config_path);
    }
    if cli.print_config {
        print!("{}", toml::to_string_pretty(&config)?);
        return Ok(());
    }

    runtime()?.block_on(engine::run(EngineOptions {
        config_dir,
        config_path,
        config,
    }))
}

/// A multi-threaded runtime, because the engine loop and the per-peer send tasks
/// must not be able to starve each other: a QUIC write that blocks on a full
/// congestion window should not delay the next mouse event.
fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(format!("wx_agent={level},wx_net=warn,wx_platform=warn")))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

fn install(config: &mut Config, config_path: &std::path::Path) -> anyhow::Result<()> {
    match autostart::install() {
        Ok(exe) => {
            match autostart::location() {
                // Linux: naming the unit is more use than naming the entry,
                // because it is the handle for every follow-up the user might
                // want — `systemctl --user status winxtend`, or reading it.
                Some(unit) => println!(
                    "registered {} to start with this session, via {}",
                    exe.display(),
                    unit.display()
                ),
                None => println!(
                    "registered {} to start with this session as \"{}\"",
                    exe.display(),
                    autostart::ENTRY_NAME
                ),
            }
            // Registration takes effect at the next login on every platform. On
            // Linux the unit is enabled but not started, and starting it now
            // would fight the agent the user may already be running.
            println!("it will start with the next login; nothing has been started now");
            config.node.autostart = true;
            config.save(config_path)?;
            Ok(())
        }
        Err(e) => {
            // Printed as well as returned, because the message is a set of
            // instructions the user is expected to follow by hand.
            eprintln!("cannot register autostart automatically:\n  {e}");
            // The last line anyhow prints is the one a user reads first, so it
            // must not contradict the one above it. Only `Unsupported` means
            // there is no mechanism here; a path systemd cannot name, or a
            // directory that cannot be written, is a failure on a platform that
            // supports this perfectly well, and saying otherwise sends the user
            // looking for a missing feature instead of at the detail above.
            match e {
                autostart::AutostartError::Unsupported { .. } => {
                    bail!("autostart registration is not supported on this platform")
                }
                _ => bail!("could not register autostart"),
            }
        }
    }
}

fn uninstall(config: &mut Config, config_path: &std::path::Path) -> anyhow::Result<()> {
    match autostart::uninstall() {
        Ok(()) => {
            println!("removed the autostart registration");
            config.node.autostart = false;
            config.save(config_path)?;
            Ok(())
        }
        Err(e) => {
            eprintln!("cannot remove autostart automatically:\n  {e}");
            match e {
                autostart::AutostartError::Unsupported { .. } => {
                    bail!("autostart registration is not supported on this platform")
                }
                _ => bail!("could not remove the autostart registration"),
            }
        }
    }
}

async fn show_status(config_dir: &std::path::Path) -> anyhow::Result<()> {
    let mut client = connect(config_dir).await?;
    let Response::Status { status } = IpcClient::expect_ok(client.call(Request::Status).await?)?
    else {
        bail!("the agent answered something other than a status");
    };

    println!(
        "node      {} ({})",
        status.node_name,
        short(&status.node_id)
    );
    println!(
        "version   {} (protocol {})",
        status.agent_version, status.protocol
    );
    println!("platform  {} / {}", status.platform, status.display_server);
    println!("port      {}", status.port);
    println!("uptime    {}", format_duration(status.uptime_secs));
    // The agent's own answer, read from the OS rather than from its config file,
    // so this and `--install` cannot disagree. See `StatusSnapshot::autostart`.
    println!(
        "autostart {}",
        if status.autostart {
            "registered"
        } else {
            "not registered"
        }
    );
    println!(
        "cursor    {}{}",
        if status.cursor.local {
            "here".to_string()
        } else {
            format!("on {}", status.cursor.owner_name)
        },
        if status.cursor.locked {
            " (locked)"
        } else {
            ""
        }
    );
    println!(
        "displays  {}",
        if status.monitors.is_empty() {
            "none (headless)".to_string()
        } else {
            status
                .monitors
                .iter()
                .map(|m| format!("{} {}x{}", m.name, m.w, m.h))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );

    // What this machine tells the mesh it can do. The agent now refuses features a
    // peer has not advertised, so the advertised set is the first thing to look at
    // when something is not happening between two machines.
    println!(
        "can do    {}",
        wx_proto::Capabilities(status.capabilities).describe()
    );

    // Printed before the peer list, because "no peers seen yet" with a firewall
    // up is the exact pair of facts a user needs to read together.
    if let Some(firewall) = &status.firewall {
        println!("\nwarning   {firewall}");
    }

    if status.peers.is_empty() {
        println!("\nno peers seen yet");
    } else {
        println!("\n{:<10} {:<20} {:<12} DETAIL", "NODE", "NAME", "STATE");
        for peer in &status.peers {
            let (state, detail) = describe(peer);
            println!(
                "{:<10} {:<20} {:<12} {}",
                short(&peer.node),
                truncate(&peer.name, 20),
                state,
                detail
            );
        }
    }

    if !status.layout.placements.is_empty() {
        println!("\nlayout revision {}", status.layout.revision);
        for p in &status.layout.placements {
            println!(
                "  {}/mon{}  {}x{} at ({}, {})",
                short(&p.node),
                p.monitor,
                p.w,
                p.h,
                p.x,
                p.y
            );
        }
    }
    Ok(())
}

/// How a peer's state reads in one line.
fn describe(peer: &ipc::PeerSnapshot) -> (&'static str, String) {
    match &peer.status {
        ipc::PeerStatusSpec::Failed { error } => ("failed", error.clone()),
        ipc::PeerStatusSpec::Connected => (
            "connected",
            peer.rtt_ms.map(|ms| format!("{ms}ms")).unwrap_or_default(),
        ),
        ipc::PeerStatusSpec::Pairing => ("pairing", "waiting for a code".into()),
        ipc::PeerStatusSpec::Connecting => ("connecting", String::new()),
        ipc::PeerStatusSpec::Discovered => (
            "discovered",
            if peer.paired {
                String::new()
            } else {
                "not paired".into()
            },
        ),
        ipc::PeerStatusSpec::Offline => (
            "offline",
            if peer.blocked {
                "blocked".into()
            } else {
                String::new()
            },
        ),
    }
}

/// Begin pairing from this side and wait for the other machine to answer.
///
/// Stays connected until the exchange finishes rather than printing the code and
/// exiting, because the code is only valid for this connection's nonces: exiting
/// would leave the user reading out a number that has already expired.
async fn start_pairing(config_dir: &std::path::Path, target: String) -> anyhow::Result<()> {
    let mut client = connect(config_dir).await?;
    let node = resolve_peer(&mut client, &target).await?;
    IpcClient::expect_ok(client.call(Request::Subscribe).await?)?;

    let Response::PairingStarted { pin, .. } = IpcClient::expect_ok(
        client
            .call(Request::BeginPairing { node: node.clone() })
            .await?,
    )?
    else {
        bail!("the agent answered something other than a pairing code");
    };
    println!("pairing code: {pin}");
    println!("run `wx-agent --pair {pin}` on the other machine");

    loop {
        if let Event::PairingFinished {
            node: finished,
            accepted,
            message,
        } = client.next_event().await?
        {
            if finished != node {
                continue;
            }
            if accepted {
                println!("paired with {}", short(&finished));
                return Ok(());
            }
            bail!(
                "pairing failed: {}",
                message.unwrap_or_else(|| "no reason given".into())
            );
        }
    }
}

async fn submit_pairing_code(
    config_dir: &std::path::Path,
    code: String,
    peer: Option<String>,
) -> anyhow::Result<()> {
    let mut client = connect(config_dir).await?;

    // A prefix is resolved against what the agent knows, so the user can type the
    // eight characters the UI showed them rather than sixty-four.
    let node = match peer {
        Some(prefix) => Some(resolve_peer(&mut client, &prefix).await?),
        None => None,
    };

    // Subscribed before the code goes in, so the outcome cannot be missed in the
    // gap between the reply and the exchange finishing.
    IpcClient::expect_ok(client.call(Request::Subscribe).await?)?;
    match client
        .call(Request::ConfirmPairing {
            node: node.clone(),
            pin: code,
        })
        .await?
    {
        Response::Error {
            code: ErrorCode::NoPairing,
            message,
        } => bail!("{message}"),
        other => IpcClient::expect_ok(other)?,
    };

    println!("code sent; waiting for the other machine");
    loop {
        if let Event::PairingFinished {
            node: finished,
            accepted,
            message,
        } = client.next_event().await?
        {
            if node.as_deref().is_some_and(|want| want != finished) {
                continue;
            }
            if accepted {
                println!("paired with {}", short(&finished));
                return Ok(());
            }
            bail!(
                "pairing failed: {}",
                message.unwrap_or_else(|| "no reason given".into())
            );
        }
    }
}

/// Expand a node id prefix into the full hex id the IPC channel expects.
async fn resolve_peer(client: &mut IpcClient, prefix: &str) -> anyhow::Result<String> {
    let Response::Peers { peers } = IpcClient::expect_ok(client.call(Request::ListPeers).await?)?
    else {
        bail!("the agent answered something other than a peer list");
    };
    let wanted = prefix.trim().to_ascii_lowercase();
    let mut matches = peers
        .into_iter()
        .filter(|p| p.node.starts_with(&wanted) || p.name.eq_ignore_ascii_case(&wanted));
    match (matches.next(), matches.next()) {
        (Some(only), None) => Ok(only.node),
        (None, _) => bail!("no known machine matches {prefix:?}"),
        // Refused rather than guessed: pairing with the wrong machine grants it
        // the ability to type on this one.
        _ => bail!("{prefix:?} matches more than one machine; be more specific"),
    }
}

async fn connect(config_dir: &std::path::Path) -> anyhow::Result<IpcClient> {
    IpcClient::connect(config_dir).await.with_context(|| {
        format!(
            "cannot reach a running agent (looked in {})",
            config_dir.display()
        )
    })
}

/// The eight-character form of a node id, matching what the UI shows.
fn short(hex: &str) -> &str {
    &hex[..8.min(hex.len())]
}

fn format_duration(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86_400,
        (secs / 3_600) % 24,
        (secs / 60) % 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    // Truncated by character, not byte: a name with an accent in it must not be
    // cut through the middle of a codepoint.
    s.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_self_consistent() {
        // Catches what clap otherwise only reports at runtime: a duplicated short
        // flag, or a `requires` naming an argument that does not exist.
        Cli::command().debug_assert();
    }

    #[test]
    fn no_arguments_means_run_the_daemon() {
        let cli = Cli::try_parse_from(["wx-agent"]).unwrap();
        assert!(!cli.install && !cli.uninstall && !cli.status);
        assert!(cli.pair.is_none());
    }

    #[test]
    fn the_modes_that_cannot_be_combined_are_refused() {
        // Installing and asking for status in one invocation has no sensible
        // meaning, and silently doing one of them would be worse than an error.
        assert!(Cli::try_parse_from(["wx-agent", "--install", "--status"]).is_err());
        assert!(Cli::try_parse_from(["wx-agent", "--status", "--pair", "123456"]).is_err());
        // A peer is only meaningful alongside a code.
        assert!(Cli::try_parse_from(["wx-agent", "--peer", "ab12"]).is_err());
    }

    #[test]
    fn pairing_takes_a_code_and_an_optional_machine() {
        let cli =
            Cli::try_parse_from(["wx-agent", "--pair", "004312", "--peer", "ab12cd"]).unwrap();
        assert_eq!(cli.pair.as_deref(), Some("004312"));
        assert_eq!(cli.peer.as_deref(), Some("ab12cd"));
    }

    #[test]
    fn overrides_do_not_require_a_config_file() {
        let cli = Cli::try_parse_from([
            "wx-agent",
            "--port",
            "24801",
            "--name",
            "desk",
            "--no-discovery",
        ])
        .unwrap();
        assert_eq!(cli.port, Some(24801));
        assert_eq!(cli.name.as_deref(), Some("desk"));
        assert!(cli.no_discovery);
    }

    #[test]
    fn durations_read_as_english_rather_than_seconds() {
        assert_eq!(format_duration(9), "9s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3_700), "1h 1m");
        assert_eq!(format_duration(90_061), "1d 1h 1m");
    }

    #[test]
    fn truncation_never_splits_a_character() {
        assert_eq!(truncate("short", 20), "short");
        let cut = truncate("åååååååååå", 5);
        assert_eq!(cut.chars().count(), 5);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn a_short_node_id_is_not_sliced_past_its_end() {
        // Node ids come off the channel, so a truncated one must not panic the CLI.
        assert_eq!(short("abc"), "abc");
        assert_eq!(short(""), "");
        assert_eq!(short("0123456789abcdef"), "01234567");
    }
}
