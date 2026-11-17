use wreq::Method;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use tauri::ipc::Response;

use crate::error::AppError;
use crate::state::AppState;

use super::client::GrindrClient;
use super::client::BASE_URL;
use super::headers::{build_headers, DeviceInfo};

const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const ACCEPT_ENCODING: &str = "accept-encoding";

#[derive(Serialize, Deserialize)]
pub struct RawResponse {
    pub status: u16,
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
}

impl GrindrClient {
    pub(super) async fn ensure_valid_session(&self) -> Result<(), AppError> {
        let needs_refresh = {
            let session = self.session.read().await;
            let expires_at = session.as_ref().map(|s| s.expires_at).unwrap_or(0);
            expires_at > 0 && expires_at < (chrono::Utc::now().timestamp() as u64 + 60)
        };

        if needs_refresh {
            let _lock = self.refresh_lock.lock().await;
            // Double-check after acquiring lock
            let still_needs_refresh = {
                let session = self.session.read().await;
                let expires_at = session.as_ref().map(|s| s.expires_at).unwrap_or(0);
                expires_at > 0 && expires_at < (chrono::Utc::now().timestamp() as u64 + 60)
            };

            if still_needs_refresh {
                let _ = Box::pin(self.refresh_token()).await?;
            }
        }
        Ok(())
    }

    fn apply_headers(
        mut req: wreq::RequestBuilder,
        items: &[(wreq::header::HeaderName, wreq::header::HeaderValue)],
    ) -> wreq::RequestBuilder {
        for (name, value) in items {
            req = req.header(name.clone(), value.clone());
        }
        req
    }

    fn build_configured_request(
        &self,
        method: Method,
        url: &str,
        path: &str,
        device: &DeviceInfo,
        auth_token: Option<&str>,
        body_bytes: Option<&[u8]>,
        content_type: Option<&str>,
    ) -> wreq::RequestBuilder {
        let mut req = self.http.request(method, url);
        if path.starts_with("/v8/sessions") || path.starts_with("/v9/sessions") {
            req = req.header("requirerealdeviceinfo", "true");
        } else if path.starts_with("/public/") {
            req = req.header("requireanondeviceinfo", "true");
        }
        let headers = build_headers(device, "Free", auth_token);
        // OkHttp emits Content-Type and Content-Length before Accept-Encoding: gzip
        let (encoding, others): (Vec<_>, Vec<_>) = headers
            .into_iter()
            .partition(|(name, _)| name.as_str() == ACCEPT_ENCODING);
        req = Self::apply_headers(req, &others);
        if let Some(bytes) = body_bytes {
            let ct = content_type.unwrap_or(JSON_CONTENT_TYPE);
            req = req
                .header("content-type", ct)
                .header("content-length", bytes.len().to_string())
                .body(bytes.to_vec());
        }
        Self::apply_headers(req, &encoding)
    }

    pub(super) async fn request_json<TReq, TResp>(
        &self,
        method: Method,
        path: &str,
        body: Option<&TReq>,
    ) -> Result<TResp, AppError>
    where
        TReq: Serialize + ?Sized,
        TResp: DeserializeOwned,
    {
        let is_auth_path =
            path.starts_with("/v8/sessions") || path.starts_with("/v9/sessions") || path.starts_with("/public/");
        let url = if path.starts_with("http") {
            path.to_owned()
        } else {
            format!("{BASE_URL}{path}")
        };
        #[cfg(debug_assertions)]
        eprintln!("[HTTP] -> {} {}", method, url);

        // Proactive refresh
        if !is_auth_path {
            if let Err(e @ AppError::TokenExpired) = self.ensure_valid_session().await {
                return Err(e);
            }
        }

        let body_bytes = body.map(serde_json::to_vec).transpose()?;
        let device = self.device.read().await;

        let auth_token = self.authorization_header().await;
        let mut response = self
            .build_configured_request(
                method.clone(),
                &url,
                path,
                &device,
                auth_token.as_deref(),
                body_bytes.as_deref(),
                Some(JSON_CONTENT_TYPE),
            )
            .send()
            .await
            .map_err(|e| {
                #[cfg(debug_assertions)]
                eprintln!("[HTTP] network error on {} {}: {e}", method, url);
                AppError::Http(e.to_string())
            })?;

        if response.status().as_u16() == 401 && !is_auth_path {
            let _lock = self.refresh_lock.lock().await;

            // Check if the token has already been refreshed by someone else since our failed request
            let current_token = self.authorization_header().await;
            if current_token == auth_token {
                if let Err(e @ AppError::TokenExpired) = Box::pin(self.refresh_token()).await {
                    return Err(e);
                }
            }

            let new_auth_token = self.authorization_header().await;
            let device = self.device.read().await;
            response = self
                .build_configured_request(
                    method.clone(),
                    &url,
                    path,
                    &device,
                    new_auth_token.as_deref(),
                    body_bytes.as_deref(),
                    Some(JSON_CONTENT_TYPE),
                )
                .send()
                .await
                .map_err(|e| {
                    #[cfg(debug_assertions)]
                    eprintln!("[HTTP] network error on {} {}: {e}", method, url);
                    AppError::Http(e.to_string())
                })?;
        }

        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        #[cfg(debug_assertions)]
        eprintln!("[HTTP] <- {} {} | Status: {}", method, url, status);

        if !status.is_success() {
            #[cfg(debug_assertions)]
            eprintln!(
                "[HTTP] error {} {} -> status={} body={}",
                method, url, status, text
            );
            let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(0) as i32;
            let message = json
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_else(|| {
                    if text.is_empty() {
                        "Unknown error"
                    } else {
                        &text
                    }
                })
                .to_owned();

            return Err(AppError::Api { code, message });
        }

        let resp = serde_json::from_str::<TResp>(&text).map_err(|e| {
            #[cfg(debug_assertions)]
            eprintln!("[HTTP] JSON decode error on {} {}: {e}", method, url);
            AppError::from(e)
        })?;
        Ok(resp)
    }

