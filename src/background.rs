use std::collections::{HashMap, HashSet};

pub const SERVER: &str = "herdr";
pub const BACKGROUND_PRIORITY: i32 = 4;
const DEEPEST: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
}

pub trait Processes {
    fn list(&mut self) -> Vec<Process>;
    fn priority(&self, pid: u32) -> Option<i32>;
    fn to_background(&self, pid: u32) -> Result<(), String>;
}

fn is_server(process: &Process) -> bool {
    process.name == SERVER && process.parent.is_none_or(|parent| parent <= 1)
}

fn under(process: &Process, parents: &HashMap<u32, Option<u32>>, servers: &HashSet<u32>) -> bool {
    std::iter::successors(process.parent, |pid| parents.get(pid).copied().flatten())
        .take(DEEPEST)
        .take_while(|pid| *pid > 1)
        .any(|pid| servers.contains(&pid))
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tree {
    pub servers: usize,
    pub under: Vec<Process>,
}

pub fn tree(processes: &[Process]) -> Tree {
    let servers: HashSet<u32> = processes.iter().filter(|process| is_server(process)).map(|p| p.pid).collect();
    let parents: HashMap<u32, Option<u32>> = processes.iter().map(|process| (process.pid, process.parent)).collect();
    let mut found: Vec<Process> =
        processes.iter().filter(|process| under(process, &parents, &servers)).cloned().collect();
    found.sort_by_key(|process| process.pid);
    Tree { servers: servers.len(), under: found }
}

pub fn escaped(tree: &Tree, processes: &dyn Processes) -> Vec<Process> {
    tree.under
        .iter()
        .filter(|process| processes.priority(process.pid).is_some_and(|priority| priority > BACKGROUND_PRIORITY))
        .cloned()
        .collect()
}

pub fn described(moved: &[Process]) -> String {
    moved.iter().map(|process| format!("{} ({})", process.name, process.pid)).collect::<Vec<_>>().join(", ")
}

type Attempt = (Process, Result<(), String>);

#[derive(Default)]
pub struct Enforcer {
    refused: HashSet<u32>,
}

impl Enforcer {
    pub fn tick(&mut self, processes: &mut dyn Processes) -> Vec<String> {
        let listed = processes.list();
        let alive: HashSet<u32> = listed.iter().map(|process| process.pid).collect();
        self.refused.retain(|pid| alive.contains(pid));
        let tree = tree(&listed);
        let (moved, refused): (Vec<Attempt>, Vec<Attempt>) = escaped(&tree, processes)
            .into_iter()
            .filter(|process| !self.refused.contains(&process.pid))
            .map(|process| {
                let outcome = processes.to_background(process.pid);
                (process, outcome)
            })
            .partition(|(_, outcome)| outcome.is_ok());
        self.refused.extend(refused.iter().map(|(process, _)| process.pid));
        let moved: Vec<Process> = moved.into_iter().map(|(process, _)| process).collect();
        let moved_line = (!moved.is_empty()).then(|| {
            format!(
                "background: put {} processes under herdr back in the background: {}",
                moved.len(),
                described(&moved)
            )
        });
        let refused_lines = refused.into_iter().map(|(process, outcome)| {
            format!(
                "background: could not put {} ({}) in the background: {}",
                process.name,
                process.pid,
                outcome.err().unwrap_or_default()
            )
        });
        moved_line.into_iter().chain(refused_lines).collect()
    }
}

#[cfg(target_os = "macos")]
mod darwin {
    const PRIO_DARWIN_PROCESS: i32 = 4;
    const PRIO_DARWIN_BG: i32 = 0x1000;
    const PROC_PIDTASKINFO: i32 = 4;
    const TASK_INFO_SIZE: usize = 96;
    const PRIORITY_AT: usize = 92;

    unsafe extern "C" {
        fn setpriority(which: i32, who: u32, priority: i32) -> i32;
        fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut u8, size: i32) -> i32;
    }

    pub fn priority(pid: u32) -> Option<i32> {
        let mut info = [0u8; TASK_INFO_SIZE];
        let pid = i32::try_from(pid).ok()?;
        let written = unsafe { proc_pidinfo(pid, PROC_PIDTASKINFO, 0, info.as_mut_ptr(), TASK_INFO_SIZE as i32) };
        let bytes: [u8; 4] = info[PRIORITY_AT..PRIORITY_AT + 4].try_into().ok()?;
        (written == TASK_INFO_SIZE as i32).then(|| i32::from_ne_bytes(bytes))
    }

    fn set(pid: u32, policy: i32) -> Result<(), String> {
        match unsafe { setpriority(PRIO_DARWIN_PROCESS, pid, policy) } {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error().to_string()),
        }
    }

    pub fn to_background(pid: u32) -> Result<(), String> {
        set(pid, PRIO_DARWIN_BG)
    }

    #[cfg(test)]
    pub fn to_foreground(pid: u32) -> Result<(), String> {
        set(pid, 0)
    }
}

pub const SUPPORTED: bool = cfg!(target_os = "macos");

