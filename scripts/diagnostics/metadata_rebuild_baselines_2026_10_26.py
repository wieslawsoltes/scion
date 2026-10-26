from pathlib import Path
import hashlib,json,os,subprocess,time,datetime
OUT=Path('/tmp/scion-final-measurements')
PUBLIC=Path('/tmp/scion-metadata-compression-work/docs/benchmarks')
env=dict(os.environ,PATH='/tmp/ferroui-macos-cargo/bin:'+os.environ['PATH'],RUSTUP_HOME='/tmp/ferroui-macos-rustup',CARGO_HOME='/tmp/ferroui-macos-cargo',CARGO_INCREMENTAL='0')
keys=('PATH','RUSTUP_HOME','CARGO_HOME','CARGO_TARGET_DIR','CARGO_INCREMENTAL','RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','CARGO_BUILD_TARGET','CARGO_BUILD_RUSTFLAGS','RUSTC','RUSTC_WRAPPER','RUSTC_WORKSPACE_WRAPPER','CC','CXX','CFLAGS','CXXFLAGS','LDFLAGS','CARGO_BUILD_JOBS')
def sha(data): return hashlib.sha256(data).hexdigest()
def output(command,root,env=env): return subprocess.check_output(command,cwd=root,env=env).decode().strip()
def build(root,target,name,command,executable,expected,kind):
 root=Path(root); report=OUT/(name+'.json'); log=OUT/(name+'.log')
 if report.exists() or log.exists(): raise RuntimeError('Refusing to overwrite build evidence')
 benv=dict(env,CARGO_TARGET_DIR=target)
 record={'recorded_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'source_directory':str(root),'source_commit':output(['git','rev-parse','HEAD'],root),'source_tree':output(['git','rev-parse','HEAD^{tree}'],root),'source_archive_sha256':sha(subprocess.check_output(['git','archive','HEAD'],cwd=root)), 'source_status_before':output(['git','status','--porcelain=v1','--untracked-files=all'],root), 'rustc':output(['rustc','-vV'],root), 'cargo':output(['cargo','-vV'],root),'environment':{key:benv.get(key) for key in keys}, 'command':command,'build_log':str(log),'timings_performed':False,'expected_executable_sha256':expected,'host_compiler':output(['cc','--version'],root),'host_linker':output(['ld','--version'],root)}
 assert record['source_status_before']=='', record['source_status_before']
 if kind=='durable':
  historical=PUBLIC/'git-comparison-2026-10-25-git-2.56-full.json'
  previous=json.loads(historical.read_text())
  recorded=previous['source']['source_sha256']
  actual={path:sha((root/path).read_bytes()) for path in recorded}
  record.update(historical_report=str(historical),historical_report_sha256=sha(historical.read_bytes()),source_files_match_historical_report=actual==recorded,source_sha256=actual,driver_source_sha256=sha((root/'crates/scion-engine/examples/comparison_driver.rs').read_bytes()))
  assert actual==recorded
 else:
  record['benchmark_source_sha256']=sha((root/'crates/scion-engine/benches/scaling.rs').read_bytes())
 report.write_text(json.dumps(record,indent=2)+'\n')
 print('Starting '+name,flush=True)
 start=time.monotonic()
 with log.open('xb') as stream: result=subprocess.run(command,cwd=root,env=benv,stdout=stream,stderr=subprocess.STDOUT)
 record.update(build_exit_code=result.returncode,build_elapsed_seconds=time.monotonic()-start,source_status_after=output(['git','status','--porcelain=v1','--untracked-files=all'],root))
 product=Path(executable)
 if result.returncode==0:
  if not product.exists():
   if kind=='scaling':
    found=[p for p in (Path(target)/'release/deps').glob('scaling-*') if p.is_file() and os.access(p,os.X_OK) and p.suffix!='.d']
    assert len(found)==1,found
    product=found[0]
   else: raise RuntimeError('Missing built executable')
  record.update(executable=str(product),executable_sha256=sha(product.read_bytes()),executable_bytes=product.stat().st_size)
  record['matches_historical_executable']=record['executable_sha256']==expected
 report.write_text(json.dumps(record,indent=2)+'\n')
 print(json.dumps({key:record.get(key) for key in ['source_commit','build_exit_code','build_elapsed_seconds','executable','executable_sha256','matches_historical_executable']}),flush=True)
 assert result.returncode==0
 assert record['source_status_after']==''
 if kind=='durable': assert record['matches_historical_executable'], 'Durable baseline hash mismatch'
 return record
build('/tmp/scion-durable-baseline','/tmp/scion-durable-baseline-target','durable-baseline-recovery',['cargo','build','--release','--locked','-p','scion-engine','--example','comparison_driver','--offline'],'/tmp/scion-durable-baseline-target/release/examples/comparison_driver','ff6a44c736b135d4ec920f4d497a92fa3cac4a2936ff284a6c4abeb1ef6aa6c6','durable')
build('/tmp/scion-scaling-baseline','/tmp/scion-scaling-baseline-target','scaling-baseline-build',['cargo','bench','--locked','--offline','-p','scion-engine','--bench','scaling','--no-run'],'/tmp/scion-scaling-baseline-target/release/deps/scaling-6a816402baef2a08','5d57e2666f61a1ec99863317055ba02b4e5dd8abf12d301ae7ab5eb0a888b831','scaling')
