mod config;
mod herdr;
mod schedule;

use config::Config;
use herdr::Cli;
use schedule::{Outcome, Scheduler};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

const TICK: Duration = Duration::from_secs(30);
const HERDR_TIMEOUT: Duration = Duration::from_secs(20);

const USAGE: &str = "nudge: prompts Herdr-hosted coding agents on a schedule

usage:
  nudge run           check the config every 30s and send each job's message when it is due
  nudge send <job>    send one job's message now
  nudge check         read the config and show where each job would go

config: $NUDGE_CONFIG, or ~/.config/nudge/config.toml
herdr:  $NUDGE_HERDR, or ~/.local/bin/herdr";

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn config_path() -> PathBuf {
    std::env::var_os("NUDGE_CONFIG").map(PathBuf::from).unwrap_or_else(|| home().join(".config/nudge/config.toml"))
}

fn cli() -> Cli {
    let binary =
        std::env::var_os("NUDGE_HERDR").map(PathBuf::from).unwrap_or_else(|| home().join(".local/bin/herdr"));
    Cli { binary, timeout: HERDR_TIMEOUT }
}

fn log(line: &str) {
    eprintln!("{} {line}", humantime::format_rfc3339_seconds(SystemTime::now()));
}

fn run() -> ExitCode {
    let path = config_path();
    let herdr = cli();
    let mut scheduler = Scheduler::default();
    let mut current = Config::default();
    let mut last_error: Option<String> = None;
    log(&format!("started, reading {}", path.display()));
    loop {
        match config::load(&path) {
            Ok(config) => {
                if config != current || last_error.is_some() {
                    log(&format!("{} jobs: {}", config.jobs.len(), names(&config)));
                }
                current = config;
                last_error = None;
            }
            Err(error) => {
                if last_error.as_ref() != Some(&error) {
                    log(&format!("config unreadable, keeping the last good one: {error}"));
                }
                last_error = Some(error);
            }
        }
        scheduler.tick(SystemTime::now(), &current, &herdr).iter().for_each(|line| log(line));
        std::thread::sleep(TICK);
    }
}

fn names(config: &Config) -> String {
    config.jobs.iter().map(|job| job.name.as_str()).collect::<Vec<_>>().join(", ")
}

fn send(name: &str) -> ExitCode {
    let config = match config::load(&config_path()) {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };
    let Some(job) = config.jobs.iter().find(|job| job.name == name) else {
        return fail(&format!("no job named {name}; the config has: {}", names(&config)));
    };
    match schedule::attempt(job, &cli()) {
        Outcome::Sent(pane) => {
            println!("sent {name} to {pane}");
            ExitCode::SUCCESS
        }
        Outcome::Waiting(reason) => fail(&format!("not sent: {reason}")),
    }
}

fn check() -> ExitCode {
    let config = match config::load(&config_path()) {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };
    let agents = herdr::Herdr::agents(&cli());
    config.jobs.iter().for_each(|job| {
        let place = match &agents {
            Ok(agents) => match schedule::pick(&job.target, agents) {
                schedule::Pick::Ready(agent) => format!("{} (ready)", agent.pane_id),
                schedule::Pick::Busy(agent) => format!("{} ({})", agent.pane_id, agent.agent_status),
                schedule::Pick::Missing => "no matching agent".into(),
                schedule::Pick::Ambiguous(count) => format!("{count} matching agents"),
            },
            Err(error) => error.clone(),
        };
        println!("{}: every {}, {} -> {place}", job.name, humantime::format_duration(job.every), job.target);
    });
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("nudge: {message}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["run"] => run(),
        ["send", name] => send(name),
        ["check"] => check(),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
