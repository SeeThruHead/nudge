use crate::config::{Park, Target};
use crate::host::{Agent, Host, Runner};
use crate::schedule::is_target;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PARKED_TITLE: &str = "(parked)";
pub const PLACEHOLDER: &str = "claude";

const EXIT_WAIT: Duration = Duration::from_secs(60);
const SETTLE: Duration = Duration::from_secs(20);
const SHELLS: [&str; 5] = ["zsh", "bash", "sh", "fish", "dash"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parked {
    pub pane: String,
    pub name: String,
    pub cwd: String,
    pub session: String,
    pub since: u64,
    #[serde(default)]
    pub named_in_herdr: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resting {
    pub pane: String,
    pub name: String,
    pub session: String,
    pub since: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub parked: Vec<Parked>,
    #[serde(default)]
    pub resting: Vec<Resting>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub pid: u32,
    pub name: String,
    pub started: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreground {
    pub claude: Option<u32>,
    pub shell: bool,
}

pub trait Panes {
    fn foreground(&self, pane: &str) -> Result<Foreground, String>;
    fn screen(&self, pane: &str) -> Result<String, String>;
    fn run(&self, pane: &str, command: &str) -> Result<(), String>;
    fn rename(&self, pane: &str, name: &str) -> Result<(), String>;
    fn children(&self, pid: u32) -> Vec<Child>;
    fn close(&self, children: &[Child]) -> Vec<Child>;
    fn placeholder(&self) -> Result<PathBuf, String>;
}

pub fn seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map(|since| since.as_secs()).unwrap_or(0)
}

pub fn moment(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

pub fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

pub fn command_line(placeholder: &Path, parked: &Parked) -> String {
    [placeholder.display().to_string(), parked.session.clone(), parked.name.clone(), parked.cwd.clone()]
        .iter()
        .map(|part| quoted(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn undimmed(line: &str) -> String {
    let (text, _, _) =
        line.chars().fold((String::new(), false, None::<String>), |(text, dim, escape), c| match escape {
            None if c == '\u{1b}' => (text, dim, Some(String::new())),
            None if dim || c.is_control() => (text, dim, None),
            None => (text + &c.to_string(), dim, None),
            Some(sequence) if sequence.is_empty() && c != '[' => (text, dim, None),
            Some(sequence) if ('@'..='~').contains(&c) && !sequence.is_empty() => {
                (text, if c == 'm' { styled_dim(&sequence[1..], dim) } else { dim }, None)
            }
            Some(sequence) => (text, dim, Some(sequence + &c.to_string())),
        });
    text
}

fn styled_dim(parameters: &str, dim: bool) -> bool {
    parameters.split(';').fold(dim, |dim, code| match code {
        "2" => true,
        "" | "0" | "22" => false,
        _ => dim,
    })
}

fn is_rule(line: &str) -> bool {
    line.trim_start().starts_with('─')
}

pub fn draft(screen: &str) -> Option<String> {
    let lines: Vec<String> = screen.lines().map(undimmed).collect();
    let prompt = lines.iter().rposition(|line| line.trim_start().starts_with('❯'))?;
    let body: Vec<&str> = lines[prompt..].iter().take_while(|line| !is_rule(line)).map(String::as_str).collect();
    let text = body.join("\n").trim_start().trim_start_matches('❯').trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn is_parked_screen(agent: &Agent) -> bool {
    agent.terminal_title.as_deref().is_some_and(|title| title.trim_end().ends_with(PARKED_TITLE))
}

fn is_resting(agent: &Agent) -> bool {
    agent.agent_status == "idle" || agent.agent_status == "done"
}

fn session(agent: &Agent) -> Option<&str> {
    agent.agent_session.as_ref().map(|session| session.value.as_str()).filter(|id| !id.is_empty())
}

fn is_shell(child: &Child) -> bool {
    SHELLS.contains(&child.name.trim_start_matches('-'))
}

fn names(children: &[Child]) -> String {
    children.iter().map(|child| format!("{} ({})", child.name, child.pid)).collect::<Vec<_>>().join(", ")
}

pub fn ready(agent: &Agent, panes: &dyn Panes) -> Result<Vec<Child>, String> {
    let foreground = panes.foreground(&agent.pane_id)?;
    let pid = foreground.claude.ok_or_else(|| "no claude process is in the foreground".to_string())?;
    let (running, helpers): (Vec<Child>, Vec<Child>) = panes.children(pid).into_iter().partition(is_shell);
    if !running.is_empty() {
        return Err(format!("it still runs a shell: {}", names(&running)));
    }
    match draft(&panes.screen(&agent.pane_id)?) {
        Some(_) => Err("a draft is in its prompt box".into()),
        None => Ok(helpers),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Exiting {
    parked: Parked,
    since: SystemTime,
    helpers: Vec<Child>,
}

#[derive(Default)]
pub struct Parker {
    pub state: State,
    exiting: Vec<Exiting>,
    said: HashMap<String, String>,
}

impl Parker {
    pub fn new(state: State) -> Parker {
        Parker { state, ..Parker::default() }
    }

    pub fn parked_panes(&self) -> Vec<String> {
        self.state
            .parked
            .iter()
            .map(|parked| parked.pane.clone())
            .chain(self.exiting.iter().map(|e| e.parked.pane.clone()))
            .collect()
    }

    fn once(&mut self, pane: &str, line: String) -> Option<String> {
        let repeated = self.said.insert(pane.to_string(), line.clone()).as_ref() == Some(&line);
        (!repeated).then_some(line)
    }

    pub fn tick(
        &mut self,
        now: SystemTime,
        park: &Park,
        never: &[Target],
        host: &dyn Host,
        panes: &dyn Panes,
    ) -> Vec<String> {
        let agents = match host.agents() {
            Ok(agents) => agents,
            Err(error) => return self.once("", format!("park: could not list agents: {error}")).into_iter().collect(),
        };
        let (woke, gone) = self.wake_up(now, &agents);
        self.keep_names(&gone, &agents, panes);
        let parked = self.finish_exits(now, panes);
        self.track(now, never, &agents);
        let started = self.start_exits(now, park, &agents, host, panes);
        woke.into_iter().chain(parked).chain(started).collect()
    }

    fn wake_up(&mut self, now: SystemTime, agents: &[Agent]) -> (Vec<String>, Vec<Parked>) {
        let (kept, gone): (Vec<Parked>, Vec<Parked>) =
            std::mem::take(&mut self.state.parked).into_iter().partition(|parked| {
                let settling = now.duration_since(moment(parked.since)).is_ok_and(|age| age < SETTLE);
                settling || agents.iter().any(|agent| agent.pane_id == parked.pane && is_parked_screen(agent))
            });
        self.state.parked = kept;
        let log = gone
            .iter()
            .map(|parked| match agents.iter().find(|agent| agent.pane_id == parked.pane) {
                Some(_) => format!("park: {} ({}) woke, resuming session {}", parked.name, parked.pane, parked.session),
                None => format!(
                    "park: {} ({}) is no longer parked; claude --resume {} picks it up",
                    parked.name, parked.pane, parked.session
                ),
            })
            .collect();
        (log, gone)
    }

    fn keep_names(&self, woken: &[Parked], agents: &[Agent], panes: &dyn Panes) {
        misnamed(self.state.parked.iter().chain(woken), agents).iter().for_each(|(pane, name)| {
            let _ = panes.rename(pane, name);
        });
    }

    fn finish_exits(&mut self, now: SystemTime, panes: &dyn Panes) -> Vec<String> {
        let (log, keep): (Vec<Option<String>>, Vec<Option<Exiting>>) = std::mem::take(&mut self.exiting)
            .into_iter()
            .map(|exiting| match panes.foreground(&exiting.parked.pane) {
                Ok(Foreground { shell: true, .. }) => {
                    let closed = panes.close(&exiting.helpers);
                    let placed = self.place(now, exiting.parked, panes);
                    let line = match closed.is_empty() {
                        true => placed,
                        false => format!("{placed}; closed what claude left running: {}", names(&closed)),
                    };
                    (Some(line), None)
                }
                _ if now.duration_since(exiting.since).is_ok_and(|waited| waited >= EXIT_WAIT) => (
                    Some(format!(
                        "park: {} ({}) did not leave claude within a minute, leaving it alone",
                        exiting.parked.name, exiting.parked.pane
                    )),
                    None,
                ),
                _ => (None, Some(exiting)),
            })
            .unzip();
        self.exiting = keep.into_iter().flatten().collect();
        log.into_iter().flatten().collect()
    }

    fn place(&mut self, now: SystemTime, parked: Parked, panes: &dyn Panes) -> String {
        let started = panes.placeholder().and_then(|path| panes.run(&parked.pane, &command_line(&path, &parked)));
        match started {
            Ok(()) => {
                let line = format!("park: parked {} ({}), session {}", parked.name, parked.pane, parked.session);
                self.state.parked.push(Parked { since: seconds(now), ..parked });
                line
            }
            Err(error) => format!(
                "park: {} ({}) left claude but the placeholder did not start ({error}); claude --resume {} picks it up",
                parked.name, parked.pane, parked.session
            ),
        }
    }

    fn track(&mut self, now: SystemTime, never: &[Target], agents: &[Agent]) {
        let held = self.parked_panes();
        let before = std::mem::take(&mut self.state.resting);
        self.state.resting = agents
            .iter()
            .filter(|agent| is_resting(agent) && !is_parked_screen(agent) && !held.contains(&agent.pane_id))
            .filter(|agent| !never.iter().any(|target| is_target(target, agent)))
            .filter_map(|agent| {
                let session = session(agent)?;
                let since = before
                    .iter()
                    .find(|resting| resting.pane == agent.pane_id && resting.session == session)
                    .map(|resting| resting.since)
                    .unwrap_or_else(|| seconds(now));
                Some(Resting { pane: agent.pane_id.clone(), name: agent.title(), session: session.to_string(), since })
            })
            .collect();
    }

    fn start_exits(
        &mut self,
        now: SystemTime,
        park: &Park,
        agents: &[Agent],
        host: &dyn Host,
        panes: &dyn Panes,
    ) -> Vec<String> {
        let due: Vec<(Resting, Agent)> = self
            .state
            .resting
            .iter()
            .filter(|resting| now.duration_since(moment(resting.since)).is_ok_and(|rested| rested >= park.after))
            .filter_map(|resting| {
                agents.iter().find(|agent| agent.pane_id == resting.pane).map(|agent| (resting.clone(), agent.clone()))
            })
            .collect();
        due.into_iter().filter_map(|(resting, agent)| self.exit(now, resting, &agent, host, panes)).collect()
    }

    fn exit(
        &mut self,
        now: SystemTime,
        resting: Resting,
        agent: &Agent,
        host: &dyn Host,
        panes: &dyn Panes,
    ) -> Option<String> {
        let parked = Parked {
            pane: resting.pane.clone(),
            name: resting.name.clone(),
            cwd: agent.cwd.clone().unwrap_or_default(),
            session: resting.session.clone(),
            since: seconds(now),
            named_in_herdr: agent.name.is_some(),
        };
        let outcome = ready(agent, panes).and_then(|helpers| host.prompt(&resting.pane, "/exit").map(|()| helpers));
        match outcome {
            Ok(helpers) => {
                self.said.remove(&resting.pane);
                self.state.resting.retain(|other| other.pane != resting.pane);
                self.exiting.push(Exiting { parked, since: now, helpers });
                Some(format!(
                    "park: {} ({}) rested {}, closing claude",
                    resting.name,
                    resting.pane,
                    rested(resting.since, now)
                ))
            }
            Err(reason) => {
                self.state
                    .resting
                    .iter_mut()
                    .filter(|other| other.pane == resting.pane)
                    .for_each(|other| other.since = seconds(now));
                self.once(&resting.pane, format!("park: not parking {} ({}): {reason}", resting.name, resting.pane))
            }
        }
    }
}

fn misnamed<'a>(parked: impl Iterator<Item = &'a Parked>, agents: &[Agent]) -> Vec<(String, String)> {
    parked
        .filter(|parked| parked.named_in_herdr)
        .filter(|parked| {
            agents.iter().any(|agent| agent.pane_id == parked.pane && agent.name.as_ref() != Some(&parked.name))
        })
        .map(|parked| (parked.pane.clone(), parked.name.clone()))
        .collect()
}

fn rested(since: u64, now: SystemTime) -> String {
    humantime::format_duration(Duration::from_secs(seconds(now).saturating_sub(since) / 60 * 60)).to_string()
}

pub fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("park.json")
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

pub fn find<'a>(state: &'a State, wanted: &str) -> Option<&'a Parked> {
    state.parked.iter().find(|parked| parked.pane == wanted || parked.name == wanted)
}

#[derive(Deserialize)]
struct Process {
    pid: u32,
    #[serde(default)]
    argv: Vec<String>,
}

#[derive(Deserialize)]
struct ProcessInfo {
    foreground_process_group_id: u32,
    shell_pid: u32,
    #[serde(default)]
    foreground_processes: Vec<Process>,
}

#[derive(Deserialize)]
struct ProcessInfoResult {
    process_info: ProcessInfo,
}

#[derive(Deserialize)]
struct ProcessInfoReply {
    result: ProcessInfoResult,
}

fn is_claude(process: &Process) -> bool {
    let Some(first) = process.argv.first() else { return false };
    let named = Path::new(first).file_name().is_some_and(|name| name == PLACEHOLDER);
    named && !first.contains("/nudge/parked/")
}

pub fn parse_foreground(json: &str) -> Result<Foreground, String> {
    let reply: ProcessInfoReply = serde_json::from_str(json)
        .map_err(|e| format!("herdr pane process-info returned something unexpected: {e}"))?;
    let info = reply.result.process_info;
    Ok(Foreground {
        claude: info.foreground_processes.iter().find(|process| is_claude(process)).map(|process| process.pid),
        shell: info.foreground_process_group_id == info.shell_pid,
    })
}

pub struct HerdrPanes {
    pub herdr: Runner,
    pub data: PathBuf,
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

pub fn install(from: &Path, dir: &Path) -> Result<PathBuf, String> {
    let target = dir.join(PLACEHOLDER);
    if same_file(from, &target) {
        return Ok(target);
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    let partial = dir.join(".claude.partial");
    std::fs::copy(from, &partial).map_err(|e| format!("could not copy nudge to {}: {e}", partial.display()))?;
    let mut perms = std::fs::metadata(&partial).map_err(|e| e.to_string())?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&partial, perms).map_err(|e| e.to_string())?;
    std::fs::rename(&partial, &target).map_err(|e| format!("could not place {}: {e}", target.display()))?;
    Ok(target)
}

fn everything() -> Vec<(Option<u32>, Child)> {
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::nothing());
    system
        .processes()
        .iter()
        .map(|(pid, process)| {
            let child = Child {
                pid: pid.as_u32(),
                name: process.name().to_string_lossy().into_owned(),
                started: process.start_time(),
            };
            (process.parent().map(|parent| parent.as_u32()), child)
        })
        .collect()
}

impl Panes for HerdrPanes {
    fn foreground(&self, pane: &str) -> Result<Foreground, String> {
        parse_foreground(&self.herdr.run(&["pane", "process-info", "--pane", pane], None)?)
    }
    fn screen(&self, pane: &str) -> Result<String, String> {
        self.herdr.run(&["pane", "read", pane, "--source", "visible", "--format", "ansi"], None)
    }
    fn run(&self, pane: &str, command: &str) -> Result<(), String> {
        self.herdr.run(&["pane", "run", pane, command], None).map(|_| ())
    }
    fn rename(&self, pane: &str, name: &str) -> Result<(), String> {
        self.herdr.run(&["agent", "rename", pane, name], None).map(|_| ())
    }
    fn children(&self, pid: u32) -> Vec<Child> {
        everything().into_iter().filter(|(parent, _)| *parent == Some(pid)).map(|(_, child)| child).collect()
    }
    fn close(&self, children: &[Child]) -> Vec<Child> {
        let alive: Vec<Child> = everything().into_iter().map(|(_, child)| child).collect();
        children
            .iter()
            .filter(|child| alive.contains(child))
            .filter(|child| crate::freeze::terminate(child.pid).is_ok())
            .cloned()
            .collect()
    }
    fn placeholder(&self) -> Result<PathBuf, String> {
        let me = std::env::current_exe().map_err(|e| format!("could not find nudge itself: {e}"))?;
        install(&me, &self.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Session;
    use std::cell::RefCell;

    fn agent(pane: &str, status: &str, cwd: &str, session: &str) -> Agent {
        Agent {
            pane_id: pane.into(),
            agent_status: status.into(),
            name: None,
            cwd: Some(cwd.into()),
            agent_session: Some(Session { value: session.into() }),
            terminal_title: Some(format!("✳ {}", cwd.rsplit('/').next().unwrap_or_default())),
        }
    }

    struct Fake {
        agents: RefCell<Vec<Agent>>,
        prompts: RefCell<Vec<(String, String)>>,
        runs: RefCell<Vec<(String, String)>>,
        renames: RefCell<Vec<(String, String)>>,
        shell: RefCell<Vec<String>>,
        children: Vec<Child>,
        closed: RefCell<Vec<Child>>,
        screen: String,
    }

    fn fake(agents: Vec<Agent>) -> Fake {
        Fake {
            agents: RefCell::new(agents),
            prompts: RefCell::new(vec![]),
            runs: RefCell::new(vec![]),
            renames: RefCell::new(vec![]),
            shell: RefCell::new(vec![]),
            children: vec![],
            closed: RefCell::new(vec![]),
            screen: "──── app ─\n❯ \u{1b}[2mTry \"fix the tests\"\u{1b}[0m\n────\n".into(),
        }
    }

    impl Host for Fake {
        fn agents(&self) -> Result<Vec<Agent>, String> {
            Ok(self.agents.borrow().clone())
        }
        fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
            self.prompts.borrow_mut().push((pane.into(), text.into()));
            Ok(())
        }
    }

    impl Panes for Fake {
        fn foreground(&self, pane: &str) -> Result<Foreground, String> {
            let shell = self.shell.borrow().iter().any(|exited| exited == pane);
            Ok(Foreground { claude: (!shell).then_some(42), shell })
        }
        fn screen(&self, _: &str) -> Result<String, String> {
            Ok(self.screen.clone())
        }
        fn run(&self, pane: &str, command: &str) -> Result<(), String> {
            self.runs.borrow_mut().push((pane.into(), command.into()));
            Ok(())
        }
        fn rename(&self, pane: &str, name: &str) -> Result<(), String> {
            self.renames.borrow_mut().push((pane.into(), name.into()));
            Ok(())
        }
        fn children(&self, _: u32) -> Vec<Child> {
            self.children.clone()
        }
        fn close(&self, children: &[Child]) -> Vec<Child> {
            self.closed.borrow_mut().extend(children.iter().cloned());
            children.to_vec()
        }
        fn placeholder(&self) -> Result<PathBuf, String> {
            Ok(PathBuf::from("/data/nudge/parked/claude"))
        }
    }

    fn child(pid: u32, name: &str) -> Child {
        Child { pid, name: name.into(), started: 5 }
    }

    #[test]
    fn an_mcp_server_claude_started_does_not_keep_it_from_parking_and_is_closed_once_claude_exits() {
        let mcp = vec![child(80, "bun"), child(81, "caffeinate")];
        let herdr = Fake { children: mcp.clone(), ..fake(vec![agent("w9:p1", "idle", "/code/app", "abc")]) };
        let mut parker = Parker::default();
        parker.tick(at(0), &park(), &[], &herdr, &herdr);
        let closing = parker.tick(at(30), &park(), &[], &herdr, &herdr);
        let early = herdr.closed.borrow().len();
        herdr.shell.borrow_mut().push("w9:p1".into());
        let parked = parker.tick(at(31), &park(), &[], &herdr, &herdr);

        assert_eq!(closing, ["park: app (w9:p1) rested 30m, closing claude"]);
        assert_eq!(early, 0);
        assert_eq!(
            parked,
            ["park: parked app (w9:p1), session abc; closed what claude left running: bun (80), caffeinate (81)"]
        );
        assert_eq!(*herdr.closed.borrow(), mcp);
    }

    fn at(minutes: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000 + minutes * 60)
    }

    fn park() -> Park {
        Park { after: Duration::from_secs(30 * 60), skip: vec![] }
    }

    #[test]
    fn parks_an_agent_resting_for_the_whole_window_behind_the_placeholder_and_notices_when_it_wakes() {
        let herdr =
            fake(vec![agent("w9:p1", "done", "/code/app", "abc"), agent("w2:p1", "working", "/code/site", "def")]);
        let mut parker = Parker::default();

        let early = [0, 10, 29].map(|minute| parker.tick(at(minute), &park(), &[], &herdr, &herdr));
        herdr.agents.borrow_mut()[0].agent_status = "idle".into();
        let closing = parker.tick(at(30), &park(), &[], &herdr, &herdr);
        let waiting = parker.tick(at(30) + Duration::from_secs(5), &park(), &[], &herdr, &herdr);
        herdr.shell.borrow_mut().push("w9:p1".into());
        herdr.agents.borrow_mut().remove(0);
        let parked = parker.tick(at(30) + Duration::from_secs(10), &park(), &[], &herdr, &herdr);
        herdr.agents.borrow_mut().push(Agent {
            terminal_title: Some("✳ app (parked)".into()),
            ..agent("w9:p1", "idle", "/code/app", "abc")
        });
        let resting = parker.tick(at(45), &park(), &[], &herdr, &herdr);
        let state = parker.state.clone();
        herdr.agents.borrow_mut()[1].terminal_title = Some("✳ app".into());
        let woke = parker.tick(at(50), &park(), &[], &herdr, &herdr);

        assert!(early.iter().all(Vec::is_empty));
        assert_eq!(closing, ["park: app (w9:p1) rested 30m, closing claude"]);
        assert_eq!(*herdr.prompts.borrow(), [("w9:p1".to_string(), "/exit".to_string())]);
        assert!(waiting.is_empty());
        assert_eq!(parked, ["park: parked app (w9:p1), session abc"]);
        assert_eq!(
            *herdr.runs.borrow(),
            [("w9:p1".to_string(), "'/data/nudge/parked/claude' 'abc' 'app' '/code/app'".to_string())]
        );
        assert!(resting.is_empty());
        assert_eq!(
            state.parked,
            [Parked {
                pane: "w9:p1".into(),
                name: "app".into(),
                cwd: "/code/app".into(),
                session: "abc".into(),
                since: seconds(at(30) + Duration::from_secs(10)),
                named_in_herdr: false,
            }]
        );
        assert!(state.resting.is_empty());
        assert_eq!(woke, ["park: app (w9:p1) woke, resuming session abc"]);
        assert!(parker.state.parked.is_empty());
    }

    #[test]
    fn never_parks_a_busy_agent_an_agent_that_runs_something_or_holds_a_draft_or_one_it_was_told_to_skip() {
        let resting = |pane: &str, cwd: &str| agent(pane, "idle", cwd, pane);
        let busy = fake(vec![resting("w1:p1", "/code/a"), agent("w2:p1", "blocked", "/code/b", "b")]);
        let running = Fake {
            children: vec![child(70, "caffeinate"), child(71, "zsh")],
            ..fake(vec![resting("w3:p1", "/code/c")])
        };
        let drafting = Fake {
            screen: "──── c ─\n❯ fix the \u{1b}[7m \u{1b}[0m\n────\n".into(),
            ..fake(vec![resting("w4:p1", "/code/d")])
        };
        let skipped = fake(vec![resting("w5:p1", "/code/homelab")]);
        let never = [Target::Cwd("/code/homelab".into())];
        let flip = |parker: &mut Parker, herdr: &Fake, minute: u64| {
            herdr.agents.borrow_mut().iter_mut().filter(|a| a.pane_id == "w1:p1").for_each(|a| {
                a.agent_status = if minute == 20 { "working" } else { "idle" }.into();
            });
            parker.tick(at(minute), &park(), &never, herdr, herdr)
        };
        let mut parkers: [Parker; 4] = Default::default();

        let busy_log: Vec<Vec<String>> = [0, 20, 40].map(|minute| flip(&mut parkers[0], &busy, minute)).to_vec();
        let running_log = [0, 30, 31].map(|minute| parkers[1].tick(at(minute), &park(), &never, &running, &running));
        let drafting_log = parkers[2].tick(at(0), &park(), &never, &drafting, &drafting);
        let drafting_due = parkers[2].tick(at(30), &park(), &never, &drafting, &drafting);
        let skipped_log = [0, 60].map(|minute| parkers[3].tick(at(minute), &park(), &never, &skipped, &skipped));

        assert!(busy_log.iter().all(Vec::is_empty));
        assert!(busy.prompts.borrow().is_empty());
        assert_eq!(running_log[1], ["park: not parking c (w3:p1): it still runs a shell: zsh (71)"]);
        assert!(running_log[2].is_empty());
        assert_eq!(parkers[1].state.resting[0].since, seconds(at(30)));
        assert!(drafting_log.is_empty());
        assert_eq!(drafting_due, ["park: not parking d (w4:p1): a draft is in its prompt box"]);
        assert!(skipped_log.iter().all(Vec::is_empty) && parkers[3].state.resting.is_empty());
        assert!([running, drafting, skipped].iter().all(|herdr| herdr.prompts.borrow().is_empty()));
    }

    #[test]
    fn keeps_the_name_herdr_gave_the_agent_while_it_is_parked_and_after_it_wakes() {
        let named = |title: &str, name: Option<&str>| Agent {
            name: name.map(String::from),
            terminal_title: Some(title.into()),
            ..agent("w1:p1", "idle", "/code/a", "s1")
        };
        let herdr = fake(vec![named("✳ a", Some("api"))]);
        let mut parker = Parker::default();
        parker.tick(at(0), &park(), &[], &herdr, &herdr);
        parker.tick(at(30), &park(), &[], &herdr, &herdr);
        herdr.shell.borrow_mut().push("w1:p1".into());
        *herdr.agents.borrow_mut() = vec![];
        parker.tick(at(31), &park(), &[], &herdr, &herdr);
        *herdr.agents.borrow_mut() = vec![named("✳ api (parked)", None)];
        parker.tick(at(32), &park(), &[], &herdr, &herdr);
        let while_parked = herdr.renames.borrow().clone();
        *herdr.agents.borrow_mut() = vec![named("✳ api (parked)", Some("api"))];
        parker.tick(at(33), &park(), &[], &herdr, &herdr);
        *herdr.agents.borrow_mut() = vec![named("✳ a", None)];
        let woke = parker.tick(at(40), &park(), &[], &herdr, &herdr);

        assert_eq!(while_parked, [("w1:p1".to_string(), "api".to_string())]);
        assert_eq!(woke, ["park: api (w1:p1) woke, resuming session s1"]);
        assert_eq!(herdr.renames.borrow().len(), 2);
    }

    #[test]
    fn a_new_session_in_the_pane_starts_the_window_again() {
        let herdr = fake(vec![agent("w1:p1", "idle", "/code/a", "first")]);
        let mut parker = Parker::default();
        parker.tick(at(0), &park(), &[], &herdr, &herdr);
        herdr.agents.borrow_mut()[0].agent_session = Some(Session { value: "second".into() });
        parker.tick(at(20), &park(), &[], &herdr, &herdr);

        assert!(parker.tick(at(30), &park(), &[], &herdr, &herdr).is_empty());
        assert_eq!(parker.tick(at(50), &park(), &[], &herdr, &herdr), ["park: a (w1:p1) rested 30m, closing claude"]);
    }

    #[test]
    fn gives_up_on_a_claude_that_does_not_exit_and_forgets_a_parked_pane_that_closed() {
        let herdr = fake(vec![agent("w1:p1", "idle", "/code/a", "s1")]);
        let mut parker = Parker::new(State {
            parked: vec![Parked {
                pane: "w7:p1".into(),
                name: "old".into(),
                cwd: "/x".into(),
                session: "s0".into(),
                since: 0,
                named_in_herdr: false,
            }],
            resting: vec![],
        });
        let first = parker.tick(at(0), &park(), &[], &herdr, &herdr);
        parker.tick(at(30), &park(), &[], &herdr, &herdr);
        let gave_up = parker.tick(at(32), &park(), &[], &herdr, &herdr);

        assert_eq!(first, ["park: old (w7:p1) is no longer parked; claude --resume s0 picks it up"]);
        assert_eq!(gave_up, ["park: a (w1:p1) did not leave claude within a minute, leaving it alone"]);
        assert!(herdr.runs.borrow().is_empty());
    }

    #[test]
    fn reads_a_draft_from_the_prompt_box_but_not_claudes_dim_suggestion() {
        let empty = "out\n\u{1b}[38;2;136;136;136m──── app ─\u{1b}[0m\r\n❯ \r\n────\n  ⏵⏵ bypass\n";
        let suggestion = "──── app ─\n❯ \u{1b}[0m\u{1b}[2mTry \"write a test\"\u{1b}[0m\n────\n";
        let typed = "❯ earlier\n──── app ─\n❯ check the\n  build please\n────\n";

        assert_eq!(draft(empty), None);
        assert_eq!(draft(suggestion), None);
        assert_eq!(draft(typed), Some("check the\n  build please".into()));
        assert_eq!(draft("no prompt here"), None);
    }

    #[test]
    fn reads_herdrs_process_info_and_tells_claude_from_the_placeholder_and_the_shell() {
        let reply = |group: u32, processes: &str| {
            format!(
                r#"{{"result":{{"process_info":{{"foreground_process_group_id":{group},"shell_pid":7,"foreground_processes":[{processes}]}}}}}}"#
            )
        };
        let claude = reply(9, r#"{"pid":11,"argv":["caffeinate"]},{"pid":9,"argv":["claude","--tools","Read"]}"#);
        let placeholder = reply(9, r#"{"pid":9,"argv":["/Users/me/.local/share/nudge/parked/claude","abc"]}"#);
        let shell = reply(7, r#"{"pid":7,"argv":["-zsh"]}"#);

        assert_eq!(parse_foreground(&claude), Ok(Foreground { claude: Some(9), shell: false }));
        assert_eq!(parse_foreground(&placeholder), Ok(Foreground { claude: None, shell: false }));
        assert_eq!(parse_foreground(&shell), Ok(Foreground { claude: None, shell: true }));
        assert!(parse_foreground("{}").is_err());
    }

    #[test]
    fn quotes_the_placeholder_command_for_the_shell_and_keeps_the_state_across_restarts() {
        let parked = Parked {
            pane: "w1:p1".into(),
            name: "Shane's app".into(),
            cwd: "/code/a b".into(),
            session: "s".into(),
            since: 5,
            named_in_herdr: false,
        };
        let dir = std::env::temp_dir().join(format!("nudge-park-{}", std::process::id()));
        let path = state_path(&dir);
        let state = State { parked: vec![parked.clone()], resting: vec![] };
        save(&path, &state).unwrap();
        let loaded = load(&path);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(command_line(Path::new("/p/claude"), &parked), r"'/p/claude' 's' 'Shane'\''s app' '/code/a b'");
        assert_eq!(loaded, state);
        assert_eq!(find(&loaded, "Shane's app"), Some(&parked));
        assert_eq!(find(&loaded, "w1:p1"), Some(&parked));
        assert_eq!(load(Path::new("/nonexistent/park.json")), State::default());
    }

    #[test]
    fn installs_a_copy_of_nudge_named_claude_and_refreshes_it_when_nudge_changes() {
        let dir = std::env::temp_dir().join(format!("nudge-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("nudge");
        std::fs::write(&source, "v1").unwrap();
        let first = install(&source, &dir.join("parked")).unwrap();
        std::fs::write(&source, "v2").unwrap();
        install(&source, &dir.join("parked")).unwrap();
        let copied = std::fs::read_to_string(&first).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&first).unwrap().permissions());
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(first.file_name().unwrap(), "claude");
        assert_eq!(copied, "v2");
        assert_eq!(mode & 0o777, 0o755);
    }
}
