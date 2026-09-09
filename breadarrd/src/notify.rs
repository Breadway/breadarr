use serde::Serialize;

#[derive(Serialize)]
struct Payload<'a> {
    title: &'a str,
    message: &'a str,
}

/// Best-effort push notification: failures are logged, never propagated.
/// The events this fires for (a review-queue item, a run of import
/// failures, the search loop halting) already have a durable home in
/// `event_history`/the TUI; the notification is a convenience nudge on top
/// of that, not the record of truth, so it should never be able to fail an
/// otherwise-successful grab/import/search cycle.
pub struct Notifier {
    client: reqwest::Client,
    webhook_url: String,
}

impl Notifier {
    /// Returns `None` when `webhook_url` is empty: callers hold an
    /// `Option<Notifier>` and simply skip notifying rather than every call
    /// site needing its own empty-string check.
    pub fn new(webhook_url: &str) -> Option<Self> {
        if webhook_url.is_empty() {
            return None;
        }
        Some(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client build"),
            webhook_url: webhook_url.to_string(),
        })
    }

    pub async fn send(&self, title: &str, message: &str) {
        let result = self
            .client
            .post(&self.webhook_url)
            .json(&Payload { title, message })
            .send()
            .await;
        match result {
            Ok(resp) if !resp.status().is_success() => {
                tracing::warn!(status = %resp.status(), "notification webhook returned an error status");
            }
            Err(e) => {
                tracing::warn!(error = %e, "notification webhook request failed");
            }
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_returns_none_for_an_empty_url() {
        assert!(Notifier::new("").is_none());
    }

    #[test]
    fn new_returns_some_for_a_configured_url() {
        assert!(Notifier::new("http://localhost:5600/message?token=x").is_some());
    }
}
