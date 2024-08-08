use std::ffi::OsString;
use std::fmt::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use octocrab::models::{actions::SelfHostedRunnerJitConfig, RunnerId};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use tokio::{process::Command, task::AbortHandle};

use super::triplet::Triplet;
use super::{
    manager::{Machines, Rescheduler},
    run_dir::RunDir,
};
use crate::{
    auth::Auth,
    config::{ConfigFile, MachineConfig},
};

const QEMU_ARGS: &[&[&str]] = &[
    &["-enable-kvm"],
    &["-nodefaults"],
    &["-nographic"],
    &["-M", "type=q35,accel=kvm,smm=on"],
    &["-cpu", "max"],
    &["-global", "ICH9-LPC.disable_s3=1"],
    &["-nic", "user,model=virtio-net-pci"],
    &["-object", "rng-random,filename=/dev/urandom,id=rng0"],
    &["-device", "virtio-rng-pci,rng=rng0,id=rng-device0"],
    &["-device", "isa-serial,chardev=bootlog"],
    &["-device", "isa-serial,chardev=telnet"],
    &["-chardev", "file,id=bootlog,path=log.txt"],
    &[
        "-chardev",
        "socket,id=telnet,server=on,wait=off,path=shell.sock",
    ],
    &[
        "-drive",
        "if=virtio,format=raw,discard=unmap,cache=unsafe,file=disk.img",
    ],
    &[
        "-drive",
        "if=virtio,format=raw,discard=unmap,cache=unsafe,file=cloud-init.img",
    ],
    &[
        "-drive",
        "if=virtio,format=raw,discard=unmap,cache=unsafe,file=job-config.img",
    ],
];

#[derive(PartialEq, Clone, Copy, Debug)]
pub(super) enum Status {
    Requested,
    Registering,
    Registered,
    Starting,
    Waiting,
    Running,
    Stopping,
    Stopped,
}

struct Inner {
    status: Status,
    run_dir: Option<RunDir>,
    abort: Option<AbortHandle>,
    jit_config: Option<SelfHostedRunnerJitConfig>,
    started: Option<Instant>,
}

pub(super) struct Machine {
    triplet: Triplet,
    machine_config: MachineConfig,
    runner_name: String,
    auth: Arc<Auth>,
    cfg: Arc<ConfigFile>,
    rescheduler: Rescheduler,
    inner: Mutex<Inner>,
}

impl Status {
    pub(super) fn is_available(&self) -> bool {
        match self {
            Self::Requested
            | Self::Registering
            | Self::Registered
            | Self::Starting
            | Self::Waiting => true,
            Self::Running | Self::Stopping | Self::Stopped => false,
        }
    }

    pub(super) fn is_stopped(&self) -> bool {
        *self == Self::Stopped
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Self::Requested => "requested",
            Self::Registering => "registering",
            Self::Registered => "registered",
            Self::Starting => "starting",
            Self::Waiting => "waiting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        })
    }
}

impl Inner {
    fn runner_id(&self) -> Option<RunnerId> {
        self.jit_config.as_ref().map(|jc| jc.runner.id)
    }
}

impl Machine {
    pub(super) fn new(
        cfg: Arc<ConfigFile>,
        auth: Arc<Auth>,
        rescheduler: Rescheduler,
        triplet: Triplet,
    ) -> Option<Arc<Self>> {
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

        let inner = Mutex::new(Inner {
            status: Status::Requested,
            run_dir: None,
            abort: None,
            jit_config: None,
            started: None,
        });

        Some(Arc::new(Self {
            triplet,
            machine_config,
            rescheduler,
            runner_name,
            auth,
            cfg,
            inner,
        }))
    }

    fn inner(&self) -> std::sync::MutexGuard<Inner> {
        self.inner.lock().unwrap()
    }

    pub(super) fn status(&self) -> Status {
        self.inner().status
    }

    pub(super) fn encoded_jit_config(&self) -> Option<String> {
        self.inner()
            .jit_config
            .as_ref()
            .map(|jc| jc.encoded_jit_config.clone())
    }

    pub(super) fn triplet(&self) -> &Triplet {
        &self.triplet
    }

