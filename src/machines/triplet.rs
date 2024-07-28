use std::path::{Path, PathBuf};

use octocrab::{
    models::{actions::SelfHostedRunnerJitConfig, RunnerGroupId},
    Octocrab,
};

#[derive(PartialEq, Eq, Clone, Hash)]
pub struct OwnerAndRepo {
    owner: String,
    repository: String,
}

#[derive(PartialEq, Eq, Clone, Hash)]
pub struct Triplet {
    owner: String,
    repository: String,
    machine_name: String,
}

impl OwnerAndRepo {
    pub fn new(owner: impl ToString, repository: impl ToString) -> Self {
        Self {
            owner: owner.to_string(),
            repository: repository.to_string(),
        }
    }

    pub fn into_triplet(self, machine_name: impl ToString) -> Triplet {
        Triplet {
            owner: self.owner,
            repository: self.repository,
            machine_name: machine_name.to_string(),
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }
}

impl std::fmt::Display for OwnerAndRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repository)
    }
}

impl Triplet {
    pub fn new(
        owner: impl ToString,
        repository: impl ToString,
        machine_name: impl ToString,
    ) -> Self {
        Self {
            owner: owner.to_string(),
            repository: repository.to_string(),
            machine_name: machine_name.to_string(),
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn machine_name(&self) -> &str {
        &self.machine_name
    }

    pub fn into_owner_and_repo(self) -> OwnerAndRepo {
        OwnerAndRepo {
            owner: self.owner,
            repository: self.repository,
        }
    }

    pub(super) fn run_dir_path(&self, base_dir_path: &Path, runner_name: &str) -> PathBuf {
        base_dir_path
            .join("runs")
            .join(&self.owner)
            .join(&self.repository)
            .join(&self.machine_name)
            .join(runner_name)
    }

    pub(super) fn disk_image_path(&self, base_dir_path: &Path, runner_name: &str) -> PathBuf {
        self.run_dir_path(base_dir_path, runner_name)
            .join("disk.img")
    }

    pub(super) fn machine_image_path(&self, base_dir_path: &Path) -> PathBuf {
        base_dir_path
            .join("machines")
            .join(&self.owner)
            .join(&self.repository)
            .join(format!("{}.img", self.machine_name))
    }

    pub(super) async fn jit_config(
        &self,
        runner_name: &str,
        installation_octocrab: &Octocrab,
    ) -> octocrab::Result<SelfHostedRunnerJitConfig> {
        let labels = vec![
            "self-hosted".to_owned(),
            "forrest".to_owned(),
            self.machine_name.clone(),
        ];

        let runner_group = RunnerGroupId(1);

        installation_octocrab
            .actions()
            .create_repo_jit_runner_config(
                &self.owner,
                &self.repository,
                runner_name,
                runner_group,
                labels,
            )
            .send()
            .await
    }
}

impl std::fmt::Display for Triplet {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}/{}/{}",
            self.owner, self.repository, self.machine_name
        )
    }
}
