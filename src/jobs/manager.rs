use std::sync::{Arc, Mutex};

use octocrab::models::workflows::Status;
use octocrab::models::JobId;

use crate::machines::Manager as MachineManager;
use crate::machines::Triplet;

use super::job::Job;

#[derive(Clone)]
pub struct Manager {
    machine_manager: MachineManager,
    jobs: Arc<Mutex<Vec<Job>>>,
}

impl Manager {
    pub fn new(machine_manager: MachineManager) -> Self {
        Self {
            machine_manager,
            jobs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn status_feedback(
        &self,
        triplet: &Triplet,
        job_id: JobId,
        status: Status,
        runner_name: Option<&str>,
    ) {
        if let (Status::InProgress, Some(runner_name)) = (&status, runner_name) {
            // We know that the runner this job is running on must be online and busy,
            // even though that information may not have trickled through yet.
            // Make sure the runner does not become eligible for termination.
            self.machine_manager
                .status_feedback(runner_name, Some(true), true);
        }

        if let (Status::Completed | Status::Failed, Some(runner_name)) = (&status, runner_name) {
            // We know that the runner this job is running on is no longer busy.
            // We do however not know if it is still online.
            self.machine_manager
                .status_feedback(runner_name, None, false);
        }

        let mut jobs = self.jobs.lock().unwrap();

        let index = jobs
            .iter()
            .position(|job| job.triplet() == triplet && job.id() == job_id);

        let has_changed = match (&status, index) {
            // Track the status of this job by either adding it to our index
            // or updating its state if we already know it.
            (Status::Pending | Status::Queued | Status::InProgress, None) => {
                jobs.push(Job::new(triplet.clone(), job_id, status));
                true
            }
            (Status::Pending | Status::Queued | Status::InProgress, Some(index)) => {
                jobs[index].update_status(status)
            }

            // The job does not need further tracking from our side.
            (Status::Completed | Status::Failed, None) => false,
            (Status::Completed | Status::Failed, Some(index)) => {
                jobs.swap_remove(index);
                true
            }

            // The status enum is marked as non-exhaustive,
            // so we have to have this wildcard match even though all current
            // cases are covered.
            _ => panic!("Got unexpected workflow status from octocrab"),
        };

        if has_changed {
            self.update_demand();
        }
    }

    fn update_demand(&self) {
        let triplets: Vec<Triplet> = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|job| job.is_queued().then_some(job.triplet().clone()))
            .collect();

        self.machine_manager.update_demand(&triplets);
    }
}
