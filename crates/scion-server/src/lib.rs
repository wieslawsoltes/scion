//! Embeddable authoritative server. Blocking backend work runs on Tokio's
//! blocking pool behind bounded admission. No source content is logged.
pub mod auth;

use auth::{Access,Policy};
use axum::{Router,Extension,body::Bytes,extract::{DefaultBodyLimit,Path,State,Request},http::{HeaderMap,StatusCode,header},middleware::{self,Next},response::{IntoResponse,Response},routing::{get,post},Json};
use base64::{Engine,engine::general_purpose::STANDARD};
use parking_lot::Mutex;
use scion_engine::Repository;
use scion_protocol::{CAPABILITY_TTL_MS,MAX_BATCH_OBJECTS,MAX_RPC_BYTES,ObjectRequest,ObjectTicket,ObjectWitness,PROTOCOL_VERSION,RpcReply,RpcRequest,ServiceInfo,Tier,now_ms};
use scion_store::{SharedStore,MAX_OBJECT_SIZE,decode_object};
use scion_types::{Error,FileChange,ObjectId,ObjectKind,RepoPath,Result,TreeEntry,validate_name};
use std::{collections::BTreeMap,sync::{Arc,atomic::{AtomicU64,Ordering}},time::Instant};
use tokio::sync::{Semaphore,OwnedSemaphorePermit};

/// Safe defaults; anonymous mode must be explicitly selected by the executable.
#[derive(Clone)]
pub struct ServerConfig {
    pub policy:Policy,
    pub development:bool,
    pub capability_key:[u8;32],
    pub max_concurrent:usize,
    pub requests_per_second:u32,
    pub burst:u32,
}
impl Default for ServerConfig {
    fn default()->Self {Self {policy:Policy::default(),development:false,capability_key:[0;32],max_concurrent:128,requests_per_second:200,burst:400}}
}

struct Bucket {tokens:f64,at:Instant}
#[derive(Default)]
struct Metrics {rpc:AtomicU64,objects:AtomicU64,errors:AtomicU64,rejected:AtomicU64,bytes:AtomicU64}

struct RequestBudget {_request:OwnedSemaphorePermit,_memory:Option<OwnedSemaphorePermit>}
pub struct ServerState {store:SharedStore,config:ServerConfig,admission:Arc<Semaphore>,upload_bytes:Arc<Semaphore>,repositories:Mutex<BTreeMap<String,Repository>>,buckets:Mutex<BTreeMap<String,Bucket>>,metrics:Metrics}
impl ServerState {
    pub fn new(store:SharedStore,config:ServerConfig)->Result<Arc<Self>> {
        config.policy.validate()?;
        if !config.development && config.capability_key==[0;32] {return Err(Error::Invalid("production requires a nonzero capability key".into()));}
        if config.max_concurrent==0 || config.requests_per_second==0 || config.burst==0 {return Err(Error::Invalid("server concurrency/rate limits must be nonzero".into()));}
        Ok(Arc::new(Self {store,admission:Arc::new(Semaphore::new(config.max_concurrent)),upload_bytes:Arc::new(Semaphore::new(1024)),repositories:Mutex::new(BTreeMap::new()),config,buckets:Mutex::new(BTreeMap::new()),metrics:Metrics::default()}))
    }
    fn authenticate(&self,h:&HeaderMap,repo:&str)->Result<Access> {
        validate_name(repo)?;
        let access=self.config.policy.authenticate(h,repo,self.config.development)?;
        self.rate_limit(&access.principal)?;
        Ok(access)
    }
    fn rate_limit(&self,key:&str)->Result<()> {
        let now=Instant::now();let mut buckets=self.buckets.lock();
        let b=buckets.entry(key.into()).or_insert(Bucket{tokens:self.config.burst as f64,at:now});
        b.tokens=(b.tokens+now.duration_since(b.at).as_secs_f64()*self.config.requests_per_second as f64).min(self.config.burst as f64);b.at=now;
        if b.tokens<1.0 {self.metrics.rejected.fetch_add(1,Ordering::Relaxed);return Err(Error::Limit("request rate exceeded".into()));}b.tokens-=1.0;Ok(())
    }
    fn repository(&self,name:&str)->Result<Repository> {
        let mut repos=self.repositories.lock();
        if let Some(repo)=repos.get(name){return Ok(repo.clone());}
        let repo=Repository::new(name,self.store.clone())?;
        if repos.len()>=10_000{return Err(Error::Limit("server repository cache limit reached".into()));}
        repos.insert(name.into(),repo.clone());Ok(repo)
    }
}

