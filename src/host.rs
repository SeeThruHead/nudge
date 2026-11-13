use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
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

pub trait Host {
    fn name(&self) -> &str {
        "host"
    }
    fn agents(&self) -> Result<Vec<Agent>, String>;
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String>;
}

const SEARCHED: [&str; 3] = [".local/bin", "/opt/homebrew/bin", "/usr/local/bin"];

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

pub fn find(name: &str, variable: &str) -> PathBuf {
    if let Some(chosen) = std::env::var_os(variable) {
        return PathBuf::from(chosen);
    }
    let from_path =
        std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect::<Vec<_>>()).unwrap_or_default();
    let usual = SEARCHED.iter().map(|dir| if dir.starts_with('/') { PathBuf::from(dir) } else { home().join(dir) });
    from_path
        .into_iter()
        .chain(usual)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| name.into())
}

pub struct Runner {
    pub binary: PathBuf,
    pub timeout: Duration,
}

fn label(binary: &Path, args: &[&str]) -> String {
    let name = binary.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    std::iter::once(name).chain(args.iter().take(2).map(|arg| arg.to_string())).collect::<Vec<_>>().join(" ")
}

const TEXT_FILE_BUSY: i32 = 26;
const SPAWN_TRIES: u32 = 20;

impl Runner {
    fn spawn(&self, args: &[&str]) -> Result<std::process::Child, String> {
        let attempt = || {
            Command::new(&self.binary)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        };
        let busy = |result: &std::io::Result<std::process::Child>| {
            result.as_ref().err().is_some_and(|e| e.raw_os_error() == Some(TEXT_FILE_BUSY))
        };
        let settled = (1..SPAWN_TRIES).fold(attempt(), |last, _| {
            if !busy(&last) {
                return last;
            }
            std::thread::sleep(Duration::from_millis(10));
            attempt()
        });
        settled.map_err(|e| format!("could not start {}: {e}", self.binary.display()))
    }

    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let what = label(&self.binary, args);
        let mut child = self.spawn(args)?;
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                break status;
            }
            if started.elapsed() > self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{what} took longer than {:?}", self.timeout));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut out = String::new();
        let mut err = String::new();
        child.stdout.take().map(|mut s| s.read_to_string(&mut out));
        child.stderr.take().map(|mut s| s.read_to_string(&mut err));
        if status.success() { Ok(out) } else { Err(format!("{what} failed: {}{}", err.trim(), out.trim())) }
    }

    pub fn run_any(&self, args: &[&str]) -> Result<String, String> {
        let what = label(&self.binary, args);
        let mut child = self.spawn(args)?;
        let started = Instant::now();
        loop {
            if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                break;
            }
            if started.elapsed() > self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{what} took longer than {:?}", self.timeout));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut out = String::new();
        child.stdout.take().map(|mut s| s.read_to_string(&mut out));
        Ok(out)
    }
}

const SHELLS: [&str; 11] = ["sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "nu", "pwsh", "powershell", "xonsh"];

fn runs_an_agent(program: Option<&str>) -> bool {
    program.is_some_and(|program| !program.is_empty() && !SHELLS.contains(&program))
}

#[derive(Deserialize)]
struct HerdrPane {
    #[serde(default)]
    agent: Option<String>,
    #[serde(flatten)]
    pane: Agent,
}

#[derive(Deserialize)]
struct HerdrAgents {
    agents: Vec<HerdrPane>,
}

#[derive(Deserialize)]
struct HerdrListing {
    result: HerdrAgents,
}

pub fn parse_herdr(json: &str) -> Result<Vec<Agent>, String> {
    serde_json::from_str::<HerdrListing>(json)
        .map(|listing| {
            listing
                .result
                .agents
                .into_iter()
                .filter(|pane| runs_an_agent(pane.agent.as_deref()))
                .map(|pane| pane.pane)
                .collect()
        })
        .map_err(|e| format!("herdr agent list returned something unexpected: {e}"))
}

pub struct Herdr(pub Runner);

impl Host for Herdr {
    fn name(&self) -> &str {
        "herdr"
    }
    fn agents(&self) -> Result<Vec<Agent>, String> {
        parse_herdr(&self.0.run(&["agent", "list"])?)
    }
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
        self.0.run(&["agent", "prompt", pane, text]).map(|_| ())
    }
}

