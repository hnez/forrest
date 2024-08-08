use std::fs::{create_dir_all, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use log::{debug, error, info};
use reflink_copy::reflink;

use crate::config::SeedBasePolicy;

use super::config_fs::ConfigFs;
use super::machine::Machine;
use super::manager::Machines;

const JOB_CONFIG_IMAGE_SIZE: u64 = 1_000_000;
const JOB_CONFIG_IMAGE_LABEL: &str = "JOBDATA";
const CLOUD_INIT_IMAGE_SIZE: u64 = 1_000_000;
const CLOUD_INIT_IMAGE_LABEL: &str = "CIDATA";

pub(super) struct RunDir {
    run_dir: PathBuf,
    machine_image: PathBuf,
    disk: PathBuf,
    _cloud_init: ConfigFs,
    job_config: Option<ConfigFs>,
    persistence_token: Option<String>,
}

fn not_found_none<V>(res: std::io::Result<V>) -> std::io::Result<Option<V>> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn pick_newer<'p>(a: &'p Path, b: &'p Path) -> std::io::Result<&'p Path> {
    let modified_a = not_found_none(a.metadata().and_then(|meta| meta.modified()))?;
    let modified_b = not_found_none(b.metadata().and_then(|meta| meta.modified()))?;

    // Pick the newer of the two files if both exist.
    // Pick b if it exists but a does not.
    // Else pick a, regardless of it existing or not.

    match (modified_a, modified_b) {
        (Some(ma), Some(mb)) => Ok(if ma > mb { a } else { b }),
        (None, Some(_)) => Ok(b),
        (Some(_), None) | (None, None) => Ok(a),
    }
}

fn find_seed_image(seed_dir: &Path) -> std::io::Result<PathBuf> {
    // Search for a *.img or *.raw file in the seed directory.

    for entry in std::fs::read_dir(seed_dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let name = entry.file_name();
        let name_bytes = name.as_encoded_bytes();

        if meta.is_file() && (name_bytes.ends_with(b".img") || name_bytes.ends_with(b".raw")) {
            return Ok(entry.path());
        }
    }

    let msg = format!(
        "No *.img or *.raw disk image found in seed directory {}",
        seed_dir.display()
    );

    Err(std::io::Error::new(ErrorKind::NotFound, msg))
}

impl RunDir {
    pub(super) fn new(machine: &Machine, machines: &Machines) -> std::io::Result<Option<Self>> {
        let triplet = machine.triplet();
        let cfg = machine.cfg();
        let machine_config = machine.machine_config();

        let base_dir = &cfg.host.base_dir;

        let seed_dir = cfg.host.base_dir.join("seeds").join(&machine_config.seed);

        let base_image = match &machine_config.base {
            Some(base_triplet) if machines.contains_key(base_triplet) => {
                info!("Delaying the startup of {machine} because its base {base_triplet} is currently running");
                return Ok(None);
            }
            Some(base_triplet) => base_triplet.machine_image_path(base_dir),
            None => find_seed_image(&seed_dir)?,
        };

        let machine_image = triplet.machine_image_path(base_dir);

        let image = match machine_config.use_base {
            SeedBasePolicy::IfNewer => pick_newer(&base_image, &machine_image)?,
            SeedBasePolicy::Always => &base_image,
            SeedBasePolicy::Never => &machine_image,
        };

        if !image.try_exists()? {
            info!(
                "Delaying the startup of {machine} because the image {} does not exist (yet)",
                image.display()
            );
            return Ok(None);
        }

        let encoded_jit_config = machine.encoded_jit_config().ok_or_else(|| {
            let msg = format!("Failed to set up run dir for {machine} due to missing jit config");
            std::io::Error::other(msg)
        })?;

        let persistence_token = cfg
            .repositories
            .get(triplet.owner())
            .and_then(|repos| repos.get(triplet.repository()))
            .and_then(|repo| repo.persistence_token.clone());

        let run_dir = triplet.run_dir_path(&cfg.host.base_dir, machine.runner_name());

        create_dir_all(&run_dir)?;

        let disk = run_dir.join("disk.img");

        // Create a copy on write copy of the disk image using reflink
        reflink(image, &disk)?;

        // Grow the disk image if required
        let target_disk_size = machine_config.disk.bytes();
        let current_disk_size = disk.metadata()?.len();

        if current_disk_size < target_disk_size {
            let disk_file = File::options().append(true).open(&disk)?;
            disk_file.set_len(target_disk_size)?;
        }

        let substitutions = &[
            ("<REPO_OWNER>", triplet.owner()),
            ("<REPO_NAME>", triplet.repository()),
            ("<MACHINE_NAME>", triplet.machine_name()),
            ("<JITCONFIG>", encoded_jit_config.as_str()),
        ];

        let _cloud_init = {
            let cloud_init_path = run_dir.join("cloud-init.img");
            let cloud_init_template_path = seed_dir.join("cloud-init");

            ConfigFs::new(
                cloud_init_path,
                CLOUD_INIT_IMAGE_SIZE,
                CLOUD_INIT_IMAGE_LABEL,
                cloud_init_template_path,
                substitutions,
            )?
        };

        let job_config = {
            let job_config_path = run_dir.join("job-config.img");
            let job_config_template_path = seed_dir.join("job-config");

            ConfigFs::new(
                job_config_path,
                JOB_CONFIG_IMAGE_SIZE,
                JOB_CONFIG_IMAGE_LABEL,
                job_config_template_path,
                substitutions,
            )?
        };

        let dir = Self {
            run_dir,
            machine_image,
            disk,
            _cloud_init,
            job_config: Some(job_config),
            persistence_token,
        };

        Ok(Some(dir))
    }

    pub(super) fn path(&self) -> &Path {
        &self.run_dir
    }

    pub(super) fn maybe_persist(&mut self) {
        let persistence_token = match &self.persistence_token {
            Some(pt) => pt.as_bytes(),
            None => return,
        };

        let dds = self.disk.display();
        let mds = self.machine_image.display();

        let inspector = match self.job_config.take().unwrap().inspect() {
            Ok(inspector) => inspector,
            Err(err) => {
                error!(
                    "Failed to inspect job config image. Will not persist {dds} to {mds}: {err}"
                );
                return;
            }
        };

        let persist_file_content = {
            let mut buf = vec![0; persistence_token.len()];

            match inspector.read_file("persist", &mut buf) {
                Ok(()) => buf,
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    info!("Job did not leave a persist file. Will not persist {dds} to {mds}");
                    return;
                }
                Err(err) => {
                    error!("Failed to read persist file. Will not persist {dds} to {mds}: {err}");
                    return;
                }
            }
        };

        if persist_file_content != persistence_token {
            error!("Job left a persist file, but it does not match the token.");
            error!("Will not persist {dds} to {mds}");
            return;
        }

        let machine_image_dir = self.machine_image.parent().unwrap();

        if let Err(err) = std::fs::create_dir_all(machine_image_dir) {
            let mdds = machine_image_dir.display();

            error!("Failed to create machine image dir {mdds}: {err}");
            return;
        }

        if let Err(err) = std::fs::rename(&self.disk, &self.machine_image) {
            error!("Failed to move image from {dds} to {mds}: {err}");
            return;
        }

        info!("Persisted disk file {dds} as {mds}");
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let disk = self.run_dir.join("disk.img");
        let ds = disk.display();

        match std::fs::remove_file(&disk) {
            Ok(()) => debug!("Removed disk file {ds}"),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                debug!("Disk file {ds} was already removed")
            }
            Err(e) => error!("Failed to remove disk image {ds}: {e}"),
        }
    }
}
