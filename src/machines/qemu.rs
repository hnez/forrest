use std::fs::{create_dir_all, File};
use std::io::ErrorKind;

use log::{error, info};
use octocrab::models::actions::SelfHostedRunnerJitConfig;
use reflink_copy::reflink;
use tokio::process::Command;

use super::{config_fs::ConfigFs, Triplet};
use crate::config::{ConfigFile, MachineConfig};

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

pub(super) async fn run(
    config: &ConfigFile,
    runner_name: &str,
    triplet: &Triplet,
    machine_config: &MachineConfig,
    jit_config: &SelfHostedRunnerJitConfig,
) -> std::io::Result<()> {
    let run_dir_path = {
        let path = triplet.run_dir_path(&config.host.base_dir, runner_name);

        create_dir_all(&path)?;

        path
    };

    // The seed dir contains the initial disk image and the scripts to set
    // up the machine and job.
    let seed_dir_path = config
        .host
        .base_dir
        .join("seeds")
        .join(&machine_config.seed);

    // Check if we already have a machine image for this machine or if
    // we need to start from a seed image.
    let machine_image_path = triplet.machine_image_path(&config.host.base_dir);

    let seed_image_path = {
        // Search for a *.img or *.raw file in the seed directory.

        let mut path = None;

        for entry in std::fs::read_dir(&seed_dir_path)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            let name = entry.file_name();
            let name_bytes = name.as_encoded_bytes();

            if meta.is_file() && (name_bytes.ends_with(b".img") || name_bytes.ends_with(b".raw")) {
                path = Some(entry.path());
                break;
            }
        }

        path.ok_or_else(|| {
            let sdp = seed_dir_path.display();
            let message = format!("No *.img or *.raw disk image found in seed directory {sdp}",);

            std::io::Error::new(ErrorKind::NotFound, message)
        })?
    };

    let base_image = {
        let machine_image_exists_and_is_newer = {
            let seed_image_modification = seed_image_path.metadata()?.modified()?;
            let machine_image_modification = machine_image_path
                .metadata()
                .and_then(|meta| meta.modified());

            machine_image_modification
                .map(|mim| mim > seed_image_modification)
                .unwrap_or(false)
        };

        if machine_image_exists_and_is_newer {
            &machine_image_path
        } else {
            &seed_image_path
        }
    };

    let disk_path = triplet.disk_image_path(&config.host.base_dir, runner_name);

    // Create a copy on write copy of the disk image using reflink
    reflink(base_image, &disk_path)?;

    // Grow the disk image if required
    let target_disk_size = machine_config.disk.bytes();
    let current_disk_size = disk_path.metadata()?.len();

    if current_disk_size < target_disk_size {
        let disk_file = File::options().append(true).open(&disk_path)?;
        disk_file.set_len(target_disk_size)?;
    }

    let substitutions = &[
        ("<REPO_OWNER>", triplet.owner()),
        ("<REPO_NAME>", triplet.repository()),
        ("<MACHINE_NAME>", triplet.machine_name()),
        ("<JITCONFIG>", jit_config.encoded_jit_config.as_str()),
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
        let ram = machine_config.ram.megabytes().to_string();
        let smp = machine_config.cpus.to_string();

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
        let code = status.code().map(|c| c.to_string());
        let printable_code = code.as_deref().unwrap_or("<None>");

        let msg = format!(
            "The qemu process for job {} {} exited with code: {}",
            triplet, runner_name, printable_code
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

        let persistence_token = config
            .repositories
            .get(triplet.owner())
            .and_then(|repos| repos.get(triplet.repository()))
            .map(|repo| repo.persistence_token.as_str())
            .unwrap_or("");

        if persistence_token.is_empty() {
            error!(
                "Could not find a persistence token for {} or it is empty",
                triplet
            );

            false
        } else {
            persistence_token == content
        }
    };

    if persist {
        let dip = disk_path.display();
        let mip = machine_image_path.display();

        info!("Persisting disk file {dip} as {mip}");

        let machine_image_dir = machine_image_path.parent().unwrap();
        std::fs::create_dir_all(machine_image_dir)?;
        std::fs::rename(disk_path, machine_image_path)?;
    }

    Ok(())
}
