//! Identical immutable-object services for regional caches and node daemons.
//! Capabilities are checked before every hit; upstream tiers are validated at startup.
use axum::{extract::{DefaultBodyLimit,Path,State},http::StatusCode,response::{IntoResponse,Response},routing::{get,post},Json,Router};
use clap::Parser;
use parking_lot::Mutex;
use scion_client::{ObjectCache,read_bounded};
use scion_protocol::{now_ms,parse_secret,ObjectRequest,RpcReply,ServiceInfo,Tier,PROTOCOL_VERSION};
use scion_store::MAX_OBJECT_SIZE;
use scion_types::{Error,ObjectId,ObjectKind,Result};
use std::{collections::HashMap,net::SocketAddr,path::PathBuf,sync::{Arc,Weak,atomic::{AtomicU64,Ordering}},time::{Duration,Instant}};
use tokio::sync::{Mutex as AsyncMutex,Semaphore};

type Key=(ObjectKind,ObjectId);
#[derive(Default)]
pub struct CacheMetrics {pub hits:AtomicU64,pub misses:AtomicU64,pub rejected:AtomicU64,pub upstream_errors:AtomicU64,pub bytes_served:AtomicU64,pub circuit_rejections:AtomicU64}
#[derive(Default)]
struct Circuit {failures:u32,open_until:Option<Instant>,probing:bool}
struct CircuitProbe<'a> {circuit:&'a Mutex<Circuit>,active:bool}
impl Drop for CircuitProbe<'_>{fn drop(&mut self){if self.active{self.circuit.lock().probing=false;}}}
struct Inner {
    cache:ObjectCache,upstream:String,tier:Tier,key:[u8;32],http:reqwest::Client,
    inflight:Mutex<HashMap<Key,Weak<AsyncMutex<()>>>>,circuit:Mutex<Circuit>,permits:Arc<Semaphore>,metrics:CacheMetrics,
}
#[derive(Clone)]
pub struct CacheService(Arc<Inner>);
impl CacheService {
    pub async fn connect(cache:ObjectCache,upstream:impl Into<String>,tier:Tier,key:[u8;32],max_inflight:usize)->Result<Self> {
        if tier==Tier::Server{return Err(Error::Invalid("a cache must be cache or daemon tier".into()));}
        let upstream=upstream.into().trim_end_matches('/').to_owned();
        let parsed=reqwest::Url::parse(&upstream).map_err(|e|Error::Invalid(e.to_string()))?;
        if !["http","https"].contains(&parsed.scheme())||!parsed.username().is_empty()||parsed.password().is_some(){return Err(Error::Invalid("upstream must be an HTTP(S) URL without credentials".into()));}
        let http=reqwest::Client::builder().timeout(Duration::from_secs(60)).connect_timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().map_err(|e|Error::Transport(e.to_string()))?;
        let response=http.get(format!("{upstream}/v1/info")).send().await.map_err(|e|Error::Transport(e.without_url().to_string()))?;
        let info:ServiceInfo=serde_json::from_slice(&read_bounded(response,64*1024).await?)?;
        Self::validate_upstream(tier,&info)?;
        Ok(Self(Arc::new(Inner{cache,upstream,tier,key,http,inflight:Mutex::new(HashMap::new()),circuit:Mutex::new(Circuit::default()),permits:Arc::new(Semaphore::new(max_inflight.max(1))),metrics:CacheMetrics::default()})))
    }
    pub fn validate_upstream(tier:Tier,info:&ServiceInfo)->Result<()> {
        if info.protocol!=PROTOCOL_VERSION||Some(info.tier)!=tier.upstream(){return Err(Error::Invalid(format!("invalid upstream tier {:?} for {:?} or incompatible protocol",info.tier,tier)));}Ok(())
    }
    pub fn router(&self)->Router {Router::new().route("/health",get(health)).route("/ready",get(health)).route("/metrics",get(metrics)).route("/v1/info",get(info)).route("/v1/repos/{repo}/objects",post(objects)).layer(DefaultBodyLimit::max(64*1024)).with_state(self.clone())}
    pub fn cache(&self)->&ObjectCache{&self.0.cache}
    pub fn metrics(&self)->&CacheMetrics{&self.0.metrics}
    pub async fn fetch(&self,repo:&str,request:ObjectRequest)->Result<Arc<Vec<u8>>> {
        request.ticket.verify(&self.0.key,repo,now_ms()).inspect_err(|_|{self.0.metrics.rejected.fetch_add(1,Ordering::Relaxed);})?;
        let ticket=&request.ticket;let kind=ticket.kind;let id=ticket.id;
        if !matches!(kind,ObjectKind::Blob|ObjectKind::Tree){return Err(Error::Permission("only trees and blobs may pass through object caches".into()));}
        if let Some(bytes)=self.load_cached(kind,id).await? {self.0.metrics.hits.fetch_add(1,Ordering::Relaxed);return Ok(bytes);}
        let gate={let mut in_flight=self.0.inflight.lock();in_flight.retain(|_,v|v.strong_count()>0);let entry=in_flight.entry((kind,id)).or_default();if let Some(gate)=entry.upgrade(){gate}else{let gate=Arc::new(AsyncMutex::new(()));*entry=Arc::downgrade(&gate);gate}};
        let _guard=gate.lock().await;
        if let Some(bytes)=self.load_cached(kind,id).await? {self.0.metrics.hits.fetch_add(1,Ordering::Relaxed);return Ok(bytes);}
        let _permit=self.0.permits.clone().try_acquire_owned().map_err(|_|Error::Limit("cache upstream concurrency limit reached".into()))?;
        let _probe=self.admit_upstream()?;
        self.0.metrics.misses.fetch_add(1,Ordering::Relaxed);
        let result=self.fetch_upstream(repo,&request).await;
        match result {
            Ok(encoded)=>{
                {let mut circuit=self.0.circuit.lock();circuit.failures=0;circuit.open_until=None;}
                let cache=self.0.cache.clone();
                tokio::task::spawn_blocking(move||{cache.put(kind,id,&encoded)?;Ok(Arc::new(encoded))}).await.map_err(|error|Error::Storage(error.to_string()))?
            },
            Err(error)=>{
                self.0.metrics.upstream_errors.fetch_add(1,Ordering::Relaxed);
                let mut circuit=self.0.circuit.lock();circuit.failures=circuit.failures.saturating_add(1);if circuit.failures>=5{circuit.open_until=Some(Instant::now()+Duration::from_secs(5));}
                Err(error)
            },
        }
    }
    async fn load_cached(&self,kind:ObjectKind,id:ObjectId)->Result<Option<Arc<Vec<u8>>>> {
        let cache=self.0.cache.clone();tokio::task::spawn_blocking(move||cache.get(kind,id)).await.map_err(|error|Error::Storage(error.to_string()))?
    }
    fn admit_upstream(&self)->Result<CircuitProbe<'_>> {
        let mut circuit=self.0.circuit.lock();
        let active=if let Some(until)=circuit.open_until {
            if Instant::now()<until||circuit.probing{self.0.metrics.circuit_rejections.fetch_add(1,Ordering::Relaxed);return Err(Error::Transport("configured upstream is temporarily unavailable; circuit is open".into()));}
            circuit.probing=true;true
        }else{false};
        Ok(CircuitProbe{circuit:&self.0.circuit,active})
    }
    async fn fetch_upstream(&self,repo:&str,request:&ObjectRequest)->Result<Vec<u8>> {
        let mut url=reqwest::Url::parse(&self.0.upstream).map_err(|e|Error::Invalid(e.to_string()))?;
        url.path_segments_mut().map_err(|_|Error::Invalid("invalid upstream URL".into()))?.pop_if_empty().extend(["v1","repos",repo,"objects"]);
        let response=self.0.http.post(url).json(request).send().await.map_err(|e|Error::Transport(e.without_url().to_string()))?;
        if !response.status().is_success(){return Err(Error::Transport(format!("configured upstream returned HTTP {}",response.status())));}
        read_bounded(response,MAX_OBJECT_SIZE+1024*1024).await
    }
}
async fn health()->&'static str{"ok"}
async fn info(State(service):State<CacheService>)->Json<ServiceInfo>{Json(ServiceInfo{protocol:PROTOCOL_VERSION,tier:service.0.tier,version:env!("CARGO_PKG_VERSION").into()})}
async fn metrics(State(service):State<CacheService>)->String{
    let m=service.metrics();format!("scion_cache_hits_total {}\nscion_cache_misses_total {}\nscion_cache_rejected_total {}\nscion_cache_upstream_errors_total {}\nscion_cache_bytes_served_total {}\nscion_cache_circuit_rejections_total {}\nscion_cache_disk_bytes {}\nscion_cache_objects {}\n",m.hits.load(Ordering::Relaxed),m.misses.load(Ordering::Relaxed),m.rejected.load(Ordering::Relaxed),m.upstream_errors.load(Ordering::Relaxed),m.bytes_served.load(Ordering::Relaxed),m.circuit_rejections.load(Ordering::Relaxed),service.cache().bytes(),service.cache().len())
}
async fn objects(State(service):State<CacheService>,Path(repo):Path<String>,Json(request):Json<ObjectRequest>)->Response{
    match service.fetch(&repo,request).await {
        Ok(bytes)=>{service.metrics().bytes_served.fetch_add(bytes.len() as u64,Ordering::Relaxed);([(axum::http::header::CONTENT_TYPE,"application/vnd.scion.object"),(axum::http::header::CACHE_CONTROL,"private, no-store")],bytes.as_ref().clone()).into_response()},
        Err(error)=>{let (status,code)=match &error{Error::Permission(_)=>(StatusCode::FORBIDDEN,"permission"),Error::Limit(_)=>(StatusCode::TOO_MANY_REQUESTS,"limit"),Error::Invalid(_)=>(StatusCode::BAD_REQUEST,"invalid"),_=>(StatusCode::BAD_GATEWAY,"upstream")};(status,Json(RpcReply::failure(code,error.to_string()))).into_response()},
    }
}

