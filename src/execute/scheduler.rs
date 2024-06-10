use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use octocrab::models::JobId;

use super::job::Job;
use crate::config::ConfigFile;

pub struct MachineNotFoundError;

/*
 * TODO: improve the job handling.
 *
 * The job handling done here is not 100% sound yet, because it assumes that
 * a spawned machine will service the job we intended it to service.
 * In practice it could however service any job with the same owner, repo and
 * labels.
 *
 * We should thus instead have machines in up to four states:
 *
 *   - STARTING (Startup until the runner is active accoring to the API)
 *   - WAITING (The runner is active according to the API but not running a job)
 *   - RUNNING (The runner is executing a job)
 *   - STOPPING (The job is finished but the qemu process has not ended yet)
 *
 * We should then keep track of the state for each job we have have seen in
 * the queued state once and keep as many STARTING/WAITING machines of each
 * kind around as there are queued jobs (limited by the available resources
 * of course).
 *
 */

struct Inner {
    queued: Vec<Arc<Job>>,
    running: Vec<Arc<Job>>,
}

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Mutex<Inner>>,
    config: Arc<ConfigFile>,
}

impl Scheduler {
    pub fn new(config: Arc<ConfigFile>) -> Self {
        let inner = Inner {
            queued: Vec::new(),
            running: Vec::new(),
        };

        Self {
            inner: Arc::new(Mutex::new(inner)),
            config,
        }
    }

    pub(super) fn config(&self) -> &ConfigFile {
        &self.config
    }

    pub fn push(
        &self,
        owner: &str,
        repo_name: &str,
        machine_name: &str,
        job_id: JobId,
        installation_octocrab: &Arc<octocrab::Octocrab>,
    ) -> Result<(), MachineNotFoundError> {
        let mut inner = self.inner.lock().unwrap();

        let exists =
            inner.queued.iter().chain(&inner.running).any(|job| {
                job.owner == owner && job.repo_name == repo_name && job.job_id == job_id
            });

        if exists {
            return Ok(());
        }

        let (persistence_token, machine) = self
            .config
            .repositories
            .get(owner)
            .and_then(|repos| repos.get(repo_name))
            .and_then(|repo| {
                repo.machines
                    .get(machine_name)
                    .map(|machine| (repo.persistence_token.clone(), machine.clone()))
            })
            .ok_or(MachineNotFoundError)?;

        let timestamp = SystemTime::now();

        let job = Job {
            owner: owner.to_string(),
            repo_name: repo_name.to_string(),
            machine_name: machine_name.to_string(),
            persistence_token,
            machine,
            job_id,
            installation_octocrab: installation_octocrab.clone(),
            timestamp,
        };

        let job = Arc::new(job);

        inner.queued.push(job);

        Ok(())
    }

    pub(super) fn pop(&self, job: &Arc<Job>) {
        let mut inner = self.inner.lock().unwrap();

        // Find `job` in the list of running jobs and remove it.
        let index = inner
            .running
            .iter()
            .enumerate()
            .find_map(|(index, running)| Arc::ptr_eq(running, job).then_some(index));

        if let Some(index) = index {
            inner.running.swap_remove(index);
        }
    }

    pub fn reschedule(&self) {
        let mut inner = self.inner.lock().unwrap();

        // We want to prioritize scheduling jobs requiring a lot of RAM,
        // because they are harder to place if we start all smaller jobs first.
        inner.queued.sort_by_key(|job| job.machine.ram.bytes());

        let ram_total = self.config.host.ram.bytes();

        loop {
            let ram_consumed = inner
                .running
                .iter()
                .map(|job| job.machine.ram.bytes())
                .sum();

            let ram_available = ram_total.saturating_sub(ram_consumed);

            // Find the largest job (in terms of RAM) that we can spawn right now (if any).
            let job_index = inner
                .queued
                .iter()
                .enumerate()
                .rev()
                .find(|(_, job)| job.machine.ram.bytes() < ram_available)
                .map(|(index, _)| index);

            if let Some(job_index) = job_index {
                let job = inner.queued.remove(job_index);
                inner.running.push(job.clone());

                // The job will call Scheduler::reschedule(self) by itself
                // once it is done to check if another job fits into RAM.
                job.spawn(self.clone());
            } else {
                // Rigth now there is no pending job that matches our requirements

                break;
            }
        }
    }
}
