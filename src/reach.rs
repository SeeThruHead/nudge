use crate::host::Runner;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Found {
    pub address: String,
    pub state: String,
    pub status: String,
    pub host: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Refusal {
    #[serde(rename = "_tag")]
    pub tag: String,
    pub message: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Answer {
    Agent { agent: Found },
    Error { error: Refusal },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Agent(Found),
    Refused(Refusal),
    Unavailable(String),
}

pub fn parse(output: &str) -> Resolved {
    match serde_json::from_str::<Answer>(output.trim()) {
        Ok(Answer::Agent { agent }) => Resolved::Agent(agent),
        Ok(Answer::Error { error }) => Resolved::Refused(error),
        Err(e) => Resolved::Unavailable(format!("reach answered something unexpected: {e}")),
    }
}

pub trait Directory {
    fn resolve(&self, address: &str) -> Resolved;
    fn send(&self, address: &str, text: &str) -> Result<(), String>;
}

pub struct Reach(pub Runner);

const MISSING: &str = "address targets need reach (brew install seethruhead/tap/reach)";

impl Directory for Reach {
    fn resolve(&self, address: &str) -> Resolved {
        if !self.0.binary.is_file() {
            return Resolved::Unavailable(MISSING.into());
        }
        match self.0.run_any(&["resolve", address, "--json"]) {
            Ok(output) => parse(&output),
            Err(error) => Resolved::Unavailable(error),
        }
    }

    fn send(&self, address: &str, text: &str) -> Result<(), String> {
        self.0.run(&["send", address, text]).map(|_| ())
    }
}

pub fn is_ready(found: &Found) -> bool {
    found.state == "asleep" || matches!(found.status.as_str(), "idle" | "unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_agent_reach_found() {
        let output = r#"{"agent":{"address":"sthdev/homelab/homelab","machine":"sthdev","space":"homelab","name":"homelab","pinned":true,"state":"live","status":"idle","host":"herdr-app","id":"t4k","cwd":"/code/homelab"}}"#;
        let Resolved::Agent(found) = parse(output) else { panic!("expected an agent") };
        assert_eq!((found.id.as_str(), found.state.as_str(), is_ready(&found)), ("t4k", "live", true));
    }

    #[test]
    fn reads_why_reach_refused() {
        let output = r#"{"error":{"_tag":"Ambiguous","address":"sthdev/d/d","candidates":["herdr-app:t1 (/w)"],"message":"sthdev/d/d matches 2 agents"}}"#;
        assert_eq!(
            parse(output),
            Resolved::Refused(Refusal { tag: "Ambiguous".into(), message: "sthdev/d/d matches 2 agents".into() })
        );
    }

    #[test]
    fn anything_else_means_reach_is_unavailable() {
        assert!(matches!(parse("Usage: reach"), Resolved::Unavailable(_)));
    }

    #[test]
    fn an_asleep_agent_is_ready_because_a_message_wakes_it_and_a_working_one_is_not() {
        let found = |state: &str, status: &str| Found {
            address: "a".into(),
            state: state.into(),
            status: status.into(),
            host: "herdr-app".into(),
            id: "t1".into(),
        };
        assert!(is_ready(&found("asleep", "unknown")));
        assert!(!is_ready(&found("live", "working")));
        assert!(!is_ready(&found("live", "blocked")));
    }
}
