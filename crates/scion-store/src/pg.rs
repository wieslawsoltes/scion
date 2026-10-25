use crate::{ImmutableRecord, Storage, decode_object, encode_object, validate_publication};
use parking_lot::Mutex;
use postgres::{Client, Config};
use scion_types::{Error, LogEntry, ObjectId, ObjectKind, Result};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

fn pg_error(e: postgres::Error) -> Error {
    Error::Storage(e.to_string())
}

/// Shared, pooled PostgreSQL storage. Supply `sslmode=require` in production;
/// certificate verification uses the platform trust store. Objects can be
/// partitioned across independent instances with [`crate::ShardedStore`].
pub struct PostgresStore {
    pool: Vec<Mutex<Client>>,
    next: AtomicUsize,
    config: Config,
    tls: tokio_postgres_rustls::MakeRustlsConnect,
}
impl PostgresStore {
    pub fn connect(url: &str, pool_size: usize) -> Result<Self> {
        if !(1..=128).contains(&pool_size) {
            return Err(Error::Invalid(
                "Postgres pool must have 1..128 connections".into(),
            ));
        }
        let (tls, _) = tokio_postgres_rustls::MakeRustlsConnect::with_native_certs()
            .map_err(|_| Error::Storage("unable to load platform TLS trust roots".into()))?;
        let mut config = url
            .parse::<Config>()
            .map_err(|_| Error::Invalid("invalid PostgreSQL connection options".into()))?;
        config
            .connect_timeout(Duration::from_secs(10))
            .application_name("scion");
        let mut pool = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            pool.push(Mutex::new(Self::open_connection(&config, &tls)?));
        }
        {
            let mut connection = pool[0].lock();
            let mut migration = connection.transaction().map_err(pg_error)?;
            migration
                .query_one("SELECT pg_advisory_xact_lock($1)", &[&0x5343494f4e5631i64])
                .map_err(pg_error)?;
            migration.batch_execute("CREATE TABLE IF NOT EXISTS scion_objects(kind TEXT NOT NULL,id TEXT NOT NULL,encoded BYTEA NOT NULL,PRIMARY KEY(kind,id));
            CREATE TABLE IF NOT EXISTS scion_records(namespace TEXT NOT NULL,key TEXT NOT NULL,value BYTEA NOT NULL,PRIMARY KEY(namespace,key));
            CREATE TABLE IF NOT EXISTS scion_log(sequence BIGSERIAL PRIMARY KEY,repo TEXT NOT NULL,id TEXT NOT NULL,UNIQUE(repo,id));
            CREATE INDEX IF NOT EXISTS scion_log_repo ON scion_log(repo,sequence);").map_err(pg_error)?;
            migration.commit().map_err(pg_error)?;
        }
        Ok(Self {
            pool,
            next: AtomicUsize::new(0),
            config,
            tls,
        })
    }
    fn open_connection(
        config: &Config,
        tls: &tokio_postgres_rustls::MakeRustlsConnect,
    ) -> Result<Client> {
        let mut client = config.connect(tls.clone()).map_err(pg_error)?;
        client
            .batch_execute("SET statement_timeout = '30s'")
            .map_err(pg_error)?;
        Ok(client)
    }
    fn client(&self) -> Result<parking_lot::MutexGuard<'_, Client>> {
        let mut connection =
            self.pool[self.next.fetch_add(1, Ordering::Relaxed) % self.pool.len()].lock();
        if connection.is_closed() {
            *connection = Self::open_connection(&self.config, &self.tls)?;
        }
        Ok(connection)
    }
    fn log_lock(repo: &str) -> i64 {
        let id = ObjectId::hash(ObjectKind::Derived, repo.as_bytes());
        i64::from_le_bytes(id.0[..8].try_into().expect("fixed width identifier"))
    }
}
impl Drop for PostgresStore {
    fn drop(&mut self) {
        // postgres::Client closes by driving its private runtime. Arc<Storage>
        // may be released by an async service, where nested block_on panics.
        let pool = std::mem::take(&mut self.pool);
        let _ = std::thread::spawn(move || drop(pool)).join();
    }
}
impl Storage for PostgresStore {
    fn put_object(&self, k: ObjectKind, b: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::hash(k, b);
        let encoded = encode_object(k, b)?;
        self.client()?.execute("INSERT INTO scion_objects(kind,id,encoded) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING",&[&k.as_str(),&id.to_string(),&encoded]).map_err(pg_error)?;
        Ok(id)
    }
    fn get_object(&self, k: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        decode_object(k, id, &self.get_encoded(k, id)?)
    }
    fn get_encoded(&self, k: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        self.client()?
            .query_opt(
                "SELECT encoded FROM scion_objects WHERE kind=$1 AND id=$2",
                &[&k.as_str(), &id.to_string()],
            )
            .map_err(pg_error)?
            .map(|r| r.get(0))
            .ok_or_else(|| Error::NotFound(format!("{k:?}/{id}")))
    }
    fn put_encoded(&self, k: ObjectKind, id: ObjectId, b: &[u8]) -> Result<()> {
        decode_object(k, id, b)?;
        self.client()?.execute("INSERT INTO scion_objects(kind,id,encoded) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING",&[&k.as_str(),&id.to_string(),&b]).map_err(pg_error)?;
        Ok(())
    }
    fn get_record(&self, n: &str, k: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .client()?
            .query_opt(
                "SELECT value FROM scion_records WHERE namespace=$1 AND key=$2",
                &[&n, &k],
            )
            .map_err(pg_error)?
            .map(|r| r.get(0)))
    }
    fn get_record_pair(
        &self,
        first: (&str, &str),
        second: (&str, &str),
    ) -> Result<[Option<Vec<u8>>; 2]> {
        let row = self.client()?.query_one(
            "SELECT (SELECT value FROM scion_records WHERE namespace=$1 AND key=$2), (SELECT value FROM scion_records WHERE namespace=$3 AND key=$4)",
            &[&first.0, &first.1, &second.0, &second.1],
        ).map_err(pg_error)?;
        Ok([row.get(0), row.get(1)])
    }
    fn put_record(&self, n: &str, k: &str, v: &[u8]) -> Result<()> {
        let mut c = self.client()?;
        c.execute("INSERT INTO scion_records(namespace,key,value) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING",&[&n,&k,&v]).map_err(pg_error)?;
        let existing: Vec<u8> = c
            .query_one(
                "SELECT value FROM scion_records WHERE namespace=$1 AND key=$2",
                &[&n, &k],
            )
            .map_err(pg_error)?
            .get(0);
        if existing != v {
            return Err(Error::Conflict(format!(
                "immutable metadata collision: {n}/{k}"
            )));
        }
        Ok(())
    }
    fn compare_and_swap(
        &self,
        n: &str,
        k: &str,
        e: Option<&[u8]>,
        v: Option<&[u8]>,
    ) -> Result<bool> {
        let mut c = self.client()?;
        let count=match (e,v) {
            (None,Some(v))=>c.execute("INSERT INTO scion_records(namespace,key,value) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING",&[&n,&k,&v]),
            (Some(e),Some(v))=>c.execute("UPDATE scion_records SET value=$4 WHERE namespace=$1 AND key=$2 AND value=$3",&[&n,&k,&e,&v]),
            (Some(e),None)=>c.execute("DELETE FROM scion_records WHERE namespace=$1 AND key=$2 AND value=$3",&[&n,&k,&e]),
            (None,None)=>return Ok(c.query_opt("SELECT 1 FROM scion_records WHERE namespace=$1 AND key=$2",&[&n,&k]).map_err(pg_error)?.is_none()),
        }.map_err(pg_error)?;
        Ok(count == 1)
    }
    fn append_log(&self, r: &str, id: ObjectId) -> Result<u64> {
        self.publish_commit(r, id, &[])
    }
    fn publish_commit(&self, r: &str, id: ObjectId, records: &[ImmutableRecord]) -> Result<u64> {
        validate_publication(r, id, records)?;
        let mut c = self.client()?;
        let mut tx = c.transaction().map_err(pg_error)?;
        // Writers share this lock and proceed concurrently. A log reader takes
        // the exclusive side to establish a committed sequence frontier: BIGSERIAL
        // allocation alone does not order transaction commits.
        tx.query_one(
            "SELECT pg_advisory_xact_lock_shared($1)",
            &[&Self::log_lock(r)],
        )
        .map_err(pg_error)?;
        let mut ordered: Vec<_> = records.iter().collect();
        ordered.sort_by_key(|record| (&record.namespace, &record.key));
        for record in ordered {
            insert_immutable(&mut tx, &record.namespace, &record.key, &record.value)?;
        }
        let id = id.to_string();
        tx.execute(
            "INSERT INTO scion_log(repo,id) VALUES ($1,$2) ON CONFLICT DO NOTHING",
            &[&r, &id],
        )
        .map_err(pg_error)?;
        insert_immutable(&mut tx, &format!("repo/{r}/changesets"), &id, b"1")?;
        let seq: i64 = tx
            .query_one(
                "SELECT sequence FROM scion_log WHERE repo=$1 AND id=$2",
                &[&r, &id],
            )
            .map_err(pg_error)?
            .get(0);
        tx.commit().map_err(pg_error)?;
        Ok(seq as u64)
    }
    fn read_log(&self, r: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>> {
        let mut c = self.client()?;
        let mut tx = c.transaction().map_err(pg_error)?;
        tx.query_one("SELECT pg_advisory_xact_lock($1)", &[&Self::log_lock(r)])
            .map_err(pg_error)?;
        let rows=tx.query("SELECT sequence,id FROM scion_log WHERE repo=$1 AND sequence>$2 ORDER BY sequence LIMIT $3",&[&r,&(after.min(i64::MAX as u64) as i64),&(limit.min(10_000) as i64)]).map_err(pg_error)?;
        let entries = rows
            .into_iter()
            .map(|row| {
                Ok(LogEntry {
                    sequence: row.get::<_, i64>(0) as u64,
                    id: row.get::<_, String>(1).parse()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit().map_err(pg_error)?;
        Ok(entries)
    }
    fn scan_records(&self, n: &str, p: &str, l: usize) -> Result<Vec<(String, Vec<u8>)>> {
        let rows=self.client()?.query("SELECT key,value FROM scion_records WHERE namespace=$1 AND key>=$2 ORDER BY key LIMIT $3",&[&n,&p,&(l.min(100_000) as i64)]).map_err(pg_error)?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, Vec<u8>>(1)))
            .take_while(|(k, _)| k.starts_with(p))
            .collect())
    }
}

fn insert_immutable(
    tx: &mut postgres::Transaction<'_>,
    namespace: &str,
    key: &str,
    value: &[u8],
) -> Result<()> {
    let inserted = tx.execute(
        "INSERT INTO scion_records(namespace,key,value) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING",
        &[&namespace, &key, &value],
    ).map_err(pg_error)?;
    if inserted == 0 {
        let previous: Vec<u8> = tx
            .query_one(
                "SELECT value FROM scion_records WHERE namespace=$1 AND key=$2",
                &[&namespace, &key],
            )
            .map_err(pg_error)?
            .get(0);
        if previous != value {
            return Err(Error::Conflict(format!(
                "immutable metadata collision: {namespace}/{key}"
            )));
        }
    }
    Ok(())
}
