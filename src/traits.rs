use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::path::PathBuf;
use tokio::process::Command;
use tracing::{debug, error};

use crate::error::AppError;
use crate::signal::AttachmentInfo;

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub(crate) trait SignalApi: Send + Sync {
    async fn send_msg(&self, recipient: &str, message: &str) -> Result<(), AppError>;
    async fn set_typing(&self, recipient: &str, typing: bool) -> Result<(), AppError>;
    async fn download_attachment(&self, attachment: &AttachmentInfo) -> Result<PathBuf, AppError>;
    async fn send_attachment(
        &self,
        recipient: &str,
        data: &[u8],
        content_type: &str,
        filename: &str,
    ) -> Result<(), AppError>;
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub(crate) trait ClaudeRunner: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn run_claude(
        &self,
        prompt: &str,
        session_id: &str,
        model: &str,
        files: &[PathBuf],
        sender: &str,
        max_budget: f64,
        system_prompt: &str,
    ) -> Result<(String, Option<f64>), AppError>;

    async fn summarize_session(&self, session_id: &str, model: &str) -> Option<String>;
}

pub(crate) struct SignalApiImpl {
    pub(crate) http: Client,
    pub(crate) api_url: String,
    pub(crate) account: String,
}

#[async_trait]
impl SignalApi for SignalApiImpl {
    async fn send_msg(&self, recipient: &str, message: &str) -> Result<(), AppError> {
        let url = format!("{}/v2/send", self.api_url);
        let body = serde_json::json!({
            "message": message,
            "number": self.account,
            "recipients": [recipient],
        });

        let resp = self.http.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "Signal send failed");
            return Err(AppError::Signal(format!("Send failed: {status}")));
        }
        Ok(())
    }

    async fn set_typing(&self, recipient: &str, typing: bool) -> Result<(), AppError> {
        let url = format!("{}/v1/typing-indicator/{}", self.api_url, self.account);
        let body = serde_json::json!({ "recipient": recipient });

        let resp = if typing {
            self.http.put(&url).json(&body).send().await?
        } else {
            self.http.delete(&url).json(&body).send().await?
        };

        if !resp.status().is_success() {
            debug!("Typing indicator failed: {}", resp.status());
        }
        Ok(())
    }

    async fn download_attachment(&self, attachment: &AttachmentInfo) -> Result<PathBuf, AppError> {
        let tmp_dir = PathBuf::from(crate::constants::TMP_DIR);
        let _ = std::fs::create_dir_all(&tmp_dir);

        let ext = attachment
            .filename
            .as_deref()
            .and_then(|f| f.rsplit('.').next())
            .or(match attachment.content_type.as_str() {
                "image/jpeg" => Some("jpg"),
                "image/png" => Some("png"),
                "image/gif" => Some("gif"),
                "image/webp" => Some("webp"),
                "application/pdf" => Some("pdf"),
                "text/plain" => Some("txt"),
                "audio/aac" => Some("aac"),
                "audio/ogg" => Some("ogg"),
                "audio/mpeg" => Some("mp3"),
                "audio/mp4" => Some("m4a"),
                "audio/x-caf" => Some("caf"),
                _ => None,
            })
            .unwrap_or("bin");

        let path = tmp_dir.join(format!(
            "{}_{}.{}",
            attachment.id,
            uuid::Uuid::new_v4(),
            ext
        ));

        let url = format!("{}/v1/attachments/{}", self.api_url, attachment.id);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(format!(
                "Failed to download attachment {}: {}",
                attachment.id,
                resp.status()
            )
            .into());
        }
        let bytes = resp.bytes().await?;
        std::fs::write(&path, &bytes)?;
        debug!(
            "Downloaded attachment {} to {}",
            attachment.id,
            path.display()
        );
        Ok(path)
    }

    async fn send_attachment(
        &self,
        recipient: &str,
        data: &[u8],
        content_type: &str,
        filename: &str,
    ) -> Result<(), AppError> {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        let url = format!("{}/v2/send", self.api_url);
        let body = serde_json::json!({
            "message": "",
            "number": self.account,
            "recipients": [recipient],
            "base64_attachments": [
                format!("data:{content_type};filename={filename};base64,{b64}")
            ],
        });

        let resp = self.http.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            error!(status = %status, body = %body_text, "Attachment send failed");
            return Err(AppError::Signal(format!(
                "Attachment send failed: {status}"
            )));
        }
        Ok(())
    }
}

pub(crate) struct ClaudeRunnerImpl;

