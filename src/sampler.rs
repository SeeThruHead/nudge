use crate::watch::{Sample, Usage};
use sysinfo::{MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

#[cfg(target_os = "macos")]
fn memory_used(_: &System) -> f32 {
    std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.memorystatus_level"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse::<f32>().ok())
        .map(|free| 100.0 - free)
        .unwrap_or(0.0)
}

#[cfg(not(target_os = "macos"))]
fn memory_used(system: &System) -> f32 {
    (1.0 - system.available_memory() as f32 / system.total_memory().max(1) as f32) * 100.0
}

pub struct Sampler {
    system: System,
}

impl Sampler {
    pub fn new() -> Sampler {
        let mut system =
            System::new_with_specifics(RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()));
        system.refresh_cpu_usage();
        Sampler { system }
    }

    pub fn sample(&mut self) -> Sample {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let mut busiest: Vec<Usage> = self
            .system
            .processes()
            .iter()
            .map(|(pid, process)| Usage {
                name: process.name().to_string_lossy().into_owned(),
                pid: pid.as_u32(),
                cpu: process.cpu_usage(),
                memory_mb: process.memory() / 1_000_000,
            })
            .collect();
        busiest.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.memory_mb.cmp(&a.memory_mb)));
        busiest.truncate(8);
        Sample {
            host: System::host_name().unwrap_or_else(|| "this machine".into()),
            cpu: self.system.global_cpu_usage(),
            memory: memory_used(&self.system),
            load: System::load_average().one as f32,
            busiest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_machines_cpu_memory_and_busiest_processes() {
        let mut sampler = Sampler::new();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let sample = sampler.sample();

        assert!((0.0..=100.0).contains(&sample.cpu));
        assert!((0.0..=100.0).contains(&sample.memory) && sample.memory > 0.0);
        assert!(sample.load >= 0.0);
        assert!(!sample.busiest.is_empty() && sample.busiest.len() <= 8);
        assert!(sample.busiest.windows(2).all(|pair| pair[0].cpu >= pair[1].cpu));
    }
}
