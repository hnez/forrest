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
use crate::{auth::Auth, config::ConfigFile};

// Machines should go from being booted to being registered with GitHub
// in less than 15 minutes.
// The timeout is quite generous because new machines have to download
// and unpack the runner binary first.
const START_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub struct Manager {
    auth: Arc<Auth>,
    config: Arc<ConfigFile>,
    machines: Arc<Mutex<Vec<Machine>>>,
}

impl Manager {
    pub fn new(config: Arc<ConfigFile>, auth: Arc<Auth>) -> Self {
        Self {
            auth,
            config,
            machines: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub(super) fn auth(&self) -> &Auth {
        &self.auth
    }

    pub(super) fn config(&self) -> &ConfigFile {
        &self.config
    }

    pub(super) fn modify_machine<F, R>(&self, runner_name: &str, fun: F) -> Option<R>
    where
        F: FnOnce(&mut Machine) -> R,
    {
        let mut machines = self.machines.lock().unwrap();

        let machine = machines
            .iter_mut()
            .find(|machine| machine.runner_name() == runner_name)?;

        Some(fun(machine))
    }

    pub(super) fn remove_machine(&self, runner_name: &str) -> Option<Machine> {
        let mut machines = self.machines.lock().unwrap();

        let index = machines
            .iter()
            .position(|machine| machine.runner_name() == runner_name)?;

        Some(machines.swap_remove(index))
    }

    pub fn status_feedback(&self, runner_name: &str, online: Option<bool>, busy: bool) -> bool {
        self.modify_machine(runner_name, |machine| machine.status_feedback(online, busy))
            .is_some()
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

        let mut machines = self.machines.lock().unwrap();

        // Remove machines where the supply surpasses the demand

        // We will traverse the list of machines from end to start and once
        // demand for a machine type reaches zero we will start killing
        // machines.
        // We'd rather kill machines that have not started yet / are not
        // already waiting for jobs, so we place those at the end of the
        // list.
        machines.sort_unstable_by_key(Machine::cost_to_kill);

        for i in (0..machines.len()).rev() {
            // Machines that are already servicing jobs do not count into the
            // supply/demand calculation.
            if !machines[i].status().is_available() {
                continue;
            }

            // Reduce the demand for this machine type by one.
            // If the demand is already zero, then kill the machine.
            match demand.get_mut(machines[i].triplet()) {
                Some(0) | None => {
                    let machine = machines.swap_remove(i);

                    machine.kill(true, self);
                }
                Some(count) => *count -= 1,
            }
        }

        // Add machines where the demand surpasses the supply

        for (triplet, count) in demand {
            let machine_config = self
                .config
                .repositories
                .get(triplet.owner())
                .and_then(|repos| repos.get(triplet.repository()))
                .and_then(|repo| repo.machines.get(triplet.machine_name()));

            let machine_config = match machine_config {
                Some(mc) => mc,
                None => {
                    error!("Got request for unkown machine triplet: {triplet}");
                    continue;
                }
            };

            for _ in 0..count {
                machines.push(Machine::new(triplet.clone(), machine_config.clone()));
            }
        }

        // We must release the lock before calling reschedule
        std::mem::drop(machines);
        self.reschedule();
    }

    pub(super) fn reschedule(&self) {
        let mut machines = self.machines.lock().unwrap();

        let mut ram_available = {
            let ram_total = self.config.host.ram.bytes();
            let ram_consumed = machines.iter().map(Machine::ram_consumed).sum();
            let ram_available = ram_total.saturating_sub(ram_consumed);

            debug!("Re-scheduling machines. {ram_available} of {ram_total} available");

            ram_available
        };

        // We want to prioritize scheduling jobs requiring a lot of RAM,
        // because they are harder to place if we start all smaller jobs first.
        machines.sort_unstable_by_key(Machine::ram_required);

        for machine in machines.iter_mut().rev() {
            if machine.status().is_started() {
                continue;
            }

            let ram_required = machine.ram_required();

            if ram_required > ram_available {
                debug!("Postpone starting {machine} due to insufficient RAM {ram_available} vs. {ram_required}");
                continue;
            }

            info!("Spawn {machine}");

            machine.spawn(self.clone());

            ram_available -= ram_required;
        }

        debug!("Machines and their new state:");

        for machine in machines.iter() {
            debug!("  - {machine}: {}", machine.status());
        }
    }

    async fn sweep(&self) {
        // Go through every user in our list ...
        for (owner, repos) in self.config.repositories.iter() {
            let octocrab = match self.auth.user(owner) {
                Some(oc) => oc,
                None => {
                    info!("Could not authenticate as {owner} (yet). Skipping");
                    continue;
                }
            };

            // ... visit each of their repositories ...
            'repo: for repository in repos.keys() {
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
                            continue 'repo;
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
                        let found = self.status_feedback(&runner_name, Some(online), busy);

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
        let mut machines = self.machines.lock().unwrap();

        let base_dir_path = Path::new(&self.config.host.base_dir);

        for index in (0..machines.len()).rev() {
            let machine = &machines[index];
            let triplet = machine.triplet();
            let runner_name = machine.runner_name();

            let starting = machine.status().is_starting();
            let start_timeout_elapsed = machine
                .runtime()
                .map(|rt| rt > START_TIMEOUT)
                .unwrap_or(false);

            if starting && start_timeout_elapsed {
                error!("Runner {runner_name} on {triplet} failed to come up in time");

                let machine_image_path = triplet.machine_image_path(base_dir_path);

                // Remove the machine from the list and kill it.
                let machine = machines.swap_remove(index);
                machine.kill(true, self);

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
                        info!("Machine image {mip} not found. Machine likely started from seed.")
                    }
                    Err(e) => error!("Failed to remove broken disk image {bip}: {e}"),
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