#[async_trait]
impl ClaudeRunner for ClaudeRunnerImpl {
    #[allow(clippy::too_many_arguments)]
    async fn run_claude(
        &self,
        prompt: &str,
        session_id: &str,
        model: &str,
        files: &[PathBuf],
        sender: &str,
        max_budget: f64,
        system_prompt: &str,
    ) -> Result<(String, Option<f64>), AppError> {
        let work_dir = crate::helpers::isolated_workdir(sender);
        std::fs::create_dir_all(&work_dir)?;

        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg(prompt)
            .arg("--session-id")
            .arg(session_id)
            .arg("--output-format")
            .arg("json")
            .arg("--model")
            .arg(model)
            .arg("--max-budget-usd")
            .arg(max_budget.to_string())
            .arg("--append-system-prompt")
            .arg(system_prompt)
            .arg("--no-session-persistence")
            .current_dir(&work_dir)
            .env_remove("CLAUDE_CODE_ENTRYPOINT");
        for file in files {
            cmd.arg("--file").arg(file);
        }
        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppError::Claude(format!(
                "claude exited with {}: {stderr}",
                output.status
            )));
        }

        let stdout = String::from_utf8(output.stdout)?;
        let parsed: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|_| serde_json::json!({"result": stdout.trim()}));

        let result = parsed["result"]
            .as_str()
            .unwrap_or_else(|| stdout.trim())
            .to_string();

        let cost = parsed["cost_usd"]
            .as_f64()
            .or_else(|| parsed["total_cost_usd"].as_f64());

        Ok((result, cost))
    }

    async fn summarize_session(&self, session_id: &str, model: &str) -> Option<String> {
        let output = Command::new("claude")
            .arg("-p")
            .arg("Summarize this conversation in 2-3 sentences. Focus on: key topics discussed, user preferences, and any unfinished tasks.")
            .arg("--session-id")
            .arg(session_id)
            .arg("--output-format")
            .arg("json")
            .arg("--max-budget-usd")
            .arg(crate::constants::SUMMARIZE_BUDGET.to_string())
            .arg("--model")
            .arg(model)
            .arg("--append-system-prompt")
            .arg(crate::NO_MEMORY_PROMPT)
            .arg("--no-session-persistence")
            .env_remove("CLAUDE_CODE_ENTRYPOINT")
            .output()
            .await
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8(output.stdout).ok()?;
        let parsed: Value = serde_json::from_str(&stdout).ok()?;
        parsed["result"].as_str().map(|s| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_wiremock(
        method: &str,
        path: &str,
        status: u16,
    ) -> (wiremock::MockServer, SignalApiImpl) {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method(method))
            .and(wiremock::matchers::path(path))
            .respond_with(wiremock::ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let api = SignalApiImpl {
            http: Client::new(),
            api_url: server.uri(),
            account: "+1234567890".to_string(),
        };
        (server, api)
    }

    #[tokio::test]
    async fn test_signal_api_send_success() {
        let (_server, api) = setup_wiremock("POST", "/v2/send", 200).await;
        assert!(api.send_msg("+recipient", "hello").await.is_ok());
    }

    #[tokio::test]
    async fn test_signal_api_send_failure() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v2/send"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("Internal error"))
            .mount(&server)
            .await;
        let api = SignalApiImpl {
            http: Client::new(),
            api_url: server.uri(),
            account: "+1234567890".to_string(),
        };
        assert!(api.send_msg("+recipient", "hello").await.is_err());
    }

    #[tokio::test]
    async fn test_signal_api_set_typing_on() {
        let (_server, api) = setup_wiremock("PUT", "/v1/typing-indicator/+1234567890", 204).await;
        assert!(api.set_typing("+recipient", true).await.is_ok());
    }

    #[tokio::test]
    async fn test_signal_api_set_typing_off() {
        let (_server, api) =
            setup_wiremock("DELETE", "/v1/typing-indicator/+1234567890", 204).await;
        assert!(api.set_typing("+recipient", false).await.is_ok());
    }

    #[tokio::test]
    async fn test_signal_api_download_attachment_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/attachments/att123"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_bytes(b"fake image data".to_vec()),
            )
            .mount(&server)
            .await;
        let api = SignalApiImpl {
            http: Client::new(),
            api_url: server.uri(),
            account: "+1234567890".to_string(),
        };

        let att = AttachmentInfo {
            id: "att123".to_string(),
            content_type: "image/png".to_string(),
            filename: Some("photo.png".to_string()),
            voice_note: false,
        };

        let path = api.download_attachment(&att).await.unwrap();
        assert!(path.exists());
        assert!(path.to_str().unwrap().ends_with(".png"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_signal_api_download_attachment_failure() {
        let (_server, api) = setup_wiremock("GET", "/v1/attachments/att404", 404).await;
        let att = AttachmentInfo {
            id: "att404".to_string(),
            content_type: "image/jpeg".to_string(),
            filename: None,
            voice_note: false,
        };
        assert!(api.download_attachment(&att).await.is_err());
    }

    #[tokio::test]
    async fn test_signal_send_failure_returns_signal_variant() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v2/send"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let api = SignalApiImpl {
            http: Client::new(),
            api_url: server.uri(),
            account: "+1234567890".to_string(),
        };
        let err = api.send_msg("+recipient", "hello").await.unwrap_err();
        assert!(matches!(err, AppError::Signal(_)));
    }

    #[test]
    fn test_signal_variant_message_preserved() {
        let err = AppError::Signal("connection refused".to_string());
        let msg = err.to_string();
        assert!(msg.contains("Signal API error"), "got: {msg}");
        assert!(msg.contains("connection refused"), "got: {msg}");
    }

    #[test]
    fn test_claude_variant_message_preserved() {
        let err = AppError::Claude("model unavailable".to_string());
        let msg = err.to_string();
        assert!(msg.contains("Claude error"), "got: {msg}");
        assert!(msg.contains("model unavailable"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_signal_api_send_multiple_messages_via_http() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v2/send"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(3)
            .mount(&server)
            .await;
        let api = SignalApiImpl {
            http: Client::new(),
            api_url: server.uri(),
            account: "+1234567890".to_string(),
        };

        for i in 1..=3 {
            assert!(api
                .send_msg("+recipient", &format!("Part {i}"))
                .await
                .is_ok());
        }
    }

    #[tokio::test]
    async fn test_signal_api_send_attachment_success() {
        let (_server, api) = setup_wiremock("POST", "/v2/send", 200).await;
        let data = b"fake png data";
        let result = api
            .send_attachment("+recipient", data, "image/png", "output.png")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_signal_api_send_attachment_failure() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v2/send"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let api = SignalApiImpl {
            http: Client::new(),
            api_url: server.uri(),
            account: "+1234567890".to_string(),
        };
        let result = api
            .send_attachment("+recipient", b"data", "image/png", "out.png")
            .await;
        assert!(result.is_err());
    }
}
