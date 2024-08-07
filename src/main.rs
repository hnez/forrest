mod auth;
mod config;
mod ingres;
mod jobs;
mod machines;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    let config_path = {
        let mut args: Vec<String> = std::env::args().collect();

        match args.len() {
            1 => "config.yaml".to_owned(),
            2 => args.remove(1),
            _ => anyhow::bail!("Usage: {} [CONFIG]", args[0]),
        }
    };

    let config = config::ConfigFile::read(&config_path);

    // We use a private key to authenticate as a GitHub application
    // and derive installation tokens from it.
    // Use a central registry of cached installation tokens for efficiency.
    let auth = auth::Auth::new(&config)?;

    // The machine manager handles our virtual machines and their relation with GitHub.
    // It makes sure we only spawn as many VMs as the host can fit,
    // that all machines we spawn eventually register as runners on GitHub,
    // stopping machines that are no longer required because
    // persisting disk images, cleaning up stale runners etc. etc.
    let machine_manager = machines::Manager::new(config.clone(), auth.clone());

    // The job manager keeps track of build jobs and their status and
    // communicates the demand for machines with the machine manager.
    // It gets its updates from from the webhook handler and poller below.
    let job_manager = jobs::Manager::new(machine_manager.clone());

    // The main method to learn about new jobs to run is via webhooks.
    // These are POST requests sent by GitHub notifying us about events.
    let mut webhook =
        ingres::webhook::WebhookHandler::new(config.clone(), auth.clone(), job_manager.clone())?;

    // Our secondary source of information are periodic polls of the GitHub API.
    // These come in handy at startup or after network outages when we may have
    // missed webhooks.
    let poller = ingres::poll::Poller::new(config.clone(), auth.clone(), job_manager);

    // Make sure we can reach GitHub and our authentication works before
    // signaling readiness to systemd.
    poller.poll_once().await?;

    // Notify systemd that we are ready to handle requests
    if let Err(e) = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]) {
        log::info!("Failed to notify systemd about service startup: {e}");
    }

    log::info!("Startup complete. Handling requests");

    tokio::select! {
        res = machine_manager.janitor() => res,
        res = webhook.run() => res,
        res = poller.poll() => res,
    }?;

    Ok(())
}
