use crate::config::{Config, Job, Target};
use crate::herdr::{Agent, Herdr};
use std::collections::HashMap;
use std::time::SystemTime;

#[derive(Debug, PartialEq, Eq)]
pub enum Pick<'a> {
    Ready(&'a Agent),
    Busy(&'a Agent),
    Missing,
    Ambiguous(usize),
}

fn is_target(target: &Target, agent: &Agent) -> bool {
    match target {
        Target::Session(id) => agent.agent_session.as_ref().is_some_and(|s| &s.value == id),
        Target::Agent(name) => agent.name.as_ref() == Some(name),
        Target::Cwd(cwd) => agent.cwd.as_ref() == Some(cwd),
    }
}

fn is_ready(agent: &Agent) -> bool {
    agent.agent_status == "idle" || agent.agent_status == "done"
}

pub fn pick<'a>(target: &Target, agents: &'a [Agent]) -> Pick<'a> {
    let found: Vec<&Agent> = agents.iter().filter(|agent| is_target(target, agent)).collect();
    match found.as_slice() {
        [] => Pick::Missing,
        [one] if is_ready(one) => Pick::Ready(one),
        [one] => Pick::Busy(one),
        many => Pick::Ambiguous(many.len()),
    }
}

pub fn text(job: &Job) -> String {
    format!("[nudge: {}, every {}] {}", job.name, humantime::format_duration(job.every), job.message)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Sent(String),
    Waiting(String),
}

pub fn attempt(job: &Job, herdr: &impl Herdr) -> Outcome {
    let agents = match herdr.agents() {
        Ok(agents) => agents,
        Err(error) => return Outcome::Waiting(error),
    };
    match pick(&job.target, &agents) {
        Pick::Ready(agent) => match herdr.prompt(&agent.pane_id, &text(job)) {
            Ok(()) => Outcome::Sent(agent.pane_id.clone()),
            Err(error) => Outcome::Waiting(error),
        },
        Pick::Busy(agent) => Outcome::Waiting(format!("{} is {}", job.target, agent.agent_status)),
        Pick::Missing => Outcome::Waiting(format!("no agent matches {}", job.target)),
        Pick::Ambiguous(count) => Outcome::Waiting(format!("{count} agents match {}, name one instead", job.target)),
    }
}

#[derive(Default)]
pub struct Scheduler {
    due: HashMap<String, SystemTime>,
    waiting: HashMap<String, String>,
}

impl Scheduler {
    pub fn tick(&mut self, now: SystemTime, config: &Config, herdr: &impl Herdr) -> Vec<String> {
        self.due.retain(|name, _| config.jobs.iter().any(|job| &job.name == name));
        config.jobs.iter().filter_map(|job| self.run(job, now, herdr)).collect()
    }