    pub(super) fn cfg(&self) -> &ConfigFile {
        &self.cfg
    }

    pub(super) fn machine_config(&self) -> &MachineConfig {
        &self.machine_config
    }

    pub(super) fn runner_name(&self) -> &str {
        &self.runner_name
    }

    pub(super) fn starting_duration(&self) -> Option<Duration> {
        let inner = self.inner();

        match inner.status {
            Status::Starting => inner.started.map(|s| s.elapsed()),
            _ => None,
        }
    }

    fn register(self: &Arc<Self>, inner: &mut Inner) {
        assert_eq!(inner.status, Status::Requested);

        let machine = self.clone();

        let task = tokio::spawn(async move {
            let installation_octocrab = machine.auth.user(machine.triplet.owner()).unwrap();

            let jit_config = machine
                .triplet
                .jit_config(&machine.runner_name, &installation_octocrab)
                .await;

            let mut inner = machine.inner();

            match jit_config {
                Ok(jc) => {
                    debug!(
                        "Registered jit runner for {}: {} {}",
                        machine.triplet, machine.runner_name, jc.runner.id
                    );

                    inner.status = Status::Registered;
                    inner.jit_config = Some(jc);
                }
                Err(err) => {
                    error!(
                        "Failed to register jit runner for {}: {err}",
                        machine.triplet
                    );

                    inner.status = Status::Stopped;
                }
            }

            // The task is about to end.
            // No need to stop it from the outside anymore.
            inner.abort = None;

            // We must release the lock before calling reschedule
            std::mem::drop(inner);
            machine.rescheduler.reschedule();
        });

        inner.status = Status::Registering;
        inner.abort = Some(task.abort_handle());
    }

    async fn qemu(&self) -> std::io::Result<()> {
        let virtfs_args = self.machine_config.shared.iter().flat_map(|dir| {
            let mut arg = OsString::new();

            let tag = &dir.tag;
            let readonly = if dir.writable { "off" } else { "on" };

            write!(&mut arg, "local,security_model=none,",).unwrap();
            write!(&mut arg, "mount_tag={tag},readonly={readonly},path=",).unwrap();

            arg.push(dir.path.as_os_str());

            ["-virtfs".into(), arg].into_iter()
        });

        let mut qemu = {
            let inner = self.inner();
            let ram = self.machine_config.ram.megabytes().to_string();
            let smp = self.machine_config.cpus.to_string();
            let pwd = inner.run_dir.as_ref().unwrap();

            let mut qemu = Command::new("/usr/bin/qemu-system-x86_64");

            qemu.kill_on_drop(true)
                .current_dir(pwd.path())
                .arg("-m")
                .arg(&ram)
                .arg("-smp")
                .arg(&smp)
                .args(QEMU_ARGS.iter().flat_map(|arg_list| *arg_list))
                .args(virtfs_args);

            qemu
        };

        let status = qemu.status().await?;

        match status.success() {
            true => Ok(()),
            false => {
                let code = status.code().map(|c| c.to_string());
                let dpc = code.as_deref().unwrap_or("<None>");

                let msg = format!("The qemu process for job {self} exited with code: {dpc}",);

                Err(std::io::Error::other(msg))
            }
        }
    }

    fn spawn(self: &Arc<Self>, inner: &mut Inner) {
        assert_eq!(inner.status, Status::Registered);

        let machine = self.clone();

        let task = tokio::spawn(async move {
            match machine.qemu().await {
                Ok(()) => {
                    info!("Machine {machine} has completed");

                    let mut inner = machine.inner();
                    inner.run_dir.as_mut().unwrap().maybe_persist();
                }
                Err(err) => error!("Failed to run machine {machine}: {err}",),
            }

            // We are about to exit anyways.
            // No need to abort this task anymore.
            machine.inner().abort = None;

            // Update our status to stopped and some other cleanup.
            machine.kill();

            // Maybe schedule new machines in the space we freed.
            machine.rescheduler.reschedule();
        });

        inner.status = Status::Starting;
        inner.started = Some(Instant::now());
        inner.abort = Some(task.abort_handle());
    }

