//! The daemon.

use std::path::PathBuf;
use std::process::ExitCode;

use nerves_hub_link_agent::agent::{Agent, Tool};
use nerves_hub_link_agent::{config::Config, identity};

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = config_path();
    log::info!("reading {}", path.display());

    let config = Config::from_toml(&std::fs::read_to_string(&path)?)?;

    // Everything that can fail on a misconfiguration fails here, before a
    // connection exists: a bad identifier, a missing fwup, a device path that
    // is not what it should be. A device that gets as far as joining should be
    // one that can actually take an update.
    let identifier = identity::resolve(config.identity.identifier())?;
    let tool = Tool::build(&config.update_tool)?;

    log::info!(
        "identifier {identifier}, update tool {} ({})",
        tool.name(),
        if config.update_tool.can_touch_the_system() {
            "can write to this system"
        } else {
            "sandboxed"
        }
    );

    let mut agent = Agent::new(config, identifier, tool).await?;

    tokio::select! {
        result = agent.run() => result?,
        _ = tokio::signal::ctrl_c() => log::info!("interrupted"),
    }

    Ok(())
}

/// Logging, in whichever of the two shapes the destination wants.
///
/// On a terminal, env_logger's default: a timestamp and a level, because
/// nothing else is going to add them.
///
/// Under systemd, neither. The journal records its own timestamp and its own
/// priority, so printing ours produces a line carrying two of each by the time
/// it reaches NervesHub -- the journal's, and the ones embedded in the message
/// text that the logging extension then ships verbatim.
///
/// The priority is the half that actually loses information. Anything a service
/// writes to stderr is recorded as `info` unless it says otherwise, so an agent
/// error arrived in NervesHub looking routine. systemd reads a leading `<N>`
/// off each line and uses it as the priority, which is what this writes.
///
/// `JOURNAL_STREAM` is set by systemd exactly when the service's output is
/// connected to the journal, so it is the condition rather than a proxy for it.
fn init_logging() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));

    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.format(|out, record| {
            use std::io::Write;

            writeln!(
                out,
                "<{}>{}: {}",
                syslog_priority(record.level()),
                record.target(),
                record.args()
            )
        });
    }

    builder.init();
}

/// The syslog priority systemd parses from a `<N>` prefix.
///
/// `Trace` and `Debug` share `debug`: syslog has no level below it, and the
/// alternative is inventing one the journal cannot store.
fn syslog_priority(level: log::Level) -> u8 {
    match level {
        log::Level::Error => 3,
        log::Level::Warn => 4,
        log::Level::Info => 6,
        log::Level::Debug | log::Level::Trace => 7,
    }
}

fn config_path() -> PathBuf {
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        if arg == "--config" || arg == "-c" {
            if let Some(path) = args.next() {
                return PathBuf::from(path);
            }
        }
    }

    std::env::var("NERVES_HUB_AGENT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/nerves-hub-link-agent.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers are systemd's, not ours: journald reads them off the `<N>`
    /// prefix, and the logging extension turns them back into the level
    /// NervesHub shows. A wrong number here is an error displayed as routine.
    #[test]
    fn levels_map_onto_syslog_priorities() {
        assert_eq!(syslog_priority(log::Level::Error), 3);
        assert_eq!(syslog_priority(log::Level::Warn), 4);
        assert_eq!(syslog_priority(log::Level::Info), 6);
        assert_eq!(syslog_priority(log::Level::Debug), 7);
        assert_eq!(syslog_priority(log::Level::Trace), 7);
    }
}