pub fn router(state:Arc<ServerState>)->Router {
    let controls=Router::new().route("/v1/repos/{repo}/rpc",post(rpc)).layer(DefaultBodyLimit::max(MAX_RPC_BYTES));
    let objects=Router::new().route("/v1/repos/{repo}/objects",post(object)).layer(DefaultBodyLimit::max(64*1024));
    let uploads=Router::new().route("/v1/repos/{repo}/blobs",post(upload)).layer(DefaultBodyLimit::max(MAX_OBJECT_SIZE));
    Router::new().merge(controls).merge(uploads).merge(objects)
        .route("/health",get(||async{"ok"}))
        .route("/ready",get(ready))
        .route("/v1/info",get(||async{Json(ServiceInfo{protocol:PROTOCOL_VERSION,tier:Tier::Server,version:env!("CARGO_PKG_VERSION").into()})}))
        .route("/metrics",get(metrics)).layer(middleware::from_fn_with_state(state.clone(),admit)).with_state(state)
}

async fn admit(State(s):State<Arc<ServerState>>,mut request:Request,next:Next)->Response {
    if !request.uri().path().starts_with("/v1/repos/"){return next.run(request).await;}
    let is_upload=request.uri().path().ends_with("/blobs");
    let needs_token=is_upload||request.uri().path().ends_with("/rpc");
    if needs_token&&!s.config.development {if let Err(e)=s.config.policy.authenticate_headers(request.headers()){return fail(e,&s);}}
    let permit=match s.admission.clone().try_acquire_owned(){Ok(p)=>p,Err(_)=>return fail(Error::Limit("server is at capacity".into()),&s)};
    // Reserve upload memory before reading the body. Chunked requests reserve
    // the complete object ceiling; length-delimited requests reserve their size.
    let memory=if is_upload {
        let size=request.headers().get(header::CONTENT_LENGTH).and_then(|h|h.to_str().ok()).and_then(|s|s.parse::<usize>().ok()).unwrap_or(MAX_OBJECT_SIZE);
        if size>MAX_OBJECT_SIZE{return fail(Error::Limit("upload exceeds object size limit".into()),&s);}
        let mib=size.div_ceil(1024*1024).max(1)as u32;
        match s.upload_bytes.clone().try_acquire_many_owned(mib){Ok(p)=>Some(p),Err(_)=>return fail(Error::Limit("upload memory budget exhausted".into()),&s)}
    }else{None};
    request.extensions_mut().insert(Arc::new(RequestBudget{_request:permit,_memory:memory}));
    next.run(request).await
}

fn fail(error:Error,state:&ServerState)->Response {
    state.metrics.errors.fetch_add(1,Ordering::Relaxed);
    let (code,status)=match &error {Error::NotFound(_)=>("not_found",StatusCode::NOT_FOUND),Error::Permission(_)=>("permission",StatusCode::FORBIDDEN),Error::Conflict(_)=>("conflict",StatusCode::CONFLICT),Error::Invalid(_)|Error::Json(_)=>("invalid",StatusCode::BAD_REQUEST),Error::Limit(_)=>("limit",StatusCode::TOO_MANY_REQUESTS),_=>("internal",StatusCode::INTERNAL_SERVER_ERROR)};
    let message=if status==StatusCode::INTERNAL_SERVER_ERROR {tracing::error!(error=%error,"request failed");"internal service error".to_owned()}else{error.to_string()};
    (status,Json(RpcReply::failure(code,message))).into_response()
}

async fn rpc(State(state):State<Arc<ServerState>>,Path(repo):Path<String>,Extension(budget):Extension<Arc<RequestBudget>>,headers:HeaderMap,Json(request):Json<RpcRequest>)->Response {
    state.metrics.rpc.fetch_add(1,Ordering::Relaxed);
    let access=match state.authenticate(&headers,&repo){Ok(a)=>a,Err(e)=>return fail(e,&state)};
    let s=state.clone();
    match tokio::task::spawn_blocking(move||{let _budget=budget;let reply=dispatch(&s,&repo,&access,request)?;let bytes=serde_json::to_vec(&reply)?;if bytes.len()>MAX_RPC_BYTES{return Err(Error::Limit("RPC result exceeds 16MiB; use paginated or immutable-object APIs".into()));}Ok::<_,Error>(bytes)}).await {
        Ok(Ok(bytes))=>([(header::CONTENT_TYPE,"application/json")],bytes).into_response(),Ok(Err(e))=>fail(e,&state),Err(e)=>fail(Error::Storage(e.to_string()),&state),
    }
}

