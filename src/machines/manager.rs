use std::{
    collections::HashMap,
    io::ErrorKind,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use log::{debug, error, info, warn};

use super::machine::Machine;
use super::triplet::Triplet;
use crate::{auth::Auth, config::Config};

// Machines should go from being booted to being registered with GitHub
// in less than 15 minutes.
// The timeout is quite generous because new machines have to download
// and unpack the runner binary first.
const START_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub type Machines = HashMap<Triplet, Vec<Arc<Machine>>>;

#[derive(Clone)]
pub struct Manager {
    auth: Arc<Auth>,
    config: Config,
    machines: Arc<Mutex<Machines>>,
}

pub struct Rescheduler {
    manager: Manager,
}

impl Manager {
    pub fn new(config: Config, auth: Arc<Auth>) -> Self {
        let machines = Arc::new(Mutex::new(HashMap::new()));

        Self {
            auth,
            config,
            machines,
        }
    }

    fn machines(&self) -> std::sync::MutexGuard<Machines> {
        let mut machines = self.machines.lock().unwrap();

        // Use the opportunity to clean up the machines.
        // Go through each entry in the HashMap<Triplet, Vec<Arc<Machine>>>,
        // remove all Machines that have already stopped from the Vec
        // and then all Triplets from the HashMap that have an empty Vec.
        machines.retain(|_triplet, triplet_machines| {
            triplet_machines.retain(|machine| !machine.status().is_stopped());

            !triplet_machines.is_empty()
        });

        machines
    }

    pub fn status_feedback(
        &self,
        triplet: &Triplet,
        runner_name: &str,
        online: Option<bool>,
        busy: bool,
    ) -> bool {
        let mut machines = self.machines();

        let machine = machines.get_mut(triplet).and_then(|triplet_machines| {
            triplet_machines
                .iter()
                .find(|machine| machine.runner_name() == runner_name)
        });

        match machine {
            Some(machine) => {
                machine.status_feedback(online, busy);
                true
            }
            None => false,
        }
    }

    pub fn update_demand<'a>(&self, requested: impl Iterator<Item = &'a Triplet>) {
        let mut demand: HashMap<Triplet, u64> = HashMap::new();

        for triplet in requested {
            if let Some(count) = demand.get_mut(triplet) {
                *count += 1
            } else {
                demand.insert(triplet.clone(), 1);
            }
        }

        debug!("Updating the machine demand with:");

        for (triplet, count) in demand.iter() {
            debug!("  - {triplet}: {count}");
        }

        let mut machines = self.machines();

        for (triplet, triplet_machines) in machines.iter_mut() {
            // Remove machines where the supply surpasses the demand

            // We will traverse the list of machines from end to start and once
            // demand for a machine type reaches zero we will start killing
            // machines.
            // We'd rather kill machines that have not started yet / are not
            // already waiting for jobs, so we place those at the end of the
            // list.
            triplet_machines.sort_unstable_by_key(|m| Machine::cost_to_kill(m));

            for machine in triplet_machines.iter().rev() {
                // Machines that are already servicing jobs do not count into the
                // supply/demand calculation.
                if !machine.status().is_available() {
                    continue;
                }

                // Reduce the demand for this machine type by one.
                // If the demand is already zero, then kill the machine.
                match demand.get_mut(triplet) {
                    Some(0) | None => machine.kill(),
                    Some(count) => *count -= 1,
                }
            }
        }

        // Add machines where the demand surpasses the supply
        let cfg = self.config.get();

        for (triplet, count) in demand {
            if !machines.contains_key(&triplet) {
                machines.insert(triplet.clone(), Vec::new());
            }

            for _ in 0..count {
                let cfg = cfg.clone();
                let auth = self.auth.clone();
                let rescheduler = self.rescheduler();

                if let Some(m) = Machine::new(cfg, auth, rescheduler, triplet.clone()) {
                    machines.get_mut(&triplet).unwrap().push(m);
                }
            }
        }

        // We must release the lock before calling reschedule
        std::mem::drop(machines);
        self.reschedule();
    }

    pub(super) fn rescheduler(&self) -> Rescheduler {
        Rescheduler {
            manager: self.clone(),
        }
    }

    pub(super) fn reschedule(&self) {
        let machines = self.machines();

        let mut ram_available = {
            let cfg = self.config.get();
            let ram_total = cfg.host.ram.bytes();
            let ram_consumed = machines
                .values()
                .flat_map(|triplet_machines| triplet_machines.iter())
                .map(|m| Machine::ram_consumed(m))
                .sum();
            let ram_available = ram_total.saturating_sub(ram_consumed);

            debug!("Re-scheduling machines. {ram_available} of {ram_total} available");

            ram_available
        };

        // We want to prioritize scheduling jobs requiring a lot of RAM,
        // because they are harder to place if we start all smaller jobs first.
        let mut machines_flat: Vec<_> = machines
            .values()
            .flat_map(|triplet_machines| triplet_machines.iter())
            .collect();

        machines_flat.sort_unstable_by_key(|m| Machine::ram_required(m));

        for machine in machines_flat.iter_mut().rev() {
            machine.reschedule(&mut ram_available, &machines);
        }

        debug!("Machines and their new state:");

        for machine in machines_flat.iter() {
            debug!("  - {machine}: {}", machine.status());
        }

        debug!("Available RAM after re-schedule: {ram_available}");
    }

    async fn sweep(&self) {
        let cfg = self.config.get();

        // Go through every user in our list ...
        for (owner, repos) in cfg.repositories.iter() {
            let octocrab = match self.auth.user(owner) {
                Some(oc) => oc,
                None => {
                    info!("Could not authenticate as {owner} (yet). Skipping");
                    continue;
                }
            };

            // ... visit each of their repositories ...
            for repository in repos.keys() {
                // ... and have a look at all of their registered runners ...
                for page in 1u32.. {
                    let runners_page = octocrab
                        .actions()
                        .list_repo_self_hosted_runners(owner.as_str(), repository.as_str())
                        .page(page)
                        .send()
                        .await;

                    let runners_page = match runners_page {
                        Ok(rp) => rp,
                        Err(e) => {
                            error!("Failed to get runners for {owner}/{repository}: {e}");
                            break;
                        }
                    };

                    if runners_page.items.is_empty() {
                        // We have reached an empty page. Time to stop.
                        break;
                    }

                    // ... which are reported by the API in pages.
                    for runner in runners_page.items {
                        let runner_name = runner.name;

                        if !runner_name.starts_with("forrest-") {
                            continue;
                        }

                        let labels: Vec<String> =
                            runner.labels.into_iter().map(|label| label.name).collect();

                        if labels.len() != 3 || labels[0] != "self-hosted" || labels[1] != "forrest"
                        {
                            error!("Runner {runner_name} on {owner}/{repository} has name starting in forrest- but wrong labels");
                            continue;
                        }

                        let machine_name = &labels[2];

                        let triplet = Triplet::new(owner, repository, machine_name);

                        // Is the runner online (the action runner software on the machine is
                        // connected to GitHubs servers) right now?
                        let online = match runner.status.as_str() {
                            "online" => true,
                            "offline" => false,
                            _ => {
                                error!("Runner {runner_name} on {owner}/{repository} has unknown online status: {}", runner.status);
                                continue;
                            }
                        };

                        // Is this runner executing a job right now?
                        let busy = runner.busy;

                        // Try to update the runner's online/busy status.
                        // Returns wether we know this runner or not.
                        let found =
                            self.status_feedback(&triplet, &runner_name, Some(online), busy);

                        // The runners name and labels sound like we created them,
                        // but we do not know about it.
                        // The runner is also not online and not busy right now.
                        // It most likely comes from a previous Forrest instance that
                        // was uncleanly shut down.
                        // Remove the runner to un-clutter the runner list.
                        if !found && !online && !busy {
                            let res = octocrab
                                .actions()
                                .delete_repo_runner(&owner, &repository, runner.id)
                                .await;

                            match res {
                                Ok(()) => info!("De-registered orphaned runner {runner_name} on {owner}/{repository}"),
                                Err(err) => warn!("Failed to de-register orphaned runner {runner_name} from {owner}/{repository}: {err}"),
                            }
                        }
                    }
                }
            }
        }

        // Go through each machine and check for timeouts
        let mut machines = self.machines();

        let base_dir_path = Path::new(&cfg.host.base_dir);

        for (triplet, triplet_machines) in machines.iter_mut() {
            for machine in triplet_machines {
                let runner_name = machine.runner_name();

                let start_timeout_elapsed = machine
                    .starting_duration()
                    .map(|rt| rt > START_TIMEOUT)
                    .unwrap_or(false);

                if start_timeout_elapsed {
                    error!("Runner {runner_name} on {triplet} failed to come up in time");

                    let machine_image_path = triplet.machine_image_path(base_dir_path);

                    machine.kill();

                    let broken_image_path = {
                        let mut filename = machine_image_path.file_name().unwrap().to_os_string();
                        filename.push(".broken");
                        machine_image_path.parent().unwrap().join(filename)
                    };

                    // Keep a copy of the broken image around for later investigation.
                    // But move the original away so that later invocations run from seed
                    // image again and hopefully succeed.
                    let res = std::fs::rename(&machine_image_path, &broken_image_path);

                    let mip = machine_image_path.display();
                    let bip = broken_image_path.display();

                    match res {
                        Ok(()) => info!("Retained broken machine image as {bip}"),
                        Err(e) if e.kind() == ErrorKind::NotFound => {
                            info!(
                                "Machine image {mip} not found. Machine likely started from seed."
                            )
                        }
                        Err(e) => error!("Failed to remove broken disk image {bip}: {e}"),
                    }
                }
            }
        }
    }

    pub async fn janitor(&self) -> std::io::Result<()> {
        loop {
            self.sweep().await;

            tokio::time::sleep(std::time::Duration::from_secs(15 * 60)).await;
        }
    }
}

impl Rescheduler {
    pub fn reschedule(&self) {
        self.manager.reschedule();
    }
}
