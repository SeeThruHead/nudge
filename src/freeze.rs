use crate::background::{Process, tree};
use crate::config::Freeze;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NEVER: [&str; 11] =
    ["claude", "herdr", "nudge", "zsh", "bash", "sh", "fish", "dash", "login", "tmux", "caffeinate"];
const ARGUMENTS_LOOKED_AT: usize = 3;
const INTERPRETERS: [&str; 7] = ["python", "python3", "node", "bun", "deno", "ruby", "perl"];
const EVENTS_KEPT: usize = 20;
const DEEPEST: usize = 64;

pub const DEFAULT_HEAVY: [&str; 14] = [
    "vitest",
    "tsc",
    "tsgo",
    "tsgolint",
    "oxlint",
    "playwright",
    "Google Chrome",
    "Chromium",
    "chrome-headless-shell",
    "cargo",
    "rustc",
    "clippy-driver",
    "bun test",
    "moon",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub idle: Option<Duration>,
    pub load: f32,
    pub memory_free: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frozen {
    pub pid: u32,
    pub started: u64,
    pub name: String,
    pub since: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub at: u64,
    pub line: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub frozen: Vec<Frozen>,
    #[serde(default)]
    pub events: Vec<Event>,
}

pub trait Signals {
    fn stop(&self, pid: u32) -> Result<(), String>;
    fn resume(&self, pid: u32) -> Result<(), String>;
}

pub fn seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map(|since| since.as_secs()).unwrap_or(0)
}

fn base_name(part: &str) -> &str {
    part.rsplit('/').next().unwrap_or(part)
}

fn component_is(component: &str, pattern: &str) -> bool {
    let rest = component.strip_prefix(pattern);
    rest.is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', '@', '.', '_']))
}

fn name_is(name: &str, pattern: &str) -> bool {
    component_is(name, pattern) || name.strip_prefix(pattern).is_some_and(|rest| rest.starts_with(' '))
}

pub fn is_heavy(process: &Process, patterns: &[String]) -> bool {
    let first: Vec<&str> = process.command.iter().take(ARGUMENTS_LOOKED_AT + 1).map(String::as_str).collect();
    let words = format!(" {} ", first.iter().map(|part| base_name(part)).collect::<Vec<_>>().join(" "));
    let in_arguments = |pattern: &str| match pattern.contains(' ') {
        true => words.contains(&format!(" {pattern} ")),
        false => first.iter().take(ARGUMENTS_LOOKED_AT).any(|part| part.split('/').any(|c| component_is(c, pattern))),
    };
    patterns.iter().any(|pattern| name_is(&process.name, pattern) || in_arguments(pattern))
}

pub fn shown_name(process: &Process) -> String {
    let interpreted = INTERPRETERS.contains(&process.name.to_lowercase().as_str());
    let script = process.command.iter().skip(1).find(|part| !part.starts_with('-')).map(|part| base_name(part));
    match (interpreted, script) {
        (true, Some(script)) => format!("{} {script}", process.name),
        _ => process.name.clone(),
    }
}

pub fn is_spared(process: &Process, me: u32) -> bool {
    let program = process.command.first().map(|first| base_name(first)).unwrap_or_default();
    let spared = |name: &str| NEVER.contains(&name.trim_start_matches('-'));
    let placeholder = process.command.first().is_some_and(|first| first.contains("/nudge/parked/"));
    process.pid == me || spared(&process.name) || spared(program) || placeholder
}

pub fn targets(listed: &[Process], patterns: &[String], me: u32) -> Vec<Process> {
    let under = tree(listed).under;
    let heavy: HashSet<u32> = under.iter().filter(|process| is_heavy(process, patterns)).map(|p| p.pid).collect();
    let parents: HashMap<u32, Option<u32>> = listed.iter().map(|process| (process.pid, process.parent)).collect();
    let in_heavy = |process: &Process| {
        std::iter::successors(Some(process.pid), |pid| parents.get(pid).copied().flatten())
            .take(DEEPEST)
            .take_while(|pid| *pid > 1)
            .any(|pid| heavy.contains(&pid))
    };
    under.into_iter().filter(|process| in_heavy(process) && !is_spared(process, me)).collect()
}

