use crate::config::{Config, Watch};
use crate::herdr::Herdr;
use crate::schedule::{Pick, pick};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub name: String,
    pub pid: u32,
    pub cpu: f32,
    pub memory_mb: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub host: String,
    pub cpu: f32,
    pub memory: f32,
    pub busiest: Vec<Usage>,
}

pub fn over(watch: &Watch, sample: &Sample) -> Option<String> {
    let cpu = watch
        .cpu
        .filter(|limit| sample.cpu > *limit)
        .map(|limit| format!("CPU {:.0}% (limit {limit:.0}%)", sample.cpu));
    let memory = watch
        .memory
        .filter(|limit| sample.memory > *limit)
        .map(|limit| format!("memory {:.0}% (limit {limit:.0}%)", sample.memory));
    let parts: Vec<String> = cpu.into_iter().chain(memory).collect();
    (!parts.is_empty()).then(|| parts.join(" and "))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Tripped(String),
    Cleared,
}

#[derive(Debug, Default)]
pub struct Breaker {
    over_since: Option<SystemTime>,
    under_since: Option<SystemTime>,
    tripped: bool,
}

fn lasted(since: SystemTime, now: SystemTime, needed: Duration) -> bool {
    now.duration_since(since).is_ok_and(|elapsed| elapsed >= needed)
}

impl Breaker {
    pub fn observe(&mut self, watch: &Watch, sample: &Sample, now: SystemTime) -> Option<Change> {
        match over(watch, sample) {
            Some(reason) => {
                self.under_since = None;
                let since = *self.over_since.get_or_insert(now);
                if self.tripped || !lasted(since, now, watch.over_for) {
                    return None;
                }
                self.tripped = true;
                Some(Change::Tripped(reason))
            }
            None => {
                self.over_since = None;
                if !self.tripped {
                    return None;
                }
                let since = *self.under_since.get_or_insert(now);
                if !lasted(since, now, watch.clear_for) {
                    return None;
                }
                self.tripped = false;
                self.under_since = None;
                Some(Change::Cleared)
            }
        }
    }
}

fn busiest(sample: &Sample, count: usize) -> String {
    sample
        .busiest
        .iter()
        .take(count)
        .map(|usage| format!("{} (pid {}) {:.0}% CPU, {} MB", usage.name, usage.pid, usage.cpu, usage.memory_mb))
        .collect::<Vec<_>>()
        .join("; ")
}

fn lead(watch: &Watch, sample: &Sample, reason: &str) -> String {
    format!(
        "[nudge: {}] {} is overloaded: {reason} for {}. Busiest: {}.",
        watch.name,
        sample.host,
        humantime::format_duration(watch.over_for),
        busiest(sample, 5)
    )
}

pub fn broadcast_text(watch: &Watch, sample: &Sample, reason: &str) -> String {
    let who = watch.investigator.as_ref().map(|target| format!(" {target} is finding out why.")).unwrap_or_default();
    format!(
        "{} Pause heavy work you started (builds, encodes, test runs, containers) and start nothing heavy until nudge says {} has cleared.{who}",
        lead(watch, sample, reason),
        watch.name
    )
}

pub fn investigator_text(watch: &Watch, sample: &Sample, reason: &str) -> String {
    let extra = if watch.message.is_empty() { String::new() } else { format!(" {}", watch.message) };
    format!(
        "{} Every agent on this machine was told to pause heavy work. Find out what is using the machine and why, then stop or fix it so Shane can keep using the computer, and say in a few lines what it was.{extra}",
        lead(watch, sample, reason)
    )
}

pub fn clear_text(watch: &Watch, sample: &Sample) -> String {
    format!(
        "[nudge: {}] cleared: {} has been back under its limits for {} (CPU {:.0}%, memory {:.0}%). Heavy work can resume.",
        watch.name,
        sample.host,
        humantime::format_duration(watch.clear_for),
        sample.cpu,
        sample.memory
    )
}

#[derive(Debug, Clone, PartialEq)]
struct Pending {
    pane: String,
    text: String,
    since: SystemTime,
}

const GIVE_UP: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub struct Watcher {
    breakers: HashMap<String, Breaker>,
    outbox: Vec<Pending>,
}

impl Watcher {
    pub fn tick(&mut self, now: SystemTime, config: &Config, sample: &Sample, herdr: &impl Herdr) -> Vec<String> {
        self.breakers.retain(|name, _| config.watches.iter().any(|watch| &watch.name == name));
        let changes: Vec<String> = config
            .watches
            .iter()
            .filter_map(|watch| {
                let change = self.breakers.entry(watch.name.clone()).or_default().observe(watch, sample, now)?;
                Some(self.queue(watch, sample, change, now, herdr))
            })
            .flatten()
            .collect();
        changes.into_iter().chain(self.deliver(now, herdr)).collect()
    }

