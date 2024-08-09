use std::time::Duration;

use serde::Deserialize;

use super::duration_human;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubConfig {
    pub app_id: u64,
    pub jwt_key_file: String,
    pub webhook_secret: String,
    #[serde(deserialize_with = "duration_human::deserialize")]
    pub polling_interval: Duration,
}