pub fn pressure(freeze: &Freeze, reading: &Reading) -> Option<String> {
    let load = (reading.load > freeze.load).then(|| format!("load {:.1} over {:.0}", reading.load, freeze.load));
    let memory = (reading.memory_free < freeze.memory_free)
        .then(|| format!("memory free {:.0}% under {:.0}%", reading.memory_free, freeze.memory_free));
    let parts: Vec<String> = load.into_iter().chain(memory).collect();
    (!parts.is_empty()).then(|| parts.join(" and "))
}

pub fn is_active(freeze: &Freeze, reading: &Reading) -> bool {
    reading.idle.is_some_and(|idle| idle < freeze.active_within)
}

fn idle_text(reading: &Reading) -> String {
    reading
        .idle
        .map(|idle| humantime::format_duration(Duration::from_secs(idle.as_secs())).to_string())
        .unwrap_or("unknown".into())
}

fn listed_names(processes: &[Frozen]) -> String {
    processes.iter().map(|frozen| format!("{} ({})", frozen.name, frozen.pid)).collect::<Vec<_>>().join(", ")
}

#[derive(Default)]
pub struct Freezer {
    pub state: State,
    calm_since: Option<SystemTime>,
}

impl Freezer {
    pub fn new(state: State) -> Freezer {
        Freezer { state, calm_since: None }
    }

    fn note(&mut self, now: SystemTime, line: String) -> String {
        let events = std::mem::take(&mut self.state.events);
        let skip = events.len().saturating_sub(EVENTS_KEPT - 1);
        self.state.events =
            events.into_iter().skip(skip).chain([Event { at: seconds(now), line: line.clone() }]).collect();
        line
    }

    pub fn thaw_all(&mut self, now: SystemTime, listed: &[Process], signals: &dyn Signals, why: &str) -> Vec<String> {
        let frozen = std::mem::take(&mut self.state.frozen);
        self.calm_since = None;
        let alive: Vec<Frozen> = frozen
            .into_iter()
            .filter(|frozen| {
                listed.iter().any(|process| process.pid == frozen.pid && process.started == frozen.started)
            })
            .collect();
        if alive.is_empty() {
            return vec![];
        }
        alive.iter().for_each(|frozen| {
            let _ = signals.resume(frozen.pid);
        });
        vec![self.note(now, format!("freeze: thawed {} processes ({why}): {}", alive.len(), listed_names(&alive)))]
    }

    fn freeze(&mut self, now: SystemTime, found: Vec<Process>, signals: &dyn Signals, why: &str) -> Vec<String> {
        let held: HashSet<u32> = self.state.frozen.iter().map(|frozen| frozen.pid).collect();
        let stopped: Vec<Frozen> = found
            .into_iter()
            .filter(|process| !held.contains(&process.pid))
            .filter(|process| signals.stop(process.pid).is_ok())
            .map(|process| Frozen {
                pid: process.pid,
                started: process.started,
                name: shown_name(&process),
                since: seconds(now),
            })
            .collect();
        if stopped.is_empty() {
            return vec![];
        }
        let line = format!("freeze: froze {} processes ({why}): {}", stopped.len(), listed_names(&stopped));
        self.state.frozen.extend(stopped);
        vec![self.note(now, line)]
    }

