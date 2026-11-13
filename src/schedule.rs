use crate::config::{Config, Job, Schedule, Target};
use crate::host::{Agent, Host};
use crate::reach::{self, Directory, Resolved};
use chrono::{DateTime, Days, Local, NaiveTime, TimeZone};
use std::collections::HashMap;
use std::time::SystemTime;

#[derive(Debug, PartialEq, Eq)]
pub enum Pick<'a> {
    Ready(&'a Agent),
    Busy(&'a Agent),
    Missing,
    Ambiguous(usize),
}

pub fn is_target(target: &Target, agent: &Agent) -> bool {
    match target {
        Target::Address(_) => false,
        Target::Session(id) => agent.agent_session.as_ref().is_some_and(|s| &s.value == id),
        Target::Agent(name) => agent.name.as_ref() == Some(name),
        Target::Cwd(cwd) => agent.cwd.as_ref() == Some(cwd),
    }
}

fn is_ready(agent: &Agent) -> bool {
    agent.agent_status == "idle" || agent.agent_status == "done" || agent.agent_status == "unknown"
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
    format!("[nudge: {}, {}] {}", job.name, job.schedule, job.message)
}

fn daily(times: &[(u32, u32)], after: DateTime<Local>) -> Option<DateTime<Local>> {
    let days = [after.date_naive(), after.date_naive().checked_add_days(Days::new(1))?];
    days.iter()
        .flat_map(|day| {
            times
                .iter()
                .filter_map(move |(hour, minute)| Some(day.and_time(NaiveTime::from_hms_opt(*hour, *minute, 0)?)))
        })
        .filter_map(|moment| Local.from_local_datetime(&moment).earliest())
        .filter(|moment| *moment > after)
        .min()
}

pub fn next_after(schedule: &Schedule, now: SystemTime) -> SystemTime {
    let local: DateTime<Local> = now.into();
    let next = match schedule {
        Schedule::Every(every) => return now + *every,
        Schedule::Cron(pattern) => {
            croner::Cron::new(pattern).parse().ok().and_then(|cron| cron.find_next_occurrence(&local, false).ok())
        }
        Schedule::At(times) => daily(times, local),
    };
    next.map(SystemTime::from).unwrap_or(now + std::time::Duration::from_secs(24 * 60 * 60))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Sent(String),
    Waiting(String),
    Failed(String),
}

fn attempt_address(job: &Job, address: &str, directory: &dyn Directory) -> Outcome {
    match directory.resolve(address) {
        Resolved::Agent(found) if reach::is_ready(&found) => match directory.send(address, &text(job)) {
            Ok(()) => Outcome::Sent(format!("{} ({}:{})", found.address, found.host, found.id)),
            Err(error) => Outcome::Waiting(error),
        },
        Resolved::Agent(found) => Outcome::Waiting(format!("{} is {}", found.address, found.status)),
        Resolved::Refused(refusal) => Outcome::Failed(refusal.message),
        Resolved::Unavailable(reason) => Outcome::Failed(reason),
    }
}

pub fn attempt(job: &Job, host: &dyn Host, directory: &dyn Directory) -> Outcome {
    if let Target::Address(address) = &job.target {
        return attempt_address(job, address, directory);
    }
    let agents = match host.agents() {
        Ok(agents) => agents,
        Err(error) => return Outcome::Waiting(error),
    };
    match pick(&job.target, &agents) {
        Pick::Ready(agent) => match host.prompt(&agent.pane_id, &text(job)) {
            Ok(()) => Outcome::Sent(agent.pane_id.clone()),
            Err(error) => Outcome::Waiting(error),
        },
        Pick::Busy(agent) => Outcome::Waiting(format!("{} is {}", job.target, agent.agent_status)),
        Pick::Missing => Outcome::Waiting(format!("no agent matches {}", job.target)),
        Pick::Ambiguous(count) => Outcome::Failed(format!(
            "{count} agents match {}, so none was picked; target one address instead",
            job.target
        )),
    }
}

const RETRY_FAILED: std::time::Duration = std::time::Duration::from_secs(60);
const RETRY_ADDRESS: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Default)]
pub struct Scheduler {
    due: HashMap<String, SystemTime>,
    waiting: HashMap<String, String>,
    alerts: Vec<String>,
}

