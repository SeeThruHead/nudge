use crate::config::{Config, Job, Schedule, Target, Via};
use crate::host::{Agent, Host};
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

pub trait Direct {
    fn send(&self, job: &Job, text: &str) -> Result<String, String>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Sent(String),
    Waiting(String),
}

pub fn attempt(job: &Job, host: &dyn Host, direct: &dyn Direct) -> Outcome {
    if job.via == Via::Claude {
        return match direct.send(job, &text(job)) {
            Ok(sent) => Outcome::Sent(sent),
            Err(reason) => Outcome::Waiting(reason),
        };
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
        Pick::Ambiguous(count) => Outcome::Waiting(format!("{count} agents match {}, name one instead", job.target)),
    }
}

#[derive(Default)]
pub struct Scheduler {
    due: HashMap<String, SystemTime>,
    waiting: HashMap<String, String>,
}

impl Scheduler {
    pub fn tick(&mut self, now: SystemTime, config: &Config, host: &dyn Host, direct: &dyn Direct) -> Vec<String> {
        self.due.retain(|name, _| config.jobs.iter().any(|job| &job.name == name));
        config.jobs.iter().filter_map(|job| self.run(job, now, host, direct)).collect()
    }

    fn run(&mut self, job: &Job, now: SystemTime, host: &dyn Host, direct: &dyn Direct) -> Option<String> {
        let due = *self.due.entry(job.name.clone()).or_insert_with(|| next_after(&job.schedule, now));
        if now < due {
            return None;
        }
        match attempt(job, host, direct) {
            Outcome::Sent(pane) => {
                self.due.insert(job.name.clone(), next_after(&job.schedule, now));
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
    use crate::host::Session;
    use std::cell::RefCell;
    use std::time::Duration;

    fn agent(pane: &str, status: &str, cwd: &str) -> Agent {
        Agent {
            pane_id: pane.into(),
            agent_status: status.into(),
            name: None,
            cwd: Some(cwd.into()),
            agent_session: None,
            terminal_title: None,
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

    fn watch() -> Job {
        Job {
            name: "encode-watch".into(),
            schedule: Schedule::Every(Duration::from_secs(30 * 60)),
            target: Target::Cwd("/code/homelab".into()),
            via: Via::Host,
            args: vec![],
            message: "Is encoding running?".into(),
        }
    }

    fn config() -> Config {
        Config { jobs: vec![watch()], ..Config::default() }
    }

    struct NoDirect;
    impl Direct for NoDirect {
        fn send(&self, _: &Job, _: &str) -> Result<String, String> {
            Err("not used".into())
        }
    }

    struct FakeClaude(RefCell<Vec<String>>, bool);
    impl Direct for FakeClaude {
        fn send(&self, job: &Job, text: &str) -> Result<String, String> {
            if self.1 {
                return Err(format!("{}: the last claude run is still going", job.name));
            }
            self.0.borrow_mut().push(text.into());
            Ok("claude (pid 42)".into())
        }
    }

    fn at(minutes: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(minutes * 60)
    }

    impl Scheduler {
        fn tick_with(&mut self, now: SystemTime, config: &Config, host: &dyn Host) -> Vec<String> {
            self.tick(now, config, host, &NoDirect)
        }
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
    fn a_claude_job_goes_straight_to_claude_code_without_a_host_and_waits_while_the_last_run_goes() {
        let claude_job = Job { via: Via::Claude, ..watch() };
        let config = Config { jobs: vec![claude_job.clone()], ..Config::default() };
        let nobody = FakeHerdr { agents: RefCell::new(Err("no host".into())), ..FakeHerdr::with(vec![]) };
        let claude = FakeClaude(RefCell::new(vec![]), false);
        let busy = FakeClaude(RefCell::new(vec![]), true);
        let mut scheduler = Scheduler::default();
        scheduler.tick(at(0), &config, &nobody, &claude);

        assert_eq!(scheduler.tick(at(30), &config, &nobody, &claude), vec!["encode-watch: sent to claude (pid 42)"]);
        assert_eq!(*claude.0.borrow(), vec![text(&claude_job)]);
        assert_eq!(
            scheduler.tick(at(60), &config, &nobody, &busy),
            vec!["encode-watch: waiting, encode-watch: the last claude run is still going"]
        );
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

        assert!(scheduler.tick_with(at(0), &config(), &herdr).is_empty());
        assert!(scheduler.tick_with(at(29), &config(), &herdr).is_empty());
        assert_eq!(scheduler.tick_with(at(30), &config(), &herdr), vec!["encode-watch: sent to w1:pH"]);
        assert!(scheduler.tick_with(at(31), &config(), &herdr).is_empty());
        assert_eq!(scheduler.tick_with(at(60), &config(), &herdr), vec!["encode-watch: sent to w1:pH"]);
        assert_eq!(
            *herdr.prompts.borrow(),
            vec![("w1:pH".to_string(), text(&watch())), ("w1:pH".to_string(), text(&watch()))]
        );
    }

    #[test]
    fn waits_for_a_busy_agent_instead_of_interrupting_it_and_logs_the_wait_once() {
        let herdr = FakeHerdr::with(vec![agent("w1:pH", "working", "/code/homelab")]);
        let mut scheduler = Scheduler::default();
        scheduler.tick_with(at(0), &config(), &herdr);

        assert_eq!(
            scheduler.tick_with(at(30), &config(), &herdr),
            vec!["encode-watch: waiting, the agent in /code/homelab is working"]
        );
        assert!(scheduler.tick_with(at(31), &config(), &herdr).is_empty());
        herdr.set(vec![agent("w1:pH", "idle", "/code/homelab")]);
        assert_eq!(scheduler.tick_with(at(32), &config(), &herdr), vec!["encode-watch: sent to w1:pH"]);
        assert!(scheduler.tick_with(at(61), &config(), &herdr).is_empty());
        assert_eq!(scheduler.tick_with(at(62), &config(), &herdr), vec!["encode-watch: sent to w1:pH"]);
    }

    #[test]
    fn says_why_when_there_is_no_agent_to_send_to_or_herdr_fails() {
        let mut scheduler = Scheduler::default();
        let nobody = FakeHerdr::with(vec![]);
        scheduler.tick_with(at(0), &config(), &nobody);
        assert_eq!(
            scheduler.tick_with(at(30), &config(), &nobody),
            vec!["encode-watch: waiting, no agent matches the agent in /code/homelab"]
        );

        let down = FakeHerdr { agents: RefCell::new(Err("herdr is not running".into())), ..FakeHerdr::with(vec![]) };
        assert_eq!(scheduler.tick_with(at(31), &config(), &down), vec!["encode-watch: waiting, herdr is not running"]);

        let refusing = FakeHerdr {
            refuse: Some("agent_blocked".into()),
            ..FakeHerdr::with(vec![agent("w1:pH", "idle", "/code/homelab")])
        };
        assert_eq!(scheduler.tick_with(at(32), &config(), &refusing), vec!["encode-watch: waiting, agent_blocked"]);
    }

    #[test]
    fn two_matching_agents_are_reported_rather_than_guessed_between() {
        let herdr =
            FakeHerdr::with(vec![agent("w1:p1", "idle", "/code/homelab"), agent("w2:p1", "idle", "/code/homelab")]);
        let mut scheduler = Scheduler::default();
        scheduler.tick_with(at(0), &config(), &herdr);
        assert_eq!(
            scheduler.tick_with(at(30), &config(), &herdr),
            vec!["encode-watch: waiting, 2 agents match the agent in /code/homelab, name one instead"]
        );
        assert!(herdr.prompts.borrow().is_empty());
    }

    #[test]
    fn a_job_removed_from_the_config_and_added_back_starts_a_fresh_interval() {
        let herdr = FakeHerdr::with(vec![agent("w1:pH", "idle", "/code/homelab")]);
        let mut scheduler = Scheduler::default();
        scheduler.tick_with(at(0), &config(), &herdr);
        scheduler.tick_with(at(10), &Config::default(), &herdr);

        assert!(scheduler.tick_with(at(30), &config(), &herdr).is_empty());
        assert_eq!(scheduler.tick_with(at(60), &config(), &herdr), vec!["encode-watch: sent to w1:pH"]);
    }
}