    fn queue(
        &mut self,
        watch: &Watch,
        sample: &Sample,
        change: Change,
        now: SystemTime,
        herdr: &impl Herdr,
    ) -> Vec<String> {
        let agents = match herdr.agents() {
            Ok(agents) => agents,
            Err(error) => {
                return vec![format!("{}: {change:?}, but could not list agents: {error}", watch.name)];
            }
        };
        let investigator = watch.investigator.as_ref().map(|target| (target, pick(target, &agents)));
        let investigator_pane = investigator.as_ref().and_then(|(_, found)| match found {
            Pick::Ready(agent) | Pick::Busy(agent) => Some(agent.pane_id.clone()),
            Pick::Missing | Pick::Ambiguous(_) => None,
        });
        let (everyone, lead) = match &change {
            Change::Tripped(reason) => (
                broadcast_text(watch, sample, reason),
                investigator_pane.clone().map(|pane| (pane, investigator_text(watch, sample, reason))),
            ),
            Change::Cleared => {
                (clear_text(watch, sample), investigator_pane.clone().map(|pane| (pane, clear_text(watch, sample))))
            }
        };
        let others: Vec<Pending> = agents
            .iter()
            .filter(|_| watch.broadcast)
            .filter(|agent| Some(&agent.pane_id) != investigator_pane.as_ref())
            .map(|agent| Pending { pane: agent.pane_id.clone(), text: everyone.clone(), since: now })
            .collect();
        let lead_count = usize::from(lead.is_some());
        let missing = match (&investigator, &investigator_pane) {
            (Some((target, Pick::Missing)), None) => {
                Some(format!("{}: no agent matches {target} to investigate", watch.name))
            }
            (Some((target, Pick::Ambiguous(count))), None) => {
                Some(format!("{}: {count} agents match {target}, name one to investigate", watch.name))
            }
            _ => None,
        };
        let summary = format!(
            "{}: {}, telling {} agents{}",
            watch.name,
            match &change {
                Change::Tripped(reason) => format!("tripped, {reason}"),
                Change::Cleared => "cleared".into(),
            },
            others.len() + lead_count,
            investigator_pane.as_ref().map(|pane| format!(", {pane} investigates")).unwrap_or_default()
        );
        self.outbox.extend(lead.map(|(pane, text)| Pending { pane, text, since: now }));
        self.outbox.extend(others);
        std::iter::once(summary).chain(missing).collect()
    }

