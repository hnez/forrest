use std::{
    io::ErrorKind,
    sync::Arc,
    time::{Duration, Instant},
};

use log::{debug, error, info, warn};
use octocrab::models::RunnerId;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use tokio::task::AbortHandle;

use super::manager::Manager;
use super::qemu;
use super::triplet::Triplet;
use crate::config::{ConfigFile, MachineConfig};

#[derive(PartialEq, Clone, Copy)]
pub(super) enum Status {
    Requested,
    Registering,
    Starting,
    Waiting,
    Running,
    Stopping,
}

pub(super) struct Machine {
    triplet: Triplet,
    machine_config: MachineConfig,
    status: Status,
    runner_name: String,
    cfg: Arc<ConfigFile>,
    abort: Option<AbortHandle>,
    runner_id: Option<RunnerId>,
    started: Option<Instant>,
}

impl Status {
    pub(super) fn is_available(&self) -> bool {
        match self {
            Self::Requested | Self::Registering | Self::Starting | Self::Waiting => true,
            Self::Running | Self::Stopping => false,
        }
    }

    pub(super) fn is_started(&self) -> bool {
        match self {
            Self::Requested => false,
            Self::Registering | Self::Starting | Self::Waiting | Self::Running | Self::Stopping => {
                true
            }
        }
    }

    pub(super) fn is_starting(&self) -> bool {
        matches!(self, Self::Starting)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let name = match self {
            Self::Requested => "requested",
            Self::Registering => "registering",
            Self::Starting => "starting",
            Self::Waiting => "waiting",
            Self::Running => "running",
            Self::Stopping => "stopping",
        };

        write!(f, "{name}")
    }
}

impl Machine {
    pub(super) fn new(cfg: Arc<ConfigFile>, triplet: Triplet) -> Option<Self> {
        let machine_config = cfg
            .repositories
            .get(triplet.owner())
            .and_then(|repos| repos.get(triplet.repository()))
            .and_then(|repo| repo.machines.get(triplet.machine_name()));

        let machine_config = match machine_config {
            Some(mc) => mc.to_owned(),
            None => {
                error!("Got request for unkown machine triplet: {triplet}");
                return None;
            }
        };

        let runner_name = {
            // Build a runner name like "forrest-build-rHCiNOhFdypjtnfj"

            let mut name = b"forrest-".to_vec();

            name.extend(triplet.machine_name().as_bytes());
            name.push(b'-');
            name.extend(thread_rng().sample_iter(&Alphanumeric).take(16));

            String::from_utf8(name).unwrap()
        };

        Some(Self {
            triplet,
            machine_config,
            status: Status::Requested,
            runner_name,
            cfg,
            abort: None,
            runner_id: None,
            started: None,
        })
    }

    pub(super) fn triplet(&self) -> &Triplet {
        &self.triplet
    }

    pub(super) fn runner_name(&self) -> &str {
        &self.runner_name
    }

    pub(super) fn status(&self) -> Status {
        self.status
    }

    pub(super) fn runtime(&self) -> Option<Duration> {
        self.started.map(|s| s.elapsed())
    }

    pub(super) fn spawn(&mut self, manager: Manager) {
        let triplet = self.triplet.clone();
        let machine_config = self.machine_config.clone();
        let runner_name = self.runner_name.clone();
        let cfg = self.cfg.clone();

        let task = tokio::spawn(async move {
            let installation_octocrab = manager.auth().user(triplet.owner()).unwrap();

            let jit_config = triplet
                .jit_config(&runner_name, &installation_octocrab)
                .await;

            let jit_config = match jit_config {
                Ok(jc) => jc,
                Err(err) => {
                    error!("Failed to register jit runner for {triplet}: {err}");
                    manager.remove_machine(&runner_name);
                    return;
                }
            };

            manager.modify_machine(&runner_name, |machine| {
                machine.status = Status::Starting;
                machine.runner_id = Some(jit_config.runner.id);
                machine.started = Some(Instant::now());
            });

            let process = qemu::run(&cfg, &runner_name, &triplet, &machine_config, &jit_config);

            match process.await {
                Ok(()) => info!("Machine {} {} has completed", triplet, runner_name),
                Err(err) => error!("Failed to run machine {triplet} {runner_name}: {err}"),
            }

            // Remove ourself from the list of machines and run clean up code
            // on the machine (but do not abort this task, as it is about to
            // end anyways).
            if let Some(machine) = manager.remove_machine(&runner_name) {
                machine.kill(false, &manager);
            }

            // Maybe schedule new machines in the place we freed.
            manager.reschedule();
        });

        self.status = Status::Registering;
        self.abort = Some(task.abort_handle());
    }

