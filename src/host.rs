use serde::Deserialize;
use std::cell::Cell;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    #[serde(default)]
    pub terminal_title: Option<String>,
}

impl Agent {
    pub fn title(&self) -> String {
        let named = self.name.clone().or_else(|| self.terminal_title.clone().map(|title| untitled(&title)));
        let folder =
            || self.cwd.as_deref().and_then(|cwd| cwd.rsplit('/').find(|part| !part.is_empty())).map(str::to_string);
        named.filter(|name| !name.is_empty()).or_else(folder).unwrap_or_else(|| self.pane_id.clone())
    }
}

fn untitled(title: &str) -> String {
    title.trim_start_matches(|c: char| !c.is_alphanumeric()).trim().to_string()
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
    fn spawn(&self, args: &[&str], piped: bool) -> Result<std::process::Child, String> {
        let attempt = || {
            Command::new(&self.binary)
                .args(args)
                .stdin(if piped { Stdio::piped() } else { Stdio::null() })
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

    pub fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
        let what = label(&self.binary, args);
        let mut child = self.spawn(args, stdin.is_some())?;
        if let (Some(text), Some(mut input)) = (stdin, child.stdin.take()) {
            input.write_all(text.as_bytes()).map_err(|e| format!("{what}: could not write its input: {e}"))?;
        }
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
}

#[derive(Deserialize)]
struct HerdrAgents {
    agents: Vec<Agent>,
}

#[derive(Deserialize)]
struct HerdrListing {
    result: HerdrAgents,
}

pub fn parse_herdr(json: &str) -> Result<Vec<Agent>, String> {
    serde_json::from_str::<HerdrListing>(json)
        .map(|listing| listing.result.agents)
        .map_err(|e| format!("herdr agent list returned something unexpected: {e}"))
}

pub struct Herdr(pub Runner);

impl Host for Herdr {
    fn name(&self) -> &str {
        "herdr"
    }
    fn agents(&self) -> Result<Vec<Agent>, String> {
        parse_herdr(&self.0.run(&["agent", "list"], None)?)
    }
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
        self.0.run(&["agent", "prompt", pane, text], None).map(|_| ())
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
        .filter(|terminal| terminal.agent.is_some() && terminal.state.as_deref() == Some("live"))
        .map(|terminal| Agent {
            pane_id: terminal.address,
            agent_status: terminal.status.unwrap_or_else(|| "unknown".into()),
            name: terminal.name,
            cwd: terminal.cwd,
            agent_session: None,
            terminal_title: None,
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
        self.0.run(&["get", address, "--json"], None).ok().as_deref().and_then(foreground_pid).and_then(claude_session)
    }
}

impl Host for HerdrApp {
    fn name(&self) -> &str {
        "herdr-app"
    }
    fn agents(&self) -> Result<Vec<Agent>, String> {
        let listed = parse_core(&self.0.run(&["ls", "--json"], None)?)?;
        Ok(listed.into_iter().map(|agent| Agent { agent_session: self.session_of(&agent.pane_id), ..agent }).collect())
    }
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
        self.0.run(&["send", pane, text], None).map(|_| ())
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

const AGENT_COMMANDS: [&str; 8] = ["claude", "codex", "opencode", "gemini", "pi", "amp", "aider", "cursor-agent"];

fn agent_commands() -> Vec<String> {
    let extra = std::env::var("NUDGE_AGENT_COMMANDS").unwrap_or_default();
    AGENT_COMMANDS
        .iter()
        .map(|name| name.to_string())
        .chain(extra.split(',').map(|name| name.trim().to_string()).filter(|name| !name.is_empty()))
        .collect()
}

pub const TMUX_FORMAT: &str =
    "#{pane_id}\t#{pane_current_path}\t#{pane_current_command}\t#{session_name}:#{window_index}";

pub fn parse_tmux(listing: &str, commands: &[String]) -> Vec<Agent> {
    listing
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                [id, cwd, command, place] if commands.iter().any(|known| known == command) => Some(Agent {
                    pane_id: id.to_string(),
                    agent_status: "unknown".into(),
                    name: Some(place.to_string()),
                    cwd: Some(cwd.to_string()),
                    agent_session: None,
                    terminal_title: None,
                }),
                _ => None,
            }
        })
        .collect()
}

fn buffer_name(pane: &str) -> String {
    format!("nudge-{}", pane.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>())
}

pub struct Tmux(pub Runner);

impl Host for Tmux {
    fn name(&self) -> &str {
        "tmux"
    }
    fn agents(&self) -> Result<Vec<Agent>, String> {
        Ok(parse_tmux(&self.0.run(&["list-panes", "-a", "-F", TMUX_FORMAT], None)?, &agent_commands()))
    }
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
        let buffer = buffer_name(pane);
        self.0.run(&["load-buffer", "-b", &buffer, "-"], Some(text))?;
        self.0.run(&["paste-buffer", "-p", "-d", "-b", &buffer, "-t", pane], None)?;
        self.0.run(&["send-keys", "-t", pane, "Enter"], None).map(|_| ())
    }
}

