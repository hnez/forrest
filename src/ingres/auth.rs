use std::sync::{Arc, Mutex};

use octocrab::models::InstallationId;
use octocrab::Octocrab;

use crate::config::ConfigFile;

pub struct Auth {
    app: Arc<Octocrab>,
    installations: Mutex<Vec<(u64, Arc<Octocrab>)>>,
}

impl Auth {
    pub fn new(config: &ConfigFile) -> anyhow::Result<Arc<Self>> {
        let app_id = octocrab::models::AppId(config.github.app_id);
        let token = {
            let pem = std::fs::read(&config.github.jwt_key_file)?;
            jsonwebtoken::EncodingKey::from_rsa_pem(&pem)?
        };

        let app = Arc::new(octocrab::Octocrab::builder().app(app_id, token).build()?);

        let installations = Mutex::new(Vec::new());

        let auth = Self { app, installations };

        Ok(Arc::new(auth))
    }

    pub(super) fn app(&self) -> Arc<Octocrab> {
        self.app.clone()
    }

    pub(super) fn installation(&self, id: InstallationId) -> Arc<Octocrab> {
        let mut cache = self.installations.lock().unwrap();

        let cached_inst_octocrab = cache
            .iter()
            .find_map(|(iid, oc)| (*iid == id.0).then_some(oc));

        match cached_inst_octocrab {
            Some(oc) => oc.clone(),
            None => {
                let oc = Arc::new(self.app.installation(id));
                cache.push((id.0, oc.clone()));
                oc
            }
        }
    }
}
