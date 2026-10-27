//! Cooperative admission: change how many commands may start; never stop a running job.
use crate::ui::{Exit, Ui};
use anyhow::{Context, Result, bail};
use lindesk_sys::psi::PsiReading;
use std::fs::File;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

static CANCEL: AtomicI32 = AtomicI32::new(0);
extern "C" fn cancel(sig: libc::c_int) {
    CANCEL.store(sig, Ordering::Relaxed);
}

#[derive(Debug)]
struct Admission {
    limit: usize,
    max: usize,
    calm: usize,
}
impl Admission {
    fn new(max: usize) -> Self {
        Self { limit: 1, max, calm: 0 }
    }
    fn observe(&mut self, pressure: Option<[f64; 3]>) {
        let Some([cpu, memory, io]) = pressure else {
            self.limit = 1;
            self.calm = 0;
            return;
        };
        if cpu >= 20.0 || memory >= 5.0 || io >= 10.0 {
            self.limit = (self.limit / 2).max(1);
            self.calm = 0;
        } else if cpu < 5.0 && memory < 1.0 && io < 2.0 {
            self.calm += 1;
            if self.calm >= 8 {
                self.limit = (self.limit + 1).min(self.max);
                self.calm = 0;
            }
        } else {
            self.calm = 0;
        }
    }
}

fn totals(root: &Path) -> Option<[u64; 3]> {
    let mut values = [0; 3];
    for (i, resource) in ["cpu", "memory", "io"].iter().enumerate() {
        values[i] = PsiReading::read(&root.join(resource)).ok()?.some.total_us;
    }
    Some(values)
}
fn delta(previous: Option<[u64; 3]>, current: Option<[u64; 3]>, elapsed: Duration) -> Option<[f64; 3]> {
    let (old, new) = (previous?, current?);
    let us = elapsed.as_micros() as f64;
    if us == 0.0 || (0..3).any(|i| new[i] < old[i]) {
        return None;
    }
    Some(std::array::from_fn(|i| ((new[i] - old[i]) as f64 * 100.0 / us).min(100.0)))
}

struct Running(Vec<(usize, Child)>);
impl Drop for Running {
    fn drop(&mut self) {
        // Error/cancellation cleanup also reaches descendants in each worker's process group.
        for (_, c) in &self.0 {
            unsafe {
                libc::kill(-(c.id() as i32), libc::SIGTERM);
            }
        }
        if !self.0.is_empty() {
            std::thread::sleep(Duration::from_millis(200));
        }
        for (_, c) in &mut self.0 {
            unsafe {
                libc::kill(-(c.id() as i32), libc::SIGKILL);
            }
            let _ = c.wait();
        }
    }
}

fn read_jobs(file: &Path) -> Result<Vec<Vec<String>>> {
    let mut bytes = Vec::new();
    File::open(file)?.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        bail!("job file exceeds 4 MiB");
    }
    let jobs: Vec<Vec<String>> = serde_json::from_slice(&bytes).context("expected a JSON array of argv arrays")?;
    if jobs.is_empty()
        || jobs.len() > 10_000
        || jobs.iter().any(|j| j.is_empty() || j[0].is_empty() || j.iter().any(|s| s.contains('\0')))
    {
        bail!("need 1..10000 commands, each with a nonempty program and no NUL bytes");
    }
    Ok(jobs)
}

pub fn run(ui: &Ui, file: &Path, max: usize) -> Result<()> {
    if ui.system {
        bail!("batch admission runs as your user; omit --system");
    }
    if ui.json {
        bail!("batch streams command output; --json is not supported");
    }
    let jobs = read_jobs(file)?;
    let exe = std::env::current_exe()?;
    let mut admission = Admission::new(max);
    let root = Path::new("/proc/pressure");
    let mut previous = totals(root);
    let mut sampled = Instant::now();
    let mut running = Running(Vec::new());
    let mut next = 0;
    let mut failed = 0;
    CANCEL.store(0, Ordering::Relaxed);
    // This one-shot CLI owns its signal dispositions until exit.
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(sig, cancel as *const () as libc::sighandler_t);
        }
    }
    ui.info(format!("Admitting {} commands, maximum {max}; starting at one slot", jobs.len()));
    while next < jobs.len() || !running.0.is_empty() {
        let signal = CANCEL.load(Ordering::Relaxed);
        if signal != 0 {
            return Err(Exit((128 + signal) as u8).into());
        }
        let mut i = 0;
        while i < running.0.len() {
            if let Some(status) = running.0[i].1.try_wait()? {
                let (index, _) = running.0.swap_remove(i);
                if !status.success() {
                    failed += 1;
                    ui.warn(format!("job {} failed: {status}", index + 1));
                }
            } else {
                i += 1;
            }
        }
        if sampled.elapsed() >= Duration::from_millis(250) {
            let now = Instant::now();
            let current = totals(root);
            let before = admission.limit;
            admission.observe(delta(previous, current, now - sampled));
            previous = current;
            sampled = now;
            if admission.limit != before {
                ui.info(format!("Admission limit: {}", admission.limit));
            }
        }
        while next < jobs.len() && running.0.len() < admission.limit {
            let mut command = Command::new(&exe);
            command
                .args(["run", "--batch", "--"])
                .args(&jobs[next])
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .process_group(0);
            let child = command.spawn().with_context(|| format!("starting job {}", next + 1))?;
            running.0.push((next, child));
            next += 1;
        }
        if !running.0.is_empty() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    ui.info(format!("Completed {} commands; {failed} failed", jobs.len()));
    if failed > 0 {
        return Err(Exit(1).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_recovers_slowly_and_yields_fast() {
        let mut a = Admission::new(4);
        for _ in 0..24 {
            a.observe(Some([0.0; 3]));
        }
        assert_eq!(a.limit, 4);
        a.observe(Some([0.0, 6.0, 0.0]));
        assert_eq!(a.limit, 2);
        a.observe(Some([25.0, 0.0, 0.0]));
        assert_eq!(a.limit, 1);
        for _ in 0..7 {
            a.observe(Some([0.0; 3]));
        }
        assert_eq!(a.limit, 1);
        a.observe(None);
        assert_eq!(a.calm, 0);
        assert_eq!(delta(Some([100; 3]), Some([200; 3]), Duration::from_millis(1)), Some([10.0; 3]));
        assert_eq!(delta(Some([200; 3]), Some([100; 3]), Duration::from_millis(1)), None);
    }
    #[test]
    fn jobs_are_argv_not_shell_and_validate_before_execution() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("jobs.json");
        std::fs::write(&f, r#"[["printf", "$HOME; $(false)"]]"#).unwrap();
        assert_eq!(read_jobs(&f).unwrap()[0][1], "$HOME; $(false)");
        for bad in ["[]", "[[]]", "[[\"\"]]", "[\"echo hi\"]"] {
            std::fs::write(&f, bad).unwrap();
            assert!(read_jobs(&f).is_err());
        }
    }
}
