//! Streaming, restartable Git migration. The importer never checks out a tree:
//! a long-lived `cat-file --batch` process supplies objects and NUL-delimited
//! first-parent raw diffs preserve whitespace and newline characters in paths.
use scion_engine::Repository;
use scion_types::{Changeset,Error,FileChange,FileEntry,FileKind,ObjectId,RepoPath,Result};
use serde::{Deserialize,Serialize};
use std::{collections::{BTreeMap,BTreeSet},io::{BufRead,BufReader,Read,Write},path::Path,process::{Child,ChildStdin,ChildStdout,Command,Stdio}};

#[derive(Debug,Clone)]
pub struct ImportOptions {pub revision:Option<String>,pub import_refs:bool}
impl Default for ImportOptions {fn default()->Self{Self{revision:None,import_refs:true}}}
#[derive(Debug,Default,Clone,Serialize,Deserialize)]
pub struct ImportReport {pub imported:usize,pub skipped:usize,pub refs:usize,pub tip:Option<ObjectId>}

/// Import reachable Git history in parent-before-child order. Durable OID mapping
/// makes interruption/restart idempotent, including a crash after commit creation.
pub fn import(repository:&Repository,source:impl AsRef<Path>,options:&ImportOptions)->Result<ImportReport> {
    let source=source.as_ref();
    let format=git(source,&["rev-parse","--show-object-format"])?;
    let format=std::str::from_utf8(&format).map_err(|_|Error::Invalid("invalid Git format".into()))?.trim();
    let namespace=format!("repo/{}/git/{format}",repository.name());
    let mut command=Command::new("git");command.arg("-C").arg(source).args(["rev-list","--topo-order","--reverse"]);
    if let Some(revision)=&options.revision {if revision.starts_with('-'){return Err(Error::Invalid("Git revision cannot be an option".into()));}command.arg(revision);}else{command.arg("--all");}
    let mut commits=command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let stdout=commits.stdout.take().ok_or_else(||Error::Storage("git rev-list has no output".into()))?;
    let mut reader=BufReader::new(stdout);
    let mut batch=GitBatch::new(source)?;let mut report=ImportReport::default();let mut line=String::new();
    while reader.read_line(&mut line)?>0 {
        let oid=line.trim().to_owned();line.clear();validate_oid(&oid)?;
        if let Some(existing)=mapping(repository,&namespace,&oid)?{report.skipped+=1;report.tip=Some(existing);continue;}
        let (kind,raw)=batch.get(&oid)?;if kind!="commit" {return Err(Error::Corrupt(format!("Git object {oid} is not a commit")));}
        let parsed=GitCommit::parse(&raw)?;
        let parents=parsed.parents.iter().map(|oid|mapping(repository,&namespace,oid)?.ok_or_else(||Error::Corrupt(format!("parent {oid} was not ingested before child")))).collect::<Result<Vec<_>>>()?;
        let mut args=vec!["diff-tree","--raw","-z","-r","--no-commit-id","--no-renames"];
        if parsed.parents.is_empty(){args.extend(["--root",&oid]);}else{args.extend([parsed.parents[0].as_str(),&oid]);}
        let raw_diff=git(source,&args)?;
        let mut changes=BTreeMap::new();
        let mut fields=raw_diff.split(|b|*b==0).filter(|s|!s.is_empty());
        while let Some(header)=fields.next() {
            let path=fields.next().ok_or_else(||Error::Corrupt("truncated NUL-delimited Git diff".into()))?;
            let path=RepoPath::new(std::str::from_utf8(path).map_err(|_|Error::Invalid("Scion's portable format requires UTF-8 Git paths".into()))?)?;
            let header=std::str::from_utf8(header).map_err(|_|Error::Corrupt("invalid Git diff header".into()))?;
            let parts:Vec<_>=header.split_whitespace().collect();
            if parts.len()!=5||!parts[0].starts_with(':'){return Err(Error::Corrupt(format!("unexpected Git raw diff header {header:?}")));}
            let mode=parts[1];let new_oid=parts[3];
            if mode=="000000" {changes.insert(path,FileChange::Delete);continue;}
            validate_oid(new_oid)?;
            let kind=match mode{"100644"=>FileKind::Regular,"100755"=>FileKind::Executable,"120000"=>FileKind::Symlink,"160000"=>FileKind::Submodule,_=>return Err(Error::Invalid(format!("unsupported Git mode {mode}")))};
            let bytes=if kind==FileKind::Submodule{new_oid.as_bytes().to_vec()}else{let (kind,bytes)=batch.get(new_oid)?;if kind!="blob"{return Err(Error::Corrupt("Git file points to a non-blob".into()));}bytes};
            let id=repository.put_blob(&bytes)?;
            changes.insert(path,FileChange::put(FileEntry{id,size:bytes.len() as u64,kind}));
        }
        let mut metadata=parsed.metadata;metadata.insert("git.oid".into(),oid.clone());metadata.insert("git.object_format".into(),format.into());
        let changeset=Changeset{parents,author:parsed.author,committer:Some(parsed.committer),timestamp_ms:parsed.timestamp_ms,message:parsed.message,changes,metadata};
        let created=repository.create_commit(changeset)?;let id=created.id()?;
        repository.store().put_record(&namespace,&oid,id.to_string().as_bytes())?;
        report.imported+=1;report.tip=Some(id);
    }
    let output=commits.wait_with_output()?;
    if !output.status.success(){return Err(Error::Storage(format!("git rev-list failed: {}",String::from_utf8_lossy(&output.stderr))));}
    if options.import_refs {
        let raw=git(source,&["for-each-ref","--format=%(refname)%00%(objectname)%00%(*objectname)"])?;
        for line in raw.split(|b|*b==b'\n').filter(|l|!l.is_empty()) {
            let fields:Vec<_>=line.split(|b|*b==0).collect();if fields.len()!=3{continue;}
            let name=std::str::from_utf8(fields[0]).map_err(|_|Error::Invalid("Git ref name must be UTF-8".into()))?;
            let oid=std::str::from_utf8(if fields[2].is_empty(){fields[1]}else{fields[2]}).map_err(|_|Error::Corrupt("invalid Git ref target".into()))?;
            let Some(target)=mapping(repository,&namespace,oid)?else{continue;};
            let name=name.strip_prefix("refs/heads/").map(str::to_owned).or_else(||name.strip_prefix("refs/tags/").map(|n|format!("tags/{n}"))).unwrap_or_else(||name.to_owned());
            let expected=repository.resolve_ref(&name)?;
            if !repository.update_ref(&name,expected,Some(target))?{return Err(Error::Conflict(format!("ref {name} changed during Git ingestion")));}
            report.refs+=1;
            // Preserve annotated tag objects separately; refs intentionally resolve to commits.
            if !fields[2].is_empty(){let oid=std::str::from_utf8(fields[1]).map_err(|_|Error::Corrupt("invalid tag OID".into()))?;let (kind,object)=batch.get(oid)?;if kind=="tag"{repository.store().put_record(&format!("repo/{}/git-tags",repository.name()),&name,&object)?;}}
        }
    }
    Ok(report)
}
fn mapping(repository:&Repository,namespace:&str,oid:&str)->Result<Option<ObjectId>>{repository.store().get_record(namespace,oid)?.map(|b|std::str::from_utf8(&b).map_err(|_|Error::Corrupt("invalid Git mapping".into()))?.parse()).transpose()}
fn validate_oid(oid:&str)->Result<()>{if ![40,64].contains(&oid.len())||!oid.bytes().all(|b|b.is_ascii_hexdigit()){return Err(Error::Invalid("invalid Git object ID".into()));}Ok(())}
fn git(source:&Path,args:&[&str])->Result<Vec<u8>>{let result=Command::new("git").arg("-C").arg(source).args(args).output()?;if !result.status.success(){return Err(Error::Storage(format!("git {} failed: {}",args.first().unwrap_or(&""),String::from_utf8_lossy(&result.stderr))));}Ok(result.stdout)}
struct GitBatch{process:Child,input:ChildStdin,output:BufReader<ChildStdout>}
impl GitBatch{
    fn new(source:&Path)->Result<Self>{let mut process=Command::new("git").arg("-C").arg(source).args(["cat-file","--batch"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;let input=process.stdin.take().ok_or_else(||Error::Storage("cat-file missing stdin".into()))?;let output=BufReader::new(process.stdout.take().ok_or_else(||Error::Storage("cat-file missing stdout".into()))?);Ok(Self{process,input,output})}
    fn get(&mut self,oid:&str)->Result<(String,Vec<u8>)>{validate_oid(oid)?;writeln!(self.input,"{oid}")?;self.input.flush()?;let mut header=String::new();self.output.read_line(&mut header)?;let fields:Vec<_>=header.split_whitespace().collect();if fields.len()!=3{return Err(Error::Corrupt(format!("Git cat-file: {header:?}")));}let size=fields[2].parse::<usize>().map_err(|_|Error::Corrupt("invalid Git object size".into()))?;if size>scion_store::MAX_OBJECT_SIZE{return Err(Error::Limit("Git object exceeds configured object limit".into()));}let mut bytes=vec![0;size];self.output.read_exact(&mut bytes)?;let mut newline=[0];self.output.read_exact(&mut newline)?;if newline[0]!=b'\n'{return Err(Error::Corrupt("Git batch framing error".into()));}Ok((fields[1].to_owned(),bytes))}
}
impl Drop for GitBatch{fn drop(&mut self){let _=self.process.kill();let _=self.process.wait();}}
struct GitCommit{parents:Vec<String>,author:String,committer:String,timestamp_ms:i64,message:String,metadata:BTreeMap<String,String>}
impl GitCommit{
    fn parse(bytes:&[u8])->Result<Self>{
        let text=std::str::from_utf8(bytes).map_err(|_|Error::Invalid("Scion commit metadata requires UTF-8; transcode legacy Git encodings before migration".into()))?;
        let (headers,message)=text.split_once("\n\n").ok_or_else(||Error::Corrupt("Git commit lacks message separator".into()))?;
        let mut parents=Vec::new();let mut author=None;let mut committer=None;let mut metadata=BTreeMap::new();
        for line in headers.lines(){if let Some(p)=line.strip_prefix("parent "){parents.push(p.into());}else if let Some(value)=line.strip_prefix("author "){let (person,time,zone)=identity(value)?;author=Some((person,time));metadata.insert("git.author_timezone".into(),zone);}else if let Some(value)=line.strip_prefix("committer "){let (person,time,zone)=identity(value)?;committer=Some(person);metadata.insert("git.committer_timestamp_ms".into(),time.to_string());metadata.insert("git.committer_timezone".into(),zone);}else if let Some(encoding)=line.strip_prefix("encoding "){metadata.insert("git.encoding".into(),encoding.into());}}
        let (author,timestamp_ms)=author.ok_or_else(||Error::Corrupt("Git commit has no author".into()))?;let committer=committer.ok_or_else(||Error::Corrupt("Git commit has no committer".into()))?;
        // Preserve raw signed/extra headers for auditing; Scion IDs are independently computed.
        metadata.insert("git.headers".into(),headers.into());
        Ok(Self{parents,author,committer,timestamp_ms,message:message.into(),metadata})
    }
}
fn identity(value:&str)->Result<(String,i64,String)>{let mut parts=value.rsplitn(3,' ');let zone=parts.next().ok_or_else(||Error::Corrupt("missing Git timezone".into()))?;let seconds=parts.next().ok_or_else(||Error::Corrupt("missing Git timestamp".into()))?.parse::<i64>().map_err(|_|Error::Corrupt("invalid Git timestamp".into()))?;let name=parts.next().ok_or_else(||Error::Corrupt("missing Git identity".into()))?;Ok((name.into(),seconds.checked_mul(1000).ok_or_else(||Error::Limit("Git timestamp overflow".into()))?,zone.into()))}

/// Export a reachable DAG through Git's streaming fast-import protocol. Export
/// preserves ordered parents, modes, bytes and first-parent deltas. Git object IDs
/// can differ because Scion's metadata contract is deliberately independent.
pub fn export(repository:&Repository,revision:ObjectId,destination:impl AsRef<Path>,reference:&str)->Result<usize>{
    if !reference.starts_with("refs/heads/")||reference.contains(['\n','\r',' '])||reference.contains(".."){return Err(Error::Invalid("export ref must be a safe refs/heads/... name".into()));}
    let destination=destination.as_ref();std::fs::create_dir_all(destination)?;
    if !destination.join("HEAD").exists()&&!destination.join(".git").exists(){let output=Command::new("git").args(["init","--bare"]).arg(destination).output()?;if !output.status.success(){return Err(Error::Storage("git init failed".into()));}}
    let mut ordered=Vec::new();let mut visited=BTreeSet::new();let mut pending=vec![(revision,false)];
    while let Some((id,expanded))=pending.pop(){if expanded{ordered.push(id);continue;}if !visited.insert(id){continue;}let commit=repository.get_changeset(id)?;pending.push((id,true));for parent in commit.parents.iter().rev(){pending.push((*parent,false));}}
    let mut process=Command::new("git").arg("-C").arg(destination).args(["fast-import","--quiet"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    let mut input=process.stdin.take().ok_or_else(||Error::Storage("fast-import lacks stdin".into()))?;
    let mut marks=BTreeMap::new();
    for (index,id) in ordered.iter().enumerate(){
        let commit=repository.get_changeset(*id)?;let mark=index+1;
        if commit.parents.is_empty() { writeln!(input,"reset {reference}\n")?; }
        let author=export_identity(&commit.author)?;let committer=export_identity(commit.committer.as_deref().unwrap_or(&commit.author))?;
        writeln!(input,"commit {reference}\nmark :{mark}\nauthor {author} {} +0000\ncommitter {committer} {} +0000",commit.timestamp_ms/1000,commit.timestamp_ms/1000)?;
        write_data(&mut input,commit.message.as_bytes())?;
        for (i,parent) in commit.parents.iter().enumerate(){let mark=marks.get(parent).ok_or_else(||Error::Corrupt("export order violated parent dependency".into()))?;writeln!(input,"{} :{mark}",if i==0{"from"}else{"merge"})?;}
        if commit.parents.is_empty(){writeln!(input,"deleteall")?;}
        for (path,change) in &commit.changes {if matches!(change,FileChange::Delete){writeln!(input,"D {}",quote_path(path.as_str()))?;}}
        for (path,change) in &commit.changes {if let FileChange::Put{entry,..}=change{let mode=match entry.kind{FileKind::Regular=>"100644",FileKind::Executable=>"100755",FileKind::Symlink=>"120000",FileKind::Submodule=>"160000"};let bytes=repository.store().get_object(scion_types::ObjectKind::Blob,entry.id)?;if entry.kind==FileKind::Submodule{let oid=std::str::from_utf8(&bytes).map_err(|_|Error::Corrupt("invalid submodule OID".into()))?;validate_oid(oid)?;writeln!(input,"M {mode} {oid} {}",quote_path(path.as_str()))?;}else{writeln!(input,"M {mode} inline {}",quote_path(path.as_str()))?;write_data(&mut input,&bytes)?;}}}
        writeln!(input)?;marks.insert(*id,mark);
    }
    // A DFS root list ends at the requested tip, but explicitly reset for clarity.
    writeln!(input,"reset {reference}\nfrom :{}\n",marks[&revision])?;drop(input);
    let output=process.wait_with_output()?;if !output.status.success(){return Err(Error::Storage(format!("git fast-import failed: {}",String::from_utf8_lossy(&output.stderr))));}Ok(ordered.len())
}
fn quote_path(path:&str)->String{let mut quoted=String::from("\"");for byte in path.bytes(){match byte{b'\\'=>quoted.push_str("\\\\"),b'"'=>quoted.push_str("\\\""),0..=31|127..=255=>quoted.push_str(&format!("\\{byte:03o}")),_=>quoted.push(byte as char)}}quoted.push('"');quoted}
fn export_identity(identity:&str)->Result<String>{if identity.contains(['\n','\r','\0']){return Err(Error::Invalid("author/committer cannot contain newlines when exporting to Git".into()));}Ok(if identity.contains('<')&&identity.ends_with('>'){identity.into()}else{format!("{identity} <scion@localhost>")})}
fn write_data(mut output:impl Write,bytes:&[u8])->Result<()>{writeln!(output,"data {}",bytes.len())?;output.write_all(bytes)?;writeln!(output)?;Ok(())}

#[cfg(test)]
mod tests{
    use super::*;use scion_store::MemoryStore;use std::sync::Arc;
    fn command(path:&Path,args:&[&str]){let out=Command::new("git").arg("-C").arg(path).args(args).output().unwrap();assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));}
    #[test] fn imports_binary_newline_names_merge_and_restart(){
        if Command::new("git").arg("--version").output().is_err(){return;}
        let temp=tempfile::tempdir().unwrap();let path=temp.path();command(path,&["init","-b","main"]);command(path,&["config","user.name","Test"]);command(path,&["config","user.email","test@example.invalid"]);
        let filename=if cfg!(windows){"a-file"}else{"a\nfile"};
        std::fs::write(path.join(filename),[0u8,255,1]).unwrap();command(path,&["add","."]);command(path,&["commit","-m","root"]);command(path,&["checkout","-b","side"]);std::fs::write(path.join("side"),b"side").unwrap();command(path,&["add","."]);command(path,&["commit","-m","side"]);command(path,&["checkout","main"]);std::fs::write(path.join("main"),b"main").unwrap();command(path,&["add","."]);command(path,&["commit","-m","main"]);command(path,&["merge","side","-m","merge"]);
        let repo=Repository::new("test",Arc::new(MemoryStore::default())).unwrap();let report=import(&repo,path,&ImportOptions::default()).unwrap();assert_eq!(report.imported,4);let head=repo.resolve_ref("main").unwrap().unwrap();assert_eq!(repo.get_changeset(head).unwrap().parents.len(),2);assert_eq!(repo.read_file(head,&RepoPath::new(filename).unwrap()).unwrap(),[0u8,255,1]);assert_eq!(import(&repo,path,&ImportOptions::default()).unwrap().skipped,4);
        let exported=tempfile::tempdir().unwrap();assert_eq!(export(&repo,head,exported.path(),"refs/heads/main").unwrap(),4);let data=git(exported.path(),&["show",&format!("main:{filename}")]).unwrap();assert_eq!(data,[0u8,255,1]);
    }
}
