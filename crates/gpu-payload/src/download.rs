use std::{fs::{self, File, TryLockError}, io::{Read, Seek, SeekFrom, Write}, path::{Path, PathBuf}, sync::atomic::{AtomicBool, Ordering}};
use crate::{CatalogEntry, PayloadError, Sha256Digest};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadProgress {
    Connecting,
    Downloading { downloaded: u64, total: u64 },
    Verifying { hashed: u64, total: u64 },
    Extracting { files: u64, total: u64 },
    Staging { files: u64, total: u64 },
    Ready,
}

pub(crate) struct LockedArchive { file: File, path: PathBuf, entry: CatalogEntry }
impl LockedArchive {
 pub(crate) fn acquire(cache_root:&Path,entry:&CatalogEntry)->Result<Self,PayloadError>{fs::create_dir_all(cache_root).map_err(|e|PayloadError::io("create GPU payload cache",cache_root.into(),e))?;let path=cache_root.join(format!("{}.zip.part",entry.archive_sha256()));let file=File::options().read(true).write(true).create(true).open(&path).map_err(|e|PayloadError::io("open partial archive",path.clone(),e))?;match file.try_lock(){Ok(())=>Ok(Self{file,path,entry:entry.clone()}),Err(TryLockError::WouldBlock)=>Err(PayloadError::AlreadyInProgress{path}),Err(TryLockError::Error(e))=>Err(PayloadError::io("lock partial archive",path,e))}}
 pub(crate) fn download(&mut self,progress:&dyn Fn(PayloadProgress),cancel:&AtomicBool)->Result<(),PayloadError>{self.file.set_len(0).map_err(|e|PayloadError::io("truncate partial archive",self.path.clone(),e))?;self.file.seek(SeekFrom::Start(0)).map_err(|e|PayloadError::io("rewind partial archive",self.path.clone(),e))?;progress(PayloadProgress::Connecting);let body=ureq::get(self.entry.archive_url()).call().map_err(|e|PayloadError::Http(format!("could not download payload {}: {e}",self.entry.payload_id())))?.into_body();let mut body=body.into_reader();let mut buffer=[0;64*1024];let mut downloaded=0;while downloaded<self.entry.archive_size(){if cancel.load(Ordering::Relaxed){return Err(PayloadError::Cancelled)}let remaining=(self.entry.archive_size()-downloaded).min(64*1024)as usize;let count=body.read(&mut buffer[..remaining]).map_err(|e|PayloadError::Http(format!("could not read payload {}: {e}",self.entry.payload_id())))?;if count==0{break}self.file.write_all(&buffer[..count]).map_err(|e|PayloadError::io("write partial archive",self.path.clone(),e))?;downloaded+=count as u64;progress(PayloadProgress::Downloading{downloaded,total:self.entry.archive_size()});}let mut extra=[0;1];if downloaded==self.entry.archive_size(){if body.read(&mut extra).map_err(|e|PayloadError::Http(e.to_string()))?>0{downloaded+=1}}if downloaded!=self.entry.archive_size(){return Err(PayloadError::ArchiveSizeMismatch{expected:self.entry.archive_size(),actual:downloaded})}self.file.sync_all().map_err(|e|PayloadError::io("flush partial archive",self.path.clone(),e))?;Ok(())}
 pub(crate) fn verify(&mut self,progress:&dyn Fn(PayloadProgress),cancel:&AtomicBool)->Result<(),PayloadError>{if cancel.load(Ordering::Relaxed){return Err(PayloadError::Cancelled)}self.file.seek(SeekFrom::Start(0)).map_err(|e|PayloadError::io("rewind partial archive",self.path.clone(),e))?;progress(PayloadProgress::Verifying{hashed:0,total:self.entry.archive_size()});let actual=Sha256Digest::hash_reader(&mut self.file)?;if actual!=*self.entry.archive_sha256(){self.file.set_len(0).map_err(|e|PayloadError::io("truncate mismatched partial archive",self.path.clone(),e))?;return Err(PayloadError::DigestMismatch{subject:format!("payload {} archive",self.entry.payload_id()),expected:self.entry.archive_sha256().clone(),actual})}Ok(())}
 pub(crate) fn path(&self)->&Path{&self.path}
}
