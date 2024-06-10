use std::path::PathBuf;

use serde::Serialize;
use serde_yml::to_writer;

use super::config_fs::ConfigFs;

const IMAGE_SIZE: u64 = 1_000_000;
const IMAGE_LABEL: &str = "CIDATA";

#[derive(Serialize)]
struct User {
    name: String,
    sudo: String,
}

#[derive(Serialize, Default)]
struct UserDataFile {
    users: Vec<User>,
    runcmd: Vec<String>,
}

#[derive(Serialize)]
struct MetaDataFile {
    #[serde(rename = "local-hostname")]
    hostname: String,
}

pub struct CloudInit {
    user_data: UserDataFile,
    meta_data: MetaDataFile,
}

impl CloudInit {
    pub fn new(hostname: &str) -> Self {
        let user_data = UserDataFile::default();
        let meta_data = MetaDataFile {
            hostname: hostname.to_owned(),
        };

        Self {
            user_data,
            meta_data,
        }
    }

    pub fn add_user(mut self, name: &str, sudo: &str) -> Self {
        let user = User {
            name: name.to_owned(),
            sudo: sudo.to_owned(),
        };

        self.user_data.users.push(user);

        self
    }

    pub fn add_command(mut self, command: &str) -> Self {
        self.user_data.runcmd.push(command.to_owned());

        self
    }

    pub fn finish(self, path: PathBuf) -> std::io::Result<ConfigFs> {
        let user_data = {
            let mut ud = "#cloud-config\n\n".to_string().into_bytes();
            to_writer(&mut ud, &self.user_data).unwrap();
            ud
        };
        let meta_data = {
            let mut md = "#cloud-config\n\n".to_string().into_bytes();
            to_writer(&mut md, &self.meta_data).unwrap();
            md
        };

        ConfigFs::builder(path, IMAGE_SIZE, IMAGE_LABEL)?
            .add_file("user-data", &user_data)?
            .add_file("meta-data", &meta_data)?
            .build()
    }
}
