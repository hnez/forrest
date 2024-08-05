use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{collections::HashMap, time::Duration};

use serde::{Deserialize, Deserializer};

use crate::machines::Triplet;

#[derive(Clone, Copy, Debug)]
pub struct SizeInBytes(u64);

impl<'de> Deserialize<'de> for SizeInBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut size_str: String = Deserialize::deserialize(deserializer)?;

        let multiplier = match size_str.pop() {
            Some('B') => 1,
            Some('K') => 1024,
            Some('M') => 1024 * 1024,
            Some('G') => 1024 * 1024 * 1024,
            Some('T') => 1024 * 1024 * 1024 * 1024,
            _ => panic!("Failed to parse size string '{size_str}': unknown unit"),
        };

        let size: u64 = size_str
            .parse()
            .expect("Failed to parse size string '{size_str}': can not parse as u64");

        Ok(SizeInBytes(size * multiplier))
    }
}

impl SizeInBytes {
    pub fn bytes(&self) -> u64 {
        self.0
    }

    pub fn kilobyes(self) -> u64 {
        self.bytes() / 1024
    }

    pub fn megabytes(&self) -> u64 {
        self.kilobyes() / 1024
    }
}

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let mut duration_str: String = Deserialize::deserialize(deserializer)?;

    let unit = duration_str.pop();

    let multiplier = match unit {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 60 * 60,
        Some('d') => 24 * 60 * 60,
        _ => panic!("Failed to parse duration string '{duration_str}': unknown unit"),
    };

    let value: u64 = duration_str
        .parse()
        .expect("Failed to parse duration string '{duration_str}': can not parse as u64");

    Ok(Duration::from_secs(value * multiplier))
}

#[allow(unused)]
#[derive(Clone, Debug, Deserialize)]
pub struct ExposedDirectory {
    pub path: PathBuf,
    pub tag: String,
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Deserialize)]
pub struct HostConfig {
    pub ram: SizeInBytes,
    pub base_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct GitHubConfig {
    pub app_id: u64,
    pub jwt_key_file: String,
    pub webhook_secret: String,
    #[serde(deserialize_with = "deserialize_duration")]
    pub polling_interval: Duration,
}

#[derive(Clone, Debug, Deserialize)]
pub enum SeedOrBaseMachine {
    Seed(String),
    Base(Triplet),
}

#[derive(Clone, Debug, Deserialize)]
pub struct MachineConfig {
    #[serde(flatten)]
    pub image: SeedOrBaseMachine,
    pub ram: SizeInBytes,
    pub cpus: u32,
    pub disk: SizeInBytes,
    #[serde(default)]
    pub shared_directories: Vec<ExposedDirectory>,
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    pub persistence_token: Option<String>,
    pub machines: HashMap<String, MachineConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ConfigFile {
    pub host: HostConfig,
    pub github: GitHubConfig,
    pub repositories: HashMap<String, HashMap<String, Repository>>,
}

impl ConfigFile {
    pub fn read<P: AsRef<Path>>(path: P) -> Arc<Self> {
        let mut fd = File::open(path).unwrap();

        let cfg = {
            // First we read the config file as generic serde_yml Value.
            let mut cfg: serde_yml::Value = serde_yml::from_reader(&mut fd).unwrap();

            // Then we apply merges / overrides like these:
            //
            // machine_templates:
            //   small: &machine-small
            //     ram: 8G
            //     …
            //   large: &machine-large
            //     << : *machine-small
            //     ram: 32G
            //
            cfg.apply_merge().unwrap();

            if let Some(cfg_mapping) = cfg.as_mapping_mut() {
                // Remove all top level fields from the config who's name ends
                // in `_templates`.
                // This allows using keys like `machine-templates` which do not
                // adhere to the syntax.

                cfg_mapping.retain(|k, _| {
                    k.as_str()
                        .map(|k| !k.ends_with("_templates"))
                        .unwrap_or(true)
                });
            }

            // And then we convert to our config format.
            serde_yml::from_value(cfg).unwrap()
        };

        Arc::new(cfg)
    }
}
