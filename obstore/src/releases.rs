//! Import the preview and stable firmware releases into the local store.
//!
//! Each board becomes two objects. The USB object is one zip of `target.json` and
//! the files that record names, which is the directory `hopspot-flash` flashes.
//! The OTA object is the single file that is sent to the board. The Local Object
//! Authority signs both.

use std::fmt;
use std::io::Cursor;

use prns_flash_manifest::{
    board_catalog, pinned_key_is_configured, sha256_hex, verify_minisign, FlashPartKind,
    ReleaseChannel, ValidatedFlashManifest, PINNED_MINISIGN_PUBLIC_KEY,
};
use url::Url;

use crate::store::{claims_from_loa_envelope, Claims, ObjectId, ObjectStore};

const CHANNEL_BASE_URL: &str = "https://reticulum.rs/releases/channels/";
const MAX_CHANNEL_BYTES: u64 = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 512 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 64 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;

/// One of the two published firmware channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirmwareSet {
    /// The preview channel.
    Preview,
    /// The stable channel.
    Stable,
}

impl FirmwareSet {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Stable => "stable",
        }
    }

    const fn wire_channel(self) -> ReleaseChannel {
        match self {
            Self::Preview => ReleaseChannel::Preview,
            Self::Stable => ReleaseChannel::Stable,
        }
    }
}

impl fmt::Display for FirmwareSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Firmware objects written by one import, plus channels the host does not publish.
#[derive(Debug, Default)]
pub struct FirmwareImport {
    pub imported: Vec<ImportedFirmware>,
    pub unpublished: Vec<String>,
}

/// One firmware artifact imported from a signed release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedFirmware {
    pub object_id: String,
    pub set: FirmwareSet,
    pub board: String,
    pub version: String,
    pub artifact: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BoardIdentity {
    set: FirmwareSet,
    board: String,
    version: String,
    commit: String,
}

impl BoardIdentity {
    fn imported(&self, object_id: &str, artifact: &str) -> ImportedFirmware {
        ImportedFirmware {
            object_id: object_id.to_string(),
            set: self.set,
            board: self.board.clone(),
            version: self.version.clone(),
            artifact: artifact.to_string(),
        }
    }
}

struct DownloadedPart {
    kind: String,
    path: String,
    offset: Option<u32>,
    bytes: Vec<u8>,
    ota: bool,
}

#[derive(Debug)]
pub struct ReleaseError {
    message: String,
    kind: ReleaseFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReleaseFailure {
    Other,
    NotFound,
    Unpublished,
}

impl ReleaseError {
    fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ReleaseFailure::Other,
        }
    }

    fn not_found(url: &str) -> Self {
        Self {
            message: format!("download failed for {url}: http status: 404"),
            kind: ReleaseFailure::NotFound,
        }
    }

    fn unpublished(set: FirmwareSet, url: &str) -> Self {
        Self {
            message: format!("{set} channel is not published at {url}"),
            kind: ReleaseFailure::Unpublished,
        }
    }

    fn is_not_found(&self) -> bool {
        self.kind == ReleaseFailure::NotFound
    }

    fn is_unpublished(&self) -> bool {
        self.kind == ReleaseFailure::Unpublished
    }
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReleaseError {}

/// Download both sets, or the sets named here, and sign each artifact into `store`.
///
/// A channel descriptor that the host answers with 404 is recorded as unpublished.
/// The other requested channels are still imported.
pub fn import_sets(
    store: &ObjectStore,
    sets: &[FirmwareSet],
) -> Result<FirmwareImport, ReleaseError> {
    if !pinned_key_is_configured() {
        return Err(ReleaseError::other(
            "release key custody is not configured; release/keys/minisign.pub still contains the fail-closed marker"
                .to_string(),
        ));
    }
    let mut outcome = FirmwareImport::default();
    for set in sets {
        fold_set(&mut outcome, import_set(store, *set))?;
    }
    finish_import(outcome)
}

