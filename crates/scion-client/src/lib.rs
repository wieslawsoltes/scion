//! Online Scion client. Source-control semantics remain on the authority; clients
//! fetch exact immutable objects, manage bounded caches and materialize selected files.
pub mod cache;
#[cfg(all(feature = "fuse", target_os = "linux"))]
pub mod fuse;
#[cfg(target_os = "linux")]
mod mount_session;
pub mod overlay;
mod status_cache;
pub mod working_copy;
#[cfg(all(feature = "fuse", target_os = "linux"))]
pub mod writable_fuse;

use base64::{Engine as _, engine::general_purpose::STANDARD};
pub use cache::ObjectCache;
use futures::StreamExt;
use scion_protocol::{
    FilePage, MAX_RPC_BYTES, ObjectRequest, ObjectTicket, ObjectWitness, RpcReply, RpcRequest,
    ServiceInfo,
};
use scion_store::{MAX_OBJECT_SIZE, decode_object};
use scion_types::{
    Changeset, Error, FileEntry, ObjectId, ObjectKind, RepoPath, Result, Tree, TreeEntry,
};
use serde::de::DeserializeOwned;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
pub use working_copy::{
    StatusEntry, StatusKind, StatusReport, StatusStats, TrackedFile, WorkingCopy, WorkingCopyState,
};

