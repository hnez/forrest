use std::fs::create_dir_all;
use std::fs::File;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use log::{debug, error, info};
use octocrab::models::JobId;
use octocrab::models::RunnerGroupId;
use reflink_copy::reflink;
use tokio::process::Command;
use tokio::task;

use super::Scheduler;
use crate::config::MachineConfig;

use super::config_fs::ConfigFs;

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
    &["-device", "pci-serial-2x,chardev1=bootlog,chardev2=telnet"],
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

const JOB_CONFIG_IMAGE_SIZE: u64 = 1_000_000;
const JOB_CONFIG_IMAGE_LABEL: &str = "JOBDATA";
const CLOUD_INIT_IMAGE_SIZE: u64 = 1_000_000;
const CLOUD_INIT_IMAGE_LABEL: &str = "CIDATA";

pub(super) struct Job {
    pub(super) owner: String,
    pub(super) repo_name: String,
    pub(super) machine_name: String,
    pub(super) persistence_token: String,
    pub(super) machine: MachineConfig,
    pub(super) job_id: JobId,
    pub(super) installation_octocrab: Arc<octocrab::Octocrab>,
    pub(super) timestamp: SystemTime,
}

impl Job {
    async fn run(
        self: &Arc<Self>,
        base_dir_path: &Path,
        run_dir_path: &Path,
        disk_path: &Path,
    ) -> std::io::Result<()> {
        // Check if we already have a machine image for this machine or if
        // we need to start from a seed image.
        let machine_image_path = base_dir_path
            .join("machines")
            .join(&self.owner)
            .join(&self.repo_name)
            .join(format!("{}.img", self.machine_name));

        let seed_dir_path = base_dir_path.join("seeds").join(&self.machine.seed);

        let seed_image_path = {
            // Search for a *.img or *.raw file in the seed directory.

            let mut path = None;

            for entry in std::fs::read_dir(&seed_dir_path)? {
                let entry = entry?;
                let meta = entry.metadata()?;
                let name = entry.file_name();
                let name_bytes = name.as_encoded_bytes();

                if meta.is_file()
                    && (name_bytes.ends_with(b".img") || name_bytes.ends_with(b".raw"))
                {
                    path = Some(entry.path());
                    break;
                }
            }

            path.ok_or_else(|| {
                let message = format!(
                    "No *.img or *.raw disk image found in seed directory {}",
                    seed_dir_path.to_string_lossy()
                );

                std::io::Error::new(ErrorKind::NotFound, message)
            })?
        };

        let base_image = {
            if machine_image_path.is_file() {
                &machine_image_path
            } else {
                &seed_image_path
            }
        };

        // Create a copy on write copy of the disk image using reflink
        reflink(base_image, disk_path)?;

        // Grow the disk image if required
        let target_disk_size = self.machine.disk.bytes();
        let current_disk_size = disk_path.metadata()?.len();

        if current_disk_size < target_disk_size {
            let disk_file = File::options().append(true).open(disk_path)?;
            disk_file.set_len(target_disk_size)?;
        }

        let jit_config = {
            let runner_name = {
                let machine_name = &self.machine_name;
                let ts = self
                    .timestamp
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_millis();

                format!("forrest-{machine_name}-{ts}")
            };

            let labels = vec![
                "self-hosted".to_owned(),
                "forrest".to_owned(),
                self.machine_name.clone(),
            ];

            let runner_group = RunnerGroupId(1);

            self.installation_octocrab
                .actions()
                .create_repo_jit_runner_config(
                    &self.owner,
                    &self.repo_name,
                    runner_name,
                    runner_group,
                    labels,
                )
                .send()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?
        };

        let substitutions = &[
            ("<REPO_OWNER>", self.owner.as_str()),
            ("<REPO_NAME>", self.repo_name.as_str()),
            ("<MACHINE_NAME>", self.machine_name.as_str()),
            ("JITCONFIG", jit_config.encoded_jit_config.as_str()),
        ];

        // We need to keep a reference to `_cloud_init` around even though
        // we do not plan to inspect it because the file is removed once
        // it is dropped.
        let _cloud_init = {
            let cloud_init_path = run_dir_path.join("cloud-init.img");
            let cloud_init_template_path = seed_dir_path.join("cloud-init");

            ConfigFs::new(
                cloud_init_path,
                CLOUD_INIT_IMAGE_SIZE,
                CLOUD_INIT_IMAGE_LABEL,
                cloud_init_template_path,
                substitutions,
            )?
        };

        let job_config = {
            let job_config_path = run_dir_path.join("job-config.img");
            let job_config_template_path = seed_dir_path.join("job-config");

            ConfigFs::new(
                job_config_path,
                JOB_CONFIG_IMAGE_SIZE,
                JOB_CONFIG_IMAGE_LABEL,
                job_config_template_path,
                substitutions,
            )?
        };

        let mut qemu = {
            let ram = self.machine.ram.megabytes().to_string();
            let smp = self.machine.cpus.to_string();

            let mut qemu = Command::new("/usr/bin/qemu-system-x86_64");

            qemu.kill_on_drop(true)
                .current_dir(run_dir_path)
                .arg("-m")
                .arg(&ram)
                .arg("-smp")
                .arg(&smp)
                .args(QEMU_ARGS.iter().flat_map(|arg_list| *arg_list));

            qemu
        };

        let status = qemu.status().await?;

        if !status.success() {
            let code = status.code().unwrap_or(0);
            let msg = format!(
                "The qemu process for job {}/{}#{} exited with code: {code}",
                self.owner, self.repo_name, self.job_id
            );

            return Err(std::io::Error::other(msg));
        }

        // Did the job leave the correct token in the config filesystem
        // to indicate that it is allowed to persist the disk image as new
        // machine image?
        let persist = {
            let persistence_file_content = job_config
                .inspect()?
                .read_file("persist")
                .unwrap_or_default();

            let content = std::str::from_utf8(&persistence_file_content)
                .unwrap_or("")
                .trim();

            self.persistence_token == content
        };

        if persist {
            let machine_image_dir = machine_image_path.parent().unwrap();

            info!(
                "Persisting disk file {} as {}",
                disk_path.to_string_lossy(),
                machine_image_path.to_string_lossy()
            );

            std::fs::create_dir_all(machine_image_dir)?;
            std::fs::rename(disk_path, machine_image_path)?;
        }

        Ok(())
    }

    pub(super) fn spawn(self: Arc<Self>, scheduler: Scheduler) {
        let job = self.clone();

        let base_dir_path = Path::new(&scheduler.config().host.base_dir).to_owned();

        let run_dir_path = {
            let ts = self
                .timestamp
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                .to_string();

            let path = base_dir_path
                .join("runs")
                .join(&self.owner)
                .join(&self.repo_name)
                .join(&self.machine_name)
                .join(ts);

            if let Err(e) = create_dir_all(&path) {
                error!(
                    "Failed to create run directory: {}: {e}",
                    path.to_string_lossy()
                );
                return;
            }

            path
        };

        let disk_path = run_dir_path.join("disk.img");

        task::spawn(async move {
            if let Err(e) = job.run(&base_dir_path, &run_dir_path, &disk_path).await {
                error!("Failed to run job: {e}");
            }

            let disk_path_str = disk_path.to_string_lossy();

            match std::fs::remove_file(&disk_path) {
                Ok(()) => debug!("Removed disk file {disk_path_str}"),
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    debug!("Disk file {disk_path_str} was already removed")
                }
                Err(e) => error!("Failed to remove disk image {disk_path_str}: {e}"),
            }

            scheduler.pop(&job);
            scheduler.reschedule();
        });
    }
}