#[derive(Deserialize)]
struct CoreTerminal {
    address: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct CoreListing {
    terminals: Vec<CoreTerminal>,
}

pub fn parse_core(json: &str) -> Result<Vec<Agent>, String> {
    let listing: CoreListing =
        serde_json::from_str(json).map_err(|e| format!("hc ls returned something unexpected: {e}"))?;
    Ok(listing
        .terminals
        .into_iter()
        .filter(|terminal| runs_an_agent(terminal.agent.as_deref()) && terminal.state.as_deref() == Some("live"))
        .map(|terminal| Agent {
            pane_id: terminal.address,
            agent_status: terminal.status.unwrap_or_else(|| "unknown".into()),
            name: terminal.name,
            cwd: terminal.cwd,
            agent_session: None,
        })
        .collect())
}

#[derive(Deserialize)]
struct CoreForeground {
    pid: u32,
}

#[derive(Deserialize)]
struct CoreDetail {
    #[serde(default)]
    foreground: Option<CoreForeground>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSessionFile {
    session_id: String,
}

pub fn foreground_pid(json: &str) -> Option<u32> {
    serde_json::from_str::<CoreDetail>(json).ok()?.foreground.map(|foreground| foreground.pid)
}

fn claude_session(pid: u32) -> Option<Session> {
    let text = std::fs::read_to_string(home().join(".claude/sessions").join(format!("{pid}.json"))).ok()?;
    serde_json::from_str::<ClaudeSessionFile>(&text).ok().map(|file| Session { value: file.session_id })
}

pub struct HerdrApp(pub Runner);

impl HerdrApp {
    fn session_of(&self, address: &str) -> Option<Session> {
        self.0.run(&["get", address, "--json"]).ok().as_deref().and_then(foreground_pid).and_then(claude_session)
    }
}

impl Host for HerdrApp {
    fn name(&self) -> &str {
        "herdr-app"
    }
    fn agents(&self) -> Result<Vec<Agent>, String> {
        let listed = parse_core(&self.0.run(&["ls", "--json"])?)?;
        Ok(listed.into_iter().map(|agent| Agent { agent_session: self.session_of(&agent.pane_id), ..agent }).collect())
    }
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
        self.0.run(&["send", pane, text]).map(|_| ())
    }
}

pub fn is_tui_pane(pane: &str) -> bool {
    pane.contains(':')
}

pub struct Herdrs {
    tui: Box<dyn Host>,
    app: Box<dyn Host>,
}

impl Herdrs {
    pub fn new(tui: Box<dyn Host>, app: Box<dyn Host>) -> Herdrs {
        Herdrs { tui, app }
    }
}

impl Host for Herdrs {
    fn name(&self) -> &str {
        "herdr+herdr-app"
    }
    fn agents(&self) -> Result<Vec<Agent>, String> {
        match (self.tui.agents(), self.app.agents()) {
            (Ok(tui), Ok(app)) => Ok(tui.into_iter().chain(app).collect()),
            (Ok(tui), Err(_)) => Ok(tui),
            (Err(_), Ok(app)) => Ok(app),
            (Err(tui), Err(app)) => Err(format!("herdr: {tui}; herdr-app: {app}")),
        }
    }
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
        if is_tui_pane(pane) {
            return self.tui.prompt(pane, text);
        }
        self.app.prompt(pane, text)
    }
}

const TIMEOUT: Duration = Duration::from_secs(20);

pub fn runner(name: &str, variable: &str) -> Runner {
    Runner { binary: find(name, variable), timeout: TIMEOUT }
}