#[derive(Deserialize)]
struct CmuxWorkspace {
    id: String,
    title: String,
}

#[derive(Deserialize)]
struct CmuxWorkspaces {
    workspaces: Vec<CmuxWorkspace>,
}

const CMUX_AGENT_KEYS: [&str; 4] = ["claude_code", "codex", "opencode", "gemini"];

fn sidebar_value<'a>(sidebar: &'a str, key: &str) -> Option<&'a str> {
    sidebar.lines().map(str::trim).find_map(|line| line.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')))
}

pub fn cmux_agent(id: &str, title: &str, sidebar: &str) -> Option<Agent> {
    let state = CMUX_AGENT_KEYS.iter().find_map(|key| sidebar_value(sidebar, key))?;
    let cwd = sidebar_value(sidebar, "focused_cwd").or_else(|| sidebar_value(sidebar, "cwd"))?;
    let status = match state.split(' ').next().unwrap_or_default() {
        "Running" => "working",
        "Idle" => "idle",
        _ => "blocked",
    };
    Some(Agent {
        pane_id: id.into(),
        agent_status: status.into(),
        name: Some(title.into()),
        cwd: Some(cwd.into()),
        agent_session: None,
        terminal_title: None,
    })
}

pub fn bracketed(text: &str) -> String {
    format!("\u{1b}[200~{text}\u{1b}[201~")
}

pub struct Cmux(pub Runner);

impl Host for Cmux {
    fn name(&self) -> &str {
        "cmux"
    }
    fn agents(&self) -> Result<Vec<Agent>, String> {
        let listing = self.0.run(&["--json", "--id-format", "uuids", "list-workspaces"], None)?;
        let workspaces: CmuxWorkspaces = serde_json::from_str(&listing)
            .map_err(|e| format!("cmux list-workspaces returned something unexpected: {e}"))?;
        workspaces
            .workspaces
            .iter()
            .map(|workspace| {
                let sidebar = self.0.run(&["sidebar-state", "--workspace", &workspace.id], None)?;
                Ok(cmux_agent(&workspace.id, &workspace.title, &sidebar))
            })
            .collect::<Result<Vec<Option<Agent>>, String>>()
            .map(|found| found.into_iter().flatten().collect())
    }
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
        let buffer = buffer_name(pane);
        self.0.run(&["set-buffer", "--name", &buffer, &bracketed(text)], None)?;
        self.0.run(&["paste-buffer", "--name", &buffer, "--workspace", pane], None)?;
        self.0.run(&["send-key", "--workspace", pane, "enter"], None).map(|_| ())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrcaTerminal {
    handle: String,
    worktree_path: String,
    #[serde(default)]
    agent_identity: Option<String>,
    #[serde(default)]
    last_output_at: Option<u64>,
    #[serde(default)]
    pane_title: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default = "yes")]
    connected: bool,
    #[serde(default = "yes")]
    writable: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct OrcaTerminals {
    terminals: Vec<OrcaTerminal>,
}

#[derive(Deserialize)]
struct OrcaListing {
    result: OrcaTerminals,
}

const ORCA_QUIET: Duration = Duration::from_secs(5);

fn epoch_millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map(|since| since.as_millis() as u64).unwrap_or(0)
}

