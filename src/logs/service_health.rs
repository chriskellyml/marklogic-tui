//! A disabled or unavailable eval endpoint must not cause a polling storm.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    Disabled,
    Unavailable,
}

fn failure(error: &str) -> Option<Failure> {
    let error = error.to_ascii_lowercase();
    if error.contains("appserver disabled") || error.contains("app server is disabled") {
        Some(Failure::Disabled)
    } else if error.contains("503")
        && (error.contains("unavailable")
            || error.contains("http 503")
            || error.contains("status 503"))
        || error.contains("connection refused")
        || error.contains("error sending request")
        || error.contains("timed out")
    {
        Some(Failure::Unavailable)
    } else {
        None
    }
}

#[derive(Default)]
pub(super) struct ServiceHealth {
    failure: Option<Failure>,
    failed_at: Option<Instant>,
    retry_at: Option<Instant>,
    failures: u8,
}

impl LogViewer {
    pub(super) fn automatic_collection_allowed(&self) -> bool {
        match self.service_health.failure {
            None => true,
            Some(Failure::Disabled) => false,
            Some(Failure::Unavailable) => self
                .service_health
                .retry_at
                .is_some_and(|time| Instant::now() >= time),
        }
    }

    pub(super) fn note_service_failure(&mut self, error: &str) {
        let Some(kind) = failure(error) else { return };
        let now = Instant::now();
        // A timeout from another in-flight request does not undo a confirmed
        // disabled AppServer; only a subsequent successful attempt can do that.
        if self.service_health.failure == Some(Failure::Disabled) && kind != Failure::Disabled {
            return;
        }
        let already_waiting = self.service_health.failure == Some(kind)
            && self.service_health.retry_at.is_some_and(|time| time > now);
        self.service_health.failure = Some(kind);
        self.service_health.failed_at = Some(now);
        if kind == Failure::Disabled {
            self.service_health.retry_at = None;
        } else if !already_waiting {
            self.service_health.failures = self.service_health.failures.saturating_add(1);
            let seconds =
                (60u64 * (1u64 << self.service_health.failures.saturating_sub(1).min(3))).min(300);
            self.service_health.retry_at = Some(now + Duration::from_secs(seconds));
        }
    }

    pub(super) fn note_service_success(&mut self) {
        if let Some(failed) = self.service_health.failed_at {
            // An old in-flight success cannot prove that a newer outage ended.
            if !self
                .activity
                .as_ref()
                .is_some_and(|activity| activity.started >= failed)
            {
                return;
            }
        }
        self.service_health = ServiceHealth::default();
        if let Some(client) = &self.client
            && self
                .errors
                .get(&client.server.host)
                .is_some_and(|error| failure(error).is_some())
        {
            self.errors.remove(&client.server.host);
        }
    }

    pub(crate) fn service_status(&self) -> Option<String> {
        let endpoint = self
            .client
            .as_ref()
            .map(|client| client.server.base_url())
            .unwrap_or_default();
        match self.service_health.failure? {
            Failure::Disabled => Some(format!(
                "AppServer disabled (HTTP 503) at {endpoint} · automatic collection paused · !: errors · R: retry"
            )),
            Failure::Unavailable => {
                let seconds = self.service_health.retry_at.map_or(0, |time| {
                    time.saturating_duration_since(Instant::now()).as_secs()
                });
                Some(format!(
                    "Log service unavailable at {endpoint} · automatic retry in {seconds}s · !: errors · R: retry now"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_service_pauses_until_a_new_explicit_request_succeeds() {
        let mut logs = LogViewer::default();
        logs.begin_activity("Catching up tracked logs", 9);
        logs.note_service_failure("Log request failed: Appserver Disabled (HTTP 503)");
        assert!(!logs.automatic_collection_allowed());
        logs.note_service_failure("Log request timed out");
        assert!(
            logs.service_status()
                .unwrap()
                .contains("AppServer disabled")
        );
        logs.note_service_success();
        assert!(
            !logs.automatic_collection_allowed(),
            "an old concurrent success must not clear a newer outage"
        );
        logs.begin_activity("Discovering logs", 1);
        logs.note_service_success();
        assert!(logs.automatic_collection_allowed());
        assert!(logs.service_status().is_none());
    }

    #[test]
    fn unavailable_service_backs_off_once_per_batch_and_increases_after_retry() {
        let mut logs = LogViewer::default();
        logs.note_service_failure("HTTP 503 Service Unavailable");
        let first = logs.service_health.retry_at.unwrap();
        assert!(!logs.automatic_collection_allowed());
        assert!(first.saturating_duration_since(Instant::now()) > Duration::from_secs(50));
        for _ in 0..8 {
            logs.note_service_failure("HTTP 503 Service Unavailable");
        }
        assert_eq!(logs.service_health.retry_at, Some(first));
        logs.service_health.retry_at = Some(Instant::now() - Duration::from_secs(1));
        assert!(logs.automatic_collection_allowed());
        logs.note_service_failure("HTTP 503 Service Unavailable");
        assert!(
            logs.service_health
                .retry_at
                .unwrap()
                .saturating_duration_since(Instant::now())
                > Duration::from_secs(110)
        );
    }
}