    pub(super) async fn request_raw(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> Result<RawResponse, AppError> {
        let is_auth_path =
            path.starts_with("/v8/sessions") || path.starts_with("/v9/sessions") || path.starts_with("/public/");
        let url = if path.starts_with("http") {
            path.to_owned()
        } else {
            format!("{BASE_URL}{path}")
        };
        #[cfg(debug_assertions)]
        eprintln!("[HTTP] -> {} {}", method, url);

        // Proactive refresh
        if !is_auth_path {
            if let Err(e @ AppError::TokenExpired) = self.ensure_valid_session().await {
                return Err(e);
            }
        }

        let is_external = path.starts_with("http");
        let auth_token = if is_auth_path || is_external {
            self.authorization_header().await
        } else {
            Some(
                self.authorization_header()
                    .await
                    .ok_or_else(|| AppError::Auth("Not logged in".to_owned()))?,
            )
        };

        let device = self.device.read().await;

        let mut response = self
            .build_configured_request(
                method.clone(),
                &url,
                path,
                &device,
                auth_token.as_deref(),
                body.as_deref(),
                content_type,
            )
            .send()
            .await
            .map_err(|e| AppError::Http(e.to_string()))?;
        #[cfg(debug_assertions)]
        eprintln!(
            "[HTTP] <- {} {} | Status: {}",
            method,
            url,
            response.status()
        );

        if response.status().as_u16() == 401 && !is_auth_path {
            let _lock = self.refresh_lock.lock().await;

            let current_token = self.authorization_header().await;
            if current_token == auth_token {
                if let Err(e @ AppError::TokenExpired) = Box::pin(self.refresh_token()).await {
                    return Err(e);
                }
            }

            let new_auth_token = self.authorization_header().await;
            let device = self.device.read().await;
            response = self
                .build_configured_request(
                    method.clone(),
                    &url,
                    path,
                    &device,
                    new_auth_token.as_deref(),
                    body.as_deref(),
                    content_type,
                )
                .send()
                .await
                .map_err(|e| AppError::Http(e.to_string()))?;
        }

        let status = response.status().as_u16();
        let response_body = response.bytes().await
            .map_err(|e| AppError::Http(e.to_string()))?
            .to_vec();

        Ok(RawResponse {
            status,
            body: response_body,
        })
    }
}

#[tauri::command]
pub async fn request(
    state: tauri::State<'_, AppState>,
    method: String,
    path: String,
    body: Option<Vec<u8>>,
    content_type: Option<String>,
) -> Result<Response, AppError> {
    let method_str = method.clone();
    let method = Method::from_str(&method).map_err(|_| AppError::Api {
        code: 400,
        message: format!("Invalid method: {method_str}"),
    })?;

    let raw = state
        .client()?
        .request_raw(method, &path, body, content_type.as_deref())
        .await;

    let raw = raw?;

    Ok(Response::new(
        rmp_serde::encode::to_vec_named(&raw).map_err(|e| AppError::Http(e.to_string()))?,
    ))
}
