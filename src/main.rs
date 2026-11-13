mod config;
mod host;
mod reach;
mod schedule;

use config::{Config, Target};
use host::Host;
use reach::{Directory, Reach, Resolved};
use schedule::{Outcome, Scheduler};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

const TICK: Duration = Duration::from_secs(5);
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(15);
const REACH_TIMEOUT: Duration = Duration::from_secs(45);

const USAGE: &str = "nudge: sends messages to coding agents, on a schedule or on demand

usage:
  nudge run           every 5s: read the config and send each job's message when it is due,
                      waiting while its agent is busy
  nudge send <job>    send one job's message now
  nudge check         read the config, show the host, where each job goes and when it runs next
  nudge --version

config: $NUDGE_CONFIG, or ~/.config/nudge/config.toml
targets: { address = \"machine/space/name\" } through reach, or { cwd }, { agent }, { session } through the host
hosts:  herdr (the TUI), herdr-app (the desktop app, through hc), or auto (both together)
binaries are found on PATH, ~/.local/bin, /opt/homebrew/bin and /usr/local/bin,
or set NUDGE_HERDR, NUDGE_HC, NUDGE_REACH";

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn config_path() -> PathBuf {
    std::env::var_os("NUDGE_CONFIG").map(PathBuf::from).unwrap_or_else(|| home().join(".config/nudge/config.toml"))
}

fn log(line: &str) {
    eprintln!("{} {line}", humantime::format_rfc3339_seconds(SystemTime::now()));
}

fn host_for(config: &Config) -> Box<dyn Host> {
    host::named(&config.host).unwrap_or_else(|_| host::named("auto").expect("auto is always a host"))
}

fn directory() -> Reach {
    Reach(host::Runner { binary: host::find("reach", "NUDGE_REACH"), timeout: REACH_TIMEOUT })
}

fn notify(command: &[String], line: &str) {
    let Some((program, args)) = command.split_first() else { return };
    let runner = host::Runner { binary: host::find(program, "NUDGE_NOTIFY"), timeout: NOTIFY_TIMEOUT };
    let with_message: Vec<&str> = args.iter().map(String::as_str).chain(std::iter::once(line)).collect();
    if let Err(error) = runner.run(&with_message) {
        log(&format!("could not notify: {error}"));
    }
}

fn run() -> ExitCode {
    let path = config_path();
    let mut scheduler = Scheduler::default();
    let mut current = Config::default();
    let mut host = host_for(&current);
    let reach = directory();
    let mut last_error: Option<String> = None;
    log(&format!("started, reading {}", path.display()));
    loop {
        match config::load(&path) {
            Ok(config) => {
                if config != current || last_error.is_some() {
                    log(&format!("host {}, {} jobs: {}", config.host, config.jobs.len(), names(&config)));
                    host = host_for(&config);
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
        scheduler.tick(SystemTime::now(), &current, host.as_ref(), &reach).iter().for_each(|line| log(line));
        scheduler.take_alerts().iter().for_each(|alert| notify(&current.notify, &format!("nudge {alert}")));
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
    match schedule::attempt(job, host_for(&config).as_ref(), &directory()) {
        Outcome::Sent(place) => {
            println!("sent {name} to {place}");
            ExitCode::SUCCESS
        }
        Outcome::Waiting(reason) => fail(&format!("not sent: {reason}")),
        Outcome::Failed(reason) => fail(&format!("not sent: {reason}")),
    }
}

fn when(at: SystemTime) -> String {
    let local: chrono::DateTime<chrono::Local> = at.into();
    local.format("%a %Y-%m-%d %H:%M").to_string()
}

fn place_of_address(address: &str, reach: &dyn Directory) -> String {
    match reach.resolve(address) {
        Resolved::Agent(found) => format!("{}:{} ({}, {})", found.host, found.id, found.state, found.status),
        Resolved::Refused(refusal) => format!("refused: {}", refusal.message),
        Resolved::Unavailable(reason) => reason,
    }
}

fn place_on_host(target: &Target, agents: &Result<Vec<host::Agent>, String>) -> String {
    match agents {
        Ok(agents) => match schedule::pick(target, agents) {
            schedule::Pick::Ready(agent) => format!("{} (ready)", agent.pane_id),
            schedule::Pick::Busy(agent) => format!("{} ({})", agent.pane_id, agent.agent_status),
            schedule::Pick::Missing => "no matching agent".into(),
            schedule::Pick::Ambiguous(count) => format!("{count} matching agents, none will be picked"),
        },
        Err(error) => error.clone(),
    }
}

fn check() -> ExitCode {
    let config = match config::load(&config_path()) {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };
    let host = host_for(&config);
    let agents = host.agents();
    let reach = directory();
    println!(
        "host: {} ({})",
        config.host,
        match &agents {
            Ok(found) => format!("{} answered with {} agents", host.name(), found.len()),
            Err(error) => error.clone(),
        }
    );
    if !config.notify.is_empty() {
        println!("notify: {}", config.notify.join(" "));
    }
    let now = SystemTime::now();
    config.jobs.iter().for_each(|job| {
        let place = match &job.target {
            Target::Address(address) => place_of_address(address, &reach),
            target => place_on_host(target, &agents),
        };
        println!(
            "{}: {}, next {}, {} -> {place}",
            job.name,
            job.schedule,
            when(schedule::next_after(&job.schedule, now)),
            job.target
        );
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
        ["--version" | "-V"] => {
            println!("nudge {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
