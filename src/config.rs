use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Deserializer};

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

    pub fn megabytes(&self) -> u64 {
        self.0 / (1024 * 1024)
    }
}

#[derive(Debug, Deserialize)]
pub struct HostConfig {
    pub ram: SizeInBytes,
    pub base_dir: String,
}

#[derive(Debug, Deserialize)]
pub struct GitHubConfig {
    pub app_id: u64,
    pub jwt_key_file: String,
    pub webhook_secret: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MachineConfig {
    pub seed: String,
    pub ram: SizeInBytes,
    pub cpus: u32,
    pub disk: SizeInBytes,
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    pub persistence_token: String,
    pub machines: HashMap<String, MachineConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ConfigFile {
    pub host: HostConfig,
    pub github: GitHubConfig,
    pub machine_templates: HashMap<String, MachineConfig>,
    pub repositories: HashMap<String, HashMap<String, Repository>>,
}

impl ConfigFile {
    pub fn read<P: AsRef<Path>>(path: P) -> Arc<Self> {
        let mut fd = File::open(path).unwrap();

        let cfg = serde_yml::from_reader(&mut fd).unwrap();

        Arc::new(cfg)
    }
}
