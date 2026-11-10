use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Session(String),
    Agent(String),
    Cwd(String),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Session(id) => write!(f, "session {id}"),
            Target::Agent(name) => write!(f, "agent {name}"),
            Target::Cwd(cwd) => write!(f, "the agent in {cwd}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    Every(Duration),
    Cron(String),
    At(Vec<(u32, u32)>),
}

impl std::fmt::Display for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Schedule::Every(every) => write!(f, "every {}", humantime::format_duration(*every)),
            Schedule::Cron(pattern) => write!(f, "cron {pattern}"),
            Schedule::At(times) => write!(
                f,
                "at {}",
                times.iter().map(|(hour, minute)| format!("{hour:02}:{minute:02}")).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    #[default]
    Host,
    Claude,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    pub schedule: Schedule,
    pub target: Target,
    pub via: Via,
    pub args: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Watch {
    pub name: String,
    pub cpu: Option<f32>,
    pub memory: Option<f32>,
    pub load: Option<f32>,
    pub over_for: Duration,
    pub clear_for: Duration,
    pub investigator: Option<Target>,
    pub broadcast: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Park {
    pub after: Duration,
    pub skip: Vec<Target>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Freeze {
    pub enabled: bool,
    pub active_within: Duration,
    pub load: f32,
    pub memory_free: f32,
    pub thaw_after: Duration,
    pub heavy: Vec<String>,
}

pub fn cores() -> f32 {
    std::thread::available_parallelism().map(|count| count.get() as f32).unwrap_or(8.0)
}

impl Default for Freeze {
    fn default() -> Freeze {
        Freeze {
            enabled: true,
            active_within: Duration::from_secs(120),
            load: cores(),
            memory_free: 20.0,
            thaw_after: Duration::from_secs(60),
            heavy: crate::freeze::DEFAULT_HEAVY.iter().map(|pattern| pattern.to_string()).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub host: String,
    pub jobs: Vec<Job>,
    pub watches: Vec<Watch>,
    pub park: Option<Park>,
    pub background: bool,
    pub freeze: Freeze,
}

impl Config {
    pub fn never_parked(&self) -> Vec<Target> {
        let skip = self.park.iter().flat_map(|park| park.skip.iter().cloned());
        let all: Vec<Target> = skip.chain(self.watches.iter().filter_map(|watch| watch.investigator.clone())).collect();
        all.iter()
            .enumerate()
            .filter(|(index, target)| !all[..*index].contains(target))
            .map(|(_, t)| t.clone())
            .collect()
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            host: "auto".into(),
            jobs: vec![],
            watches: vec![],
            park: None,
            background: true,
            freeze: Freeze::default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Times {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJob {
    name: String,
    every: Option<String>,
    cron: Option<String>,
    at: Option<Times>,
    target: Target,
    #[serde(default)]
    via: Via,
    #[serde(default)]
    args: Vec<String>,
    message: String,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWatch {
    name: String,
    cpu: Option<f32>,
    memory: Option<f32>,
    load: Option<f32>,
    #[serde(rename = "for", default = "two_minutes")]
    over_for: String,
    #[serde(default = "five_minutes")]
    clear_for: String,
    investigator: Option<Target>,
    #[serde(default = "yes")]
    broadcast: bool,
    #[serde(default)]
    message: String,
}

fn two_minutes() -> String {
    "2m".into()
}

fn five_minutes() -> String {
    "5m".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPark {
    #[serde(default = "thirty_minutes")]
    after: String,
    #[serde(default)]
    skip: Vec<Target>,
}

fn thirty_minutes() -> String {
    "30m".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFreeze {
    #[serde(default = "yes")]
    enabled: bool,
    active_within: Option<String>,
    load: Option<f32>,
    memory_free: Option<f32>,
    thaw_after: Option<String>,
    heavy: Option<Vec<String>>,
    #[serde(default)]
    also: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackground {
    #[serde(default = "yes")]
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "auto")]
    host: String,
    #[serde(default)]
    job: Vec<RawJob>,
    #[serde(default)]
    watch: Vec<RawWatch>,
    park: Option<RawPark>,
    background: Option<RawBackground>,
    freeze: Option<RawFreeze>,
}

fn auto() -> String {
    "auto".into()
}

const HOSTS: [&str; 6] = ["auto", "herdr", "herdr-app", "tmux", "cmux", "orca"];

const SHORTEST: Duration = Duration::from_secs(60);

fn every(name: &str, text: &str) -> Result<Schedule, String> {
    let every =
        humantime::parse_duration(text).map_err(|e| format!("job {name}: every = {text:?} is not a duration ({e})"))?;
    if every < SHORTEST {
        return Err(format!("job {name}: every must be at least 1m"));
    }
    Ok(Schedule::Every(every))
}

fn cron(name: &str, pattern: &str) -> Result<Schedule, String> {
    croner::Cron::new(pattern)
        .parse()
        .map(|_| Schedule::Cron(pattern.trim().to_string()))
        .map_err(|e| format!("job {name}: cron = {pattern:?} is not a cron pattern ({e})"))
}

fn clock(name: &str, text: &str) -> Result<(u32, u32), String> {
    let wrong = || format!("job {name}: at = {text:?} is not a time like \"09:30\"");
    let (hour, minute) = text.trim().split_once(':').ok_or_else(wrong)?;
    let hour: u32 = hour.parse().map_err(|_| wrong())?;
    let minute: u32 = minute.parse().map_err(|_| wrong())?;
    if hour > 23 || minute > 59 {
        return Err(wrong());
    }
    Ok((hour, minute))
}

fn at(name: &str, times: Times) -> Result<Schedule, String> {
    let texts = match times {
        Times::One(text) => vec![text],
        Times::Many(texts) => texts,
    };
    if texts.is_empty() {
        return Err(format!("job {name}: at needs at least one time"));
    }
    texts.iter().map(|text| clock(name, text)).collect::<Result<Vec<_>, _>>().map(Schedule::At)
}

fn schedule(raw: &mut RawJob) -> Result<Schedule, String> {
    match (raw.every.take(), raw.cron.take(), raw.at.take()) {
        (Some(text), None, None) => every(&raw.name, &text),
        (None, Some(pattern), None) => cron(&raw.name, &pattern),
        (None, None, Some(times)) => at(&raw.name, times),
        _ => Err(format!("job {}: give exactly one of every, cron or at", raw.name)),
    }
}

fn job(mut raw: RawJob) -> Result<Job, String> {
    let schedule = schedule(&mut raw)?;
    if raw.message.trim().is_empty() {
        return Err(format!("job {}: message is empty", raw.name));
    }
    if raw.via == Via::Claude && matches!(raw.target, Target::Agent(_)) {
        return Err(format!("job {}: via = \"claude\" needs a cwd or session target", raw.name));
    }
    if raw.via == Via::Host && !raw.args.is_empty() {
        return Err(format!("job {}: args are only for via = \"claude\"", raw.name));
    }
    Ok(Job {
        name: raw.name,
        schedule,
        target: raw.target,
        via: raw.via,
        args: raw.args,
        message: raw.message.trim().to_string(),
    })
}

fn duration(name: &str, key: &str, text: &str) -> Result<Duration, String> {
    humantime::parse_duration(text).map_err(|e| format!("watch {name}: {key} = {text:?} is not a duration ({e})"))
}

fn percent(name: &str, key: &str, value: Option<f32>) -> Result<Option<f32>, String> {
    match value {
        Some(limit) if !(1.0..=100.0).contains(&limit) => Err(format!("{name}: {key} must be a percent from 1 to 100")),
        other => Ok(other),
    }
}

fn positive(name: &str, key: &str, value: Option<f32>) -> Result<Option<f32>, String> {
    match value {
        Some(limit) if limit <= 0.0 => Err(format!("{name}: {key} must be above 0")),
        other => Ok(other),
    }
}

fn watch(raw: RawWatch) -> Result<Watch, String> {
    let owner = format!("watch {}", raw.name);
    let cpu = percent(&owner, "cpu", raw.cpu)?;
    let memory = percent(&owner, "memory", raw.memory)?;
    let load = positive(&owner, "load", raw.load)?;
    if cpu.is_none() && memory.is_none() && load.is_none() {
        return Err(format!("watch {}: give a cpu, memory or load limit", raw.name));
    }
    if raw.investigator.is_none() && !raw.broadcast {
        return Err(format!("watch {}: with broadcast = false it needs an investigator", raw.name));
    }
    Ok(Watch {
        over_for: duration(&raw.name, "for", &raw.over_for)?,
        clear_for: duration(&raw.name, "clear_for", &raw.clear_for)?,
        name: raw.name,
        cpu,
        memory,
        load,
        investigator: raw.investigator,
        broadcast: raw.broadcast,
        message: raw.message.trim().to_string(),
    })
}

fn park(raw: RawPark) -> Result<Park, String> {
    let after = humantime::parse_duration(&raw.after)
        .map_err(|e| format!("park: after = {:?} is not a duration ({e})", raw.after))?;
    if after < SHORTEST {
        return Err("park: after must be at least 1m".into());
    }
    Ok(Park { after, skip: raw.skip })
}

fn freeze_duration(key: &str, text: Option<String>, default: Duration) -> Result<Duration, String> {
    text.map(|text| {
        humantime::parse_duration(&text).map_err(|e| format!("freeze: {key} = {text:?} is not a duration ({e})"))
    })
    .unwrap_or(Ok(default))
}

fn freeze(raw: Option<RawFreeze>) -> Result<Freeze, String> {
    let defaults = Freeze::default();
    let Some(raw) = raw else { return Ok(defaults) };
    let load = positive("freeze", "load", raw.load)?.unwrap_or(defaults.load);
    let memory_free = percent("freeze", "memory_free", raw.memory_free)?.unwrap_or(defaults.memory_free);
    let heavy: Vec<String> = raw.heavy.unwrap_or(defaults.heavy).into_iter().chain(raw.also).collect();
    if raw.enabled && heavy.iter().all(|pattern| pattern.trim().is_empty()) {
        return Err("freeze: heavy names nothing to freeze".into());
    }
    Ok(Freeze {
        enabled: raw.enabled,
        active_within: freeze_duration("active_within", raw.active_within, defaults.active_within)?,
        load,
        memory_free,
        thaw_after: freeze_duration("thaw_after", raw.thaw_after, defaults.thaw_after)?,
        heavy,
    })
}

fn unique<'a>(names: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let mut names: Vec<&str> = names.collect();
    names.sort_unstable();
    match names.windows(2).find(|pair| pair[0] == pair[1]) {
        Some(pair) => Err(format!("two jobs or watches are named {}", pair[0])),
        None => Ok(()),
    }
}

pub fn parse(text: &str) -> Result<Config, String> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
    if !HOSTS.contains(&raw.host.as_str()) {
        return Err(format!("host = {:?} is not one of {}", raw.host, HOSTS.join(", ")));
    }
    let jobs = raw.job.into_iter().map(job).collect::<Result<Vec<_>, _>>()?;
    let watches = raw.watch.into_iter().map(watch).collect::<Result<Vec<_>, _>>()?;
    unique(jobs.iter().map(|j| j.name.as_str()).chain(watches.iter().map(|w| w.name.as_str())))?;
    let park = raw.park.map(park).transpose()?;
    let background = raw.background.is_none_or(|background| background.enabled);
    let freeze = freeze(raw.freeze)?;
    Ok(Config { host: raw.host, jobs, watches, park, background, freeze })
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[[job]]
name = "encode-watch"
every = "30m"
target = { cwd = "/Users/me/code/ops" }
message = """
Check that encoding is running.
"""

[[job]]
name = "by-name"
every = "1h 30m"
target = { agent = "homelab" }
message = "hi"
"#;

    #[test]
    fn reads_jobs_with_their_interval_target_and_trimmed_message() {
        let config = parse(EXAMPLE).unwrap();
        assert_eq!(
            config.jobs,
            vec![
                Job {
                    name: "encode-watch".into(),
                    schedule: Schedule::Every(Duration::from_secs(30 * 60)),
                    target: Target::Cwd("/Users/me/code/ops".into()),
                    via: Via::Host,
                    args: vec![],
                    message: "Check that encoding is running.".into(),
                },
                Job {
                    name: "by-name".into(),
                    schedule: Schedule::Every(Duration::from_secs(90 * 60)),
                    target: Target::Agent("homelab".into()),
                    via: Via::Host,
                    args: vec![],
                    message: "hi".into(),
                },
            ]
        );
    }

    #[test]
    fn an_empty_file_has_no_jobs() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn reads_cron_and_daily_schedules_the_host_and_direct_claude_jobs() {
        let text = r#"
host = "tmux"

[[job]]
name = "standup"
cron = "0 9 * * 1-5"
target = { cwd = "/code/app" }
message = "Summarise yesterday."

[[job]]
name = "twice"
at = ["09:00", "17:30"]
target = { session = "abc" }
via = "claude"
args = ["--permission-mode", "acceptEdits"]
message = "Check the build."
"#;
        let config = parse(text).unwrap();

        assert_eq!(config.host, "tmux");
        assert_eq!(config.jobs[0].schedule, Schedule::Cron("0 9 * * 1-5".into()));
        assert_eq!(config.jobs[1].schedule, Schedule::At(vec![(9, 0), (17, 30)]));
        assert_eq!(config.jobs[1].via, Via::Claude);
        assert_eq!(config.jobs[1].args, ["--permission-mode", "acceptEdits"]);
        assert_eq!(config.jobs[1].schedule.to_string(), "at 09:00, 17:30");
        assert_eq!(parse("").unwrap().host, "auto");
    }

    #[test]
    fn rejects_schedules_hosts_and_claude_jobs_that_cannot_work() {
        let job =
            |extra: &str| format!("[[job]]\nname = \"x\"\ntarget = {{ agent = \"a\" }}\nmessage = \"m\"\n{extra}");
        let errors = [
            job(""),
            job("every = \"5m\"\ncron = \"0 9 * * *\"\n"),
            job("cron = \"every tuesday\"\n"),
            job("at = \"25:00\"\n"),
            job("every = \"5m\"\nvia = \"claude\"\n"),
            job("every = \"5m\"\nargs = [\"-x\"]\n"),
            "host = \"screen\"\n".to_string(),
        ]
        .map(|text| parse(&text).unwrap_err());

        assert_eq!(errors[0], "job x: give exactly one of every, cron or at");
        assert_eq!(errors[1], "job x: give exactly one of every, cron or at");
        assert!(errors[2].starts_with("job x: cron = \"every tuesday\" is not a cron pattern"));
        assert_eq!(errors[3], "job x: at = \"25:00\" is not a time like \"09:30\"");
        assert_eq!(errors[4], "job x: via = \"claude\" needs a cwd or session target");
        assert_eq!(errors[5], "job x: args are only for via = \"claude\"");
        assert!(errors[6].starts_with("host = \"screen\" is not one of"));
    }

    #[test]
    fn rejects_an_interval_under_a_minute() {
        let text = "[[job]]\nname = \"x\"\nevery = \"10s\"\ntarget = { agent = \"a\" }\nmessage = \"m\"\n";
        assert_eq!(parse(text).unwrap_err(), "job x: every must be at least 1m");
    }

    #[test]
    fn rejects_an_interval_that_is_not_a_duration() {
        let text = "[[job]]\nname = \"x\"\nevery = \"often\"\ntarget = { agent = \"a\" }\nmessage = \"m\"\n";
        assert!(parse(text).unwrap_err().starts_with("job x: every = \"often\" is not a duration"));
    }

    #[test]
    fn rejects_an_empty_message() {
        let text = "[[job]]\nname = \"x\"\nevery = \"5m\"\ntarget = { agent = \"a\" }\nmessage = \"  \"\n";
        assert_eq!(parse(text).unwrap_err(), "job x: message is empty");
    }

    #[test]
    fn rejects_two_jobs_with_one_name() {
        let one = "[[job]]\nname = \"x\"\nevery = \"5m\"\ntarget = { agent = \"a\" }\nmessage = \"m\"\n";
        assert_eq!(parse(&format!("{one}{one}")).unwrap_err(), "two jobs or watches are named x");
    }

    #[test]
    fn rejects_an_unknown_target_kind_and_unknown_keys() {
        let target = "[[job]]\nname = \"x\"\nevery = \"5m\"\ntarget = { pane = \"w1:p1\" }\nmessage = \"m\"\n";
        assert!(parse(target).is_err());
        let key = "[[job]]\nname = \"x\"\nevery = \"5m\"\ntarget = { agent = \"a\" }\nmessage = \"m\"\nevry = \"1m\"\n";
        assert!(parse(key).is_err());
    }

    #[test]
    fn reads_a_watch_with_its_limits_durations_and_investigator() {
        let text = r#"
[[watch]]
name = "overload"
cpu = 85
memory = 90
load = 12
for = "3m"
investigator = { cwd = "/Users/me/code/ops" }
message = "Someone is gaming tonight."
"#;
        assert_eq!(
            parse(text).unwrap().watches,
            vec![Watch {
                name: "overload".into(),
                cpu: Some(85.0),
                memory: Some(90.0),
                load: Some(12.0),
                over_for: Duration::from_secs(180),
                clear_for: Duration::from_secs(300),
                investigator: Some(Target::Cwd("/Users/me/code/ops".into())),
                broadcast: true,
                message: "Someone is gaming tonight.".into(),
            }]
        );
    }

    #[test]
    fn rejects_a_watch_with_no_limit_a_limit_out_of_range_or_nobody_to_tell() {
        let errors = [
            "[[watch]]\nname = \"w\"\n",
            "[[watch]]\nname = \"w\"\ncpu = 150\n",
            "[[watch]]\nname = \"w\"\ncpu = 80\nbroadcast = false\n",
            "[[watch]]\nname = \"w\"\ncpu = 80\nfor = \"soon\"\n",
            "[[watch]]\nname = \"w\"\nload = 0\n",
        ]
        .map(|text| parse(text).unwrap_err());
        assert_eq!(errors[0], "watch w: give a cpu, memory or load limit");
        assert_eq!(errors[1], "watch w: cpu must be a percent from 1 to 100");
        assert_eq!(errors[2], "watch w: with broadcast = false it needs an investigator");
        assert!(errors[3].starts_with("watch w: for = \"soon\" is not a duration"));
        assert_eq!(errors[4], "watch w: load must be above 0");
    }

    #[test]
    fn a_job_and_a_watch_cannot_share_a_name() {
        let text = "[[job]]\nname = \"x\"\nevery = \"5m\"\ntarget = { agent = \"a\" }\nmessage = \"m\"\n[[watch]]\nname = \"x\"\ncpu = 80\n";
        assert_eq!(parse(text).unwrap_err(), "two jobs or watches are named x");
    }

    #[test]
    fn parking_is_off_until_a_park_table_turns_it_on_and_never_parks_skipped_agents_or_investigators() {
        let text = r#"
[park]
after = "45m"
skip = [{ agent = "deploys" }, { cwd = "/code/homelab" }]

[[watch]]
name = "overload"
cpu = 85
investigator = { cwd = "/code/homelab" }
"#;
        let config = parse(text).unwrap();

        assert_eq!(parse("").unwrap().park, None);
        assert_eq!(parse("[park]\n").unwrap().park, Some(Park { after: Duration::from_secs(30 * 60), skip: vec![] }));
        assert_eq!(config.park.as_ref().map(|park| park.after), Some(Duration::from_secs(45 * 60)));
        assert_eq!(config.never_parked(), [Target::Agent("deploys".into()), Target::Cwd("/code/homelab".into())]);
        assert_eq!(parse("[park]\nafter = \"10s\"\n").unwrap_err(), "park: after must be at least 1m");
        assert!(
            parse("[park]\nafter = \"later\"\n").unwrap_err().starts_with("park: after = \"later\" is not a duration")
        );
        assert!(parse("[park]\nwindow = \"30m\"\n").is_err());
    }

    #[test]
    fn keeps_herdr_processes_in_the_background_unless_turned_off() {
        assert!(parse("").unwrap().background);
        assert!(parse("[background]\n").unwrap().background);
        assert!(!parse("[background]\nenabled = false\n").unwrap().background);
        assert!(parse("[background]\non = false\n").is_err());
    }

    #[test]
    fn freezes_heavy_work_by_default_with_this_machines_core_count_and_takes_overrides() {
        let text = "[freeze]\nload = 6\nmemory_free = 15\nactive_within = \"90s\"\nalso = [\"ffmpeg\"]\n";
        let tuned = parse(text).unwrap().freeze;

        assert_eq!(parse("").unwrap().freeze, Freeze::default());
        assert_eq!(Freeze::default().load, cores());
        assert_eq!((tuned.load, tuned.memory_free, tuned.active_within), (6.0, 15.0, Duration::from_secs(90)));
        assert_eq!(tuned.heavy.last().map(String::as_str), Some("ffmpeg"));
        assert!(tuned.heavy.contains(&"vitest".to_string()));
        assert_eq!(parse("[freeze]\nheavy = [\"burner\"]\n").unwrap().freeze.heavy, ["burner"]);
        assert!(!parse("[freeze]\nenabled = false\n").unwrap().freeze.enabled);
        assert_eq!(parse("[freeze]\nheavy = []\n").unwrap_err(), "freeze: heavy names nothing to freeze");
        assert_eq!(
            parse("[freeze]\nmemory_free = 0\n").unwrap_err(),
            "freeze: memory_free must be a percent from 1 to 100"
        );
        assert!(parse("[freeze]\nthaw_after = \"soon\"\n").unwrap_err().starts_with("freeze: thaw_after"));
    }

    #[test]
    fn load_names_the_file_it_could_not_read() {
        let error = load(Path::new("/nonexistent/nudge.toml")).unwrap_err();
        assert!(error.starts_with("could not read /nonexistent/nudge.toml"));
    }
}