    pub(super) fn reschedule(self: &Arc<Self>, ram_available: &mut u64, machines: &Machines) {
        let mut inner = self.inner();

        match inner.status {
            Status::Requested => self.register(&mut inner),
            Status::Registered => {
                let ram_required = self.ram_required();

                if ram_required > *ram_available {
                    debug!("Postpone starting {self} due to insufficient RAM {ram_available} vs. {ram_required}");
                    return;
                }

                match RunDir::new(self, machines) {
                    Ok(run_dir) => inner.run_dir = run_dir,
                    Err(err) => {
                        error!("Failed to set up run dir for {self}: {err}");
                        inner.status = Status::Stopped;
                        return;
                    }
                }

                if inner.run_dir.is_some() {
                    self.spawn(&mut inner);
                    *ram_available -= ram_required;
                }
            }
            Status::Registering
            | Status::Starting
            | Status::Waiting
            | Status::Running
            | Status::Stopping
            | Status::Stopped => {}
        }
    }

    pub(super) fn kill(self: &Arc<Self>) {
        let mut inner_locked = self.inner();

        if let Some(abort) = inner_locked.abort.take() {
            abort.abort()
        }

        inner_locked.status = Status::Stopped;

        if let Some(runner_id) = inner_locked.runner_id() {
            // We have to de-register the runner

            let machine = self.clone();

            tokio::spawn(async move {
                let octocrab = machine.auth.user(machine.triplet.owner()).unwrap();

                let res = octocrab
                    .actions()
                    .delete_repo_runner(
                        machine.triplet.owner(),
                        machine.triplet.repository(),
                        runner_id,
                    )
                    .await;

                machine.inner().jit_config = None;

                match res {
                    Ok(()) => info!(
                        "De-registered {} on {}",
                        machine.runner_name, machine.triplet
                    ),
                    Err(err) => {
                        warn!(
                            "Failed to de-register {} from {}: {err}",
                            machine.runner_name, machine.triplet
                        )
                    }
                }
            });
        }
    }

    pub(super) fn cost_to_kill(&self) -> u32 {
        match self.inner().status {
            Status::Requested => 0,
            Status::Registering => 1,
            Status::Registered => 2,
            Status::Starting => 3,
            Status::Waiting => 4,
            Status::Running | Status::Stopping | Status::Stopped => u32::MAX,
        }
    }

    /// Get the amount of RAM (in bytes) the machine would consume if it were started
    pub(super) fn ram_required(&self) -> u64 {
        self.machine_config.ram.bytes()
    }

    // Get the amount of RAM (in bytes) the machine consumes
    pub(super) fn ram_consumed(&self) -> u64 {
        match self.inner().status {
            Status::Requested | Status::Registering | Status::Registered | Status::Stopped => 0,
            Status::Starting | Status::Waiting | Status::Running | Status::Stopping => {
                self.ram_required()
            }
        }
    }

    pub(super) fn status_feedback(&self, online: Option<bool>, busy: bool) {
        let mut inner = self.inner();

        let new = match (&inner.status, online, busy) {
            // Stay in the current state
            (Status::Requested, _, _) => Status::Requested,
            (Status::Registering, _, _) => Status::Registering,
            (Status::Registered, _, _) => Status::Registered,
            (Status::Starting, Some(false) | None, _) => Status::Starting,
            (Status::Waiting, Some(true) | None, false) => Status::Waiting,
            (Status::Running, Some(true) | None, true) => Status::Running,
            (Status::Stopping, _, _) => Status::Stopping,
            (Status::Stopped, _, _) => Status::Stopped,

            // The action runner on the machine has registered itself
            // but does not run a job yet.
            (Status::Starting, Some(true), false) => Status::Waiting,

            // The action runner has taken up a job
            (Status::Starting | Status::Waiting, _, true) => Status::Running,

            // The job is complete and the machine about to stop
            (Status::Waiting, Some(false), _)
            | (Status::Running, Some(false), _)
            | (Status::Running, _, false) => {
                inner.jit_config = None;

                Status::Stopping
            }
        };

        if inner.status != new {
            info!(
                "Machine {self} transitioned from state {} to {new}",
                inner.status
            );
            inner.status = new;
        }
    }
}

impl std::fmt::Display for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} {}", self.triplet, self.runner_name)
    }
}