    fn run(&mut self, job: &Job, now: SystemTime, herdr: &impl Herdr) -> Option<String> {
        let due = *self.due.entry(job.name.clone()).or_insert(now + job.every);
        if now < due {
            return None;
        }
        match attempt(job, herdr) {
            Outcome::Sent(pane) => {
                self.due.insert(job.name.clone(), now + job.every);
                self.waiting.remove(&job.name);
                Some(format!("{}: sent to {pane}", job.name))
            }
            Outcome::Waiting(reason) => {
                let line = format!("{}: waiting, {reason}", job.name);
                let repeated = self.waiting.insert(job.name.clone(), reason.clone()).as_ref() == Some(&reason);
                (!repeated).then_some(line)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::Session;
    use std::cell::RefCell;
    use std::time::Duration;

    fn agent(pane: &str, status: &str, cwd: &str) -> Agent {
        Agent {
            pane_id: pane.into(),
            agent_status: status.into(),
            name: None,
            cwd: Some(cwd.into()),
            agent_session: None,
        }
    }

    struct FakeHerdr {
        agents: RefCell<Result<Vec<Agent>, String>>,
        prompts: RefCell<Vec<(String, String)>>,
        refuse: Option<String>,
    }

    impl FakeHerdr {
        fn with(agents: Vec<Agent>) -> Self {
            FakeHerdr { agents: RefCell::new(Ok(agents)), prompts: RefCell::new(vec![]), refuse: None }
        }
        fn set(&self, agents: Vec<Agent>) {
            *self.agents.borrow_mut() = Ok(agents);
        }
    }

    impl Herdr for FakeHerdr {
        fn agents(&self) -> Result<Vec<Agent>, String> {
            self.agents.borrow().clone()
        }
        fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
            if let Some(error) = &self.refuse {
                return Err(error.clone());
            }
            self.prompts.borrow_mut().push((pane.into(), text.into()));
            Ok(())
        }
    }

    fn watch() -> Job {
        Job {
            name: "encode-watch".into(),
            every: Duration::from_secs(30 * 60),
            target: Target::Cwd("/code/homelab".into()),
            message: "Is encoding running?".into(),
        }
    }

    fn config() -> Config {
        Config { jobs: vec![watch()], watches: vec![] }
    }

    fn at(minutes: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(minutes * 60)
    }

    #[test]
    fn picks_a_target_by_session_name_or_folder() {
        let named = Agent { name: Some("homelab".into()), ..agent("w1:p2", "idle", "/elsewhere") };
        let session = Agent { agent_session: Some(Session { value: "abc".into() }), ..agent("w1:p3", "idle", "/x") };
        let agents = vec![agent("w1:p1", "idle", "/code/homelab"), named.clone(), session.clone()];

        assert_eq!(pick(&Target::Cwd("/code/homelab".into()), &agents), Pick::Ready(&agents[0]));
        assert_eq!(pick(&Target::Agent("homelab".into()), &agents), Pick::Ready(&named));
        assert_eq!(pick(&Target::Session("abc".into()), &agents), Pick::Ready(&session));
        assert_eq!(pick(&Target::Session("zzz".into()), &agents), Pick::Missing);
    }

    #[test]
    fn an_agent_that_is_working_or_blocked_is_busy_and_done_counts_as_ready() {
        let working = vec![agent("w1:p1", "working", "/code/homelab")];
        let blocked = vec![agent("w1:p1", "blocked", "/code/homelab")];
        let done = vec![agent("w1:p1", "done", "/code/homelab")];
        let target = Target::Cwd("/code/homelab".into());

        assert_eq!(pick(&target, &working), Pick::Busy(&working[0]));
        assert_eq!(pick(&target, &blocked), Pick::Busy(&blocked[0]));
        assert_eq!(pick(&target, &done), Pick::Ready(&done[0]));
    }

    #[test]
    fn two_agents_in_one_folder_are_ambiguous() {
        let agents = vec![agent("w1:p1", "idle", "/code/homelab"), agent("w2:p1", "idle", "/code/homelab")];
        assert_eq!(pick(&Target::Cwd("/code/homelab".into()), &agents), Pick::Ambiguous(2));
    }

    #[test]
    fn the_message_says_it_came_from_nudge_and_how_often() {
        assert_eq!(text(&watch()), "[nudge: encode-watch, every 30m] Is encoding running?");
    }

    #[test]
    fn sends_once_per_interval_starting_one_interval_after_start() {
        let herdr = FakeHerdr::with(vec![agent("w1:pH", "idle", "/code/homelab")]);
        let mut scheduler = Scheduler::default();

        assert!(scheduler.tick(at(0), &config(), &herdr).is_empty());
        assert!(scheduler.tick(at(29), &config(), &herdr).is_empty());
        assert_eq!(scheduler.tick(at(30), &config(), &herdr), vec!["encode-watch: sent to w1:pH"]);
        assert!(scheduler.tick(at(31), &config(), &herdr).is_empty());
        assert_eq!(scheduler.tick(at(60), &config(), &herdr), vec!["encode-watch: sent to w1:pH"]);
        assert_eq!(
            *herdr.prompts.borrow(),
            vec![("w1:pH".to_string(), text(&watch())), ("w1:pH".to_string(), text(&watch()))]
        );
    }

    #[test]
    fn waits_for_a_busy_agent_instead_of_interrupting_it_and_logs_the_wait_once() {
        let herdr = FakeHerdr::with(vec![agent("w1:pH", "working", "/code/homelab")]);
        let mut scheduler = Scheduler::default();
        scheduler.tick(at(0), &config(), &herdr);

        assert_eq!(
            scheduler.tick(at(30), &config(), &herdr),
            vec!["encode-watch: waiting, the agent in /code/homelab is working"]
        );
        assert!(scheduler.tick(at(31), &config(), &herdr).is_empty());
        herdr.set(vec![agent("w1:pH", "idle", "/code/homelab")]);
        assert_eq!(scheduler.tick(at(32), &config(), &herdr), vec!["encode-watch: sent to w1:pH"]);
        assert!(scheduler.tick(at(61), &config(), &herdr).is_empty());
        assert_eq!(scheduler.tick(at(62), &config(), &herdr), vec!["encode-watch: sent to w1:pH"]);
    }

    #[test]
    fn says_why_when_there_is_no_agent_to_send_to_or_herdr_fails() {
        let mut scheduler = Scheduler::default();
        let nobody = FakeHerdr::with(vec![]);
        scheduler.tick(at(0), &config(), &nobody);
        assert_eq!(
            scheduler.tick(at(30), &config(), &nobody),
            vec!["encode-watch: waiting, no agent matches the agent in /code/homelab"]
        );

        let down = FakeHerdr { agents: RefCell::new(Err("herdr is not running".into())), ..FakeHerdr::with(vec![]) };
        assert_eq!(scheduler.tick(at(31), &config(), &down), vec!["encode-watch: waiting, herdr is not running"]);

        let refusing = FakeHerdr {
            refuse: Some("agent_blocked".into()),
            ..FakeHerdr::with(vec![agent("w1:pH", "idle", "/code/homelab")])
        };
        assert_eq!(scheduler.tick(at(32), &config(), &refusing), vec!["encode-watch: waiting, agent_blocked"]);
    }

    #[test]
    fn two_matching_agents_are_reported_rather_than_guessed_between() {
        let herdr =
            FakeHerdr::with(vec![agent("w1:p1", "idle", "/code/homelab"), agent("w2:p1", "idle", "/code/homelab")]);
        let mut scheduler = Scheduler::default();
        scheduler.tick(at(0), &config(), &herdr);
        assert_eq!(
            scheduler.tick(at(30), &config(), &herdr),
            vec!["encode-watch: waiting, 2 agents match the agent in /code/homelab, name one instead"]
        );
        assert!(herdr.prompts.borrow().is_empty());
    }

    #[test]
    fn a_job_removed_from_the_config_and_added_back_starts_a_fresh_interval() {
        let herdr = FakeHerdr::with(vec![agent("w1:pH", "idle", "/code/homelab")]);
        let mut scheduler = Scheduler::default();
        scheduler.tick(at(0), &config(), &herdr);
        scheduler.tick(at(10), &Config::default(), &herdr);

        assert!(scheduler.tick(at(30), &config(), &herdr).is_empty());
        assert_eq!(scheduler.tick(at(60), &config(), &herdr), vec!["encode-watch: sent to w1:pH"]);
    }
}