pub struct Machine {
    system: sysinfo::System,
}

impl Machine {
    pub fn new() -> Machine {
        Machine { system: sysinfo::System::new() }
    }
}

impl Processes for Machine {
    fn list(&mut self) -> Vec<Process> {
        self.system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing(),
        );
        self.system
            .processes()
            .iter()
            .map(|(pid, process)| Process {
                pid: pid.as_u32(),
                parent: process.parent().map(|parent| parent.as_u32()),
                name: process.name().to_string_lossy().into_owned(),
            })
            .collect()
    }

    #[cfg(target_os = "macos")]
    fn priority(&self, pid: u32) -> Option<i32> {
        darwin::priority(pid)
    }

    #[cfg(not(target_os = "macos"))]
    fn priority(&self, _: u32) -> Option<i32> {
        None
    }

    #[cfg(target_os = "macos")]
    fn to_background(&self, pid: u32) -> Result<(), String> {
        darwin::to_background(pid)
    }

    #[cfg(not(target_os = "macos"))]
    fn to_background(&self, _: u32) -> Result<(), String> {
        Err("background policy is macOS only".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn process(pid: u32, parent: u32, name: &str) -> Process {
        Process { pid, parent: Some(parent), name: name.into() }
    }

    struct Fake {
        processes: Vec<Process>,
        priorities: RefCell<HashMap<u32, i32>>,
        refuse: Vec<u32>,
        asked: RefCell<Vec<u32>>,
    }

    impl Processes for Fake {
        fn list(&mut self) -> Vec<Process> {
            self.processes.clone()
        }
        fn priority(&self, pid: u32) -> Option<i32> {
            self.priorities.borrow().get(&pid).copied()
        }
        fn to_background(&self, pid: u32) -> Result<(), String> {
            self.asked.borrow_mut().push(pid);
            if self.refuse.contains(&pid) {
                return Err("Operation not permitted".into());
            }
            self.priorities.borrow_mut().insert(pid, BACKGROUND_PRIORITY);
            Ok(())
        }
    }

    fn machine() -> Fake {
        let processes = vec![
            process(10, 1, "herdr"),
            process(11, 10, "zsh"),
            process(12, 11, "claude"),
            process(13, 12, "bun"),
            process(14, 13, "node"),
            process(15, 14, "Google Chrome for Testing"),
            process(20, 9, "herdr"),
            process(21, 20, "ssh"),
            process(30, 1, "WindowServer"),
            process(40, 1, "herdr"),
            process(41, 40, "zsh"),
        ];
        let priorities =
            [(10, 31), (11, 4), (12, 4), (13, 4), (14, 46), (15, 46), (20, 31), (21, 31), (30, 79), (40, 37)]
                .into_iter()
                .chain([(41, 31)])
                .collect();
        Fake { processes, priorities: RefCell::new(priorities), refuse: vec![], asked: RefCell::new(vec![]) }
    }

    #[test]
    fn finds_every_process_under_each_herdr_server_but_not_the_servers_a_client_or_the_rest_of_the_machine() {
        let found = tree(&machine().processes);

        assert_eq!(found.servers, 2);
        assert_eq!(found.under.iter().map(|process| process.pid).collect::<Vec<_>>(), [11, 12, 13, 14, 15, 41]);
    }

    #[test]
    fn puts_processes_that_escaped_the_background_back_and_leaves_the_rest_alone() {
        let mut fake = machine();
        let mut enforcer = Enforcer::default();

        let first = enforcer.tick(&mut fake);
        let second = enforcer.tick(&mut fake);

        assert_eq!(
            first,
            [
                "background: put 3 processes under herdr back in the background: node (14), Google Chrome for Testing (15), zsh (41)"
            ]
        );
        assert!(second.is_empty());
        assert_eq!(*fake.asked.borrow(), [14, 15, 41]);
    }

    #[test]
    fn says_once_when_a_process_refuses_and_does_not_ask_it_again() {
        let mut fake = Fake { refuse: vec![14], ..machine() };
        let mut enforcer = Enforcer::default();

        let first = enforcer.tick(&mut fake);
        let second = enforcer.tick(&mut fake);

        assert_eq!(first[1], "background: could not put node (14) in the background: Operation not permitted");
        assert!(second.is_empty());
        assert_eq!(fake.asked.borrow().iter().filter(|pid| **pid == 14).count(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn moves_a_real_process_into_the_background() {
        let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let pid = child.id();
        let machine = Machine::new();
        darwin::to_foreground(pid).unwrap();
        let before = machine.priority(pid);

        let moved = machine.to_background(pid);
        let priority = machine.priority(pid);
        let _ = child.kill();
        let _ = child.wait();

        assert!(before.is_some_and(|priority| priority > BACKGROUND_PRIORITY));
        assert_eq!(moved, Ok(()));
        assert_eq!(priority, Some(BACKGROUND_PRIORITY));
        assert_eq!(machine.priority(u32::MAX >> 1), None);
    }
}
