use crate::config::{Job, Target};
use crate::schedule::Direct;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

pub struct Claude {
    pub binary: PathBuf,
    pub logs: PathBuf,
    running: RefCell<HashMap<String, Child>>,
}

pub fn arguments(job: &Job, text: &str) -> Vec<String> {
    let place = match &job.target {
        Target::Session(id) => vec!["--resume".to_string(), id.clone()],
        Target::Cwd(_) | Target::Agent(_) => vec!["--continue".to_string()],
    };
    ["-p".to_string(), text.to_string()].into_iter().chain(place).chain(job.args.iter().cloned()).collect()
}

impl Claude {
    pub fn new(binary: PathBuf, logs: PathBuf) -> Claude {
        Claude { binary, logs, running: RefCell::new(HashMap::new()) }
    }

    fn still_going(&self, name: &str) -> bool {
        let mut running = self.running.borrow_mut();
        let going = running.get_mut(name).is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        if !going {
            running.remove(name);
        }
        going
    }
}

impl Direct for Claude {
    fn send(&self, job: &Job, text: &str) -> Result<String, String> {
        if self.still_going(&job.name) {
            return Err(format!("the last claude run for {} is still going", job.name));
        }
        std::fs::create_dir_all(&self.logs).map_err(|e| format!("could not make {}: {e}", self.logs.display()))?;
        let log_path = self.logs.join(format!("{}.log", job.name));
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("could not open {}: {e}", log_path.display()))?;
        let errors = log.try_clone().map_err(|e| e.to_string())?;
        let mut command = Command::new(&self.binary);
        command.args(arguments(job, text)).stdin(Stdio::null()).stdout(log).stderr(errors);
        if let Target::Cwd(dir) = &job.target {
            command.current_dir(dir);
        }
        let child = command.spawn().map_err(|e| format!("could not start {}: {e}", self.binary.display()))?;
        let described = format!("claude (pid {}, output in {})", child.id(), log_path.display());
        self.running.borrow_mut().insert(job.name.clone(), child);
        Ok(described)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Schedule, Via};
    use std::time::Duration;

    fn job(target: Target) -> Job {
        Job {
            name: "standup".into(),
            schedule: Schedule::Every(Duration::from_secs(3600)),
            target,
            via: Via::Claude,
            args: vec!["--permission-mode".into(), "acceptEdits".into()],
            message: "Summarise yesterday.".into(),
        }
    }

    #[test]
    fn resumes_a_session_by_id_or_continues_the_latest_in_a_folder() {
        assert_eq!(
            arguments(&job(Target::Session("abc".into())), "hi"),
            ["-p", "hi", "--resume", "abc", "--permission-mode", "acceptEdits"]
        );
        assert_eq!(
            arguments(&job(Target::Cwd("/code/app".into())), "hi"),
            ["-p", "hi", "--continue", "--permission-mode", "acceptEdits"]
        );
    }

    #[test]
    fn runs_claude_in_the_folder_logs_its_output_and_waits_for_the_last_run_to_finish() {
        let dir = std::env::temp_dir().join(format!("nudge-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("claude");
        std::fs::write(&binary, "#!/bin/sh\necho \"in $(pwd): $*\"\nsleep 1\n").unwrap();
        let mut perms = std::fs::metadata(&binary).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&binary, perms).unwrap();
        let folder = std::fs::canonicalize(&dir).unwrap();
        let claude = Claude::new(binary, dir.join("logs"));
        let target = job(Target::Cwd(folder.display().to_string()));

        assert!(claude.send(&target, "hi").unwrap().starts_with("claude (pid "));
        assert_eq!(claude.send(&target, "again"), Err("the last claude run for standup is still going".into()));
        std::thread::sleep(Duration::from_millis(1500));
        let log = std::fs::read_to_string(dir.join("logs/standup.log")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(log.trim(), format!("in {}: -p hi --continue --permission-mode acceptEdits", folder.display()));
    }
}