async fn upload(State(state):State<Arc<ServerState>>,Path(repo):Path<String>,Extension(budget):Extension<Arc<RequestBudget>>,headers:HeaderMap,body:Bytes)->Response {
    let access=match state.authenticate(&headers,&repo){Ok(a)=>a,Err(e)=>return fail(e,&state)};
    if !access.permissions.write {return fail(Error::Permission("repository upload denied".into()),&state);}
    let s=state.clone();
    match tokio::task::spawn_blocking(move||{let _budget=budget;let r=s.repository(&repo)?;let id=upload_blob(&r,&access,&body)?;RpcReply::success(id)}).await {
        Ok(Ok(reply))=>Json(reply).into_response(),Ok(Err(e))=>fail(e,&state),Err(e)=>fail(Error::Storage(e.to_string()),&state),
    }
}

async fn object(State(state):State<Arc<ServerState>>,Path(repo):Path<String>,Extension(budget):Extension<Arc<RequestBudget>>,Json(request):Json<ObjectRequest>)->Response {
    if let Err(e)=request.ticket.verify(&state.config.capability_key,&repo,now_ms()) {return fail(e,&state);}
    if let Err(e)=state.rate_limit("object-capabilities") {return fail(e,&state);}
    let s=state.clone();
    match tokio::task::spawn_blocking(move||{let _budget=budget;let t=request.ticket;let bytes=s.store.get_encoded(t.kind,t.id)?;decode_object(t.kind,t.id,&bytes)?;Ok::<_,Error>(bytes)}).await {
        Ok(Ok(bytes))=>{state.metrics.objects.fetch_add(1,Ordering::Relaxed);state.metrics.bytes.fetch_add(bytes.len()as u64,Ordering::Relaxed);([(header::CONTENT_TYPE,"application/vnd.scion.object"),(header::CACHE_CONTROL,"private, no-store")],bytes).into_response()},
        Ok(Err(e))=>fail(e,&state),Err(e)=>fail(Error::Storage(e.to_string()),&state),
    }
}

fn upload_blob(repo:&Repository,a:&Access,bytes:&[u8])->Result<ObjectId> {
    let id=repo.put_blob(bytes)?;
    repo.store().put_record(&format!("repo/{}/uploads/{}",repo.name(),a.principal),&id.to_string(),b"1")?;
    Ok(id)
}

fn ticket(state:&ServerState,repo:&Repository,a:&Access,w:ObjectWitness)->Result<ObjectTicket> {
    match w.kind {
        ObjectKind::Blob=>{a.permissions.require(&w.path,false)?;let entry=repo.file(w.commit,&w.path)?.ok_or_else(||Error::NotFound(w.path.to_string()))?;if entry.id!=w.id{return Err(Error::Permission("object does not match the authorized path".into()));}},
        ObjectKind::Tree=>{
            if !a.permissions.allows_subtree(&w.path,false){return Err(Error::Permission("tree contains restricted descendants".into()));}
            let id=tree_at(repo,w.commit,&w.path)?;if id!=w.id{return Err(Error::Permission("tree does not match the authorized path".into()));}
        },
        ObjectKind::Changeset=>{a.permissions.require_full(false)?;repo.get_changeset(w.commit)?;if w.id!=w.commit{return Err(Error::Permission("changeset witness mismatch".into()));}},
        ObjectKind::Derived=>return Err(Error::Permission("derived data is server-private".into())),
    }
    let mut ticket=ObjectTicket{repo:repo.name().into(),commit:w.commit,path:w.path,kind:w.kind,id:w.id,expires_ms:now_ms().saturating_add(CAPABILITY_TTL_MS),signature:String::new()};
    ticket.sign(&state.config.capability_key)?;Ok(ticket)
}

fn tree_at(repo:&Repository,commit:ObjectId,path:&RepoPath)->Result<ObjectId> {
    if path.is_root(){return repo.root_tree(commit);}
    match repo.entry(commit,path)? {Some(TreeEntry::Directory{id})=>Ok(id),_=>Err(Error::NotFound(format!("directory {path}")))}
}

