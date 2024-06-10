use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use log::{debug, error, info, warn};
use octocrab::models::webhook_events::EventInstallation;
use octocrab::models::webhook_events::{WebhookEvent, WebhookEventPayload};
use octocrab::models::JobId;
use sha2::Sha256;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::ReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

use crate::config::ConfigFile;
use crate::execute::Scheduler;

use super::auth::Auth;

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);
const WEBHOOK_SIZE_LIMIT: u64 = 4 * 1024 * 1024;
const ERROR_RESPONSE: &[u8] = b"HTTP/1.1 400 Bad Request\r
Server: Forrest\r
Content-Length: 35\r
\r
Your request could not be processed
";
const OK_RESPONSE: &[u8] = b"HTTP/1.1 204 No Content\r
Server: Forrest\r
Content-Length: 0\r
\r
";

pub struct WebhookHandler {
    config: Arc<ConfigFile>,
    auth: Arc<Auth>,
    scheduler: Scheduler,
    listener: UnixListener,
}

impl WebhookHandler {
    pub fn new(
        config: Arc<ConfigFile>,
        auth: Arc<Auth>,
        scheduler: Scheduler,
    ) -> std::io::Result<Self> {
        let listener = {
            let path = Path::new(&config.host.base_dir).join("webhook.sock");

            let _ = std::fs::remove_file(&path);

            let listener = UnixListener::bind(&path)?;

            std::fs::set_permissions(path, Permissions::from_mode(0o777))?;

            listener
        };

        Ok(Self {
            config,
            auth,
            scheduler,
            listener,
        })
    }

    pub async fn run(&mut self) -> std::io::Result<()> {
        loop {
            let (sock, _) = self.listener.accept().await?;
            let config = self.config.clone();
            let auth = self.auth.clone();
            let scheduler = self.scheduler.clone();

            tokio::task::spawn(async move {
                let timeout_error = Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Handler function took too long to run",
                ));

                let res = timeout(
                    WEBHOOK_TIMEOUT,
                    webook_handler(sock, &config, &auth, scheduler),
                )
                .await
                .or(timeout_error);

                if let Err(err) = res {
                    warn!("Webhook handler failed due to: {err}");
                }
            });
        }
    }
}

async fn read_req<'a>(secret: &[u8], read: ReadHalf<'a>) -> std::io::Result<WebhookEvent> {
    // Limit the maxium request size and buffer the stream so we can read
    // individual bytes like when searching for a '\n'.
    let mut read = BufReader::new(read.take(WEBHOOK_SIZE_LIMIT));

    let mut line = String::new();

    read.read_line(&mut line).await?;

    if line.trim_end() != "POST /webhook HTTP/1.1" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Got unexpected request",
        ));
    }

    let mut content_length: Option<usize> = None;
    let mut event_type: Option<String> = None;
    let mut signature: Option<Vec<u8>> = None;

    loop {
        line.clear();
        read.read_line(&mut line).await?;
        line.make_ascii_lowercase();

        if line.trim().is_empty() {
            // We are done with the headers
            break;
        }

        if let Some(cl) = line.strip_prefix("content-length:") {
            content_length = cl.trim().parse().ok();
        }

        if let Some(et) = line.strip_prefix("x-github-event:") {
            event_type = Some(et.trim().to_owned());
        }

        if let Some(sig) = line
            .strip_prefix("x-hub-signature-256:")
            .and_then(|sig| sig.trim().strip_prefix("sha256="))
        {
            signature = hex::decode(sig).ok();
        }
    }

    let content_length = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Missing Content-Length header",
        )
    })?;

    let event_type = event_type.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Missing X-GitHub-Event header",
        )
    })?;

    let signature = signature.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Missing X-Hub-Signature-256 header",
        )
    })?;

    if (content_length as u64) > WEBHOOK_SIZE_LIMIT {
        Err(std::io::Error::other("Content-Length is too large"))?;
    }

    let content = {
        let mut content = vec![0; content_length];
        read.read_exact(&mut content).await?;

        let mut hmac: Hmac<Sha256> = Hmac::new_from_slice(secret).unwrap();
        hmac.update(&content);
        let content_valid = hmac.verify_slice(&signature);

        if content_valid.is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HMAC signature does not match",
            ));
        }

        content
    };

    WebhookEvent::try_from_header_and_body(&event_type, &content).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Failed to parse request body",
        )
    })
}

async fn workflow_job_handler(
    event: WebhookEvent,
    config: &ConfigFile,
    auth: &Auth,
    scheduler: Scheduler,
) {
    let job = match event.specific {
        WebhookEventPayload::WorkflowJob(job) => job,
        _ => return,
    };

    let owner = match event.sender {
        Some(author) => author.login,
        None => return,
    };

    let repo_name = match event.repository {
        Some(repo) => repo.name,
        None => return,
    };

    debug!("Got workflow_job webhook event for {owner}/{repo_name}!");

    let exists = config
        .repositories
        .get(&owner)
        .and_then(|repos| repos.get(&repo_name))
        .is_some();

    if !exists {
        info!("Refusing to service webhook from unlisted user/repo {owner}/{repo_name}");
        return;
    }

    let installation_id = match event.installation {
        Some(EventInstallation::Full(inst)) => inst.id,
        Some(EventInstallation::Minimal(inst)) => inst.id,
        None => {
            error!("Got webhook event that was not sent by an installation");
            return;
        }
    };

    let job_id = match job.workflow_job.get("id").and_then(|id| id.as_u64()) {
        Some(ji) => JobId(ji),
        None => return,
    };

    let status = job
        .workflow_job
        .get("status")
        .and_then(|status| status.as_str());

    if status != Some("queued") {
        return;
    }

    let labels = match job
        .workflow_job
        .get("labels")
        .and_then(|labels| labels.as_array())
    {
        Some(lb) => lb,
        None => return,
    };

    if labels.len() != 3 {
        debug!(
            "Ignoring job with {} != 3 labels on {owner}/{repo_name}",
            labels.len()
        );
        return;
    }

    if labels[0].as_str() != Some("self-hosted") || labels[1].as_str() != Some("forrest") {
        debug!("Ignoring job that does not have 'self-hosted' and 'forrest' as first labels on {owner}/{repo_name}");
        return;
    }

    let machine_name = match labels[2].as_str() {
        Some(mn) => mn,
        None => return,
    };

    let installation_octocrab = auth.installation(installation_id);

    let res = scheduler.push(
        &owner,
        &repo_name,
        machine_name,
        job_id,
        &installation_octocrab,
    );

    if res.is_err() {
        error!("Failed to setup job with machine type {machine_name} for {owner}/{repo_name}");
    } else {
        info!("Scheduler job with machine type {machine_name} for {owner}/{repo_name}");
    }

    scheduler.reschedule();
}

async fn webook_handler(
    mut sock: UnixStream,
    config: &ConfigFile,
    auth: &Auth,
    scheduler: Scheduler,
) -> std::io::Result<()> {
    let (read, mut write) = sock.split();

    let secret = config.github.webhook_secret.as_bytes();

    let response = match read_req(secret, read).await {
        Ok(res) => {
            workflow_job_handler(res, config, auth, scheduler).await;

            OK_RESPONSE
        }
        Err(e) => {
            error!("Got malformed webhook request: {e}");

            ERROR_RESPONSE
        }
    };

    write.write_all(response).await
}