fn import_set(
    store: &ObjectStore,
    set: FirmwareSet,
) -> Result<Vec<ImportedFirmware>, ReleaseError> {
    let channel_url = channel_url(set)?;
    let channel_bytes = match download(&channel_url, MAX_CHANNEL_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.is_not_found() => {
            return Err(ReleaseError::unpublished(set, &channel_url));
        }
        Err(error) => return Err(error),
    };
    let channel_signature = download_signature(&channel_url)?;
    verify_minisign(
        &channel_bytes,
        &channel_signature,
        PINNED_MINISIGN_PUBLIC_KEY,
    )
    .map_err(|error| {
        ReleaseError::other(format!("{set} channel signature was rejected: {error}"))
    })?;
    let descriptor = prns_flash_manifest::ValidatedChannelDescriptor::from_json(
        &channel_bytes,
        set.wire_channel(),
    )
    .map_err(|error| {
        ReleaseError::other(format!("{set} channel descriptor is invalid: {error}"))
    })?;
    let manifest_url = descriptor.manifest_url();
    enforce_https(manifest_url)?;
    let manifest_bytes = download(manifest_url, MAX_MANIFEST_BYTES)?;
    if sha256_hex(&manifest_bytes) != descriptor.manifest_sha256().as_str() {
        return Err(ReleaseError::other(format!(
            "{set} manifest does not match the signed channel digest"
        )));
    }
    let manifest_signature = download_signature(manifest_url)?;
    verify_minisign(
        &manifest_bytes,
        &manifest_signature,
        PINNED_MINISIGN_PUBLIC_KEY,
    )
    .map_err(|error| {
        ReleaseError::other(format!("{set} manifest signature was rejected: {error}"))
    })?;
    let catalog = board_catalog().map_err(|error| ReleaseError::other(error.to_string()))?;
    let manifest = ValidatedFlashManifest::from_json(&manifest_bytes, &catalog)
        .map_err(|error| ReleaseError::other(format!("{set} manifest is invalid: {error}")))?;
    if manifest.release().channel() != set.wire_channel() {
        return Err(ReleaseError::other(format!(
            "{set} manifest is signed for a different channel"
        )));
    }
    if manifest.release().version().as_str() != descriptor.version().as_str() {
        return Err(ReleaseError::other(format!(
            "{set} manifest version does not match the signed channel"
        )));
    }
    let version = manifest.release().version().as_str();
    let commit = manifest.release().commit();
    let mut imported = Vec::new();
    for target in manifest.targets() {
        imported.extend(import_board(
            store,
            set,
            manifest_url,
            version,
            commit,
            target,
        )?);
    }
    Ok(imported)
}

fn import_board(
    store: &ObjectStore,
    set: FirmwareSet,
    manifest_url: &str,
    version: &str,
    commit: &str,
    target: &prns_flash_manifest::ReleaseTarget,
) -> Result<Vec<ImportedFirmware>, ReleaseError> {
    let mut parts = Vec::new();
    for part in target.parts() {
        if part.size() == 0 || part.size() > MAX_ARTIFACT_BYTES {
            return Err(ReleaseError::other(format!(
                "{set} {} {} is outside 1..={MAX_ARTIFACT_BYTES} bytes",
                target.board_id().as_str(),
                part_kind(part.kind())
            )));
        }
        let url = artifact_url(manifest_url, part.path().as_str())?;
        let bytes = download(&url, download_limit(part.size())?)?;
        accept_bytes(&bytes, part.size(), part.sha256().as_str()).map_err(|error| {
            ReleaseError::other(format!(
                "{set} {} {} {}: {error}",
                target.board_id().as_str(),
                version,
                part_kind(part.kind())
            ))
        })?;
        parts.push(DownloadedPart {
            kind: part_kind(part.kind()).to_string(),
            path: part.path().as_str().to_string(),
            offset: part.offset(),
            bytes,
            ota: false,
        });
    }
    if parts.is_empty() {
        return Err(ReleaseError::other(format!(
            "{set} {} has no firmware files",
            target.board_id().as_str()
        )));
    }
    mark_ota_members(&mut parts);
    let record = target_record(target)?;
    let zip = usb_archive(&record, &parts)?;
    let identity = BoardIdentity {
        set,
        board: target.board_id().as_str().to_string(),
        version: version.to_string(),
        commit: commit.to_string(),
    };
    let mut imported = Vec::new();
    let usb_id = import_bytes(store, &zip, &usb_claim_lines(&identity))?;
    imported.push(identity.imported(usb_id.as_str(), "usb"));
    for part in parts.iter().filter(|part| part.ota) {
        let id = import_bytes(store, &part.bytes, &ota_claim_lines(&identity, part))?;
        imported.push(identity.imported(id.as_str(), &part.kind));
    }
    Ok(imported)
}