fn check_commit(repo:&Repository,a:&Access,c:&scion_types::Changeset)->Result<()> {
    if !a.permissions.write {return Err(Error::Permission("repository commit denied".into()));}
    for (path,change) in &c.changes {
        a.permissions.require(path,true)?;
        if let FileChange::Put{entry,copy_from}=change {
            // Do not let a caller launder another repository's secret blob by ID.
            let uploaded=repo.store().get_record(&format!("repo/{}/uploads/{}",repo.name(),a.principal),&entry.id.to_string())?.is_some();
            let previous=if let Some(parent)=c.parents.first(){a.permissions.allows(path,false)&&repo.file(*parent,path)?.is_some_and(|e|e.id==entry.id)}else{false};
            let copied=if let Some(source)=copy_from {a.permissions.require(&source.path,false)?;repo.file(source.changeset,&source.path)?.is_some_and(|e|e.id==entry.id)}else{false};
            if !uploaded&&!previous&&!copied{return Err(Error::Permission("blob must be uploaded by this principal or copied from an authorized path".into()));}
            if let Some(parent)=c.parents.first() {if matches!(repo.entry(*parent,path)?,Some(TreeEntry::Directory{..})) && !a.permissions.allows_subtree(path,true){return Err(Error::Permission("cannot replace a protected directory".into()));}}
        }
    }
    Ok(())
}

fn visible_page(repo:&Repository,a:&Access,commit:ObjectId,prefix:&RepoPath,after:Option<&RepoPath>,limit:usize)->Result<scion_protocol::FilePage> {
    let mut cursor=after.cloned();let mut visible=BTreeMap::new();
    loop {
        let (entries,next)=repo.list_files_page(commit,prefix,cursor.as_ref(),1024)?;
        for (path,entry) in entries {
            if !a.permissions.allows(&path,false){continue;}
            if visible.len()==limit {
                let next=visible.last_key_value().map(|(p,_)|p.clone());
                return Ok(scion_protocol::FilePage{entries:visible,next});
            }
            visible.insert(path,entry);
        }
        match next {Some(next)=>cursor=Some(next),None=>return Ok(scion_protocol::FilePage{entries:visible,next:None})}
    }
}

