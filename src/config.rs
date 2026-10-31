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
pub struct Job {
    pub name: String,
    pub every: Duration,
    pub target: Target,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Watch {
    pub name: String,
    pub cpu: Option<f32>,
    pub memory: Option<f32>,
    pub over_for: Duration,
    pub clear_for: Duration,
    pub investigator: Option<Target>,
    pub broadcast: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub jobs: Vec<Job>,
    pub watches: Vec<Watch>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJob {
    name: String,
    every: String,
    target: Target,
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
struct RawConfig {
    #[serde(default)]
    job: Vec<RawJob>,
    #[serde(default)]
    watch: Vec<RawWatch>,
}

const SHORTEST: Duration = Duration::from_secs(60);

fn job(raw: RawJob) -> Result<Job, String> {
    let every = humantime::parse_duration(&raw.every)
        .map_err(|e| format!("job {}: every = {:?} is not a duration ({e})", raw.name, raw.every))?;
    if every < SHORTEST {
        return Err(format!("job {}: every must be at least 1m", raw.name));
    }
    if raw.message.trim().is_empty() {
        return Err(format!("job {}: message is empty", raw.name));
    }
    Ok(Job { name: raw.name, every, target: raw.target, message: raw.message.trim().to_string() })
}

fn duration(name: &str, key: &str, text: &str) -> Result<Duration, String> {
    humantime::parse_duration(text).map_err(|e| format!("watch {name}: {key} = {text:?} is not a duration ({e})"))
}

fn percent(name: &str, key: &str, value: Option<f32>) -> Result<Option<f32>, String> {
    match value {
        Some(limit) if !(1.0..=100.0).contains(&limit) => {
            Err(format!("watch {name}: {key} must be a percent from 1 to 100"))
        }
        other => Ok(other),
    }
}

fn watch(raw: RawWatch) -> Result<Watch, String> {
    let cpu = percent(&raw.name, "cpu", raw.cpu)?;
    let memory = percent(&raw.name, "memory", raw.memory)?;
    if cpu.is_none() && memory.is_none() {
        return Err(format!("watch {}: give a cpu or memory limit", raw.name));
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
        investigator: raw.investigator,
        broadcast: raw.broadcast,
        message: raw.message.trim().to_string(),
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
    let jobs = raw.job.into_iter().map(job).collect::<Result<Vec<_>, _>>()?;
    let watches = raw.watch.into_iter().map(watch).collect::<Result<Vec<_>, _>>()?;
    unique(jobs.iter().map(|j| j.name.as_str()).chain(watches.iter().map(|w| w.name.as_str())))?;
    Ok(Config { jobs, watches })
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
target = { cwd = "/Users/sth/code/homelab" }
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
                    every: Duration::from_secs(30 * 60),
                    target: Target::Cwd("/Users/sth/code/homelab".into()),
                    message: "Check that encoding is running.".into(),
                },
                Job {
                    name: "by-name".into(),
                    every: Duration::from_secs(90 * 60),
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
for = "3m"
investigator = { cwd = "/Users/sth/code/homelab" }
message = "Shane is gaming tonight."
"#;
        assert_eq!(
            parse(text).unwrap().watches,
            vec![Watch {
                name: "overload".into(),
                cpu: Some(85.0),
                memory: Some(90.0),
                over_for: Duration::from_secs(180),
                clear_for: Duration::from_secs(300),
                investigator: Some(Target::Cwd("/Users/sth/code/homelab".into())),
                broadcast: true,
                message: "Shane is gaming tonight.".into(),
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
        ]
        .map(|text| parse(text).unwrap_err());
        assert_eq!(errors[0], "watch w: give a cpu or memory limit");
        assert_eq!(errors[1], "watch w: cpu must be a percent from 1 to 100");
        assert_eq!(errors[2], "watch w: with broadcast = false it needs an investigator");
        assert!(errors[3].starts_with("watch w: for = \"soon\" is not a duration"));
    }

    #[test]
    fn a_job_and_a_watch_cannot_share_a_name() {
        let text = "[[job]]\nname = \"x\"\nevery = \"5m\"\ntarget = { agent = \"a\" }\nmessage = \"m\"\n[[watch]]\nname = \"x\"\ncpu = 80\n";
        assert_eq!(parse(text).unwrap_err(), "two jobs or watches are named x");
    }

    #[test]
    fn load_names_the_file_it_could_not_read() {
        let error = load(Path::new("/nonexistent/nudge.toml")).unwrap_err();
        assert!(error.starts_with("could not read /nonexistent/nudge.toml"));
    }
}