    pub fn tick(
        &mut self,
        now: SystemTime,
        freeze: &Freeze,
        reading: &Reading,
        listed: &[Process],
        signals: &dyn Signals,
        me: u32,
    ) -> Vec<String> {
        let live: Vec<Frozen> = std::mem::take(&mut self.state.frozen)
            .into_iter()
            .filter(|frozen| {
                listed.iter().any(|process| process.pid == frozen.pid && process.started == frozen.started)
            })
            .collect();
        self.state.frozen = live;
        if !freeze.enabled {
            return self.thaw_all(now, listed, signals, "freeze is off");
        }
        let active = is_active(freeze, reading);
        let pressed = pressure(freeze, reading);
        match (active, pressed) {
            (false, _) => self.thaw_all(now, listed, signals, &format!("Shane idle {}", idle_text(reading))),
            (true, Some(reason)) => {
                self.calm_since = None;
                let why = format!("{reason} while Shane is at the Mac, idle {}", idle_text(reading));
                self.freeze(now, targets(listed, &freeze.heavy, me), signals, &why)
            }
            (true, None) => {
                let calm = *self.calm_since.get_or_insert(now);
                let settled = now.duration_since(calm).is_ok_and(|calm_for| calm_for >= freeze.thaw_after);
                if !settled {
                    return vec![];
                }
                let why = format!(
                    "load {:.1} and memory free {:.0}% under the limits for {}",
                    reading.load,
                    reading.memory_free,
                    humantime::format_duration(freeze.thaw_after)
                );
                self.thaw_all(now, listed, signals, &why)
            }
        }
    }
}

pub fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("freeze.json")
}

pub fn load(path: &Path) -> State {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

pub fn save(path: &Path, state: &State) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    let text = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, text).map_err(|e| format!("could not write {}: {e}", partial.display()))?;
    std::fs::rename(&partial, path).map_err(|e| format!("could not write {}: {e}", path.display()))
}

pub fn parse_idle(ioreg: &str) -> Option<Duration> {
    ioreg
        .lines()
        .find_map(|line| line.split_once("\"HIDIdleTime\" = ").map(|(_, value)| value.trim().to_string()))
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_nanos)
}

pub fn idle() -> Option<Duration> {
    let out = std::process::Command::new("/usr/sbin/ioreg").args(["-c", "IOHIDSystem"]).output().ok()?;
    parse_idle(&String::from_utf8_lossy(&out.stdout))
}

pub fn read() -> Reading {
    Reading {
        idle: idle(),
        load: sysinfo::System::load_average().one as f32,
        memory_free: crate::sampler::memory_free(),
    }
}

pub struct Kill;

#[cfg(target_os = "macos")]
const SIGSTOP: i32 = 17;
#[cfg(target_os = "macos")]
const SIGCONT: i32 = 19;
#[cfg(not(target_os = "macos"))]
const SIGSTOP: i32 = 19;
#[cfg(not(target_os = "macos"))]
const SIGCONT: i32 = 18;

const SIGTERM: i32 = 15;

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

fn send(pid: u32, signal: i32) -> Result<(), String> {
    let pid = i32::try_from(pid).map_err(|e| e.to_string())?;
    match unsafe { kill(pid, signal) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error().to_string()),
    }
}

pub fn terminate(pid: u32) -> Result<(), String> {
    send(pid, SIGTERM)
}

impl Signals for Kill {
    fn stop(&self, pid: u32) -> Result<(), String> {
        send(pid, SIGSTOP)
    }
    fn resume(&self, pid: u32) -> Result<(), String> {
        send(pid, SIGCONT)
    }
}