pub fn named(choice: &str) -> Result<Box<dyn Host>, String> {
    match choice {
        "herdr" => Ok(Box::new(Herdr(runner("herdr", "NUDGE_HERDR")))),
        "herdr-app" => Ok(Box::new(HerdrApp(runner("hc", "NUDGE_HC")))),
        "auto" => Ok(Box::new(Herdrs::new(named("herdr")?, named("herdr-app")?))),
        other => Err(format!("host = {other:?} is not one of auto, herdr, herdr-app")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reviewer_target(agents: &[Agent]) -> Option<String> {
        match crate::schedule::pick(&crate::config::Target::Agent("reviewer".into()), agents) {
            crate::schedule::Pick::Ready(agent) => Some(agent.pane_id.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_shell_split_beside_an_agent_in_the_tui_is_never_the_agent_its_name_finds() {
        let json = r#"{"id":"cli:agent:list","result":{"type":"agent_list","agents":[
            {"agent_status":"unknown","cwd":"/code/ops","pane_id":"w1:p2","tab_id":"w1:t1","name":"reviewer"},
            {"agent":"zsh","agent_status":"unknown","cwd":"/code/ops","pane_id":"w1:p3","tab_id":"w1:t1","name":"reviewer"},
            {"agent":"claude","agent_status":"idle","cwd":"/code/ops","pane_id":"w1:p1","tab_id":"w1:t1","name":"reviewer"}]}}"#;
        let agents = parse_herdr(json).unwrap();

        assert_eq!(agents.iter().map(|agent| agent.pane_id.as_str()).collect::<Vec<_>>(), vec!["w1:p1"]);
        assert_eq!(reviewer_target(&agents), Some("w1:p1".into()));
    }

    #[test]
    fn a_shell_split_beside_an_agent_in_the_app_is_never_the_agent_its_name_finds() {
        let json = r#"{"hidden_finished":0,"terminals":[
            {"address":"t9","agent":"zsh","cwd":"/code/ops","state":"live","status":null,"name":"reviewer","where":"ops / 1"},
            {"address":"t8","agent":"claude-code","cwd":"/code/ops","state":"live","status":"idle","name":"reviewer","where":"ops / 1"},
            {"address":"t10","cwd":"/code/ops","state":"live","status":null,"name":"reviewer","where":"ops / 1"}]}"#;
        let agents = parse_core(json).unwrap();

        assert_eq!(agents.iter().map(|agent| agent.pane_id.as_str()).collect::<Vec<_>>(), vec!["t8"]);
        assert_eq!(reviewer_target(&agents), Some("t8".into()));
    }

    #[test]
    fn reads_the_agent_list_herdr_prints() {
        let json = r#"{"id":"cli:agent:list","result":{"type":"agent_list","agents":[
            {"agent":"claude","agent_status":"working","cwd":"/Users/me/code/ops","pane_id":"w1:pH",
             "agent_session":{"agent":"claude","kind":"id","value":"5536"},"revision":8},
            {"agent":"claude","agent_status":"idle","pane_id":"w7:p1","name":"amazon-export"}]}}"#;
        assert_eq!(
            parse_herdr(json).unwrap(),
            vec![
                Agent {
                    pane_id: "w1:pH".into(),
                    agent_status: "working".into(),
                    name: None,
                    cwd: Some("/Users/me/code/ops".into()),
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
        assert!(parse_herdr("{}").unwrap_err().starts_with("herdr agent list returned something unexpected"));
    }

    #[test]
    fn reads_the_live_agent_terminals_herdr_core_lists() {
        let json = r#"{"hidden_finished":0,"terminals":[
            {"address":"t5","agent":"claude-code","cwd":"/code/app","state":"live","status":"idle","turn":0,"where":"app / 1"},
            {"address":"t6","cwd":"/code/app","state":"live","status":null,"turn":0},
            {"address":"t7","agent":"codex","name":"reviewer","cwd":"/code/site","state":"live","status":"working"},
            {"address":"t8","agent":"claude-code","cwd":"/code/old","state":"finished","status":"idle"}]}"#;

        assert_eq!(
            parse_core(json).unwrap(),
            vec![
                Agent {
                    pane_id: "t5".into(),
                    agent_status: "idle".into(),
                    name: None,
                    cwd: Some("/code/app".into()),
                    agent_session: None,
                },
                Agent {
                    pane_id: "t7".into(),
                    agent_status: "working".into(),
                    name: Some("reviewer".into()),
                    cwd: Some("/code/site".into()),
                    agent_session: None,
                },
            ]
        );
        assert!(parse_core("{}").unwrap_err().starts_with("hc ls returned something unexpected"));
        assert_eq!(foreground_pid(r#"{"address":"t5","foreground":{"name":"claude","pid":4242}}"#), Some(4242));
        assert_eq!(foreground_pid(r#"{"address":"t5"}"#), None);
    }

    #[test]
    fn herdr_app_sends_through_hc() {
        let ok =
            HerdrApp(script("hc", r#"[ "$1 $2 $3" = "send t5 check encoding" ] || { echo "got $*" >&2; exit 1; }"#));
        assert_eq!(ok.prompt("t5", "check encoding"), Ok(()));
    }

    #[test]
    fn both_herdrs_list_together_and_each_pane_goes_to_its_own() {
        let tui = Herdr(script(
            "herdr",
            r#"[ "$1" = agent ] && [ "$2" = list ] && { echo '{"result":{"agents":[{"agent":"claude","pane_id":"w1:p1","agent_status":"idle"}]}}'; exit 0; }
               [ "$3" = "w1:p1" ] || { echo "tui got $*" >&2; exit 1; }"#,
        ));
        let app = HerdrApp(script(
            "hc",
            r#"[ "$1" = ls ] && { echo '{"terminals":[{"address":"t5","agent":"claude-code","state":"live","status":"idle"}]}'; exit 0; }
               [ "$1" = get ] && exit 1
               [ "$2" = t5 ] || { echo "app got $*" >&2; exit 1; }"#,
        ));
        let both = Herdrs::new(Box::new(tui), Box::new(app));
        let only_app = Herdrs::new(Box::new(Down), Box::new(HerdrApp(script("hc", r#"echo '{"terminals":[]}'"#))));

        assert_eq!(both.agents().unwrap().iter().map(|a| a.pane_id.as_str()).collect::<Vec<_>>(), ["w1:p1", "t5"]);
        assert_eq!(both.prompt("w1:p1", "x"), Ok(()));
        assert_eq!(both.prompt("t5", "x"), Ok(()));
        assert_eq!(only_app.agents(), Ok(vec![]));
        assert!(Herdrs::new(Box::new(Down), Box::new(Down)).agents().unwrap_err().contains("herdr-app: not running"));
    }

    fn script(name: &str, body: &str) -> Runner {
        let dir = std::env::temp_dir().join(format!("nudge-test-{}-{name}-{}", std::process::id(), body.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        Runner { binary: path, timeout: Duration::from_secs(5) }
    }

    #[test]
    fn passes_the_pane_and_text_to_herdr_agent_prompt_and_reports_its_errors() {
        let ok = Herdr(script(
            "herdr",
            r#"[ "$1 $2 $3 $4" = "agent prompt w1:pH check encoding" ] || { echo "got $*" >&2; exit 1; }"#,
        ));
        let refusing = Herdr(script("herdr", r#"echo "agent_blocked" >&2; exit 2"#));

        assert_eq!(ok.prompt("w1:pH", "check encoding"), Ok(()));
        assert_eq!(refusing.prompt("w1:pH", "x"), Err("herdr agent prompt failed: agent_blocked".into()));
    }

    #[test]
    fn gives_up_on_a_host_that_hangs() {
        let hanging = Herdr(Runner { timeout: Duration::from_millis(200), ..script("herdr", "sleep 5") });
        assert!(hanging.agents().unwrap_err().starts_with("herdr agent list took longer than"));
    }

    struct Down;
    impl Host for Down {
        fn name(&self) -> &str {
            "down"
        }
        fn agents(&self) -> Result<Vec<Agent>, String> {
            Err("not running".into())
        }
        fn prompt(&self, _: &str, _: &str) -> Result<(), String> {
            Err("not running".into())
        }
    }

    #[test]
    fn only_the_herdr_tui_and_app_are_hosts() {
        assert_eq!(named("auto").map(|host| host.name().to_string()), Ok("herdr+herdr-app".into()));
        assert!(named("tmux").err().is_some_and(|error| error.ends_with("is not one of auto, herdr, herdr-app")));
    }
}
