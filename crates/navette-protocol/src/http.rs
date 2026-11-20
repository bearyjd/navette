//! Request and response bodies of the daemon's HTTP routes, shared by the
//! daemon and the CLI so the wire shape is declared once.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

/// Body of `POST /v1/sessions/{session}/files`: metadata reserved before a
/// file's bytes are uploaded.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct FilePreflight {
    pub name: String,
    pub mime: String,
    pub size: u64,
}

/// Answer to a [`FilePreflight`].
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct FilePreflightResponse {
    pub transfer_id: String,
    pub upload_url: String,
    pub expires_at: u64,
}

/// The daemon's view of a file transfer.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct FileTransferStatus {
    pub transfer_id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub bytes_received: u64,
    pub state: FileTransferState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferState {
    AwaitingUpload,
    Queued,
    Materializing,
    Delivered,
    Failed,
    Cancelled,
    /// A state added by a newer daemon; lets older clients still decode the
    /// status instead of failing the whole response.
    #[serde(other)]
    Unknown,
}

/// Body of `POST /v1/wake`. Omitted fields take the daemon's defaults.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct WakeRequest {
    pub mac: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broadcast: Option<Ipv4Addr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_preflight_round_trips() {
        let value = FilePreflight {
            name: "a.txt".into(),
            mime: "text/plain".into(),
            size: 3,
        };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(
            json,
            json!({"name": "a.txt", "mime": "text/plain", "size": 3})
        );
        assert_eq!(
            serde_json::from_value::<FilePreflight>(json).unwrap(),
            value
        );
    }

    #[test]
    fn file_preflight_response_round_trips() {
        let value = FilePreflightResponse {
            transfer_id: "t1".into(),
            upload_url: "/v1/up/t1".into(),
            expires_at: 9,
        };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(
            serde_json::from_value::<FilePreflightResponse>(json).unwrap(),
            value
        );
    }

    #[test]
    fn file_transfer_status_round_trips_and_omits_absent_expiry() {
        let value = FileTransferStatus {
            transfer_id: "t1".into(),
            name: "a.txt".into(),
            mime: "text/plain".into(),
            size: 3,
            bytes_received: 3,
            state: FileTransferState::AwaitingUpload,
            expires_at: None,
        };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["state"], "awaiting_upload");
        assert!(json.get("expires_at").is_none());
        assert_eq!(
            serde_json::from_value::<FileTransferStatus>(json).unwrap(),
            value
        );
    }

    #[test]
    fn every_known_state_round_trips() {
        for state in [
            FileTransferState::AwaitingUpload,
            FileTransferState::Queued,
            FileTransferState::Materializing,
            FileTransferState::Delivered,
            FileTransferState::Failed,
            FileTransferState::Cancelled,
        ] {
            let json = serde_json::to_value(state).unwrap();
            assert_eq!(
                serde_json::from_value::<FileTransferState>(json).unwrap(),
                state
            );
        }
    }

    #[test]
    fn unknown_state_decodes_to_unknown() {
        let status: FileTransferStatus = serde_json::from_value(json!({
            "transfer_id": "t1", "name": "a", "mime": "text/plain",
            "size": 1, "bytes_received": 0, "state": "paused_by_future_daemon"
        }))
        .unwrap();
        assert_eq!(status.state, FileTransferState::Unknown);
        assert_eq!(status.expires_at, None);
    }

    #[test]
    fn wake_request_omits_absent_fields_and_round_trips() {
        let minimal = WakeRequest {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            broadcast: None,
            port: None,
        };
        let json = serde_json::to_value(&minimal).unwrap();
        assert_eq!(json, json!({"mac": "aa:bb:cc:dd:ee:ff"}));
        assert_eq!(
            serde_json::from_value::<WakeRequest>(json).unwrap(),
            minimal
        );

        let explicit = WakeRequest {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            broadcast: Some(Ipv4Addr::new(192, 168, 1, 255)),
            port: Some(7),
        };
        let json = serde_json::to_value(&explicit).unwrap();
        assert_eq!(json["broadcast"], "192.168.1.255");
        assert_eq!(
            serde_json::from_value::<WakeRequest>(json).unwrap(),
            explicit
        );
    }
}
