use serde::Deserialize;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Session {
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Agent {
    pub pane_id: String,
    pub agent_status: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub agent_session: Option<Session>,
}

#[derive(Deserialize)]
struct Agents {
    agents: Vec<Agent>,
}

#[derive(Deserialize)]
struct Listing {
    result: Agents,
}

pub fn parse_agents(json: &str) -> Result<Vec<Agent>, String> {
    serde_json::from_str::<Listing>(json)
        .map(|listing| listing.result.agents)
        .map_err(|e| format!("herdr agent list returned something unexpected: {e}"))
}

pub trait Herdr {
    fn agents(&self) -> Result<Vec<Agent>, String>;
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String>;
}

pub struct Cli {
    pub binary: PathBuf,
    pub timeout: Duration,
}

impl Cli {
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let mut child = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", self.binary.display()))?;
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                break status;
            }
            if started.elapsed() > self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("herdr {} took longer than {:?}", args[..2].join(" "), self.timeout));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut out = String::new();
        let mut err = String::new();
        child.stdout.take().map(|mut s| s.read_to_string(&mut out));
        child.stderr.take().map(|mut s| s.read_to_string(&mut err));
        if status.success() {
            Ok(out)
        } else {
            Err(format!("herdr {} failed: {}{}", args[..2].join(" "), err.trim(), out.trim()))
        }
    }
}

impl Herdr for Cli {
    fn agents(&self) -> Result<Vec<Agent>, String> {
        parse_agents(&self.run(&["agent", "list"])?)
    }

    fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
        self.run(&["agent", "prompt", pane, text]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_agent_list_herdr_prints() {
        let json = r#"{"id":"cli:agent:list","result":{"type":"agent_list","agents":[
            {"agent":"claude","agent_status":"working","cwd":"/Users/sth/code/homelab","pane_id":"w1:pH",
             "agent_session":{"agent":"claude","kind":"id","value":"5536"},"revision":8},
            {"agent":"claude","agent_status":"idle","pane_id":"w7:p1","name":"amazon-export"}]}}"#;
        assert_eq!(
            parse_agents(json).unwrap(),
            vec![
                Agent {
                    pane_id: "w1:pH".into(),
                    agent_status: "working".into(),
                    name: None,
                    cwd: Some("/Users/sth/code/homelab".into()),
                    agent_session: Some(Session { value: "5536".into() }),
                },
                Agent {
                    pane_id: "w7:p1".into(),
                    agent_status: "idle".into(),
                    name: Some("amazon-export".into()),
                    cwd: None,
                    agent_session: None,
                },
            ]
        );
    }

    #[test]
    fn says_so_when_the_list_is_not_what_herdr_prints() {
        assert!(parse_agents("{}").unwrap_err().starts_with("herdr agent list returned something unexpected"));
    }

    fn cli(script: &str, timeout: Duration) -> Cli {
        let dir = std::env::temp_dir().join(format!("nudge-test-{}-{}", std::process::id(), script.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("herdr");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        Cli { binary: path, timeout }
    }

    #[test]
    fn passes_the_pane_and_text_to_herdr_agent_prompt() {
        let herdr = cli(r#"[ "$1 $2 $3 $4" = "agent prompt w1:pH check encoding" ] || { echo "got $*" >&2; exit 1; }"#, Duration::from_secs(5));
        assert_eq!(herdr.prompt("w1:pH", "check encoding"), Ok(()));
    }

    #[test]
    fn reports_herdrs_error_when_it_fails() {
        let herdr = cli(r#"echo "agent_blocked" >&2; exit 2"#, Duration::from_secs(5));
        assert_eq!(herdr.prompt("w1:pH", "x"), Err("herdr agent prompt failed: agent_blocked".into()));
    }

    #[test]
    fn gives_up_on_a_herdr_that_hangs() {
        let herdr = cli("sleep 5", Duration::from_millis(200));
        assert!(herdr.agents().unwrap_err().starts_with("herdr agent list took longer than"));
    }
}