impl Scheduler {
    pub fn tick(
        &mut self,
        now: SystemTime,
        config: &Config,
        host: &dyn Host,
        directory: &dyn Directory,
    ) -> Vec<String> {
        self.due.retain(|name, _| config.jobs.iter().any(|job| &job.name == name));
        config.jobs.iter().filter_map(|job| self.run(job, now, host, directory)).collect()
    }

    pub fn take_alerts(&mut self) -> Vec<String> {
        std::mem::take(&mut self.alerts)
    }

    fn first_time(&mut self, job: &Job, reason: &str) -> bool {
        self.waiting.insert(job.name.clone(), reason.to_string()).as_deref() != Some(reason)
    }

    fn run(&mut self, job: &Job, now: SystemTime, host: &dyn Host, directory: &dyn Directory) -> Option<String> {
        let due = *self.due.entry(job.name.clone()).or_insert_with(|| next_after(&job.schedule, now));
        if now < due {
            return None;
        }
        match attempt(job, host, directory) {
            Outcome::Sent(place) => {
                self.due.insert(job.name.clone(), next_after(&job.schedule, now));
                self.waiting.remove(&job.name);
                Some(format!("{}: sent to {place}", job.name))
            }
            Outcome::Waiting(reason) => {
                if matches!(job.target, Target::Address(_)) {
                    self.due.insert(job.name.clone(), now + RETRY_ADDRESS);
                }
                let line = format!("{}: waiting, {reason}", job.name);
                self.first_time(job, &reason).then_some(line)
            }
            Outcome::Failed(reason) => {
                self.due.insert(job.name.clone(), now + RETRY_FAILED);
                let line = format!("{}: not sent, {reason}", job.name);
                if !self.first_time(job, &reason) {
                    return None;
                }
                self.alerts.push(line.clone());
                Some(line)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Session;
    use crate::reach::{Found, Refusal};
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

    impl Host for FakeHerdr {
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

    struct FakeReach {
        answer: RefCell<Resolved>,
        sent: RefCell<Vec<(String, String)>>,
    }

    impl FakeReach {
        fn answering(answer: Resolved) -> Self {
            FakeReach { answer: RefCell::new(answer), sent: RefCell::new(vec![]) }
        }
        fn none() -> Self {
            FakeReach::answering(Resolved::Unavailable("no reach in this test".into()))
        }
    }

    impl Directory for FakeReach {
        fn resolve(&self, _address: &str) -> Resolved {
            self.answer.borrow().clone()
        }
        fn send(&self, address: &str, text: &str) -> Result<(), String> {
            self.sent.borrow_mut().push((address.into(), text.into()));
            Ok(())
        }
    }

    fn found(state: &str, status: &str) -> Resolved {
        Resolved::Agent(Found {
            address: "sthdev/homelab/homelab".into(),
            state: state.into(),
            status: status.into(),
            host: "herdr-app".into(),
            id: "t4k".into(),
        })
    }

    fn addressed() -> Config {
        Config {
            jobs: vec![Job { target: Target::Address("sthdev/homelab/homelab".into()), ..watch() }],
            ..Config::default()
        }
    }

    fn watch() -> Job {
        Job {
            name: "encode-watch".into(),
            schedule: Schedule::Every(Duration::from_secs(30 * 60)),
            target: Target::Cwd("/code/homelab".into()),
            message: "Is encoding running?".into(),
        }
    }

    fn config() -> Config {
        Config { jobs: vec![watch()], ..Config::default() }
    }

    fn at(minutes: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(minutes * 60)
    }

    fn local(text: &str) -> SystemTime {
        let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
        Local.from_local_datetime(&naive).earliest().unwrap().into()
    }

    #[test]
    fn a_cron_or_daily_job_is_due_at_its_next_local_time() {
        let monday_night = local("2026-11-02 22:00");
        let weekdays = Schedule::Cron("0 9 * * 1-5".into());
        let twice = Schedule::At(vec![(9, 0), (17, 30)]);

        assert_eq!(next_after(&weekdays, monday_night), local("2026-11-03 09:00"));
        assert_eq!(next_after(&weekdays, local("2026-11-06 10:00")), local("2026-11-09 09:00"));
        assert_eq!(next_after(&twice, local("2026-11-03 10:00")), local("2026-11-03 17:30"));
        assert_eq!(next_after(&twice, monday_night), local("2026-11-03 09:00"));
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

        assert!(scheduler.tick(at(0), &config(), &herdr, &FakeReach::none()).is_empty());
        assert!(scheduler.tick(at(29), &config(), &herdr, &FakeReach::none()).is_empty());
        assert_eq!(scheduler.tick(at(30), &config(), &herdr, &FakeReach::none()), vec!["encode-watch: sent to w1:pH"]);
        assert!(scheduler.tick(at(31), &config(), &herdr, &FakeReach::none()).is_empty());
        assert_eq!(scheduler.tick(at(60), &config(), &herdr, &FakeReach::none()), vec!["encode-watch: sent to w1:pH"]);
        assert_eq!(
            *herdr.prompts.borrow(),
            vec![("w1:pH".to_string(), text(&watch())), ("w1:pH".to_string(), text(&watch()))]
        );
    }

    #[test]
    fn waits_for_a_busy_agent_instead_of_interrupting_it_and_logs_the_wait_once() {
        let herdr = FakeHerdr::with(vec![agent("w1:pH", "working", "/code/homelab")]);
        let mut scheduler = Scheduler::default();
        scheduler.tick(at(0), &config(), &herdr, &FakeReach::none());

        assert_eq!(
            scheduler.tick(at(30), &config(), &herdr, &FakeReach::none()),
            vec!["encode-watch: waiting, the agent in /code/homelab is working"]
        );
        assert!(scheduler.tick(at(31), &config(), &herdr, &FakeReach::none()).is_empty());
        herdr.set(vec![agent("w1:pH", "idle", "/code/homelab")]);
        assert_eq!(scheduler.tick(at(32), &config(), &herdr, &FakeReach::none()), vec!["encode-watch: sent to w1:pH"]);
        assert!(scheduler.tick(at(61), &config(), &herdr, &FakeReach::none()).is_empty());
        assert_eq!(scheduler.tick(at(62), &config(), &herdr, &FakeReach::none()), vec!["encode-watch: sent to w1:pH"]);
    }

    #[test]
    fn says_why_when_there_is_no_agent_to_send_to_or_herdr_fails() {
        let mut scheduler = Scheduler::default();
        let nobody = FakeHerdr::with(vec![]);
        scheduler.tick(at(0), &config(), &nobody, &FakeReach::none());
        assert_eq!(
            scheduler.tick(at(30), &config(), &nobody, &FakeReach::none()),
            vec!["encode-watch: waiting, no agent matches the agent in /code/homelab"]
        );

        let down = FakeHerdr { agents: RefCell::new(Err("herdr is not running".into())), ..FakeHerdr::with(vec![]) };
        assert_eq!(
            scheduler.tick(at(31), &config(), &down, &FakeReach::none()),
            vec!["encode-watch: waiting, herdr is not running"]
        );

        let refusing = FakeHerdr {
            refuse: Some("agent_blocked".into()),
            ..FakeHerdr::with(vec![agent("w1:pH", "idle", "/code/homelab")])
        };
        assert_eq!(
            scheduler.tick(at(32), &config(), &refusing, &FakeReach::none()),
            vec!["encode-watch: waiting, agent_blocked"]
        );
    }

    #[test]
    fn two_matching_agents_are_reported_rather_than_guessed_between() {
        let herdr =
            FakeHerdr::with(vec![agent("w1:p1", "idle", "/code/homelab"), agent("w2:p1", "idle", "/code/homelab")]);
        let mut scheduler = Scheduler::default();
        scheduler.tick(at(0), &config(), &herdr, &FakeReach::none());
        assert_eq!(
            scheduler.tick(at(30), &config(), &herdr, &FakeReach::none()),
            vec![
                "encode-watch: not sent, 2 agents match the agent in /code/homelab, so none was picked; target one address instead"
            ]
        );
        assert!(herdr.prompts.borrow().is_empty());
        assert_eq!(scheduler.take_alerts().len(), 1);
    }

    #[test]
    fn an_address_target_is_sent_through_reach_and_never_through_the_host() {
        let herdr = FakeHerdr::with(vec![agent("t9", "idle", "/code/homelab")]);
        let reach = FakeReach::answering(found("live", "idle"));
        let mut scheduler = Scheduler::default();
        scheduler.tick(at(0), &addressed(), &herdr, &reach);

        assert_eq!(
            scheduler.tick(at(30), &addressed(), &herdr, &reach),
            vec!["encode-watch: sent to sthdev/homelab/homelab (herdr-app:t4k)"]
        );
        assert_eq!(reach.sent.borrow()[0].0, "sthdev/homelab/homelab");
        assert!(herdr.prompts.borrow().is_empty());
    }

    #[test]
    fn an_asleep_address_is_sent_to_so_it_wakes_and_a_working_one_waits_half_a_minute() {
        let herdr = FakeHerdr::with(vec![]);
        let reach = FakeReach::answering(found("live", "working"));
        let mut scheduler = Scheduler::default();
        scheduler.tick(at(0), &addressed(), &herdr, &reach);

        assert_eq!(
            scheduler.tick(at(30), &addressed(), &herdr, &reach),
            vec!["encode-watch: waiting, sthdev/homelab/homelab is working"]
        );
        *reach.answer.borrow_mut() = found("asleep", "unknown");
        let soon = at(30) + Duration::from_secs(10);
        assert!(scheduler.tick(soon, &addressed(), &herdr, &reach).is_empty());
        let later = at(30) + Duration::from_secs(30);
        assert_eq!(
            scheduler.tick(later, &addressed(), &herdr, &reach),
            vec!["encode-watch: sent to sthdev/homelab/homelab (herdr-app:t4k)"]
        );
    }

    #[test]
    fn an_address_reach_refuses_fails_loudly_once_and_nothing_else_gets_the_message() {
        let herdr = FakeHerdr::with(vec![agent("t9", "idle", "/code/homelab")]);
        let refusal = Refusal { tag: "NotFound".into(), message: "no agent is at sthdev/homelab/homelab".into() };
        let reach = FakeReach::answering(Resolved::Refused(refusal));
        let mut scheduler = Scheduler::default();
        scheduler.tick(at(0), &addressed(), &herdr, &reach);

        assert_eq!(
            scheduler.tick(at(30), &addressed(), &herdr, &reach),
            vec!["encode-watch: not sent, no agent is at sthdev/homelab/homelab"]
        );
        assert_eq!(scheduler.take_alerts(), vec!["encode-watch: not sent, no agent is at sthdev/homelab/homelab"]);
        assert!(scheduler.tick(at(32), &addressed(), &herdr, &reach).is_empty());
        assert!(scheduler.take_alerts().is_empty());
        assert!(herdr.prompts.borrow().is_empty());
        assert!(reach.sent.borrow().is_empty());
    }

    #[test]
    fn a_job_removed_from_the_config_and_added_back_starts_a_fresh_interval() {
        let herdr = FakeHerdr::with(vec![agent("w1:pH", "idle", "/code/homelab")]);
        let mut scheduler = Scheduler::default();
        scheduler.tick(at(0), &config(), &herdr, &FakeReach::none());
        scheduler.tick(at(10), &Config::default(), &herdr, &FakeReach::none());

        assert!(scheduler.tick(at(30), &config(), &herdr, &FakeReach::none()).is_empty());
        assert_eq!(scheduler.tick(at(60), &config(), &herdr, &FakeReach::none()), vec!["encode-watch: sent to w1:pH"]);
    }
}