    fn deliver(&mut self, now: SystemTime, herdr: &impl Herdr) -> Vec<String> {
        let (log, keep): (Vec<Option<String>>, Vec<Option<Pending>>) = std::mem::take(&mut self.outbox)
            .into_iter()
            .map(|pending| match herdr.prompt(&pending.pane, &pending.text) {
                Ok(()) => (None, None),
                Err(_) if lasted(pending.since, now, GIVE_UP) => {
                    (Some(format!("gave up telling {} after 15 minutes", pending.pane)), None)
                }
                Err(_) => (None, Some(pending)),
            })
            .unzip();
        self.outbox = keep.into_iter().flatten().collect();
        log.into_iter().flatten().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Target;
    use crate::herdr::Agent;
    use std::cell::RefCell;

    fn watch() -> Watch {
        Watch {
            name: "overload".into(),
            cpu: Some(85.0),
            memory: Some(90.0),
            over_for: Duration::from_secs(120),
            clear_for: Duration::from_secs(300),
            investigator: Some(Target::Cwd("/code/homelab".into())),
            broadcast: true,
            message: "Shane is gaming.".into(),
        }
    }

    fn sample(cpu: f32, memory: f32) -> Sample {
        Sample {
            host: "STHMAC".into(),
            cpu,
            memory,
            busiest: vec![
                Usage { name: "ffmpeg".into(), pid: 41, cpu: 780.0, memory_mb: 900 },
                Usage { name: "dagger".into(), pid: 7, cpu: 120.0, memory_mb: 2100 },
            ],
        }
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn names_each_limit_that_is_over() {
        assert_eq!(
            [
                over(&watch(), &sample(94.0, 50.0)),
                over(&watch(), &sample(94.0, 95.0)),
                over(&watch(), &sample(40.0, 50.0))
            ],
            [Some("CPU 94% (limit 85%)".into()), Some("CPU 94% (limit 85%) and memory 95% (limit 90%)".into()), None]
        );
    }

    #[test]
    fn trips_only_after_the_limit_holds_and_clears_only_after_it_stays_under() {
        let mut breaker = Breaker::default();
        let high = sample(95.0, 50.0);
        let low = sample(20.0, 50.0);
        let steps = [
            breaker.observe(&watch(), &high, at(0)),
            breaker.observe(&watch(), &low, at(60)),
            breaker.observe(&watch(), &high, at(70)),
            breaker.observe(&watch(), &high, at(189)),
            breaker.observe(&watch(), &high, at(190)),
            breaker.observe(&watch(), &high, at(400)),
            breaker.observe(&watch(), &low, at(500)),
            breaker.observe(&watch(), &high, at(600)),
            breaker.observe(&watch(), &low, at(700)),
            breaker.observe(&watch(), &low, at(1000)),
        ];

        assert_eq!(
            steps,
            [
                None,
                None,
                None,
                None,
                Some(Change::Tripped("CPU 95% (limit 85%)".into())),
                None,
                None,
                None,
                None,
                Some(Change::Cleared)
            ]
        );
    }

    struct FakeHerdr {
        agents: Vec<Agent>,
        refuse: RefCell<Vec<String>>,
        prompts: RefCell<Vec<(String, String)>>,
    }

    impl Herdr for FakeHerdr {
        fn agents(&self) -> Result<Vec<Agent>, String> {
            Ok(self.agents.clone())
        }
        fn prompt(&self, pane: &str, text: &str) -> Result<(), String> {
            if self.refuse.borrow().iter().any(|refused| refused == pane) {
                return Err("agent_blocked".into());
            }
            self.prompts.borrow_mut().push((pane.into(), text.into()));
            Ok(())
        }
    }

    fn agent(pane: &str, status: &str, cwd: &str) -> Agent {
        Agent {
            pane_id: pane.into(),
            agent_status: status.into(),
            name: None,
            cwd: Some(cwd.into()),
            agent_session: None,
        }
    }

    #[test]
    fn tells_every_agent_to_pause_the_investigator_to_find_out_why_and_retries_one_that_is_blocked() {
        let herdr = FakeHerdr {
            agents: vec![
                agent("w1:p1", "working", "/code/homelab"),
                agent("w2:p1", "working", "/code/encode"),
                agent("w3:p1", "blocked", "/code/site"),
            ],
            refuse: RefCell::new(vec!["w3:p1".into()]),
            prompts: RefCell::new(vec![]),
        };
        let config = Config { jobs: vec![], watches: vec![watch()] };
        let mut watcher = Watcher::default();

        let quiet = watcher.tick(at(0), &config, &sample(95.0, 50.0), &herdr);
        let tripped = watcher.tick(at(120), &config, &sample(95.0, 50.0), &herdr);
        let sent: Vec<String> = herdr.prompts.borrow().iter().map(|(pane, _)| pane.clone()).collect();
        herdr.refuse.borrow_mut().clear();
        watcher.tick(at(130), &config, &sample(95.0, 50.0), &herdr);
        let prompts = herdr.prompts.borrow();

        assert_eq!(quiet, Vec::<String>::new());
        assert_eq!(
            tripped,
            vec!["overload: tripped, CPU 95% (limit 85%), telling 3 agents, w1:p1 investigates".to_string()]
        );
        assert_eq!(sent, vec!["w1:p1".to_string(), "w2:p1".to_string()]);
        assert_eq!(prompts.last().map(|(pane, _)| pane.as_str()), Some("w3:p1"));
        assert_eq!(
            prompts[0].1,
            "[nudge: overload] STHMAC is overloaded: CPU 95% (limit 85%) for 2m. Busiest: ffmpeg (pid 41) 780% CPU, 900 MB; dagger (pid 7) 120% CPU, 2100 MB. Every agent on this machine was told to pause heavy work. Find out what is using the machine and why, then stop or fix it so Shane can keep using the computer, and say in a few lines what it was. Shane is gaming."
        );
        assert_eq!(
            prompts[1].1,
            "[nudge: overload] STHMAC is overloaded: CPU 95% (limit 85%) for 2m. Busiest: ffmpeg (pid 41) 780% CPU, 900 MB; dagger (pid 7) 120% CPU, 2100 MB. Pause heavy work you started (builds, encodes, test runs, containers) and start nothing heavy until nudge says overload has cleared. the agent in /code/homelab is finding out why."
        );
    }

    #[test]
    fn says_it_has_cleared_and_gives_up_on_an_agent_that_never_takes_the_message() {
        let herdr = FakeHerdr {
            agents: vec![agent("w2:p1", "blocked", "/code/encode")],
            refuse: RefCell::new(vec!["w2:p1".into()]),
            prompts: RefCell::new(vec![]),
        };
        let config = Config { jobs: vec![], watches: vec![watch()] };
        let mut watcher = Watcher::default();
        watcher.tick(at(0), &config, &sample(95.0, 95.0), &herdr);

        let tripped = watcher.tick(at(120), &config, &sample(95.0, 95.0), &herdr);
        let gave_up = watcher.tick(at(120 + 901), &config, &sample(10.0, 10.0), &herdr);
        let cleared = watcher.tick(at(120 + 901 + 300), &config, &sample(10.0, 10.0), &herdr);

        assert_eq!(
            tripped,
            vec![
                "overload: tripped, CPU 95% (limit 85%) and memory 95% (limit 90%), telling 1 agents".to_string(),
                "overload: no agent matches the agent in /code/homelab to investigate".to_string()
            ]
        );
        assert_eq!(gave_up, vec!["gave up telling w2:p1 after 15 minutes".to_string()]);
        assert_eq!(
            cleared,
            vec![
                "overload: cleared, telling 1 agents".to_string(),
                "overload: no agent matches the agent in /code/homelab to investigate".to_string()
            ]
        );
        assert_eq!(
            clear_text(&watch(), &sample(10.0, 10.0)),
            "[nudge: overload] cleared: STHMAC has been back under its limits for 5m (CPU 10%, memory 10%). Heavy work can resume."
        );
    }
}
