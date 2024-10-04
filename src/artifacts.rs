use std::fs::{create_dir_all, remove_file, rename};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::bail;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use log::{debug, trace, warn};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use crate::machines::{Artifact, Manager as MachineManager};

pub struct ArtifactsHandler {
    machine_manager: MachineManager,
}

fn tokens(request: &Request<Incoming>) -> (String, String) {
    let (scheme, run_token, extra_token) = {
        let authorization = request
            .headers()
            .get("Authorization")
            .and_then(|auth| auth.to_str().ok())
            .unwrap_or("");

        let mut components = authorization.split_ascii_whitespace();

        (components.next(), components.next(), components.next())
    };

    if scheme != Some("Bearer") {
        return ("".into(), "".into());
    }

    (
        run_token.unwrap_or("").into(),
        extra_token.unwrap_or("").into(),
    )
}

fn path_components(request: &Request<Incoming>) -> Option<(String, PathBuf)> {
    // input: "/artifact/<artifact store name>//<a>/<b>..."
    // split: ["", "artifact", "<artifact store name>", "", "<a>", "<b>" ...]
    // filter: ["artifact", "<artifact store name>", "a", "b"]
    let mut components = request.uri().path().split('/').filter(|c| !c.is_empty());

    let _prefix = components.next();
    let name = components.next().map(|name| name.to_string())?;

    // Assemble a PathBuf from the trailing path components,
    // but return None if any of them is "." or ".." to prevent path traversal.
    let path: Option<PathBuf> = components
        .map(|c| (c != "." && c != "..").then_some(c))
        .collect();

    let path = path?;

    // Prevent paths that are completely empty
    if path.as_os_str().is_empty() {
        return None;
    }

    Some((name, path))
}

async fn body_to_disk<'a>(
    mut body: Incoming,
    fs_path: &Path,
    fs_path_tmp: &Path,
    artifact: &Artifact<'a>,
) -> anyhow::Result<()> {
    if let Some(parent) = fs_path.parent() {
        create_dir_all(parent)?;
    }

    let mut file = File::create(&fs_path_tmp).await?;

    while let Some(frame) = body.frame().await {
        let frame = frame?;
        let data: &[u8] = match frame.data_ref() {
            Some(data) => data,
            None => continue,
        };

        trace!(
            "Read {} bytes to be written to {}",
            data.len(),
            fs_path_tmp.display()
        );

        if !artifact.consume_quota(data.len() as u64) {
            bail!("Quota exceeded");
        }

        file.write_all(data).await?;
    }

    file.sync_all().await?;

    rename(fs_path_tmp, fs_path)?;

    Ok(())
}

impl ArtifactsHandler {
    pub fn new(machine_manager: MachineManager) -> Self {
        Self { machine_manager }
    }

    pub async fn handle(&self, request: Request<Incoming>) -> anyhow::Result<Response<String>> {
        if request.method() != Method::PUT {
            return Ok(Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .body("Only artifact upload is implemented".into())
                .unwrap());
        }

        let (run_token, extra_token) = tokens(&request);
        let (name, req_path) = match path_components(&request) {
            Some(np) => np,
            None => {
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body("Request did not contain artifact store name or valid path".into())
                    .unwrap());
            }
        };

        let machine = match self.machine_manager.machine_by_run_token(&run_token) {
            Some(machine) => machine,
            None => {
                return Ok(Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body("Provided run token does not belong to a known machine".into())
                    .unwrap());
            }
        };

        let artifact = match machine.artifact(&name, &extra_token) {
            Some(artifact) => artifact,
            None => {
                return Ok(Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body("The requested artifact is not configured for this machine type".into())
                    .unwrap());
            }
        };

        // From the PathBuf::push() documentation:
        // "If path is absolute, it replaces the current path".
        // So we have to make very sure that the path is always relative.
        assert!(req_path.is_relative());

        let fs_path = {
            let mut path = artifact.path();
            path.push(&req_path);
            path
        };

        let fs_path_tmp = {
            let mut suffix = b".tmp-frst-".to_vec();
            suffix.extend(thread_rng().sample_iter(&Alphanumeric).take(5));
            let suffix = String::from_utf8(suffix).unwrap();

            let mut path = fs_path.to_path_buf();
            path.as_mut_os_string().push(suffix);
            path
        };

        let body = request.into_body();

        match body_to_disk(body, &fs_path, &fs_path_tmp, &artifact).await {
            Ok(()) => {
                debug!("Saved artifact for {machine} as {}", fs_path.display());

                let url = {
                    let mut url = artifact.url().into_bytes();
                    url.extend(req_path.as_os_str().as_bytes());
                    url
                };

                Ok(Response::builder()
                    .status(StatusCode::CREATED)
                    .header("Content-Location", url)
                    .body("".into())
                    .unwrap())
            }
            Err(e) => {
                warn!(
                    "Failed to save artifact for {machine} as {}: {e}",
                    fs_path.display()
                );

                // Best effort cleanup of the files we created
                let _ = remove_file(fs_path_tmp);
                let _ = remove_file(fs_path);

                Ok(Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body("Failed to store artifact to disk".into())
                    .unwrap())
            }
        }
    }
}