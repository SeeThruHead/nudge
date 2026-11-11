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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    pub schedule: Schedule,
    pub target: Target,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub host: String,
    pub jobs: Vec<Job>,
}

impl Default for Config {
    fn default() -> Config {
        Config { host: "auto".into(), jobs: vec![] }
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
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "auto")]
    host: String,
    #[serde(default)]
    job: Vec<RawJob>,
    #[serde(rename = "watch")]
    _retired_watch: Option<toml::Value>,
    #[serde(rename = "park")]
    _retired_park: Option<toml::Value>,
    #[serde(rename = "freeze")]
    _retired_freeze: Option<toml::Value>,
    #[serde(rename = "background")]
    _retired_background: Option<toml::Value>,
}

fn auto() -> String {
    "auto".into()
}

const HOSTS: [&str; 3] = ["auto", "herdr", "herdr-app"];

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
    Ok(Job { name: raw.name, schedule, target: raw.target, message: raw.message.trim().to_string() })
}

fn unique<'a>(names: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let mut names: Vec<&str> = names.collect();
    names.sort_unstable();
    match names.windows(2).find(|pair| pair[0] == pair[1]) {
        Some(pair) => Err(format!("two jobs are named {}", pair[0])),
        None => Ok(()),
    }
}

pub fn parse(text: &str) -> Result<Config, String> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
    if !HOSTS.contains(&raw.host.as_str()) {
        return Err(format!("host = {:?} is not one of {}", raw.host, HOSTS.join(", ")));
    }
    let jobs = raw.job.into_iter().map(job).collect::<Result<Vec<_>, _>>()?;
    unique(jobs.iter().map(|j| j.name.as_str()))?;
    Ok(Config { host: raw.host, jobs })
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
                    message: "Check that encoding is running.".into(),
                },
                Job {
                    name: "by-name".into(),
                    schedule: Schedule::Every(Duration::from_secs(90 * 60)),
                    target: Target::Agent("homelab".into()),
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
    fn reads_cron_and_daily_schedules_and_the_host() {
        let text = r#"
host = "herdr-app"

[[job]]
name = "standup"
cron = "0 9 * * 1-5"
target = { cwd = "/code/app" }
message = "Summarise yesterday."

[[job]]
name = "twice"
at = ["09:00", "17:30"]
target = { session = "abc" }
message = "Check the build."
"#;
        let config = parse(text).unwrap();

        assert_eq!(config.host, "herdr-app");
        assert_eq!(config.jobs[0].schedule, Schedule::Cron("0 9 * * 1-5".into()));
        assert_eq!(config.jobs[1].schedule, Schedule::At(vec![(9, 0), (17, 30)]));
        assert_eq!(config.jobs[1].schedule.to_string(), "at 09:00, 17:30");
        assert_eq!(parse("").unwrap().host, "auto");
    }

    #[test]
    fn rejects_schedules_and_hosts_that_cannot_work() {
        let job =
            |extra: &str| format!("[[job]]\nname = \"x\"\ntarget = {{ agent = \"a\" }}\nmessage = \"m\"\n{extra}");
        let errors = [
            job(""),
            job("every = \"5m\"\ncron = \"0 9 * * *\"\n"),
            job("cron = \"every tuesday\"\n"),
            job("at = \"25:00\"\n"),
            "host = \"tmux\"\n".to_string(),
        ]
        .map(|text| parse(&text).unwrap_err());

        assert_eq!(errors[0], "job x: give exactly one of every, cron or at");
        assert_eq!(errors[1], "job x: give exactly one of every, cron or at");
        assert!(errors[2].starts_with("job x: cron = \"every tuesday\" is not a cron pattern"));
        assert_eq!(errors[3], "job x: at = \"25:00\" is not a time like \"09:30\"");
        assert_eq!(errors[4], "host = \"tmux\" is not one of auto, herdr, herdr-app");
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
        assert_eq!(parse(&format!("{one}{one}")).unwrap_err(), "two jobs are named x");
    }

    #[test]
    fn rejects_an_unknown_target_kind_and_unknown_keys() {
        let target = "[[job]]\nname = \"x\"\nevery = \"5m\"\ntarget = { pane = \"w1:p1\" }\nmessage = \"m\"\n";
        assert!(parse(target).is_err());
        let key = "[[job]]\nname = \"x\"\nevery = \"5m\"\ntarget = { agent = \"a\" }\nmessage = \"m\"\nevry = \"1m\"\n";
        assert!(parse(key).is_err());
    }

    #[test]
    fn still_reads_a_config_with_the_retired_watch_park_freeze_and_background_tables_and_ignores_them() {
        let job = "[[job]]\nname = \"x\"\nevery = \"5m\"\ntarget = { agent = \"a\" }\nmessage = \"m\"\n";
        let retired = r#"
[[watch]]
name = "overload"
cpu = 85
investigator = { cwd = "/code/homelab" }

[park]
after = "45m"
skip = [{ agent = "deploys" }]

[freeze]
enabled = false
also = ["ffmpeg"]

[background]
enabled = false
"#;
        assert_eq!(parse(&format!("{job}{retired}")).unwrap(), parse(job).unwrap());
        assert_eq!(parse(retired).unwrap(), Config::default());
    }

    #[test]
    fn load_names_the_file_it_could_not_read() {
        let error = load(Path::new("/nonexistent/nudge.toml")).unwrap_err();
        assert!(error.starts_with("could not read /nonexistent/nudge.toml"));
    }
}
