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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub jobs: Vec<Job>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJob {
    name: String,
    every: String,
    target: Target,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    job: Vec<RawJob>,
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

fn unique(jobs: &[Job]) -> Result<(), String> {
    let mut names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
    names.sort_unstable();
    match names.windows(2).find(|pair| pair[0] == pair[1]) {
        Some(pair) => Err(format!("two jobs are named {}", pair[0])),
        None => Ok(()),
    }
}

pub fn parse(text: &str) -> Result<Config, String> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
    let jobs = raw.job.into_iter().map(job).collect::<Result<Vec<_>, _>>()?;
    unique(&jobs)?;
    Ok(Config { jobs })
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
    fn load_names_the_file_it_could_not_read() {
        let error = load(Path::new("/nonexistent/nudge.toml")).unwrap_err();
        assert!(error.starts_with("could not read /nonexistent/nudge.toml"));
    }
}