fn channel_url(set: FirmwareSet) -> Result<String, ReleaseError> {
    let base = std::env::var("PRNS_FLASH_CHANNEL_BASE_URL")
        .unwrap_or_else(|_| CHANNEL_BASE_URL.to_string());
    let url = format!(
        "{}{}.json",
        base.trim_end_matches('/').to_string() + "/",
        set.as_str()
    );
    enforce_https(&url)?;
    Ok(url)
}

fn part_kind(kind: FlashPartKind) -> &'static str {
    match kind {
        FlashPartKind::Bootloader => "bootloader",
        FlashPartKind::PartitionTable => "partition-table",
        FlashPartKind::Application => "application",
        FlashPartKind::Uf2 => "uf2",
        FlashPartKind::DfuApplication => "dfu-application",
        FlashPartKind::DfuInitPacket => "dfu-init-packet",
    }
}

fn mark_ota_members(parts: &mut [DownloadedPart]) {
    let has_application = parts
        .iter()
        .any(|part| part.kind == "application" || part.kind == "dfu-application");
    for part in parts {
        part.ota = match part.kind.as_str() {
            "application" | "dfu-application" => true,
            "uf2" => !has_application,
            _ => false,
        };
    }
}

fn target_record(target: &prns_flash_manifest::ReleaseTarget) -> Result<Vec<u8>, ReleaseError> {
    let mut json = serde_json::to_vec_pretty(&target.to_wire()).map_err(|error| {
        ReleaseError::other(format!(
            "could not encode {} target.json: {error}",
            target.board_id().as_str()
        ))
    })?;
    if !json.ends_with(b"\n") {
        json.push(b'\n');
    }
    Ok(json)
}

