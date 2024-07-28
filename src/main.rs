mod config;
//mod execute;
//mod ingres;
mod auth;
mod jobs;
mod machines;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    let config = config::ConfigFile::read("config.yaml");

    // We use a private key to authenticate as a GitHub application
    // and derive installation tokens from it.
    // Use a central registry of cached installation tokens for efficiency.
    let auth = auth::Auth::new(&config)?;

    // The scheduler decides when to start new virtual machines to handle jobs.
    // let scheduler = execute::Scheduler::new(config.clone());

    // The main method to learn about new jobs to run is via webhooks.
    // These are POST requests sent by GitHub notifying us about events.
    //let mut webhook =
    //    ingres::webhook::WebhookHandler::new(config.clone(), auth.clone(), scheduler.clone())?;

    // Our secondary source of information are periodic polls of the GitHub API.
    // These come in handy at startup or after network outages when we may have
    // missed webhooks.
    //let poller = ingres::poll::Poller::new(config.clone(), auth.clone(), scheduler.clone());

    // Notify systemd that we are ready to handle requests
    if let Err(e) = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]) {
        log::info!("Failed to notify systemd about service startup: {e}");
    }

    log::info!("Startup complete. Handling requests");

    let machine_manager = machines::Manager::new(config.clone(), auth.clone());
    let job_manager = jobs::Manager::new(machine_manager.clone());

    let triplet = machines::Triplet::new("hnez", "forrest", "build");
    let job_id = octocrab::models::JobId(0);
    let status = octocrab::models::workflows::Status::Queued;

    job_manager.status_feedback(&triplet, job_id, status, None);

    tokio::select! {
        res = machine_manager.janitor() => res,
        //res = webhook.run() => res,
        //res = poller.run() => res,
    }?;

    Ok(())
}
