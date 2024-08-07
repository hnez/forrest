use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{TimeDelta, Utc};
use log::{debug, error, info};
use octocrab::models::RunId;

use crate::auth::Auth;
use crate::config::{Config, Repository};
use crate::jobs::Manager as JobManager;
use crate::machines::OwnerAndRepo;

const MAX_NEW_RUN_AGE: TimeDelta = TimeDelta::days(2);

pub struct Poller {
    auth: Arc<Auth>,
    config: Config,
    job_manager: JobManager,
    most_recent_run_id: Arc<Mutex<HashMap<OwnerAndRepo, RunId>>>,
}

impl Poller {
    pub fn new(config: Config, auth: Arc<Auth>, job_manager: JobManager) -> Self {
        let most_recent_run_id = Arc::new(Mutex::new(HashMap::new()));

        Self {
            auth,
            config,
            job_manager,
            most_recent_run_id,
        }
    }

    async fn get_new_workflow_runs(
        &self,
        oar: &OwnerAndRepo,
        runs: &mut HashSet<RunId>,
    ) -> octocrab::Result<()> {
        let octocrab = self.auth.user(oar.owner()).unwrap();
        let workflows = octocrab.workflows(oar.owner(), oar.repository());

        let mut prev_run_id = None;

        for page in 1u32.. {
            let workflow_runs = workflows.list_all_runs().page(page).send().await?;

            if page == 0 {
                // The first run on the first page is the newest one.
                // Save its id for later run so we know where to stop looking
                // for new runs.
                if let Some(newest_run) = workflow_runs.items.first() {
                    prev_run_id = self
                        .most_recent_run_id
                        .lock()
                        .unwrap()
                        .insert(oar.clone(), newest_run.id);
                }
            }

            if workflow_runs.items.is_empty() {
                // We have reached an empty page. Time to stop.
                break;
            }

            for run in workflow_runs.items {
                if prev_run_id.map(|p| p == run.id).unwrap_or(false) {
                    // We have seen this run_id in a previous round of polling.
                    // This means we can stop here.
                    return Ok(());
                }

                let age = Utc::now() - run.created_at;

                if age > MAX_NEW_RUN_AGE {
                    // Runs older than a few days are likely not relevant to us anymore.
                    return Ok(());
                }

                runs.insert(run.id);
            }
        }

        Ok(())
    }

    async fn poll_run(&self, oar: &OwnerAndRepo, run_id: RunId) -> octocrab::Result<()> {
        let octocrab = self.auth.user(oar.owner()).unwrap();
        let workflows = octocrab.workflows(oar.owner(), oar.repository());

        for page in 1u32.. {
            let jobs = workflows.list_jobs(run_id).page(page).send().await?;

            if jobs.items.is_empty() {
                // We have reached an empty page. Time to stop.
                break;
            }

            for job in jobs.items {
                let machine_name = {
                    let labels = &job.labels;

                    if labels.len() != 3 {
                        debug!("Ignoring job with {} != 3 labels on {oar}", labels.len());

                        continue;
                    }

                    if labels[0] != "self-hosted" || labels[1] != "forrest" {
                        debug!("Ignoring job that does not have 'self-hosted' and 'forrest' as first labels on {oar}");

                        continue;
                    }

                    &labels[2]
                };

                let triplet = oar.clone().into_triplet(machine_name);

                self.job_manager.status_feedback(
                    &triplet,
                    job.id,
                    run_id,
                    job.status,
                    job.runner_name.as_deref(),
                );
            }
        }

        Ok(())
    }

    async fn poll_repository(
        &self,
        oar: &OwnerAndRepo,
        mut run_ids: HashSet<RunId>,
    ) -> octocrab::Result<()> {
        // Add new runs that we do not know yet to the list of runs to poll.
        self.get_new_workflow_runs(oar, &mut run_ids).await?;

        for run_id in run_ids {
            self.poll_run(oar, run_id).await?;
        }

        Ok(())
    }

    async fn poll_installation(
        &self,
        user: &str,
        repos: &HashMap<String, Repository>,
        runs_of_interest: &mut HashMap<OwnerAndRepo, HashSet<RunId>>,
    ) {
        for repo_name in repos.keys() {
            let oar = OwnerAndRepo::new(user, repo_name);
            let run_ids = runs_of_interest.remove(&oar).unwrap_or_default();

            debug!("Polling for repository {oar}");

            let res = self.poll_repository(&oar, run_ids).await;

            if let Err(e) = res {
                error!("Failed to poll {oar} for queued jobs: {e}");
            }
        }
    }

    pub async fn poll_once(&self) -> octocrab::Result<()> {
        let cfg = self.config.get();

        // These are runs for which we have jobs in "interesting" states,
        // like "pending", "queued" or "in_progress".
        let mut runs_of_interest = self.job_manager.runs_of_interest();

        for page in 1u32.. {
            let installations = self
                .auth
                .app()
                .apps()
                .installations()
                .page(page)
                .send()
                .await?;

            if installations.items.is_empty() {
                // We have reached an empty page. Time to stop.
                break;
            }

            for installation in installations.items {
                let user = &installation.account.login;

                debug!("Polling for user {user}");

                if let Some(repos) = cfg.repositories.get(user) {
                    self.auth.update_user(user, installation.id);

                    self.poll_installation(user, repos, &mut runs_of_interest)
                        .await;
                } else {
                    info!("Refusing to service unlisted user \"{user}\"");
                }
            }
        }

        Ok(())
    }

    pub async fn poll(&self) -> std::io::Result<()> {
        loop {
            debug!("Poll for pending jobs");

            if let Err(e) = self.poll_once().await {
                error!("Failed to poll for installations: {e}");
            }

            tokio::time::sleep(self.config.get().github.polling_interval).await;
        }
    }
}
