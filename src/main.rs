mod background;
mod claude;
mod config;
mod host;
mod park;
mod parked;
mod sampler;
mod schedule;
mod watch;

use claude::Claude;
use config::{Config, Via};
use host::Host;
use park::{HerdrPanes, Parker};
use sampler::Sampler;
use schedule::{Outcome, Scheduler};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};
use watch::Watcher;

const TICK: Duration = Duration::from_secs(5);

const USAGE: &str =
    "nudge: sends scheduled prompts to coding agents, and tells them to back off when this machine is overloaded

usage:
  nudge run           every 5s: read the config, send each job's message when it is due, and check each
                      watch's CPU, memory and load limits, telling the agents when one trips or clears
  nudge send <job>    send one job's message now
  nudge check         read the config, show the host, where each job goes and when, how each watch stands,
                      which agents are parked and when each resting agent parks, and which processes
                      under Herdr are not in the background
  nudge wake <agent>  resume a parked agent (by name or pane) with no message
                      (nudge run also keeps every process under the Herdr server in the background, on macOS)
  nudge --version

config: $NUDGE_CONFIG, or ~/.config/nudge/config.toml
hosts:  herdr, tmux, cmux or orca (host = \"auto\" uses the first that answers), or claude -p for via = \"claude\"
binaries are found on PATH, ~/.local/bin, /opt/homebrew/bin and /usr/local/bin, or set
NUDGE_HERDR, NUDGE_TMUX, NUDGE_CMUX, NUDGE_ORCA, NUDGE_CLAUDE";

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn config_path() -> PathBuf {
    std::env::var_os("NUDGE_CONFIG").map(PathBuf::from).unwrap_or_else(|| home().join(".config/nudge/config.toml"))
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state")).join("nudge")
}

fn placeholder_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
        .join("nudge/parked")
}

fn claude() -> Claude {
    Claude::new(host::find("claude", "NUDGE_CLAUDE"), state_dir())
}

fn herdr() -> host::Herdr {
    host::Herdr(host::runner("herdr", "NUDGE_HERDR"))
}

fn panes() -> HerdrPanes {
    HerdrPanes { herdr: host::runner("herdr", "NUDGE_HERDR"), data: placeholder_dir() }
}

fn log(line: &str) {
    eprintln!("{} {line}", humantime::format_rfc3339_seconds(SystemTime::now()));
}

fn host_for(config: &Config) -> Box<dyn Host> {
    host::named(&config.host).unwrap_or_else(|_| host::named("auto").expect("auto is always a host"))
}

