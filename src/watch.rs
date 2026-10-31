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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub cpu: f32,
    pub memory: f32,
}

pub fn over(watch: &Watch, level: Level) -> Option<String> {
    let cpu =
        watch.cpu.filter(|limit| level.cpu > *limit).map(|limit| format!("CPU {:.0}% (limit {limit:.0}%)", level.cpu));
    let memory = watch
        .memory
        .filter(|limit| level.memory > *limit)
        .map(|limit| format!("memory {:.0}% (limit {limit:.0}%)", level.memory));
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
    seen: Vec<(SystemTime, Level)>,
    tripped: bool,
}

fn lasted(since: SystemTime, now: SystemTime, needed: Duration) -> bool {
    now.duration_since(since).is_ok_and(|elapsed| elapsed >= needed)
}

fn age(at: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(at).unwrap_or_default()
}

impl Breaker {
    fn average(&self, now: SystemTime, span: Duration) -> Option<Level> {
        if !self.seen.iter().any(|(at, _)| age(*at, now) >= span) {
            return None;
        }
        let recent: Vec<Level> =
            self.seen.iter().filter(|(at, _)| age(*at, now) <= span).map(|(_, level)| *level).collect();
        let count = recent.len().max(1) as f32;
        Some(Level {
            cpu: recent.iter().map(|level| level.cpu).sum::<f32>() / count,
            memory: recent.iter().map(|level| level.memory).sum::<f32>() / count,
        })
    }

    pub fn observe(&mut self, watch: &Watch, sample: &Sample, now: SystemTime) -> Option<Change> {
        let keep = watch.over_for.max(watch.clear_for) + Duration::from_secs(30);
        self.seen.retain(|(at, _)| age(*at, now) <= keep);
        self.seen.push((now, Level { cpu: sample.cpu, memory: sample.memory }));
        if self.tripped {
            let level = self.average(now, watch.clear_for)?;
            if over(watch, level).is_some() {
                return None;
            }
            self.tripped = false;
            return Some(Change::Cleared);
        }
        let reason = over(watch, self.average(now, watch.over_for)?)?;
        self.tripped = true;
        self.seen.retain(|(at, _)| *at == now);
        Some(Change::Tripped(reason))
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
        "[nudge: {}] {} is overloaded: {reason} on average over the last {}. Busiest now: {}.",
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
        "[nudge: {}] cleared: {} has averaged under its limits for {} (CPU {:.0}%, memory {:.0}% now). Heavy work can resume.",
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
        let level = |cpu, memory| Level { cpu, memory };
        assert_eq!(
            [over(&watch(), level(94.0, 50.0)), over(&watch(), level(94.0, 95.0)), over(&watch(), level(40.0, 50.0))],
            [Some("CPU 94% (limit 85%)".into()), Some("CPU 94% (limit 85%) and memory 95% (limit 90%)".into()), None]
        );
    }

    fn feed(breaker: &mut Breaker, cpus: &[f32], start: u64) -> Vec<Option<Change>> {
        cpus.iter()
            .enumerate()
            .map(|(index, cpu)| breaker.observe(&watch(), &sample(*cpu, 50.0), at(start + index as u64 * 10)))
            .collect()
    }

    #[test]
    fn trips_on_the_average_over_the_window_so_bursty_load_still_counts() {
        let mut bursty = Breaker::default();
        let mut light = Breaker::default();
        let mut steady = Breaker::default();
        let swings: Vec<f32> = (0..13).map(|index| if index % 2 == 0 { 99.0 } else { 80.0 }).collect();

        let bursts = feed(&mut bursty, &swings, 0);
        let calm = feed(&mut light, &[90.0, 40.0, 50.0, 60.0, 90.0, 40.0, 50.0, 60.0, 90.0, 40.0, 50.0, 60.0, 90.0], 0);
        let early = feed(&mut steady, &[95.0; 12], 0);

        assert_eq!(bursts.iter().filter(|change| change.is_some()).count(), 1);
        assert_eq!(bursts[12], Some(Change::Tripped("CPU 90% (limit 85%)".into())));
        assert!(calm.iter().all(Option::is_none));
        assert!(early.iter().all(Option::is_none));
    }

    #[test]
    fn clears_only_once_the_average_stays_under_for_the_clear_window() {
        let mut breaker = Breaker::default();
        let tripped = feed(&mut breaker, &[95.0; 13], 0);
        let after: Vec<f32> = [vec![95.0; 6], vec![20.0; 30]].concat();
        let changes = feed(&mut breaker, &after, 130);
        let cleared_at = changes.iter().position(|change| change == &Some(Change::Cleared));

        assert_eq!(tripped[12], Some(Change::Tripped("CPU 95% (limit 85%)".into())));
        assert_eq!(changes.iter().filter(|change| change.is_some()).count(), 1);
        assert!(cleared_at.is_some_and(|index| index > 6 + 15));
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
            "[nudge: overload] STHMAC is overloaded: CPU 95% (limit 85%) on average over the last 2m. Busiest now: ffmpeg (pid 41) 780% CPU, 900 MB; dagger (pid 7) 120% CPU, 2100 MB. Every agent on this machine was told to pause heavy work. Find out what is using the machine and why, then stop or fix it so Shane can keep using the computer, and say in a few lines what it was. Shane is gaming."
        );
        assert_eq!(
            prompts[1].1,
            "[nudge: overload] STHMAC is overloaded: CPU 95% (limit 85%) on average over the last 2m. Busiest now: ffmpeg (pid 41) 780% CPU, 900 MB; dagger (pid 7) 120% CPU, 2100 MB. Pause heavy work you started (builds, encodes, test runs, containers) and start nothing heavy until nudge says overload has cleared. the agent in /code/homelab is finding out why."
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
            "[nudge: overload] cleared: STHMAC has averaged under its limits for 5m (CPU 10%, memory 10% now). Heavy work can resume."
        );
    }
}
