//! Push-notification client (ported from yral-video-upload-service).
//!
//! Posts to `https://metadata.yral.com/notifications/{principal}/send`. Fire-and-
//! observe: the request is awaited but its result only logs / Sentry-captures on
//! failure — it never errors the caller. `log::*` swapped for `tracing::*` to match
//! storage conventions.

use std::fmt::Display;

use candid::Principal;
use serde::{Deserialize, Serialize};

const METADATA_SERVER_URL: &str = "https://multi-service.naitik.yral.com/api/v1";

// Consumed by handlers + UploadState in later tasks; allow(dead_code) until then.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct NotificationClient {
    api_key: String,
}

#[allow(dead_code)]
impl NotificationClient {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }

    pub async fn send_notification(&self, data: NotificationType, user_principal: Principal) {
        let client = reqwest::Client::new();
        let url = format!(
            "{}/notifications/{}/send",
            METADATA_SERVER_URL,
            user_principal.to_text()
        );

        let title = data.to_string();
        let notification = Notification {
            notification: NotificationInfo {
                title,
                body: String::new(),
            },
            data,
        };

        let res = client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&notification)
            .send()
            .await;

        match res {
            Ok(response) => {
                if response.status().is_success() {
                    tracing::info!(
                        "Notification sent successfully to user {}",
                        user_principal.to_text()
                    );
                } else if let Ok(body) = response.text().await {
                    let msg = format!(
                        "Failed to send notification to user {}: {}",
                        user_principal.to_text(),
                        body
                    );
                    tracing::error!("{}", msg);
                    sentry::capture_message(&msg, sentry::Level::Error);
                }
            }
            Err(req_err) => {
                let msg = format!(
                    "Error sending notification request to user {}: {}",
                    user_principal.to_text(),
                    req_err
                );
                tracing::error!("{}", msg);
                sentry::capture_message(&msg, sentry::Level::Error);
            }
        }
    }
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub struct NotificationInfo {
    pub title: String,
    pub body: String,
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub struct Notification {
    pub notification: NotificationInfo,
    pub data: NotificationType,
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum NotificationType {
    VideoUploadedToDraft {
        user_principal: Principal,
        post_id: String,
    },
    VideoPublished {
        user_principal: Principal,
        post_id: String,
    },
}

impl Display for NotificationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotificationType::VideoUploadedToDraft { .. } => write!(
                f,
                "Your video was generated and added to Drafts in Profile section!"
            ),
            NotificationType::VideoPublished { .. } => {
                write!(f, "Your video has been published successfully")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_with_type_tag_and_title() {
        let n = NotificationType::VideoPublished {
            user_principal: Principal::anonymous(),
            post_id: "p".into(),
        };
        let j = serde_json::to_value(&n).unwrap();
        assert_eq!(j["type"], "VideoPublished");
        assert_eq!(j["post_id"], "p");
        assert!(n.to_string().contains("published"));
    }
}