fn dispatch(state:&ServerState,name:&str,a:&Access,request:RpcRequest)->Result<RpcReply> {
    let repo=state.repository(name)?;
    if !a.permissions.read&&!a.permissions.write{return Err(Error::Permission("repository access denied".into()));}
    let bounded=|limit:usize|limit.clamp(1,10_000);
    match request {
        RpcRequest::PutBlob{data}=>{if !a.permissions.write{return Err(Error::Permission("repository upload denied".into()));}let bytes=STANDARD.decode(data).map_err(|_|Error::Invalid("invalid base64 blob".into()))?;RpcReply::success(upload_blob(&repo,a,&bytes)?)},
        RpcRequest::Commit{changeset}=>{check_commit(&repo,a,&changeset)?;let c=repo.create_commit(changeset)?;tracing::info!(repo=name,principal=%a.principal,changeset=%c.id()?,"changeset created");RpcReply::success(c.id()?)},
        RpcRequest::GetChangeset{id}=>{a.permissions.require_full(false)?;RpcReply::success(repo.get_changeset(id)?)},
        RpcRequest::Resolve{revision}=>{if !a.permissions.read{return Err(Error::Permission("read access required".into()));}RpcReply::success(repo.resolve(&revision)?)},
        RpcRequest::GetRef{name}=>{if !a.permissions.read{return Err(Error::Permission("read access required".into()));}RpcReply::success(repo.resolve_ref(&name)?)},
        RpcRequest::ListRefs=>{if !a.permissions.read{return Err(Error::Permission("read access required".into()));}RpcReply::success(repo.list_refs()?)},
        RpcRequest::SetRef{name,expected,target}=>{if !a.permissions.admin{return Err(Error::Permission("ref changes require repository admin permission".into()));}RpcReply::success(repo.update_ref(&name,expected,target)?)},
        RpcRequest::RootTree{commit}=>{a.permissions.require_full(false)?;RpcReply::success(repo.root_tree(commit)?)},
        RpcRequest::TreeAt{commit,path}=>{if !a.permissions.allows_subtree(&path,false){return Err(Error::Permission("tree contains restricted descendants".into()));}RpcReply::success(tree_at(&repo,commit,&path)?)},
        RpcRequest::ListFiles{commit,prefix}=>{let files=repo.list_files(commit,&prefix)?.into_iter().filter(|(path,_)|a.permissions.allows(path,false)).collect::<BTreeMap<_,_>>();RpcReply::success(files)},
        RpcRequest::ListFilesPage{commit,prefix,after,limit}=>RpcReply::success(visible_page(&repo,a,commit,&prefix,after.as_ref(),bounded(limit))?),
        RpcRequest::ReadFile{commit,path}=>{a.permissions.require(&path,false)?;RpcReply::success(STANDARD.encode(repo.read_file(commit,&path)?))},
        RpcRequest::Diff{base,target}=>RpcReply::success(repo.diff_filtered(base,target,|path|a.permissions.allows(path,false))?),
        RpcRequest::Log{revision,limit}=>{a.permissions.require_full(false)?;RpcReply::success(repo.log(revision,bounded(limit))?)},
        RpcRequest::History{revision,path,limit}=>{if !a.permissions.allows_subtree(&path,false){return Err(Error::Permission("history contains restricted descendants".into()));}RpcReply::success(repo.history(revision,&path,bounded(limit))?)},
        RpcRequest::Blame{revision,path}=>{a.permissions.require(&path,false)?;RpcReply::success(repo.blame(revision,&path)?)},
        RpcRequest::Search{revision,prefix,query,limit}=>RpcReply::success(repo.search_filtered(revision,&prefix,&query,bounded(limit),|path|a.permissions.allows(path,false))?),
        RpcRequest::Copies{revision}=>{let copies=repo.copies(revision)?.into_iter().filter(|(p,s)|a.permissions.allows(p,false)&&a.permissions.allows(&s.path,false)).collect::<BTreeMap<_,_>>();RpcReply::success(copies)},
        RpcRequest::IsAncestor{ancestor,descendant}=>{a.permissions.require_full(false)?;RpcReply::success(repo.is_ancestor(ancestor,descendant)?)},
        RpcRequest::MergeBases{a:one,b}=>{a.permissions.require_full(false)?;RpcReply::success(repo.merge_bases(one,b)?)},
        RpcRequest::NthParent{revision,distance}=>{a.permissions.require_full(false)?;RpcReply::success(repo.nth_first_parent(revision,distance)?)},
        RpcRequest::Merge{ours,theirs,author,message,timestamp_ms}=>{a.permissions.require_full(false)?;a.permissions.require_full(true)?;RpcReply::success(repo.merge(ours,theirs,author,message,timestamp_ms)?)},
        RpcRequest::Rebase{revision,onto}=>{a.permissions.require_full(false)?;a.permissions.require_full(true)?;RpcReply::success(repo.rebase(revision,onto)?)},
        RpcRequest::Revert{revision,onto,mainline,author,message,timestamp_ms}=>{a.permissions.require_full(false)?;a.permissions.require_full(true)?;RpcReply::success(repo.revert(revision,onto,mainline,author,message,timestamp_ms)?)},
        RpcRequest::Bisect{good,bad,skip}=>{a.permissions.require_full(false)?;if skip.len()>10_000{return Err(Error::Limit("too many skipped commits".into()));}RpcReply::success(repo.bisect_candidate(good,bad,&skip)?)},
        RpcRequest::ObjectAccess{commit,path,kind,id}=>RpcReply::success(ticket(state,&repo,a,ObjectWitness{commit,path,kind,id})?),
        RpcRequest::ObjectAccessBatch{objects}=>{if objects.len()>MAX_BATCH_OBJECTS{return Err(Error::Limit("object batch too large".into()));}RpcReply::success(objects.into_iter().map(|w|ticket(state,&repo,a,w)).collect::<Result<Vec<_>>>()?)},
        RpcRequest::ChangeLog{after,limit}=>{a.permissions.require_full(false)?;RpcReply::success(state.store.read_log(name,after,bounded(limit))?)},
    }
}

async fn ready(State(s):State<Arc<ServerState>>)->Response {
    match tokio::task::spawn_blocking(move||s.store.get_record("health","probe")).await {Ok(Ok(_))=>(StatusCode::OK,"ready").into_response(),_=>(StatusCode::SERVICE_UNAVAILABLE,"storage unavailable").into_response()}
}
async fn metrics(State(s):State<Arc<ServerState>>)->Response {
    let m=&s.metrics;
    let body=format!("# TYPE scion_rpc_requests_total counter\nscion_rpc_requests_total {}\n# TYPE scion_object_requests_total counter\nscion_object_requests_total {}\n# TYPE scion_errors_total counter\nscion_errors_total {}\n# TYPE scion_rejected_total counter\nscion_rejected_total {}\n# TYPE scion_object_bytes_total counter\nscion_object_bytes_total {}\n",m.rpc.load(Ordering::Relaxed),m.objects.load(Ordering::Relaxed),m.errors.load(Ordering::Relaxed),m.rejected.load(Ordering::Relaxed),m.bytes.load(Ordering::Relaxed));
    ([(header::CONTENT_TYPE,"text/plain; version=0.0.4")],body).into_response()
}