fn run() -> ExitCode {
    let path = config_path();
    let direct = claude();
    let mut scheduler = Scheduler::default();
    let mut watcher = Watcher::default();
    let mut sampler = Sampler::new();
    let mut current = Config::default();
    let mut host = host_for(&current);
    let mut last_error: Option<String> = None;
    let park_path = park::state_path(&state_dir());
    let mut parker = Parker::new(park::load(&park_path));
    let (parking_host, parking_panes) = (herdr(), panes());
    let mut enforcer = background::Enforcer::default();
    let mut machine = background::Machine::new();
    log(&format!("started, reading {}", path.display()));
    loop {
        match config::load(&path) {
            Ok(config) => {
                if config != current || last_error.is_some() {
                    log(&format!(
                        "host {}, {} jobs, {} watches: {}",
                        config.host,
                        config.jobs.len(),
                        config.watches.len(),
                        names(&config)
                    ));
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
        let now = SystemTime::now();
        scheduler.tick(now, &current, host.as_ref(), &direct).iter().for_each(|line| log(line));
        if !current.watches.is_empty() {
            watcher
                .tick(now, &current, &sampler.sample(), host.as_ref(), &parker.parked_panes())
                .iter()
                .for_each(|line| log(line));
        }
        if current.background && background::SUPPORTED {
            enforcer.tick(&mut machine).iter().for_each(|line| log(line));
        }
        if let Some(park) = &current.park {
            let before = parker.state.clone();
            parker
                .tick(now, park, &current.never_parked(), &parking_host, &parking_panes)
                .iter()
                .for_each(|line| log(line));
            let changed = parker.state != before;
            let saved = changed.then(|| park::save(&park_path, &parker.state));
            saved.and_then(Result::err).iter().for_each(|error| log(&format!("park: {error}")));
        }
        std::thread::sleep(TICK);
    }
}

fn names(config: &Config) -> String {
    config
        .jobs
        .iter()
        .map(|job| job.name.as_str())
        .chain(config.watches.iter().map(|watch| watch.name.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn send(name: &str) -> ExitCode {
    let config = match config::load(&config_path()) {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };
    let Some(job) = config.jobs.iter().find(|job| job.name == name) else {
        return fail(&format!("no job named {name}; the config has: {}", names(&config)));
    };
    match schedule::attempt(job, host_for(&config).as_ref(), &claude()) {
        Outcome::Sent(place) => {
            println!("sent {name} to {place}");
            ExitCode::SUCCESS
        }
        Outcome::Waiting(reason) => fail(&format!("not sent: {reason}")),
    }
}

fn when(at: SystemTime) -> String {
    let local: chrono::DateTime<chrono::Local> = at.into();
    local.format("%a %Y-%m-%d %H:%M").to_string()
}

fn check() -> ExitCode {
    let config = match config::load(&config_path()) {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };
    let host = host_for(&config);
    let agents = host.agents();
    println!(
        "host: {} ({})",
        config.host,
        match &agents {
            Ok(found) => format!("{} answered with {} agents", host.name(), found.len()),
            Err(error) => error.clone(),
        }
    );
    let now = SystemTime::now();
    config.jobs.iter().for_each(|job| {
        let place = match (job.via, &agents) {
            (Via::Claude, _) => format!("claude -p ({})", claude().binary.display()),
            (Via::Host, Ok(agents)) => match schedule::pick(&job.target, agents) {
                schedule::Pick::Ready(agent) => format!("{} (ready)", agent.pane_id),
                schedule::Pick::Busy(agent) => format!("{} ({})", agent.pane_id, agent.agent_status),
                schedule::Pick::Missing => "no matching agent".into(),
                schedule::Pick::Ambiguous(count) => format!("{count} matching agents"),
            },
            (Via::Host, Err(error)) => error.clone(),
        };
        println!(
            "{}: {}, next {}, {} -> {place}",
            job.name,
            job.schedule,
            when(schedule::next_after(&job.schedule, now)),
            job.target
        );
    });
    let mut sampler = Sampler::new();
    std::thread::sleep(Duration::from_secs(1));
    let sample = sampler.sample();
    config.watches.iter().for_each(|watch| {
        let state = watch::over(watch, watch::Level { cpu: sample.cpu, memory: sample.memory, load: sample.load })
            .unwrap_or_else(|| "under its limits".into());
        println!(
            "{}: CPU {:.0}%, memory {:.0}%, load {:.1} now, {state}; trips when the average over {} is over",
            watch.name,
            sample.cpu,
            sample.memory,
            sample.load,
            humantime::format_duration(watch.over_for)
        );
    });
    check_parking(&config);
    check_background(&config);
    ExitCode::SUCCESS
}

fn check_parking(config: &Config) {
    let Some(park) = &config.park else {
        println!("park: off (add a [park] table to park agents that rest)");
        return;
    };
    let never = config.never_parked();
    let state = park::load(&park::state_path(&state_dir()));
    let now = SystemTime::now();
    println!(
        "park: after {} resting{}",
        humantime::format_duration(park.after),
        if never.is_empty() {
            String::new()
        } else {
            format!(", never {}", never.iter().map(|target| target.to_string()).collect::<Vec<_>>().join(", "))
        }
    );
    state.parked.iter().for_each(|parked| {
        println!(
            "  parked: {} ({}) since {}, session {}",
            parked.name,
            parked.pane,
            when(park::moment(parked.since)),
            parked.session
        );
    });
    state.resting.iter().for_each(|resting| {
        let due = park::moment(resting.since) + park.after;
        let left = due.duration_since(now).map(|left| Duration::from_secs(left.as_secs() / 60 * 60 + 60));
        let parks = match left {
            Ok(left) => format!("parks in about {}", humantime::format_duration(left)),
            Err(_) => "parks on the next check".into(),
        };
        println!("  resting: {} ({}) since {}, {parks}", resting.name, resting.pane, when(park::moment(resting.since)));
    });
}

fn check_background(config: &Config) {
    if !config.background {
        println!("background: off ([background] enabled = false)");
        return;
    }
    if !background::SUPPORTED {
        println!("background: on, but background policy is macOS only, so nothing is changed here");
        return;
    }
    let mut machine = background::Machine::new();
    let tree = background::tree(&background::Processes::list(&mut machine));
    let escaped = background::escaped(&tree, &machine);
    let state = match escaped.as_slice() {
        [] => {
            "all at background base priority (4); ps can still show a single thread briefly boosted higher".to_string()
        }
        some => {
            format!(
                "{} above background base priority now (nudge run moves them): {}",
                some.len(),
                background::described(some)
            )
        }
    };
    println!("background: on, {} processes under {} herdr servers, {state}", tree.under.len(), tree.servers);
}

fn wake(wanted: &str) -> ExitCode {
    let state = park::load(&park::state_path(&state_dir()));
    let Some(parked) = park::find(&state, wanted) else {
        let names: Vec<String> =
            state.parked.iter().map(|parked| format!("{} ({})", parked.name, parked.pane)).collect();
        return fail(&format!(
            "no parked agent is named {wanted}; parked now: {}",
            if names.is_empty() { "none".into() } else { names.join(", ") }
        ));
    };
    match host::runner("herdr", "NUDGE_HERDR").run(&["pane", "send-keys", &parked.pane, "enter"], None) {
        Ok(_) => {
            println!("woke {} ({}), resuming session {}", parked.name, parked.pane, parked.session);
            ExitCode::SUCCESS
        }
        Err(error) => fail(&error),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("nudge: {message}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let called = std::env::args().next().unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if std::path::Path::new(&called).file_name().is_some_and(|name| name == park::PLACEHOLDER) {
        return parked::main(&args);
    }
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["run"] => run(),
        ["send", name] => send(name),
        ["wake", wanted] => wake(wanted),
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