/// Reusable HTTP client with independent control and object transports.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    object_http: reqwest::Client,
    server: String,
    objects: String,
    repo: String,
    token: Option<Arc<String>>,
    cache: Option<ObjectCache>,
}
impl Client {
    pub fn new(
        server: impl Into<String>,
        objects: Option<String>,
        repo: impl Into<String>,
        token: Option<String>,
        cache: Option<ObjectCache>,
    ) -> Result<Self> {
        let server = server.into().trim_end_matches('/').to_owned();
        let objects = objects
            .unwrap_or_else(|| server.clone())
            .trim_end_matches('/')
            .to_owned();
        let repo = repo.into();
        scion_types::validate_name(&repo)?;
        for url in [&server, &objects] {
            if url.starts_with("unix://") {
                continue;
            }
            let parsed = reqwest::Url::parse(url).map_err(|e| Error::Invalid(e.to_string()))?;
            if !["http", "https"].contains(&parsed.scheme())
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                return Err(Error::Invalid(
                    "server URL must be HTTP(S) without credentials, query or fragment".into(),
                ));
            }
        }
        let builder = || {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(120))
                .redirect(reqwest::redirect::Policy::none())
        };
        let http = builder().build().map_err(transport)?;
        #[cfg(unix)]
        let object_http = if let Some(path) = objects.strip_prefix("unix://") {
            builder().unix_socket(path).build().map_err(transport)?
        } else {
            http.clone()
        };
        #[cfg(not(unix))]
        let object_http = {
            if objects.starts_with("unix://") {
                return Err(Error::Invalid(
                    "Unix socket transport is unavailable on this platform".into(),
                ));
            }
            http.clone()
        };
        if server.starts_with("unix://") {
            return Err(Error::Invalid(
                "control server must use HTTP(S); use --objects for a daemon Unix socket".into(),
            ));
        }
        Ok(Self {
            http,
            object_http,
            server,
            objects,
            repo,
            token: token.map(Arc::new),
            cache,
        })
    }
    pub fn server(&self) -> &str {
        &self.server
    }
    pub fn objects(&self) -> &str {
        &self.objects
    }
    pub fn repo(&self) -> &str {
        &self.repo
    }
    fn endpoint(&self, base: &str, part: &str) -> Result<reqwest::Url> {
        let base = if base.starts_with("unix://") {
            "http://localhost"
        } else {
            base
        };
        let mut url = reqwest::Url::parse(base).map_err(|e| Error::Invalid(e.to_string()))?;
        url.path_segments_mut()
            .map_err(|_| Error::Invalid("URL cannot be a base".into()))?
            .pop_if_empty()
            .extend(["v1", "repos", &self.repo, part]);
        Ok(url)
    }
    pub async fn info(&self) -> Result<ServiceInfo> {
        self.http
            .get(format!("{}/v1/info", self.server))
            .send()
            .await
            .map_err(transport)?
            .error_for_status()
            .map_err(transport)?
            .json()
            .await
            .map_err(transport)
    }
    pub async fn rpc<T: DeserializeOwned>(&self, request: RpcRequest) -> Result<T> {
        for attempt in 0..5 {
            let mut call = self
                .http
                .post(self.endpoint(&self.server, "rpc")?)
                .json(&request);
            if let Some(token) = &self.token {
                call = call.bearer_auth(token.as_str());
            }
            let response = call.send().await.map_err(transport)?;
            let status = response.status();
            let bytes = read_bounded(response, MAX_RPC_BYTES).await?;
            // Admission rejection happens before execution, so retrying 429 is
            // safe even for CAS mutations. Ambiguous transport failures are not retried.
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < 4 {
                tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
                continue;
            }
            let reply: RpcReply = serde_json::from_slice(&bytes).map_err(|_| {
                Error::Transport(format!(
                    "server returned HTTP {status} without an RPC response"
                ))
            })?;
            return reply.decode();
        }
        Err(Error::Limit("server admission retries exhausted".into()))
    }
    pub async fn resolve(&self, revision: impl Into<String>) -> Result<ObjectId> {
        self.rpc(RpcRequest::Resolve {
            revision: revision.into(),
        })
        .await
    }
    pub async fn put_blob(&self, bytes: &[u8]) -> Result<ObjectId> {
        if bytes.len() > MAX_OBJECT_SIZE {
            return Err(Error::Limit("file exceeds object size limit".into()));
        }
        if bytes.len() <= 8 * 1024 * 1024 {
            return self
                .rpc(RpcRequest::PutBlob {
                    data: STANDARD.encode(bytes),
                })
                .await;
        }
        let mut call = self
            .http
            .post(self.endpoint(&self.server, "blobs")?)
            .body(bytes.to_vec());
        if let Some(token) = &self.token {
            call = call.bearer_auth(token.as_str());
        }
        let response = call.send().await.map_err(transport)?;
        let bytes = read_bounded(response, MAX_RPC_BYTES).await?;
        serde_json::from_slice::<RpcReply>(&bytes)?.decode()
    }
    pub async fn create_commit(&self, changeset: Changeset) -> Result<ObjectId> {
        self.rpc(RpcRequest::Commit { changeset }).await
    }
    pub async fn object_ticket(&self, witness: ObjectWitness) -> Result<ObjectTicket> {
        self.rpc(RpcRequest::ObjectAccess {
            commit: witness.commit,
            path: witness.path,
            kind: witness.kind,
            id: witness.id,
        })
        .await
    }
    pub async fn fetch_object(&self, witness: ObjectWitness) -> Result<Vec<u8>> {
        // Reauthorize before reading a shared local cache. Content addresses alone
        // are not capabilities, even when another principal previously fetched them.
        let ticket = self.object_ticket(witness.clone()).await?;
        if let Some(cache) = self.cache.clone() {
            let kind = witness.kind;
            let id = witness.id;
            let payload = tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>> {
                cache
                    .get(kind, id)?
                    .map(|encoded| decode_object(kind, id, &encoded))
                    .transpose()
            })
            .await
            .map_err(|error| Error::Storage(error.to_string()))??;
            if let Some(payload) = payload {
                return Ok(payload);
            }
        }
        self.fetch_ticket(ticket).await
    }
    pub async fn fetch_ticket(&self, ticket: ObjectTicket) -> Result<Vec<u8>> {
        if ticket.repo != self.repo {
            return Err(Error::Permission(
                "object ticket belongs to another repository".into(),
            ));
        }
        let mut last = None;
        for attempt in 0..3 {
            let response = self
                .object_http
                .post(self.endpoint(&self.objects, "objects")?)
                .json(&ObjectRequest {
                    ticket: ticket.clone(),
                })
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    let encoded = read_bounded(response, MAX_OBJECT_SIZE + 1024 * 1024).await?;
                    let cache = self.cache.clone();
                    let kind = ticket.kind;
                    let id = ticket.id;
                    return tokio::task::spawn_blocking(move || {
                        let payload = decode_object(kind, id, &encoded)?;
                        if let Some(cache) = cache {
                            cache.put(kind, id, &encoded)?;
                        }
                        Ok(payload)
                    })
                    .await
                    .map_err(|error| Error::Storage(error.to_string()))?;
                }
                Ok(response) => {
                    let status = response.status();
                    if status.is_client_error()
                        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                        && status != reqwest::StatusCode::REQUEST_TIMEOUT
                    {
                        return Err(Error::Permission(format!(
                            "object request rejected: HTTP {status}"
                        )));
                    }
                    last = Some(Error::Transport(format!(
                        "object tier returned HTTP {status}"
                    )));
                }
                Err(error) => last = Some(transport(error)),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
            }
        }
        Err(last.unwrap_or_else(|| Error::Transport("object fetch failed".into())))
    }
    pub async fn read_file(&self, revision: ObjectId, path: RepoPath) -> Result<Vec<u8>> {
        let page: FilePage = self
            .rpc(RpcRequest::ListFilesPage {
                commit: revision,
                prefix: path.clone(),
                after: None,
                limit: 1,
            })
            .await?;
        let entry = page
            .entries
            .get(&path)
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        self.fetch_object(ObjectWitness {
            commit: revision,
            path,
            kind: ObjectKind::Blob,
            id: entry.id,
        })
        .await
    }
    /// Walk only trees intersecting sparse selections. Permission-limited trees
    /// fall back to the authority's filtered manifest API without exposing names.
    pub async fn manifest(
        &self,
        revision: ObjectId,
        sparse: &[RepoPath],
    ) -> Result<BTreeMap<RepoPath, FileEntry>> {
        let selection = if sparse.is_empty() {
            vec![RepoPath::root()]
        } else {
            sparse.to_vec()
        };
        let root: Result<ObjectId> = self.rpc(RpcRequest::RootTree { commit: revision }).await;
        let root = match root {
            Ok(root) => root,
            Err(Error::Permission(_)) => return self.filtered_manifest(revision, &selection).await,
            Err(e) => return Err(e),
        };
        let mut files = BTreeMap::new();
        let mut pending = vec![(RepoPath::root(), root)];
        while let Some((path, id)) = pending.pop() {
            let bytes = match self
                .fetch_object(ObjectWitness {
                    commit: revision,
                    path: path.clone(),
                    kind: ObjectKind::Tree,
                    id,
                })
                .await
            {
                Ok(bytes) => bytes,
                Err(Error::Permission(_)) => {
                    return self.filtered_manifest(revision, &selection).await;
                }
                Err(e) => return Err(e),
            };
            let tree: Tree = serde_json::from_slice(&bytes)?;
            tree.validate()?;
            for (name, entry) in tree.entries {
                let child = path.join(&name)?;
                match entry {
                    TreeEntry::Directory { id }
                        if selection
                            .iter()
                            .any(|s| s.contains(&child) || child.contains(s)) =>
                    {
                        pending.push((child, id))
                    }
                    TreeEntry::File { entry } if selection.iter().any(|s| s.contains(&child)) => {
                        files.insert(child, entry);
                    }
                    _ => {}
                }
            }
        }
        Ok(files)
    }
    async fn filtered_manifest(
        &self,
        revision: ObjectId,
        selection: &[RepoPath],
    ) -> Result<BTreeMap<RepoPath, FileEntry>> {
        let mut files = BTreeMap::new();
        for prefix in selection {
            let mut after = None;
            loop {
                let page: FilePage = self
                    .rpc(RpcRequest::ListFilesPage {
                        commit: revision,
                        prefix: prefix.clone(),
                        after: after.clone(),
                        limit: 1000,
                    })
                    .await?;
                files.extend(page.entries);
                match page.next {
                    None => break,
                    Some(next) => {
                        if after.as_ref().is_some_and(|previous| previous >= &next) {
                            return Err(Error::Corrupt("manifest cursor did not advance".into()));
                        }
                        after = Some(next);
                    }
                }
            }
        }
        Ok(files)
    }
}
fn transport(error: reqwest::Error) -> Error {
    Error::Transport(error.without_url().to_string())
}
/// Consume network responses with an explicit encoded-byte limit.
pub async fn read_bounded(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(Error::Limit("HTTP response exceeds byte limit".into()));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport)?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(Error::Limit("HTTP response exceeds byte limit".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn shared_local_cache_does_not_bypass_authorization() {
        let data = b"private";
        let id = ObjectId::hash(ObjectKind::Blob, data);
        let directory = tempfile::tempdir().unwrap();
        let cache = ObjectCache::open(directory.path(), 100_000).unwrap();
        cache
            .put(
                ObjectKind::Blob,
                id,
                &scion_store::encode_object(ObjectKind::Blob, data).unwrap(),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/v1/repos/test/rpc",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    axum::Json(RpcReply::failure("permission", "denied")),
                )
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client =
            Client::new(format!("http://{address}"), None, "test", None, Some(cache)).unwrap();
        let result = client
            .fetch_object(ObjectWitness {
                commit: id,
                path: RepoPath::new("secret").unwrap(),
                kind: ObjectKind::Blob,
                id,
            })
            .await;
        assert!(matches!(result, Err(Error::Permission(_))));
        server.abort();
    }
}
