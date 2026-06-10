use std::time::Duration;

use futures::future::select_all;
use log::{debug, error, warn};

use super::types::ClusterResponse;
use crate::utils::strings::truncate_bytes;

const MAX_CLUSTER_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_CLUSTER_MEMBERS: usize = 1024;

/// Errors from the Patroni REST API client.
#[derive(Debug)]
pub enum PatroniError {
    AllUrlsFailed(Vec<(String, String)>),
}

impl std::fmt::Display for PatroniError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatroniError::AllUrlsFailed(errors) => {
                write!(f, "all patroni urls failed:")?;
                for (url, err) in errors {
                    write!(f, " {url}: {err};")?;
                }
                Ok(())
            }
        }
    }
}

/// HTTP client for the Patroni REST API.
#[derive(Clone)]
pub struct PatroniClient {
    http: reqwest::Client,
}

impl PatroniClient {
    pub fn new(
        request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(request_timeout)
            .connect_timeout(connect_timeout)
            .no_proxy()
            .build()?;
        Ok(Self { http })
    }

    /// Fetch /cluster from all URLs in parallel.
    /// Returns first successful response, lets the rest complete via their own timeouts.
    pub async fn fetch_cluster(&self, urls: &[String]) -> Result<ClusterResponse, PatroniError> {
        if urls.is_empty() {
            return Err(PatroniError::AllUrlsFailed(vec![]));
        }

        // Each future resolves to (url, Result<ClusterResponse, String>) so we always
        // have the originating URL regardless of success or failure.
        let futs: Vec<_> = urls
            .iter()
            .map(|url| {
                let base = url.trim_end_matches('/').trim_end_matches("/cluster");
                let request_url = format!("{base}/cluster");
                let http = self.http.clone();
                let url_owned = url.clone();
                Box::pin(async move {
                    debug!("fetching /cluster from {request_url}");
                    let outcome: Result<ClusterResponse, String> = async {
                        let resp = http
                            .get(&request_url)
                            .send()
                            .await
                            .map_err(|e| format!("{e}"))?;

                        if !resp.status().is_success() {
                            let status = resp.status();
                            let body = read_limited_body(resp)
                                .await
                                .map(|body| String::from_utf8_lossy(&body).into_owned())
                                .unwrap_or_else(|e| e);
                            return Err(format!("HTTP {status}: {}", truncate_bytes(&body, 512)));
                        }

                        let body = read_limited_body(resp).await?;
                        parse_cluster_body(&body)
                    }
                    .await;

                    (url_owned, outcome)
                })
            })
            .collect();

        let mut remaining = futs;
        let mut errors: Vec<(String, String)> = Vec::new();

        while !remaining.is_empty() {
            let ((url, outcome), _idx, rest) = select_all(remaining).await;

            match outcome {
                Ok(cluster) => {
                    debug!(
                        "got /cluster from {}: {} members",
                        url,
                        cluster.members.len()
                    );
                    // Dropping `rest` cancels the remaining in-flight futures.
                    // reqwest respects its own timeouts for any leaked tasks.
                    return Ok(cluster);
                }
                Err(e) => {
                    warn!("patroni url {url} failed: {e}");
                    errors.push((url, e));
                }
            }

            remaining = rest;
        }

        error!("all patroni api urls failed");
        Err(PatroniError::AllUrlsFailed(errors))
    }
}

async fn read_limited_body(mut resp: reqwest::Response) -> Result<Vec<u8>, String> {
    if let Some(len) = resp.content_length() {
        if len > MAX_CLUSTER_RESPONSE_BYTES as u64 {
            return Err(format!(
                "response body exceeds {MAX_CLUSTER_RESPONSE_BYTES} byte limit"
            ));
        }
    }

    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("reading body: {e}"))?
    {
        append_limited_body_chunk(&mut body, &chunk)?;
    }
    Ok(body)
}

fn append_limited_body_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    let next_len = body
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| "response body length overflow".to_string())?;
    if next_len > MAX_CLUSTER_RESPONSE_BYTES {
        return Err(format!(
            "response body exceeds {MAX_CLUSTER_RESPONSE_BYTES} byte limit"
        ));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn parse_cluster_body(body: &[u8]) -> Result<ClusterResponse, String> {
    if body.len() > MAX_CLUSTER_RESPONSE_BYTES {
        return Err(format!(
            "response body exceeds {MAX_CLUSTER_RESPONSE_BYTES} byte limit"
        ));
    }

    let cluster = serde_json::from_slice::<ClusterResponse>(body).map_err(|e| {
        let body = String::from_utf8_lossy(body);
        format!(
            "json parse: {e}, body: {}",
            truncate_bytes(body.as_ref(), 512)
        )
    })?;

    if cluster.members.len() > MAX_CLUSTER_MEMBERS {
        return Err(format!(
            "members exceeds {MAX_CLUSTER_MEMBERS} limit: {}",
            cluster.members.len()
        ));
    }

    Ok(cluster)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member_json(idx: usize) -> String {
        format!(
            r#"{{"name":"n{idx}","role":"replica","state":"streaming","host":"10.0.0.{idx}","port":5432}}"#
        )
    }

    #[test]
    fn limited_body_chunk_rejects_oversize_before_append() {
        let mut body = vec![b'a'; MAX_CLUSTER_RESPONSE_BYTES - 1];
        let err = append_limited_body_chunk(&mut body, b"bc").unwrap_err();

        assert!(
            err.contains("exceeds"),
            "oversize append must fail with a clear error, got {err}"
        );
        assert_eq!(
            body.len(),
            MAX_CLUSTER_RESPONSE_BYTES - 1,
            "rejected chunks must not be appended"
        );
    }

    #[test]
    fn parse_cluster_body_rejects_oversize_before_json_parse() {
        let body = vec![b' '; MAX_CLUSTER_RESPONSE_BYTES + 1];
        let err = parse_cluster_body(&body).unwrap_err();

        assert!(
            err.contains("exceeds"),
            "oversize body must fail before JSON parsing, got {err}"
        );
    }

    #[test]
    fn parse_cluster_body_rejects_too_many_members() {
        let members = (0..=MAX_CLUSTER_MEMBERS)
            .map(member_json)
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(r#"{{"members":[{members}]}}"#);
        let err = parse_cluster_body(body.as_bytes()).unwrap_err();

        assert!(
            err.contains("members exceeds"),
            "oversize member list must be rejected, got {err}"
        );
    }
}