    pub(super) fn kill(self, do_abort: bool, manager: &Manager) {
        if let Some(abort) = self.abort {
            if do_abort {
                abort.abort()
            }
        }

        let disk_path = self
            .triplet
            .disk_image_path(&self.cfg.host.base_dir, &self.runner_name);
        let dps = disk_path.display();

        match std::fs::remove_file(&disk_path) {
            Ok(()) => debug!("Removed disk file {dps}"),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                debug!("Disk file {dps} was already removed")
            }
            Err(e) => error!("Failed to remove disk image {dps}: {e}"),
        }

        if let Some(runner_id) = self.runner_id {
            // We have to de-register the runner

            let triplet = self.triplet;
            let runner_name = self.runner_name;
            let octocrab = manager.auth().user(triplet.owner()).unwrap();

            tokio::spawn(async move {
                let res = octocrab
                    .actions()
                    .delete_repo_runner(triplet.owner(), triplet.repository(), runner_id)
                    .await;

                match res {
                    Ok(()) => info!("De-registered {runner_name} on {triplet}"),
                    Err(err) => {
                        warn!("Failed to de-register {runner_name} from {triplet}: {err}")
                    }
                }
            });
        }
    }

    pub(super) fn cost_to_kill(&self) -> u32 {
        match self.status {
            Status::Requested => 0,
            Status::Registering => 1,
            Status::Starting => 2,
            Status::Waiting => 3,
            Status::Running | Status::Stopping => u32::MAX,
        }
    }

    /// Get the amount of RAM (in bytes) the machine would consume if it were started
    pub(super) fn ram_required(&self) -> u64 {
        self.machine_config.ram.bytes()
    }

    // Get the amount of RAM (in bytes) the machine consumes
    //
    // Or will consume soon in case the machine is in the process of registering
    // a jit runner and will spawn qemu next.
    pub(super) fn ram_consumed(&self) -> u64 {
        match self.status {
            Status::Requested => 0,
            Status::Registering
            | Status::Starting
            | Status::Waiting
            | Status::Running
            | Status::Stopping => self.ram_required(),
        }
    }

    pub(super) fn status_feedback(&mut self, online: Option<bool>, busy: bool) {
        let new = match (&self.status, online, busy) {
            // Stay in the current state
            (Status::Requested, _, _) => Status::Requested,
            (Status::Registering, _, _) => Status::Registering,
            (Status::Starting, Some(false) | None, _) => Status::Starting,
            (Status::Waiting, Some(true) | None, false) => Status::Waiting,
            (Status::Running, Some(true) | None, true) => Status::Running,
            (Status::Stopping, _, _) => Status::Stopping,

            // The action runner on the machine has registered itself
            // but does not run a job yet.
            (Status::Starting, Some(true), false) => Status::Waiting,

            // The action runner has taken up a job
            (Status::Starting | Status::Waiting, _, true) => Status::Running,

            // The job is complete and the machine about to stop
            (Status::Waiting, Some(false), _)
            | (Status::Running, Some(false), _)
            | (Status::Running, _, false) => {
                self.runner_id = None;

                Status::Stopping
            }
        };

        if self.status != new {
            info!(
                "Machine {self} transitioned from state {} to {new}",
                self.status
            );
            self.status = new;
        }
    }
}

impl std::fmt::Display for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} {}", self.triplet, self.runner_name)
    }
}