pub fn parse_orca(json: &str, now: SystemTime) -> Result<Vec<Agent>, String> {
    let listing: OrcaListing =
        serde_json::from_str(json).map_err(|e| format!("orca terminal list returned something unexpected: {e}"))?;
    let quiet_since = epoch_millis(now).saturating_sub(ORCA_QUIET.as_millis() as u64);
    Ok(listing
        .result
        .terminals
        .into_iter()
        .filter(|terminal| terminal.agent_identity.is_some() && terminal.connected && terminal.writable)
        .map(|terminal| Agent {
            agent_status: if terminal.last_output_at.is_some_and(|at| at > quiet_since) { "working" } else { "idle" }
                .into(),
            name: terminal.pane_title.or(terminal.title),
            cwd: Some(terminal.worktree_path),
            pane_id: terminal.handle,
            agent_session: None,
            terminal_title: None,
        })
        .collect())
}

pub struct Orca(pub Runner);

impl Host for Orca {
    fn name(&self) -> &str {
        "orca"
    }
    fn agents(&self) -> Result<Vec<Agent>, String> {
        parse_orca(&self.0.run(&["terminal", "list", "--json"], None)?, SystemTime::now())
    }
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
        self.0.run(&["terminal", "send", "--terminal", pane, "--text", text, "--enter", "--json"], None).map(|_| ())
    }
}

pub struct Auto {
    hosts: Vec<Box<dyn Host>>,
    chosen: Cell<Option<usize>>,
}

impl Auto {
    pub fn new(hosts: Vec<Box<dyn Host>>) -> Auto {
        Auto { hosts, chosen: Cell::new(None) }
    }

    fn answering(&self) -> Result<(usize, Vec<Agent>), String> {
        let tried: Vec<Result<(usize, Vec<Agent>), String>> = self
            .hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                host.agents().map(|agents| (index, agents)).map_err(|e| format!("{}: {e}", host.name()))
            })
            .take_while(Result::is_err)
            .collect();
        let failures: Vec<String> = tried.into_iter().filter_map(Result::err).collect();
        match self.hosts.get(failures.len()) {
            Some(host) => host.agents().map(|agents| (failures.len(), agents)),
            None => Err(format!("no host answered ({})", failures.join("; "))),
        }
    }
}

