use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use url::Url;
use crate::{PayloadError, Sha256Digest};
const CATALOG_SCHEMA_VERSION: u32 = 1;
const PAYLOAD_ABI_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct GuestTarget { pub distribution: String, pub release: String, pub architecture: String, pub kernel_release: String, pub payload_abi: u32 }
impl GuestTarget { pub fn ubuntu_26_04_amd64(kernel_release: impl Into<String>) -> Self { Self { distribution: "ubuntu".into(), release: "26.04".into(), architecture: "amd64".into(), kernel_release: kernel_release.into(), payload_abi: 1 } } }
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RendererCapability { D3d12Gallium, DznVulkan }
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MesaPolicy { Distro, Bundled }
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Source { pub url: String, pub commit: String, pub version: String }
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct License { pub spdx: String, pub path: String }
#[derive(Clone, Debug, Deserialize)]
struct CatalogDocument { schema_version: u32, entries: Vec<CatalogEntry> }
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CatalogEntry { payload_id: String, target: GuestTarget, archive_url: String, archive_size: u64, expanded_size_limit: u64, file_count_limit: u64, archive_sha256: Sha256Digest, payload_manifest_sha256: Sha256Digest, required_renderers: Vec<RendererCapability>, mesa_policy: MesaPolicy, sources: Vec<Source>, licenses: Vec<License> }
impl CatalogEntry {
 fn validate(&self) -> Result<(), PayloadError> {
  if self.payload_id.is_empty() || self.target.payload_abi != PAYLOAD_ABI_VERSION || self.archive_size == 0 || self.expanded_size_limit < self.archive_size || self.file_count_limit == 0 || self.required_renderers.is_empty() { return Err(PayloadError::InvalidCatalog("missing or invalid required catalog field".into())); }
  validate_url(&self.archive_url)?;
  if self.sources.iter().any(|source| source.url.is_empty() || source.version.is_empty() || source.commit.len() != 40 || !source.commit.bytes().all(|byte| byte.is_ascii_hexdigit())) { return Err(PayloadError::InvalidCatalog("sources must carry immutable commits and provenance".into())); }
  if self.licenses.iter().any(|license| license.spdx.is_empty() || license.path.is_empty()) || self.licenses.is_empty() { return Err(PayloadError::InvalidCatalog("licenses must carry SPDX expressions and paths".into())); }
  Ok(())
 }
 pub fn payload_id(&self) -> &str { &self.payload_id } pub fn target(&self) -> &GuestTarget { &self.target } pub fn archive_url(&self) -> &str { &self.archive_url } pub fn archive_size(&self) -> u64 { self.archive_size } pub fn expanded_size_limit(&self) -> u64 { self.expanded_size_limit } pub fn file_count_limit(&self) -> u64 { self.file_count_limit } pub fn archive_sha256(&self) -> &Sha256Digest { &self.archive_sha256 } pub fn payload_manifest_sha256(&self) -> &Sha256Digest { &self.payload_manifest_sha256 } pub fn required_renderers(&self) -> &[RendererCapability] { &self.required_renderers } pub fn mesa_policy(&self) -> &MesaPolicy { &self.mesa_policy } pub fn sources(&self) -> &[Source] { &self.sources } pub fn licenses(&self) -> &[License] { &self.licenses }
}
pub struct PayloadCatalog { entries: Vec<CatalogEntry> }
impl PayloadCatalog { pub fn from_json(bytes: &[u8]) -> Result<Self, PayloadError> { let doc: CatalogDocument = serde_json::from_slice(bytes).map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?; if doc.schema_version != CATALOG_SCHEMA_VERSION { return Err(PayloadError::InvalidCatalog("unknown catalog schema version".into())); } let mut ids = HashSet::new(); let mut targets = HashSet::new(); for entry in &doc.entries { entry.validate()?; if !ids.insert(entry.payload_id.clone()) || !targets.insert(entry.target.clone()) { return Err(PayloadError::InvalidCatalog("duplicate payload ID or target".into())); } } Ok(Self { entries: doc.entries }) } pub fn embedded() -> Result<Self, PayloadError> { Self::from_json(include_bytes!("../catalog/catalog.json")) } pub fn entries(&self) -> &[CatalogEntry] { &self.entries } pub fn select(&self, target: &GuestTarget) -> Result<&CatalogEntry, PayloadError> { self.entries.iter().find(|entry| entry.target == *target).ok_or_else(|| PayloadError::UnsupportedTarget(target.clone())) } }
fn validate_url(value: &str) -> Result<Url, PayloadError> { let url = Url::parse(value).map_err(|error| PayloadError::InvalidCatalog(format!("invalid archive URL: {error}")))?; if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() { return Err(PayloadError::InvalidCatalog("archive URL must be immutable HTTPS without credentials, query, or fragment".into())); } Ok(url) }

#[cfg(test)]
mod tests { use crate::{GuestTarget, PayloadCatalog, PayloadError}; const Z: &str = "0000000000000000000000000000000000000000000000000000000000000000"; const C: &str = "14794180686c2fb6307fbe359c359bec765249f3"; fn catalog() -> String { format!(r#"{{"schema_version":1,"entries":[{{"payload_id":"ubuntu-26.04-amd64-7.0.0-14-v1","target":{{"distribution":"ubuntu","release":"26.04","architecture":"amd64","kernel_release":"7.0.0-14-generic","payload_abi":1}},"archive_url":"https://downloads.example.test/payload.zip","archive_size":1,"expanded_size_limit":2,"file_count_limit":3,"archive_sha256":"{Z}","payload_manifest_sha256":"{Z}","required_renderers":["d3d12-gallium","dzn-vulkan"],"mesa_policy":"bundled","sources":[{{"url":"https://github.com/microsoft/WSL2-Linux-Kernel","commit":"{C}","version":"1"}}],"licenses":[{{"spdx":"GPL-2.0","path":"licenses/GPL-2.0.txt"}}]}}]}}"#) } #[test] fn a_catalog_selects_only_the_exact_kernel_tuple(){let c=PayloadCatalog::from_json(catalog().as_bytes()).unwrap();assert_eq!(c.select(&GuestTarget::ubuntu_26_04_amd64("7.0.0-14-generic")).unwrap().payload_id(),"ubuntu-26.04-amd64-7.0.0-14-v1");assert!(matches!(c.select(&GuestTarget::ubuntu_26_04_amd64("other")),Err(PayloadError::UnsupportedTarget(_))));} #[test] fn production_urls_cannot_carry_mutable_or_secret_structure(){for url in ["http://downloads.example.test/payload.zip","https://user:secret@downloads.example.test/payload.zip","https://downloads.example.test/payload.zip?latest=1","https://downloads.example.test/payload.zip#latest"]{assert!(matches!(PayloadCatalog::from_json(catalog().replace("https://downloads.example.test/payload.zip",url).as_bytes()),Err(PayloadError::InvalidCatalog(_))));}} #[test] fn an_empty_embedded_catalog_is_valid_until_a_tested_recipe_is_published(){assert!(PayloadCatalog::embedded().unwrap().entries().is_empty());} }
