use std::{fs::{self, File}, io::Write, path::{Path, PathBuf}};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};
use crate::{PayloadError, Sha256Digest};

pub struct PackRequest<'a> { pub prepared_directory: &'a Path, pub recipe_path: &'a Path, pub archive_path: &'a Path, pub catalog_entry_path: &'a Path }
pub struct BuiltArtifact { archive_size: u64, expanded_size: u64, file_count: u64, archive_sha256: Sha256Digest, payload_manifest_sha256: Sha256Digest }
impl BuiltArtifact { pub fn archive_size(&self)->u64{self.archive_size} pub fn expanded_size(&self)->u64{self.expanded_size} pub fn file_count(&self)->u64{self.file_count} pub fn archive_sha256(&self)->&Sha256Digest{&self.archive_sha256} pub fn payload_manifest_sha256(&self)->&Sha256Digest{&self.payload_manifest_sha256} }

pub fn pack(request: PackRequest<'_>) -> Result<BuiltArtifact, PayloadError> {
    let recipe = fs::read(request.recipe_path).map_err(|e| PayloadError::io("read GPU payload recipe", request.recipe_path.into(), e))?;
    let value: serde_json::Value = serde_json::from_slice(&recipe).map_err(|e| PayloadError::InvalidCatalog(e.to_string()))?;
    let files = collect_files(request.prepared_directory)?;
    let mut manifest_files = Vec::new(); let mut expanded = 0;
    for (relative, path) in &files { let bytes = fs::read(path).map_err(|e| PayloadError::io("read prepared file", path.clone(), e))?; expanded += bytes.len() as u64; manifest_files.push(serde_json::json!({"path":relative,"size":bytes.len(),"sha256":Sha256Digest::hash_reader(bytes.as_slice())?})); }
    let manifest = serde_json::json!({"schema_version":1,"payload_id":value["payload_id"],"target":value["target"],"files":manifest_files});
    let mut manifest_bytes = serde_json::to_vec(&manifest).map_err(|e| PayloadError::InvalidManifest(e.to_string()))?; manifest_bytes.push(b'\n');
    let payload_manifest_sha256 = Sha256Digest::hash_reader(manifest_bytes.as_slice())?;
    let output = File::create(request.archive_path).map_err(|e| PayloadError::io("create payload archive", request.archive_path.into(), e))?;
    let mut writer = ZipWriter::new(output); let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated).compression_level(Some(6)).unix_permissions(0o644);
    writer.start_file("payload.json", options).map_err(|e| PayloadError::Archive(e.to_string()))?; writer.write_all(&manifest_bytes).map_err(|e| PayloadError::Archive(e.to_string()))?;
    for (relative,path) in &files { writer.start_file(relative,options).map_err(|e|PayloadError::Archive(e.to_string()))?; let mut input=File::open(path).map_err(|e|PayloadError::io("open prepared file",path.clone(),e))?; std::io::copy(&mut input,&mut writer).map_err(|e|PayloadError::Archive(e.to_string()))?; }
    writer.finish().map_err(|e|PayloadError::Archive(e.to_string()))?.sync_all().map_err(|e|PayloadError::io("flush payload archive",request.archive_path.into(),e))?;
    let archive_size=fs::metadata(request.archive_path).map_err(|e|PayloadError::io("measure payload archive",request.archive_path.into(),e))?.len(); let archive_sha256=Sha256Digest::hash_reader(File::open(request.archive_path).map_err(|e|PayloadError::io("read payload archive",request.archive_path.into(),e))?)?;
    let mut entry=value; entry["archive_size"]=serde_json::json!(archive_size); entry["expanded_size_limit"]=serde_json::json!(expanded); entry["file_count_limit"]=serde_json::json!(files.len()); entry["archive_sha256"]=serde_json::json!(archive_sha256); entry["payload_manifest_sha256"]=serde_json::json!(payload_manifest_sha256); fs::write(request.catalog_entry_path,serde_json::to_vec_pretty(&entry).map_err(|e|PayloadError::InvalidCatalog(e.to_string()))?).map_err(|e|PayloadError::io("write catalog entry",request.catalog_entry_path.into(),e))?;
    Ok(BuiltArtifact{archive_size,expanded_size:expanded,file_count:files.len()as u64,archive_sha256,payload_manifest_sha256})
}
fn collect_files(root:&Path)->Result<Vec<(String,PathBuf)>,PayloadError>{fn walk(root:&Path,current:&Path,out:&mut Vec<(String,PathBuf)>)->Result<(),PayloadError>{for item in fs::read_dir(current).map_err(|e|PayloadError::io("read prepared directory",current.into(),e))?{let item=item.map_err(|e|PayloadError::io("read prepared directory entry",current.into(),e))?;let path=item.path();let metadata=fs::symlink_metadata(&path).map_err(|e|PayloadError::io("read prepared metadata",path.clone(),e))?;if metadata.file_type().is_symlink(){return Err(PayloadError::UnsafeArchive(path.display().to_string()))}if metadata.is_dir(){walk(root,&path,out)?}else if metadata.is_file(){let relative=path.strip_prefix(root).expect("walk stays below root").to_string_lossy().replace('\\',"/");if relative=="payload.json"{return Err(PayloadError::InvalidManifest("prepared directory must not contain payload.json".into()))}out.push((relative,path));}else{return Err(PayloadError::UnsafeArchive(path.display().to_string()))}}Ok(())}let mut files=Vec::new();walk(root,root,&mut files)?;files.sort_by(|a,b|a.0.cmp(&b.0));Ok(files)}