/// Shared options for the two separately shippable cache-tier executables.
#[derive(Debug,Parser)]
#[command(version,about="Verified, bounded Scion immutable-object cache")]
pub struct CacheOptions {
    #[arg(long,env="SCION_LISTEN")] pub listen:Option<SocketAddr>,
    #[arg(long,env="SCION_UPSTREAM")] pub upstream:Option<String>,
    #[arg(long,default_value="scion-cache-data")] pub data:PathBuf,
    #[arg(long,env="SCION_CAPABILITY_KEY",hide_env_values=true)] pub capability_key:String,
    #[arg(long,default_value_t=1024*1024*1024)] pub max_bytes:u64,
    #[arg(long,default_value_t=64)] pub max_inflight:usize,
    #[arg(long)] pub socket:Option<PathBuf>,
    #[arg(long)] pub snapshot:Option<PathBuf>,
    #[arg(long)] pub snapshot_out:Option<PathBuf>,
    #[arg(long,default_value_t=300)] pub snapshot_interval:u64,
}
/// Start a cache or daemon. No fallback upstream is configured or attempted.
pub async fn run(options:CacheOptions,tier:Tier)->Result<()> {
    let key=parse_secret(&options.capability_key)?;
    let cache=ObjectCache::open(&options.data,options.max_bytes)?;
    if let Some(path)=&options.snapshot {cache.import_snapshot(std::fs::File::open(path)?)?;}
    let upstream=options.upstream.unwrap_or_else(||if tier==Tier::Daemon{"http://127.0.0.1:7421"}else{"http://127.0.0.1:7420"}.into());
    let service=CacheService::connect(cache,upstream,tier,key,options.max_inflight).await?;
    let periodic=if let Some(path)=options.snapshot_out.clone().filter(|_|options.snapshot_interval>0){
        let cache=service.cache().clone();let seconds=options.snapshot_interval;
        Some(tokio::spawn(async move{let mut interval=tokio::time::interval(Duration::from_secs(seconds));interval.tick().await;loop{interval.tick().await;let cache=cache.clone();let path=path.clone();match tokio::task::spawn_blocking(move||write_snapshot(&cache,&path)).await{Ok(Ok(()))=>{},Ok(Err(error))=>tracing::error!(%error,"periodic cache snapshot failed"),Err(error)=>tracing::error!(%error,"snapshot worker failed")}}}))
    }else{None};
    let listen=options.listen.unwrap_or_else(||SocketAddr::from(([127,0,0,1],if tier==Tier::Daemon{7422}else{7421})));
    let tcp=tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen,?tier,"Scion object tier listening");
    #[cfg(unix)] {
        if let Some(socket)=&options.socket {
            if tier!=Tier::Daemon{return Err(Error::Invalid("Unix sockets are only supported for the node daemon".into()));}
            if socket.exists(){return Err(Error::Conflict(format!("socket path already exists: {}",socket.display())));}
            let unix=tokio::net::UnixListener::bind(socket)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(socket,std::fs::Permissions::from_mode(0o600))?;
            let result=tokio::select!{
                r=axum::serve(tcp,service.router())=>r,
                r=axum::serve(unix,service.router())=>r,
                _=tokio::signal::ctrl_c()=>Ok(()),
            };
            std::fs::remove_file(socket)?;result?;
        }else{axum::serve(tcp,service.router()).with_graceful_shutdown(async{let _=tokio::signal::ctrl_c().await;}).await?;}
    }
    #[cfg(not(unix))] {
        if options.socket.is_some(){return Err(Error::Invalid("Unix sockets are unavailable on this platform".into()));}
        axum::serve(tcp,service.router()).with_graceful_shutdown(async{let _=tokio::signal::ctrl_c().await;}).await?;
    }
    if let Some(periodic)=periodic{periodic.abort();}
    if let Some(path)=options.snapshot_out {let cache=service.cache().clone();tokio::task::spawn_blocking(move||write_snapshot(&cache,&path)).await.map_err(|error|Error::Storage(error.to_string()))??;}

    Ok(())
}
fn write_snapshot(cache:&ObjectCache,path:&std::path::Path)->Result<()> {
    let parent=path.parent().filter(|p|!p.as_os_str().is_empty()).unwrap_or_else(||std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;let mut temp=tempfile::NamedTempFile::new_in(parent)?;
    cache.export_snapshot(&mut temp)?;temp.as_file().sync_all()?;temp.persist(path).map_err(|error|Error::Io(error.error))?;
    #[cfg(unix)] std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn tiering_cannot_skip_regional_cache(){let server=ServiceInfo{protocol:1,tier:Tier::Server,version:"test".into()};assert!(CacheService::validate_upstream(Tier::Cache,&server).is_ok());assert!(CacheService::validate_upstream(Tier::Daemon,&server).is_err());let cache=ServiceInfo{tier:Tier::Cache,..server};assert!(CacheService::validate_upstream(Tier::Daemon,&cache).is_ok());assert!(CacheService::validate_upstream(Tier::Cache,&cache).is_err());}
    #[tokio::test]
    async fn concurrent_misses_coalesce_and_expired_tickets_cannot_read_hits() {
        use scion_protocol::{ObjectTicket,CAPABILITY_TTL_MS};
        let payload=b"cache me";let id=ObjectId::hash(ObjectKind::Blob,payload);
        let encoded=scion_store::encode_object(ObjectKind::Blob,payload).unwrap();
        let calls=Arc::new(AtomicU64::new(0));let count=calls.clone();
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
        let app=Router::new().route("/v1/info",get(||async{Json(ServiceInfo{protocol:1,tier:Tier::Server,version:"test".into()})})).route("/v1/repos/test/objects",post(move||{let count=count.clone();let encoded=encoded.clone();async move{count.fetch_add(1,Ordering::Relaxed);tokio::time::sleep(Duration::from_millis(20)).await;encoded}}));
        let upstream=tokio::spawn(async move{axum::serve(listener,app).await.unwrap();});
        let temp=tempfile::tempdir().unwrap();let key=[9;32];
        let service=CacheService::connect(ObjectCache::open(temp.path(),100_000).unwrap(),format!("http://{address}"),Tier::Cache,key,4).await.unwrap();
        let mut ticket=ObjectTicket{repo:"test".into(),commit:id,path:scion_types::RepoPath::new("file").unwrap(),kind:ObjectKind::Blob,id,expires_ms:now_ms()+CAPABILITY_TTL_MS,signature:String::new()};ticket.sign(&key).unwrap();
        let results=futures::future::join_all((0..16).map(|_|service.fetch("test",ObjectRequest{ticket:ticket.clone()}))).await;
        assert!(results.iter().all(Result::is_ok));assert_eq!(calls.load(Ordering::Relaxed),1);
        let mut expired=ticket;expired.expires_ms=now_ms()-1;expired.sign(&key).unwrap();
        assert!(matches!(service.fetch("test",ObjectRequest{ticket:expired}).await,Err(Error::Permission(_))));
        assert_eq!(calls.load(Ordering::Relaxed),1);upstream.abort();
    }

}
