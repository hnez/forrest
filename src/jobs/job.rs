use octocrab::models::workflows::Status;
use octocrab::models::JobId;

use crate::machines::Triplet;

pub(super) struct Job {
    triplet: Triplet,
    id: JobId,
    status: Status,
}

impl Job {
    pub(super) fn new(triplet: Triplet, id: JobId, status: Status) -> Self {
        Self {
            triplet,
            id,
            status,
        }
    }

    pub(super) fn triplet(&self) -> &Triplet {
        &self.triplet
    }

    pub(super) fn id(&self) -> JobId {
        self.id
    }

    pub(super) fn is_queued(&self) -> bool {
        matches!(self.status, Status::Queued)
    }

    pub(super) fn update_status(&mut self, status: Status) -> bool {
        if self.status != status {
            self.status = status;
            true
        } else {
            false
        }
    }
}