fn usb_archive(target_json: &[u8], parts: &[DownloadedPart]) -> Result<Vec<u8>, ReleaseError> {
    let mut entries = vec![("target.json".to_string(), target_json.to_vec())];
    for part in parts {
        if entries.iter().any(|(name, _)| name == &part.path) {
            return Err(ReleaseError::other(format!(
                "USB zip path {} is repeated",
                part.path
            )));
        }
        entries.push((part.path.clone(), part.bytes.clone()));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let modified = zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
        .map_err(|error| ReleaseError::other(format!("USB zip timestamp is invalid: {error}")))?;
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(modified)
        .unix_permissions(0o644);
    for (name, bytes) in &entries {
        writer.start_file(name, options).map_err(|error| {
            ReleaseError::other(format!("could not add {name} to the USB zip: {error}"))
        })?;
        std::io::Write::write_all(&mut writer, bytes).map_err(|error| {
            ReleaseError::other(format!("could not write {name} into the USB zip: {error}"))
        })?;
    }
    let cursor = writer
        .finish()
        .map_err(|error| ReleaseError::other(format!("could not finish the USB zip: {error}")))?;
    let bytes = cursor.into_inner();
    if bytes.is_empty() || bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(ReleaseError::other(format!(
            "USB zip is {} bytes, outside 1..={MAX_ARTIFACT_BYTES}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

fn usb_claim_lines(identity: &BoardIdentity) -> Vec<String> {
    vec![
        "object-type=firmware".to_string(),
        format!("channel={}", identity.set.as_str()),
        format!("board={}", identity.board),
        format!("version={}", identity.version),
        "artifact=usb".to_string(),
        format!("commit={}", identity.commit),
        "provenance=reticulum-release".to_string(),
        "flash-mode=usb".to_string(),
    ]
}

fn ota_claim_lines(identity: &BoardIdentity, part: &DownloadedPart) -> Vec<String> {
    let mut lines = vec![
        "object-type=firmware".to_string(),
        format!("channel={}", identity.set.as_str()),
        format!("board={}", identity.board),
        format!("version={}", identity.version),
        format!("artifact={}", part.kind),
        format!("path={}", part.path),
        format!("commit={}", identity.commit),
        "provenance=reticulum-release".to_string(),
    ];
    if let Some(offset) = part.offset {
        lines.push(format!("offset={offset}"));
    }
    lines.push("flash-mode=ota".to_string());
    lines
}

fn import_bytes(
    store: &ObjectStore,
    bytes: &[u8],
    claim_lines: &[String],
) -> Result<ObjectId, ReleaseError> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(ReleaseError::other(format!(
            "object is {} bytes, outside 1..={MAX_ARTIFACT_BYTES}",
            bytes.len()
        )));
    }
    let claims =
        Claims::parse(claim_lines).map_err(|error| ReleaseError::other(error.to_string()))?;
    let id = store
        .import_reader(Cursor::new(bytes), bytes.len() as u64, &claims)
        .map_err(|error| ReleaseError::other(error.to_string()))?;
    if id.as_str() != sha256_hex(bytes) {
        return Err(ReleaseError::other(format!(
            "stored {} but the bytes hash to {}",
            id.as_str(),
            sha256_hex(bytes)
        )));
    }
    let envelope = store
        .read_loa_envelope(&id)
        .map_err(|error| ReleaseError::other(error.to_string()))?
        .ok_or_else(|| ReleaseError::other(format!("{} has no LOA envelope", id.as_str())))?;
    claims_from_loa_envelope(&envelope, id.as_str())
        .map_err(|error| ReleaseError::other(error.to_string()))?;
    Ok(id)
}

fn accept_bytes(bytes: &[u8], size: u64, sha256: &str) -> Result<(), ReleaseError> {
    if size == 0 || size > MAX_ARTIFACT_BYTES {
        return Err(ReleaseError::other(format!(
            "artifact length {size} is outside 1..={MAX_ARTIFACT_BYTES}"
        )));
    }
    if bytes.len() as u64 != size {
        return Err(ReleaseError::other(format!(
            "downloaded {downloaded} bytes, the release declares {size}",
            downloaded = bytes.len()
        )));
    }
    let digest = sha256_hex(bytes);
    if digest != sha256 {
        return Err(ReleaseError::other(format!(
            "downloaded digest {digest} does not match the release digest {sha256}"
        )));
    }
    Ok(())
}

fn artifact_url(manifest_url: &str, path: &str) -> Result<String, ReleaseError> {
    let root = manifest_url
        .strip_suffix("flash-manifest.json")
        .filter(|root| root.ends_with('/'))
        .ok_or_else(|| {
            ReleaseError::other(format!(
                "manifest URL {manifest_url} is not an immutable flash-manifest.json path"
            ))
        })?;
    if path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ReleaseError::other(format!(
            "artifact path {path} is not relative"
        )));
    }
    let joined = format!("{root}{path}");
    let manifest = Url::parse(manifest_url).map_err(|error| {
        ReleaseError::other(format!("invalid manifest URL {manifest_url}: {error}"))
    })?;
    let artifact = Url::parse(&joined)
        .map_err(|error| ReleaseError::other(format!("invalid artifact URL {joined}: {error}")))?;
    if artifact.scheme() != "https" || artifact.host() != manifest.host() {
        return Err(ReleaseError::other(format!(
            "artifact URL {joined} leaves the signed release host"
        )));
    }
    Ok(joined)
}

fn download_signature(url: &str) -> Result<String, ReleaseError> {
    let signature_url = format!("{url}.minisig");
    let bytes = download(&signature_url, MAX_SIGNATURE_BYTES)?;
    String::from_utf8(bytes)
        .map_err(|error| ReleaseError::other(format!("{signature_url} is not UTF-8: {error}")))
}

/// ureq's limit reader rejects a body of exactly `limit` bytes: once those
/// bytes are consumed, the read that should observe EOF finds no budget left.
/// hopspot-flash allows one extra byte, then the declared size and digest are
/// checked on the bytes that were actually read.
fn download_limit(size: u64) -> Result<u64, ReleaseError> {
    size.checked_add(1)
        .ok_or_else(|| ReleaseError::other("artifact size overflows the download limit"))
}

fn download(url: &str, limit: u64) -> Result<Vec<u8>, ReleaseError> {
    enforce_https(url)?;
    let mut response = ureq::get(url)
        .header(
            "User-Agent",
            concat!("object-services/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|error| download_failure(url, error))?;
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|error| ReleaseError::other(format!("could not read {url}: {error}")))
}

fn download_failure(url: &str, error: ureq::Error) -> ReleaseError {
    match error {
        ureq::Error::StatusCode(404) => ReleaseError::not_found(url),
        other => ReleaseError::other(format!("download failed for {url}: {other}")),
    }
}

fn fold_set(
    outcome: &mut FirmwareImport,
    result: Result<Vec<ImportedFirmware>, ReleaseError>,
) -> Result<(), ReleaseError> {
    match result {
        Ok(objects) => outcome.imported.extend(objects),
        Err(error) if error.is_unpublished() => outcome.unpublished.push(error.to_string()),
        Err(error) => return Err(error),
    }
    Ok(())
}

fn finish_import(outcome: FirmwareImport) -> Result<FirmwareImport, ReleaseError> {
    if outcome.imported.is_empty() && !outcome.unpublished.is_empty() {
        return Err(ReleaseError::other(outcome.unpublished.join("; ")));
    }
    Ok(outcome)
}

fn enforce_https(url: &str) -> Result<(), ReleaseError> {
    let parsed = Url::parse(url)
        .map_err(|error| ReleaseError::other(format!("invalid release URL {url}: {error}")))?;
    if parsed.scheme() != "https" {
        return Err(ReleaseError::other(format!(
            "release URL must use HTTPS: {url}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(set: FirmwareSet) -> BoardIdentity {
        BoardIdentity {
            set,
            board: "heltec-v4-r8".to_string(),
            version: "0.3.7".to_string(),
            commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        }
    }

    fn part(kind: &str, bytes: &[u8]) -> DownloadedPart {
        DownloadedPart {
            kind: kind.to_string(),
            path: format!("firmware/hopspot/heltec-v4-r8/0.3.7/{kind}.bin"),
            offset: Some(0x10000),
            bytes: bytes.to_vec(),
            ota: false,
        }
    }

    #[test]
    fn a_preview_application_is_the_ota_file() {
        let board = identity(FirmwareSet::Preview);
        let application = part("application", b"firmware-bytes");
        let claims = Claims::parse(ota_claim_lines(&board, &application)).expect("claims");
        assert_eq!(claims.claim("object-type"), Some("firmware"));
        assert_eq!(claims.claim("channel"), Some("preview"));
        assert_eq!(claims.claim("board"), Some("heltec-v4-r8"));
        assert_eq!(claims.claim("version"), Some("0.3.7"));
        assert_eq!(claims.claim("artifact"), Some("application"));
        assert_eq!(claims.claim("flash-mode"), Some("ota"));
        assert!(claims.claim("mode").is_none());
        assert_eq!(claims.claim("provenance"), Some("reticulum-release"));
        assert_eq!(claims.claim("offset"), Some("65536"));
    }

    #[test]
    fn a_usb_zip_is_tagged_for_the_local_flasher() {
        let claims =
            Claims::parse(usb_claim_lines(&identity(FirmwareSet::Stable))).expect("claims");
        assert_eq!(claims.claim("channel"), Some("stable"));
        assert_eq!(claims.claim("artifact"), Some("usb"));
        assert_eq!(claims.claim("flash-mode"), Some("usb"));
        assert!(claims.claim("mode").is_none());
    }

    #[test]
    fn ota_keeps_the_application_or_a_standalone_uf2() {
        let mut esp = vec![
            part("bootloader", b"boot"),
            part("partition-table", b"table"),
            part("application", b"app"),
        ];
        mark_ota_members(&mut esp);
        assert_eq!(
            esp.iter()
                .filter(|part| part.ota)
                .map(|part| part.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["application"]
        );
        let mut uf2 = vec![part("uf2", b"one"), part("uf2", b"two")];
        mark_ota_members(&mut uf2);
        assert!(uf2.iter().all(|part| part.ota));
        let mut nrf = vec![
            part("dfu-application", b"app"),
            part("dfu-init-packet", b"dat"),
            part("uf2", b"recovery"),
        ];
        mark_ota_members(&mut nrf);
        assert_eq!(
            nrf.iter()
                .filter(|part| part.ota)
                .map(|part| part.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["dfu-application"]
        );
    }

    #[test]
    fn a_usb_zip_contains_target_json_and_the_part_files() {
        let mut parts = vec![
            part("bootloader", b"boot"),
            part("partition-table", b"table"),
            part("application", b"app-bytes"),
        ];
        mark_ota_members(&mut parts);
        let archive = usb_archive(b"{}\n", &parts).expect("zip");
        let again = usb_archive(b"{}\n", &parts).expect("zip");
        assert_eq!(archive, again);
        let mut reader = zip::ZipArchive::new(Cursor::new(archive)).expect("read");
        let mut names = Vec::new();
        for index in 0..reader.len() {
            names.push(reader.by_index(index).expect("entry").name().to_string());
        }
        names.sort();
        assert_eq!(
            names,
            vec![
                "firmware/hopspot/heltec-v4-r8/0.3.7/application.bin".to_string(),
                "firmware/hopspot/heltec-v4-r8/0.3.7/bootloader.bin".to_string(),
                "firmware/hopspot/heltec-v4-r8/0.3.7/partition-table.bin".to_string(),
                "target.json".to_string(),
            ]
        );
        let mut application = reader
            .by_name("firmware/hopspot/heltec-v4-r8/0.3.7/application.bin")
            .expect("application");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut application, &mut bytes).expect("read");
        assert_eq!(bytes, b"app-bytes");
    }

    #[test]
    fn an_artifact_url_stays_on_the_signed_release_host() {
        let url = artifact_url(
            "https://reticulum.rs/releases/0.3.7/flash-manifest.json",
            "firmware/hopspot/heltec-v4-r8/0.3.7/application.bin",
        )
        .expect("url");
        assert_eq!(
            url,
            "https://reticulum.rs/releases/0.3.7/firmware/hopspot/heltec-v4-r8/0.3.7/application.bin"
        );
        assert!(artifact_url(
            "https://reticulum.rs/releases/0.3.7/flash-manifest.json",
            "../other.bin"
        )
        .is_err());
    }

    #[test]
    fn a_missing_channel_does_not_stop_the_other_set() {
        let mut outcome = FirmwareImport::default();
        fold_set(
            &mut outcome,
            Err(ReleaseError::unpublished(
                FirmwareSet::Preview,
                "https://reticulum.rs/releases/channels/preview.json",
            )),
        )
        .expect("skip unpublished preview");
        assert_eq!(
            outcome.unpublished,
            vec![
                "preview channel is not published at https://reticulum.rs/releases/channels/preview.json"
                    .to_string()
            ]
        );
        fold_set(
            &mut outcome,
            Ok(vec![ImportedFirmware {
                object_id: "abc".to_string(),
                set: FirmwareSet::Stable,
                board: "heltec-v4-r8".to_string(),
                version: "0.3.7-hotfix.5".to_string(),
                artifact: "application".to_string(),
            }]),
        )
        .expect("continue");
        let finished = finish_import(outcome).expect("stable still counts");
        assert_eq!(finished.imported.len(), 1);
        assert_eq!(finished.unpublished.len(), 1);
    }

    #[test]
    fn a_later_failure_stays_separate_from_an_unpublished_channel() {
        let mut outcome = FirmwareImport::default();
        fold_set(
            &mut outcome,
            Err(ReleaseError::unpublished(
                FirmwareSet::Preview,
                "https://reticulum.rs/releases/channels/preview.json",
            )),
        )
        .expect("record unpublished preview");
        let error = fold_set(
            &mut outcome,
            Err(ReleaseError::other(
                "could not read https://reticulum.rs/releases/0.3.7-hotfix.5/firmware/hopspot/t1000-e/0.3.7-hotfix.5/t1000e.dat",
            )),
        )
        .expect_err("download failure");
        assert_eq!(
            error.to_string(),
            "could not read https://reticulum.rs/releases/0.3.7-hotfix.5/firmware/hopspot/t1000-e/0.3.7-hotfix.5/t1000e.dat"
        );
    }

    #[test]
    fn the_download_limit_leaves_room_for_end_of_file() {
        assert_eq!(download_limit(14).expect("limit"), 15);
    }

    #[test]
    fn an_unpublished_channel_alone_is_an_error() {
        let mut outcome = FirmwareImport::default();
        fold_set(
            &mut outcome,
            Err(ReleaseError::unpublished(
                FirmwareSet::Preview,
                "https://reticulum.rs/releases/channels/preview.json",
            )),
        )
        .expect("record");
        let error = finish_import(outcome).expect_err("nothing imported");
        assert!(error
            .to_string()
            .contains("preview channel is not published"));
    }

    #[test]
    fn a_404_marks_the_channel_descriptor_missing() {
        let error = download_failure(
            "https://reticulum.rs/releases/channels/preview.json",
            ureq::Error::StatusCode(404),
        );
        assert!(error.is_not_found());
        assert!(!error.is_unpublished());
        assert!(error.to_string().contains("http status: 404"));
    }

    #[test]
    fn a_wrong_digest_is_refused_before_it_is_stored() {
        let error = accept_bytes(b"nope", 4, &"ab".repeat(32)).expect_err("digest");
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn importing_signs_the_ota_file_and_the_usb_zip() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open(root.path()).expect("store");
        let board = identity(FirmwareSet::Preview);
        let application = part("application", b"firmware-bytes");
        let ota = import_bytes(
            &store,
            &application.bytes,
            &ota_claim_lines(&board, &application),
        )
        .expect("ota");
        let archive = usb_archive(b"{}\n", &[application]).expect("zip");
        let usb = import_bytes(&store, &archive, &usb_claim_lines(&board)).expect("usb");
        let ota_envelope = store
            .read_loa_envelope(&ota)
            .expect("read")
            .expect("envelope");
        let ota_claims = claims_from_loa_envelope(&ota_envelope, ota.as_str()).expect("signature");
        assert_eq!(ota_claims.claim("flash-mode"), Some("ota"));
        assert_eq!(
            std::fs::read(root.path().join("data").join(ota.as_str()).join("data")).expect("data"),
            b"firmware-bytes"
        );
        let usb_envelope = store
            .read_loa_envelope(&usb)
            .expect("read")
            .expect("envelope");
        let usb_claims = claims_from_loa_envelope(&usb_envelope, usb.as_str()).expect("signature");
        assert_eq!(usb_claims.claim("flash-mode"), Some("usb"));
        assert_ne!(ota.as_str(), usb.as_str());
    }
}