impl Host for Auto {
    fn name(&self) -> &str {
        self.chosen.get().and_then(|index| self.hosts.get(index)).map(|host| host.name()).unwrap_or("auto")
    }
    fn agents(&self) -> Result<Vec<Agent>, String> {
        let (index, agents) = self.answering()?;
        self.chosen.set(Some(index));
        Ok(agents)
    }
    fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
        let index = match self.chosen.get() {
            Some(index) => index,
            None => self.answering()?.0,
        };
        self.hosts[index].prompt(pane, text)
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
        "tmux" => Ok(Box::new(Tmux(runner("tmux", "NUDGE_TMUX")))),
        "cmux" => Ok(Box::new(Cmux(runner("cmux", "NUDGE_CMUX")))),
        "orca" => Ok(Box::new(Orca(runner("orca", "NUDGE_ORCA")))),
        "auto" => Ok(Box::new(Auto::new(
            std::iter::once(Ok(Box::new(Herdrs::new(named("herdr")?, named("herdr-app")?)) as Box<dyn Host>))
                .chain(["cmux", "orca", "tmux"].iter().map(|name| named(name)))
                .collect::<Result<Vec<_>, _>>()?,
        ))),
        other => Err(format!("host = {other:?} is not one of auto, herdr, herdr-app, tmux, cmux, orca")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    terminal_title: None,
                },
                Agent {
                    pane_id: "w7:p1".into(),
                    agent_status: "idle".into(),
                    name: Some("amazon-export".into()),
                    cwd: None,
                    agent_session: None,
                    terminal_title: None,
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
                    terminal_title: None,
                },
                Agent {
                    pane_id: "t7".into(),
                    agent_status: "working".into(),
                    name: Some("reviewer".into()),
                    cwd: Some("/code/site".into()),
                    agent_session: None,
                    terminal_title: None,
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
            r#"[ "$1" = agent ] && [ "$2" = list ] && { echo '{"result":{"agents":[{"pane_id":"w1:p1","agent_status":"idle"}]}}'; exit 0; }
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

    #[test]
    fn tmux_finds_panes_running_a_known_agent() {
        let listing = "%1\t/code/app\tclaude\twork:1\n%2\t/code/app\tzsh\twork:2\n%3\t/code/site\tcodex\tsite:0\n";
        let agents = parse_tmux(listing, &agent_commands());

        assert_eq!(agents.iter().map(|agent| agent.pane_id.as_str()).collect::<Vec<_>>(), ["%1", "%3"]);
        assert_eq!(agents[0].cwd.as_deref(), Some("/code/app"));
        assert_eq!(agents[0].name.as_deref(), Some("work:1"));
        assert_eq!(agents[0].agent_status, "unknown");
    }

    #[test]
    fn cmux_reads_the_agent_state_and_folder_from_the_sidebar() {
        let running = "cwd=/code/app\nclaude_code=Running for 2m\n";
        let waiting = "focused_cwd=/code/site\ncwd=/elsewhere\ncodex=Needs input\n";

        assert_eq!(
            cmux_agent("ws1", "app", running).map(|a| (a.agent_status, a.cwd)),
            Some(("working".into(), Some("/code/app".into())))
        );
        assert_eq!(
            cmux_agent("ws2", "site", waiting).map(|a| (a.agent_status, a.cwd)),
            Some(("blocked".into(), Some("/code/site".into())))
        );
        assert_eq!(cmux_agent("ws3", "shell", "cwd=/code\n"), None);
        assert_eq!(bracketed("hi"), "\u{1b}[200~hi\u{1b}[201~");
    }

    #[test]
    fn orca_lists_agent_terminals_and_calls_one_with_recent_output_working() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let recent = epoch_millis(now) - 1_000;
        let json = format!(
            r#"{{"ok":true,"result":{{"terminals":[
            {{"handle":"t1","worktreePath":"/code/app","agentIdentity":"claude","lastOutputAt":{recent},"title":"api","connected":true,"writable":true}},
            {{"handle":"t2","worktreePath":"/code/app","agentIdentity":"codex","lastOutputAt":1,"paneTitle":"REVIEWER","connected":true,"writable":true}},
            {{"handle":"t3","worktreePath":"/code/app","lastOutputAt":null,"connected":true,"writable":true}},
            {{"handle":"t4","worktreePath":"/code/app","agentIdentity":"claude","connected":false,"writable":true}}]}}}}"#
        );
        let agents = parse_orca(&json, now).unwrap();

        assert_eq!(
            agents.iter().map(|a| (a.pane_id.as_str(), a.agent_status.as_str(), a.name.as_deref())).collect::<Vec<_>>(),
            [("t1", "working", Some("api")), ("t2", "idle", Some("REVIEWER"))]
        );
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

    #[test]
    fn tmux_pastes_the_text_from_stdin_then_presses_enter() {
        let log = std::env::temp_dir().join(format!("nudge-tmux-{}.log", std::process::id()));
        let tmux = Tmux(script(
            "tmux",
            &format!(
                r#"echo "$*" >> {0}; [ "$1" = load-buffer ] && {{ cat >> {0}; echo >> {0}; }}; exit 0"#,
                log.display()
            ),
        ));
        tmux.prompt("%3", "check the build").unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);

        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            [
                "load-buffer -b nudge-3 -",
                "check the build",
                "paste-buffer -p -d -b nudge-3 -t %3",
                "send-keys -t %3 Enter"
            ]
        );
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
    fn auto_uses_the_first_host_that_answers_and_says_why_when_none_do() {
        let up = Herdr(script("herdr", r#"echo '{"result":{"agents":[]}}'"#));
        let auto = Auto::new(vec![Box::new(Down), Box::new(up)]);
        let nothing = Auto::new(vec![Box::new(Down)]);

        assert_eq!(auto.agents(), Ok(vec![]));
        assert_eq!(auto.name(), "herdr");
        assert_eq!(nothing.agents(), Err("no host answered (down: not running)".into()));
        assert!(named("screen").err().is_some_and(|error| error.contains("is not one of")));
    }
}
