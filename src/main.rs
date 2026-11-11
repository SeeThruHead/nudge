mod claude;
mod config;
mod freeze;
mod host;
mod park;
mod parked;
mod processes;
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use watch::Watcher;

const TICK: Duration = Duration::from_secs(5);
const NAP: Duration = Duration::from_millis(250);

static STOPPING: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn signal(signal: i32, handler: extern "C" fn(i32)) -> usize;
}

extern "C" fn asked_to_stop(_: i32) {
    STOPPING.store(true, Ordering::SeqCst);
}

fn stop_on_signals() {
    [1, 2, 15].iter().for_each(|number| {
        unsafe { signal(*number, asked_to_stop) };
    });
}

fn stopping_within(wait: Duration) -> bool {
    let naps = (wait.as_millis() / NAP.as_millis()).max(1);
    (0..naps).any(|_| {
        std::thread::sleep(NAP);
        STOPPING.load(Ordering::SeqCst)
    })
}

fn save_freeze(path: &std::path::Path, before: &freeze::State, after: &freeze::State) {
    let saved = (before != after).then(|| freeze::save(path, after));
    saved.and_then(Result::err).iter().for_each(|error| log(&format!("freeze: {error}")));
}

const USAGE: &str =
    "nudge: sends scheduled prompts to coding agents, and tells them to back off when this machine is overloaded

usage:
  nudge run           every 5s: read the config, send each job's message when it is due, and check each
                      watch's CPU, memory and load limits, telling the agents when one trips or clears
  nudge send <job>    send one job's message now
  nudge check         read the config, show the host, where each job goes and when, how each watch stands,
                      which agents are parked and when each resting agent parks, and whether heavy
                      work under Herdr is frozen
  nudge wake <agent>  resume a parked agent (by name or pane) with no message
  nudge thaw          continue every process nudge froze (if nudge run is not running to do it)
  nudge --version

config: $NUDGE_CONFIG, or ~/.config/nudge/config.toml
hosts:  herdr (the TUI), herdr-app (the desktop app, through hc), tmux, cmux or orca (host = \"auto\" uses both
        Herdrs together, else the first that answers), or claude -p for via = \"claude\"
binaries are found on PATH, ~/.local/bin, /opt/homebrew/bin and /usr/local/bin, or set
NUDGE_HERDR, NUDGE_HC, NUDGE_TMUX, NUDGE_CMUX, NUDGE_ORCA, NUDGE_CLAUDE";

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
    let mut machine = processes::Machine::new();
    let freeze_path = freeze::state_path(&state_dir());
    let mut freezer = freeze::Freezer::new(freeze::load(&freeze_path));
    let me = std::process::id();
    stop_on_signals();
    log(&format!("started, reading {}", path.display()));
    let left_frozen = freezer.state.clone();
    freezer
        .thaw_all(SystemTime::now(), &machine.list(), &freeze::Kill, "nudge started")
        .iter()
        .for_each(|line| log(line));
    save_freeze(&freeze_path, &left_frozen, &freezer.state);
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
        let listed = if freeze::SUPPORTED { machine.list() } else { vec![] };
        if freeze::SUPPORTED {
            let before = freezer.state.clone();
            freezer
                .tick(now, &current.freeze, &freeze::read(), &listed, &freeze::Kill, me)
                .iter()
                .for_each(|line| log(line));
            save_freeze(&freeze_path, &before, &freezer.state);
        }
        scheduler.tick(now, &current, host.as_ref(), &direct).iter().for_each(|line| log(line));
        if !current.watches.is_empty() {
            watcher
                .tick(now, &current, &sampler.sample(), host.as_ref(), &parker.parked_panes())
                .iter()
                .for_each(|line| log(line));
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
        if stopping_within(TICK) {
            let before = freezer.state.clone();
            let listed = machine.list();
            freezer
                .thaw_all(SystemTime::now(), &listed, &freeze::Kill, "nudge stopping")
                .iter()
                .for_each(|line| log(line));
            save_freeze(&freeze_path, &before, &freezer.state);
            log("stopped");
            return ExitCode::SUCCESS;
        }
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
    check_freeze(&config);
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

fn check_freeze(config: &Config) {
    let limits = &config.freeze;
    if !limits.enabled {
        println!("freeze: off ([freeze] enabled = false)");
        return;
    }
    if !freeze::SUPPORTED {
        println!(
            "freeze: on, but it needs macOS to tell whether someone is at the computer, so nothing is frozen here"
        );
        return;
    }
    let reading = freeze::read();
    let at_mac = match (freeze::is_active(limits, &reading), reading.idle) {
        (true, Some(idle)) => format!("Shane is at the Mac (idle {}s)", idle.as_secs()),
        (false, Some(idle)) => {
            format!("nobody is at the Mac (idle {})", humantime::format_duration(Duration::from_secs(idle.as_secs())))
        }
        (_, None) => "could not read how long the Mac has been idle".into(),
    };
    let pressure = freeze::pressure(limits, &reading).unwrap_or_else(|| "under the limits".into());
    println!(
        "freeze: on, {at_mac}; load {:.1} (limit {:.0}), memory free {:.0}% (limit {:.0}%): {pressure}",
        reading.load, limits.load, reading.memory_free, limits.memory_free
    );
    println!(
        "  freezes {} under Herdr while someone used the Mac in the last {} and it is over a limit; thaws once idle that long or under the limits for {}",
        limits.heavy.join(", "),
        humantime::format_duration(limits.active_within),
        humantime::format_duration(limits.thaw_after)
    );
    let state = freeze::load(&freeze::state_path(&state_dir()));
    state.frozen.iter().for_each(|frozen| {
        println!("  frozen: {} ({}) since {}", frozen.name, frozen.pid, when(park::moment(frozen.since)));
    });
    state.events.iter().rev().take(5).for_each(|event| println!("  {} {}", when(park::moment(event.at)), event.line));
}

fn thaw() -> ExitCode {
    let path = freeze::state_path(&state_dir());
    let mut freezer = freeze::Freezer::new(freeze::load(&path));
    let before = freezer.state.clone();
    let mut machine = processes::Machine::new();
    let lines = freezer.thaw_all(SystemTime::now(), &machine.list(), &freeze::Kill, "nudge thaw");
    save_freeze(&path, &before, &freezer.state);
    match lines.as_slice() {
        [] => println!("nothing is frozen"),
        some => some.iter().for_each(|line| println!("{line}")),
    }
    ExitCode::SUCCESS
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
        ["thaw"] => thaw(),
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
