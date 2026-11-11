use std::collections::{HashMap, HashSet};

pub const SERVER: &str = "herdr";
pub const CORE: &str = "herdr-core";
const DEEPEST: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub command: Vec<String>,
    pub started: u64,
}

fn is_server(process: &Process) -> bool {
    let detached = process.parent.is_none_or(|parent| parent <= 1);
    detached && (process.name == SERVER || process.name == CORE)
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

pub struct Machine {
    system: sysinfo::System,
}

impl Machine {
    pub fn new() -> Machine {
        Machine { system: sysinfo::System::new() }
    }

    pub fn list(&mut self) -> Vec<Process> {
        self.system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::OnlyIfNotSet),
        );
        self.system
            .processes()
            .iter()
            .map(|(pid, process)| Process {
                pid: pid.as_u32(),
                parent: process.parent().map(|parent| parent.as_u32()),
                name: process.name().to_string_lossy().into_owned(),
                command: process.cmd().iter().map(|part| part.to_string_lossy().into_owned()).collect(),
                started: process.start_time(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, parent: u32, name: &str) -> Process {
        Process { pid, parent: Some(parent), name: name.into(), command: vec![name.into()], started: 1 }
    }

    #[test]
    fn finds_every_process_under_each_herdr_server_but_not_the_servers_a_client_or_the_rest_of_the_machine() {
        let found = tree(&[
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
        ]);

        assert_eq!(found.servers, 2);
        assert_eq!(found.under.iter().map(|process| process.pid).collect::<Vec<_>>(), [11, 12, 13, 14, 15, 41]);
    }

    #[test]
    fn counts_the_herdr_app_core_as_a_server_but_not_its_clients() {
        let core = |pid: u32, parent: u32, role: &str| Process {
            command: vec!["/Users/me/.local/bin/herdr-core".into(), role.into()],
            ..process(pid, parent, "herdr-core")
        };
        let found = tree(&[
            core(50, 1, "--hc-launchd"),
            core(51, 50, "hold"),
            process(52, 51, "zsh"),
            process(53, 52, "claude"),
            core(60, 1, "hold"),
            process(61, 60, "zsh"),
            core(70, 52, "connect"),
        ]);

        assert_eq!(found.servers, 2);
        assert_eq!(found.under.iter().map(|p| p.pid).collect::<Vec<_>>(), [51, 52, 53, 61, 70]);
    }
}
