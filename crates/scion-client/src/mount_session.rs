//! Linux mount identity validation for the pre-commit kernel writeback barrier.
//! Stale records are never followed. The active lock, mountinfo identity and the
//! opened descriptor's kernel mount ID must all agree before `syncfs` is called.
use fs2::FileExt;
use scion_types::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::{fd::AsRawFd, unix::ffi::OsStringExt},
    path::{Path, PathBuf},
};
#[derive(Debug, Serialize, Deserialize)]
struct Session {
    version: u32,
    mountpoint: PathBuf,
    mount_id: u64,
    source: String,
}
#[derive(Debug, PartialEq, Eq)]
struct Mount {
    id: u64,
    path: PathBuf,
    source: String,
    filesystem: String,
}
fn parse_mount(line: &str) -> Option<Mount> {
    let fields: Vec<_> = line.split(' ').collect();
    let separator = fields.iter().position(|s| *s == "-")?;
    if separator < 6 || fields.len() < separator + 4 {
        return None;
    }
    let path = PathBuf::from(std::ffi::OsString::from_vec(unescape(fields[4])?));
    let source = String::from_utf8(unescape(fields[separator + 2])?).ok()?;
    Some(Mount {
        id: fields[0].parse().ok()?,
        path,
        source,
        filesystem: fields[separator + 1].into(),
    })
}
fn unescape(text: &str) -> Option<Vec<u8>> {
    let mut result = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if i + 3 >= bytes.len()
                || !bytes[i + 1..i + 4]
                    .iter()
                    .all(|b| (b'0'..=b'7').contains(b))
            {
                return None;
            }
            let value = (u16::from(bytes[i + 1] - b'0') * 64)
                + (u16::from(bytes[i + 2] - b'0') * 8)
                + u16::from(bytes[i + 3] - b'0');
            result.push(u8::try_from(value).ok()?);
            i += 4;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    Some(result)
}
fn mounts() -> Result<Vec<Mount>> {
    Ok(fs::read_to_string("/proc/self/mountinfo")?
        .lines()
        .filter_map(parse_mount)
        .collect())
}
#[cfg(feature = "fuse")]
pub(crate) fn register(root: &Path, mountpoint: &Path, source: &str) -> Result<()> {
    use std::io::Write;
    let mount = mounts()?
        .into_iter()
        .find(|m| {
            m.path == mountpoint && m.source == source && m.filesystem == "fuse.scion-overlay"
        })
        .ok_or_else(|| Error::Conflict("cannot identify active Scion overlay mount".into()))?;
    let session = Session {
        version: 1,
        mountpoint: mountpoint.into(),
        mount_id: mount.id,
        source: source.into(),
    };
    let mut temp = tempfile::NamedTempFile::new_in(root)?;
    temp.write_all(&serde_json::to_vec(&session)?)?;
    temp.as_file().sync_all()?;
    temp.persist(root.join("mounted.json"))
        .map_err(|e| Error::Io(e.error))?;
    File::open(root)?.sync_all()?;
    Ok(())
}
pub(crate) fn flush_active(root: &Path) -> Result<()> {
    let lock_path = root.join("mount.lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::Invalid("overlay mount lock is a symlink".into()));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    match lock.try_lock_exclusive() {
        Ok(()) => return Ok(()), // No active mount: do not read/follow stale records.
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(e) => return Err(e.into()),
    }
    let path = root.join("mounted.json");
    let metadata = fs::symlink_metadata(&path).map_err(|_| {
        Error::Conflict(
            "overlay mount is starting or its session record is unavailable; retry commit".into(),
        )
    })?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(Error::Corrupt("invalid mount session record".into()));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(Error::Corrupt("mount session record too large".into()));
    }
    let session: Session = serde_json::from_slice(&bytes)?;
    if session.version != 1
        || !session.mountpoint.is_absolute()
        || !session.source.starts_with("scion-overlay:")
    {
        return Err(Error::Corrupt("invalid Scion mount identity".into()));
    }
    if !mounts()?.iter().any(|m| {
        m.id == session.mount_id
            && m.path == session.mountpoint
            && m.source == session.source
            && m.filesystem == "fuse.scion-overlay"
    }) {
        return Err(Error::Conflict(
            "active overlay mount identity differs from its recorded session".into(),
        ));
    }
    let file = File::open(&session.mountpoint)?;
    let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    let mount_id = fdinfo.lines().find_map(|line| {
        line.strip_prefix("mnt_id:")
            .and_then(|value| value.trim().parse::<u64>().ok())
    });
    if mount_id != Some(session.mount_id) {
        return Err(Error::Conflict(
            "overlay mount changed while preparing commit".into(),
        ));
    }
    rustix::fs::syncfs(file)
        .map_err(|e| Error::Io(std::io::Error::from_raw_os_error(e.raw_os_error())))?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mountinfo_parser_decodes_spaces_and_checks_complete_identity() {
        let mount=parse_mount("73 42 0:64 / /tmp/a\\040b rw,nosuid - fuse.scion-overlay scion-overlay:abc rw,user_id=1000").unwrap();
        assert_eq!(
            mount,
            Mount {
                id: 73,
                path: PathBuf::from("/tmp/a b"),
                source: "scion-overlay:abc".into(),
                filesystem: "fuse.scion-overlay".into()
            }
        );
        assert!(parse_mount("malformed").is_none());
        assert!(unescape("bad\\999").is_none());
    }
    #[test]
    fn stale_sessions_are_ignored_and_active_mismatches_never_follow_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("mounted.json"), b"not even valid JSON").unwrap();
        flush_active(dir.path()).unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join("mount.lock"))
            .unwrap();
        lock.lock_exclusive().unwrap();
        let fake = Session {
            version: 1,
            mountpoint: "/path/that/must/not/be/opened".into(),
            mount_id: u64::MAX,
            source: "scion-overlay:fake".into(),
        };
        fs::write(
            dir.path().join("mounted.json"),
            serde_json::to_vec(&fake).unwrap(),
        )
        .unwrap();
        assert!(matches!(flush_active(dir.path()), Err(Error::Conflict(_))));
    }
}
