use std::path::PathBuf;

use serde::Deserialize;

use super::size_in_bytes::SizeInBytes;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub ram: SizeInBytes,
    pub base_dir: PathBuf,
}
