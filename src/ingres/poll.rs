use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info};

use super::auth::Auth;
use crate::config::{ConfigFile, Repository};
use crate::execute::Scheduler;

pub struct Poller {
    auth: Arc<Auth>,
    config: Arc<ConfigFile>,
    scheduler: Scheduler,
}

impl Poller {
    pub fn new(config: Arc<ConfigFile>, auth: Arc<Auth>, scheduler: Scheduler) -> Self {
        Self {
            auth,
            config,
            scheduler,
        }
    }

    async fn poll_repository(
        &self,
        user: &str,
        repo_name: &str,
        installation_octocrab: &Arc<octocrab::Octocrab>,
    ) -> octocrab::Result<()> {
        let workflows = installation_octocrab.workflows(user, repo_name);
        let queued_workflow_runs = workflows.list_all_runs().status("queued").send().await?;

        for run in queued_workflow_runs.items {
            let github_jobs = workflows.list_jobs(run.id).send().await?;

            for github_job in github_jobs.items.into_iter() {
                if !matches!(
                    github_job.status,
                    octocrab::models::workflows::Status::Queued
                ) {
                    continue;
                }

                let machine_name = {
                    let labels = &github_job.labels;

                    if labels.len() != 3 {
                        debug!(
                            "Ignoring job with {} != 3 labels on {user}/{repo_name}",
                            labels.len()
                        );

                        continue;
                    }

                    if labels[0] != "self-hosted" || labels[1] != "forrest" {
                        debug!("Ignoring job that does not have 'self-hosted' and 'forrest' as first labels on {user}/{repo_name}");

                        continue;
                    }

                    &labels[2]
                };

                let res = self.scheduler.push(
                    user,
                    repo_name,
                    machine_name,
                    github_job.id,
                    installation_octocrab,
                );

                if res.is_err() {
                    error!("Failed to setup job with machine type {machine_name} for {user}/{repo_name}");
                }
            }
        }

        Ok(())
    }

    async fn poll_installation(
        &self,
        user: &str,
        installation_octocrab: Arc<octocrab::Octocrab>,
        repos: &HashMap<String, Repository>,
    ) {
        for repo_name in repos.keys() {
            debug!("Polling for repository {user}/{repo_name}");

            let res = self
                .poll_repository(user, repo_name, &installation_octocrab)
                .await;

            if let Err(e) = res {
                error!("Failed to poll {user}/{repo_name} for queued jobs: {e}");
            }
        }
    }

    async fn poll_installations(&self) -> octocrab::Result<()> {
        // This only gets the first page of installations
        let installations = self.auth.app().apps().installations().send().await?;

        for installation in installations {
            let user = &installation.account.login;

            debug!("Polling for user {user}");

            if let Some(repos) = self.config.repositories.get(user) {
                let installation_octocrab = self.auth.installation(installation.id);

                self.poll_installation(user, installation_octocrab, repos)
                    .await;
            } else {
                info!("Refusing to service unlisted user \"{user}\"");
            }
        }

        // Start processing jobs if we got some new ones
        self.scheduler.reschedule();

        Ok(())
    }

    pub async fn run(&self) -> std::io::Result<()> {
        loop {
            debug!("Poll for pending jobs");

            if let Err(e) = self.poll_installations().await {
                error!("Failed to poll for installations: {e}");
            }

            // TODO: make configurable
            tokio::time::sleep(Duration::from_secs(15 * 60)).await;
        }
    }
}