pub const SUPPORTED: bool = cfg!(target_os = "macos");

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn process(pid: u32, parent: u32, name: &str, command: &[&str]) -> Process {
        Process {
            pid,
            parent: Some(parent),
            name: name.into(),
            command: command.iter().map(|part| part.to_string()).collect(),
            started: 100 + pid as u64,
        }
    }

    fn machine() -> Vec<Process> {
        vec![
            process(10, 1, "herdr", &["/Users/me/.local/bin/herdr"]),
            process(11, 10, "zsh", &["-zsh"]),
            process(12, 11, "claude", &["claude", "--tools", "Read"]),
            process(13, 12, "moon", &["moon", "run", "core:test"]),
            process(14, 13, "bun", &["/Users/me/.bun/bin/bun", "--bun", "vitest", "run"]),
            process(15, 14, "node", &["node", "/code/app/node_modules/.bin/vitest", "run"]),
            process(16, 15, "Google Chrome for Testing", &["/Applications/Chrome.app/Contents/MacOS/Google Chrome"]),
            process(17, 12, "zsh", &["/bin/zsh", "-c", "cat tsconfig.json"]),
            process(18, 17, "cat", &["cat", "tsconfig.json"]),
            process(19, 12, "cargo", &["cargo", "build", "--release"]),
            process(20, 19, "rustc", &["rustc", "--crate-name", "nudge"]),
            process(21, 12, "bun", &["bun", "test"]),
            process(22, 12, "bun", &["bun", "run", "dev"]),
            process(23, 12, "node", &["node", "/x/node_modules/playwright-core/cli.js", "run-driver"]),
            process(24, 12, "nudge", &["/opt/homebrew/bin/nudge", "run"]),
            process(30, 1, "cargo", &["cargo", "build"]),
            process(31, 1, "Google Chrome", &["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"]),
        ]
    }

    fn defaults() -> Vec<String> {
        DEFAULT_HEAVY.iter().map(|pattern| pattern.to_string()).collect()
    }

    #[test]
    fn freezes_heavy_work_under_herdr_and_what_it_started_but_never_claude_shells_nudge_or_anything_outside() {
        let found: Vec<u32> = targets(&machine(), &defaults(), 999).iter().map(|process| process.pid).collect();

        assert_eq!(found, [13, 14, 15, 16, 19, 20, 21, 23]);
        assert_eq!(targets(&machine(), &defaults(), 24).len(), 8);
    }

    struct Fake {
        stopped: RefCell<Vec<u32>>,
        resumed: RefCell<Vec<u32>>,
    }

    impl Signals for Fake {
        fn stop(&self, pid: u32) -> Result<(), String> {
            self.stopped.borrow_mut().push(pid);
            Ok(())
        }
        fn resume(&self, pid: u32) -> Result<(), String> {
            self.resumed.borrow_mut().push(pid);
            Ok(())
        }
    }

    fn fake() -> Fake {
        Fake { stopped: RefCell::new(vec![]), resumed: RefCell::new(vec![]) }
    }

    fn limits() -> Freeze {
        Freeze {
            enabled: true,
            active_within: Duration::from_secs(120),
            load: 10.0,
            memory_free: 20.0,
            thaw_after: Duration::from_secs(60),
            heavy: vec!["cargo".into()],
        }
    }

    fn reading(idle: u64, load: f32, memory_free: f32) -> Reading {
        Reading { idle: Some(Duration::from_secs(idle)), load, memory_free }
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000 + seconds)
    }

    #[test]
    fn freezes_only_while_shane_is_at_the_mac_and_it_is_loaded_and_thaws_once_he_leaves() {
        let signals = fake();
        let mut freezer = Freezer::default();
        let away = freezer.tick(at(0), &limits(), &reading(600, 14.0, 50.0), &machine(), &signals, 999);
        let calm = freezer.tick(at(5), &limits(), &reading(3, 6.0, 50.0), &machine(), &signals, 999);
        let froze = freezer.tick(at(10), &limits(), &reading(3, 12.3, 50.0), &machine(), &signals, 999);
        let again = freezer.tick(at(15), &limits(), &reading(3, 12.0, 50.0), &machine(), &signals, 999);
        let left = freezer.tick(at(200), &limits(), &reading(121, 12.0, 50.0), &machine(), &signals, 999);

        assert!(away.is_empty() && calm.is_empty() && again.is_empty());
        assert_eq!(
            froze,
            [
                "freeze: froze 2 processes (load 12.3 over 10 while Shane is at the Mac, idle 3s): cargo (19), rustc (20)"
            ]
        );
        assert_eq!(*signals.stopped.borrow(), [19, 20]);
        assert_eq!(left, ["freeze: thawed 2 processes (Shane idle 2m 1s): cargo (19), rustc (20)"]);
        assert_eq!(*signals.resumed.borrow(), [19, 20]);
        assert_eq!(freezer.state.events.len(), 2);
    }

    #[test]
    fn thaws_after_load_and_memory_stay_under_the_limits_for_a_while_and_freezes_on_low_memory() {
        let signals = fake();
        let mut freezer = Freezer::default();
        let froze = freezer.tick(at(0), &limits(), &reading(1, 4.0, 12.0), &machine(), &signals, 999);
        let early = freezer.tick(at(30), &limits(), &reading(1, 4.0, 40.0), &machine(), &signals, 999);
        let late = freezer.tick(at(89), &limits(), &reading(1, 4.0, 40.0), &machine(), &signals, 999);
        let thawed = freezer.tick(at(90), &limits(), &reading(1, 4.0, 40.0), &machine(), &signals, 999);

        assert!(froze[0].contains("memory free 12% under 20%"));
        assert!(early.is_empty() && late.is_empty());
        assert!(
            thawed[0].starts_with("freeze: thawed 2 processes (load 4.0 and memory free 40% under the limits for 1m)")
        );
    }

    #[test]
    fn a_restarted_nudge_thaws_what_it_froze_but_not_a_new_process_that_reused_the_pid() {
        let signals = fake();
        let state = State {
            frozen: vec![
                Frozen { pid: 19, started: 119, name: "cargo".into(), since: 0 },
                Frozen { pid: 20, started: 7, name: "rustc".into(), since: 0 },
            ],
            events: vec![],
        };
        let mut freezer = Freezer::new(state);

        let thawed = freezer.thaw_all(at(0), &machine(), &signals, "nudge started");

        assert_eq!(thawed, ["freeze: thawed 1 processes (nudge started): cargo (19)"]);
        assert_eq!(*signals.resumed.borrow(), [19]);
        assert!(freezer.state.frozen.is_empty());
    }

    #[test]
    fn turning_it_off_thaws_everything() {
        let signals = fake();
        let mut freezer = Freezer::default();
        freezer.tick(at(0), &limits(), &reading(1, 14.0, 50.0), &machine(), &signals, 999);
        let off = Freeze { enabled: false, ..limits() };

        assert_eq!(freezer.tick(at(5), &off, &reading(1, 14.0, 50.0), &machine(), &signals, 999).len(), 1);
        assert_eq!(*signals.resumed.borrow(), [19, 20]);
    }

    #[test]
    fn names_a_script_by_the_script_it_runs() {
        let names: Vec<String> =
            machine().iter().filter(|p| [14, 15, 23, 19].contains(&p.pid)).map(shown_name).collect();

        assert_eq!(names, ["bun vitest", "node vitest", "cargo", "node cli.js"]);
    }

    #[test]
    fn reads_how_long_since_the_keyboard_or_mouse_was_used() {
        let ioreg = "    | |   \"HIDIdleTime\" = 29971422125\n    | |   \"HIDIdleTimeDelta\" = 1\n";

        assert_eq!(parse_idle(ioreg), Some(Duration::from_nanos(29_971_422_125)));
        assert_eq!(parse_idle("nothing"), None);
    }

    #[test]
    fn stops_and_continues_a_real_process() {
        let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let state = |pid: u32| {
            let out = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        Kill.stop(child.id()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let stopped = state(child.id());
        Kill.resume(child.id()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let running = state(child.id());
        let _ = child.kill();
        let _ = child.wait();

        assert!(stopped.starts_with('T'));
        assert!(!running.starts_with('T'));
    }
}
