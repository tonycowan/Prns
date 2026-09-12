use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use personal_rns::runtime::RemoteControlTargetPairingConfirmation;

const CONTROL_DIRECTORY_NAME: &str = ".prnsd-control/remote-control";
const CONTROL_VERSION: &str = "prnsd-remote-control-v1";
const REQUEST_PREFIX: &str = "request-";
const INFLIGHT_PREFIX: &str = "inflight-";
const RESULT_PREFIX: &str = "result-";
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

static CONTROL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) type PendingPairingConfirmation =
    std::sync::Arc<std::sync::Mutex<Option<RemoteControlTargetPairingConfirmation>>>;
pub(crate) type OpenPairingState = std::sync::Arc<std::sync::Mutex<Option<PairingWindowSnapshot>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairingControlKind {
    Open,
    Close,
    Approve,
    Reject,
    Status,
}

impl PairingControlKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Close => "close",
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::Status => "status",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "close" => Some(Self::Close),
            "approve" => Some(Self::Approve),
            "reject" => Some(Self::Reject),
            "status" => Some(Self::Status),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairingWindowSnapshot {
    pub(crate) invitation_code: String,
    pub(crate) endpoint_hash: String,
    pub(crate) expires_at_millis: u64,
    /// Present when a controller has begun pairing and both sides should compare
    /// these six digits before approving.
    pub(crate) confirmation_digits: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PairingStatus {
    Closed,
    Open(PairingWindowSnapshot),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PairingControlSuccess {
    Opened(PairingWindowSnapshot),
    Closed,
    Approved,
    Rejected,
    Status(PairingStatus),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairingControlFailure {
    OpenFailed,
    CloseFailed,
    NoPendingConfirmation,
    ApproveFailed,
    RejectFailed,
    StateUnavailable,
}

impl PairingControlFailure {
    const fn code(self) -> &'static str {
        match self {
            Self::OpenFailed => "open-failed",
            Self::CloseFailed => "close-failed",
            Self::NoPendingConfirmation => "no-pending-confirmation",
            Self::ApproveFailed => "approve-failed",
            Self::RejectFailed => "reject-failed",
            Self::StateUnavailable => "state-unavailable",
        }
    }

    fn from_code(value: &str) -> Option<Self> {
        match value {
            "open-failed" => Some(Self::OpenFailed),
            "close-failed" => Some(Self::CloseFailed),
            "no-pending-confirmation" => Some(Self::NoPendingConfirmation),
            "approve-failed" => Some(Self::ApproveFailed),
            "reject-failed" => Some(Self::RejectFailed),
            "state-unavailable" => Some(Self::StateUnavailable),
            _ => None,
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::OpenFailed => "the daemon could not open Remote Control pairing",
            Self::CloseFailed => "the daemon could not close Remote Control pairing",
            Self::NoPendingConfirmation => "there is no pending pairing confirmation",
            Self::ApproveFailed => "the daemon could not approve the pending controller",
            Self::RejectFailed => "the daemon could not reject the pending controller",
            Self::StateUnavailable => "the daemon's pairing state is unavailable",
        }
    }
}

#[derive(Debug)]
pub(crate) enum PairingCliError {
    CommandContext(crate::command_context::CommandContextError),
    Control(io::Error),
    TimedOut,
    InvalidResult,
    Operation(PairingControlFailure),
}

impl core::fmt::Display for PairingCliError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CommandContext(error) => error.fmt(formatter),
            Self::Control(error) => write!(formatter, "pairing control failed: {error}"),
            Self::TimedOut => write!(
                formatter,
                "the daemon did not acknowledge the request within {} seconds",
                CONTROL_TIMEOUT.as_secs()
            ),
            Self::InvalidResult => formatter.write_str("the daemon returned an invalid result"),
            Self::Operation(failure) => formatter.write_str(failure.description()),
        }
    }
}

impl std::error::Error for PairingCliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CommandContext(error) => Some(error),
            Self::Control(error) => Some(error),
            Self::TimedOut | Self::InvalidResult | Self::Operation(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DecodedRequest {
    id: u128,
    kind: PairingControlKind,
}

pub(crate) struct ClaimedPairingControlRequest {
    request: DecodedRequest,
    inflight_path: PathBuf,
    result_path: PathBuf,
}

impl ClaimedPairingControlRequest {
    pub(crate) const fn kind(&self) -> PairingControlKind {
        self.request.kind
    }

    pub(crate) fn finish(self, result: Result<PairingControlSuccess, PairingControlFailure>) {
        let encoded = encode_result(self.request, result);
        if let Err(error) = atomic_write(&self.result_path, encoded.as_bytes()) {
            tracing::warn!(event = "remote_control_pairing_result_failed", error = %error);
            return;
        }
        if let Err(error) = remove_file(&self.inflight_path) {
            tracing::warn!(event = "remote_control_pairing_cleanup_failed", error = %error);
        }
    }
}

pub(crate) async fn next_control_request(
    config_dir: &Path,
) -> io::Result<ClaimedPairingControlRequest> {
    let root = control_root(config_dir);
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let mut requests = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                decode_request(&path, REQUEST_PREFIX).map(|request| (path, request))
            })
            .collect::<Vec<_>>();
        requests.sort_by_key(|(_, request)| request.id);
        for (path, request) in requests {
            let inflight_path = control_path(&root, INFLIGHT_PREFIX, request.id);
            match fs::rename(&path, &inflight_path) {
                Ok(()) => {
                    return Ok(ClaimedPairingControlRequest {
                        request,
                        inflight_path,
                        result_path: control_path(&root, RESULT_PREFIX, request.id),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

pub(crate) async fn run_cli(args: crate::cli::PairingArgs) -> Result<(), PairingCliError> {
    let (kind, config) = match args.command {
        crate::cli::PairingCommand::Open(args) => (PairingControlKind::Open, args.config),
        crate::cli::PairingCommand::Close(args) => (PairingControlKind::Close, args.config),
        crate::cli::PairingCommand::Approve(args) => (PairingControlKind::Approve, args.config),
        crate::cli::PairingCommand::Reject(args) => (PairingControlKind::Reject, args.config),
        crate::cli::PairingCommand::Status(args) => (PairingControlKind::Status, args.config),
    };
    let discovered = crate::command_context::discover(config.as_deref())
        .map_err(PairingCliError::CommandContext)?;
    let result = request_control(&discovered.dir, kind).await?;
    match result {
        PairingControlSuccess::Opened(snapshot) => print_open(&snapshot),
        PairingControlSuccess::Closed => println!("Remote Control pairing is closed."),
        PairingControlSuccess::Approved => println!("Approved the pending Remote Control pairing."),
        PairingControlSuccess::Rejected => println!("Rejected the pending Remote Control pairing."),
        PairingControlSuccess::Status(status) => print_status(&status),
    }
    Ok(())
}

fn print_open(snapshot: &PairingWindowSnapshot) {
    println!("{}", snapshot.invitation_code);
    println!("Endpoint: {}", snapshot.endpoint_hash);
    println!("Expires at (timeline ms): {}", snapshot.expires_at_millis);
}

fn print_status(status: &PairingStatus) {
    match status {
        PairingStatus::Closed => println!("Remote Control pairing is closed."),
        PairingStatus::Open(snapshot) => {
            println!("Remote Control pairing is open.");
            print_open(snapshot);
            match snapshot.confirmation_digits.as_deref() {
                Some(digits) => {
                    println!("Confirmation digits: {digits}");
                    println!("Compare these with the controller, then run: prnsd pairing approve");
                }
                None => println!("Confirmation: none"),
            }
        }
    }
}

async fn request_control(
    config_dir: &Path,
    kind: PairingControlKind,
) -> Result<PairingControlSuccess, PairingCliError> {
    let root = control_root(config_dir);
    prepare_control_root(&root).map_err(PairingCliError::Control)?;
    let id = next_control_id();
    let request_path = control_path(&root, REQUEST_PREFIX, id);
    let result_path = control_path(&root, RESULT_PREFIX, id);
    atomic_write(
        &request_path,
        format!("{CONTROL_VERSION}\n{id:032x}\n{}\n", kind.as_str()).as_bytes(),
    )
    .map_err(PairingCliError::Control)?;
    let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
    loop {
        match fs::read_to_string(&result_path) {
            Ok(text) => {
                let result = decode_result(DecodedRequest { id, kind }, &text)
                    .ok_or(PairingCliError::InvalidResult)?;
                let _ = remove_file(&result_path);
                return result.map_err(PairingCliError::Operation);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(PairingCliError::Control(error)),
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = remove_file(&request_path);
            return Err(PairingCliError::TimedOut);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn encode_result(
    request: DecodedRequest,
    result: Result<PairingControlSuccess, PairingControlFailure>,
) -> String {
    let prefix = format!("{CONTROL_VERSION}\n{:032x}\n", request.id);
    match result {
        Err(failure) => format!("{prefix}failed\n{}\n", failure.code()),
        Ok(PairingControlSuccess::Opened(snapshot)) => {
            format!("{prefix}ok\nopened\n{}", encode_snapshot(&snapshot))
        }
        Ok(PairingControlSuccess::Closed) => format!("{prefix}ok\nclosed\n"),
        Ok(PairingControlSuccess::Approved) => format!("{prefix}ok\napproved\n"),
        Ok(PairingControlSuccess::Rejected) => format!("{prefix}ok\nrejected\n"),
        Ok(PairingControlSuccess::Status(PairingStatus::Closed)) => {
            format!("{prefix}ok\nstatus-closed\n")
        }
        Ok(PairingControlSuccess::Status(PairingStatus::Open(snapshot))) => {
            format!("{prefix}ok\nstatus-open\n{}", encode_snapshot(&snapshot))
        }
    }
}

fn encode_snapshot(snapshot: &PairingWindowSnapshot) -> String {
    format!(
        "{}\n{}\n{}\n{}\n",
        snapshot.invitation_code,
        snapshot.endpoint_hash,
        snapshot.expires_at_millis,
        snapshot.confirmation_digits.as_deref().unwrap_or("none"),
    )
}

fn decode_result(
    request: DecodedRequest,
    text: &str,
) -> Option<Result<PairingControlSuccess, PairingControlFailure>> {
    let mut lines = text.lines();
    if lines.next()? != CONTROL_VERSION
        || u128::from_str_radix(lines.next()?, 16).ok()? != request.id
    {
        return None;
    }
    match lines.next()? {
        "failed" => {
            let failure = PairingControlFailure::from_code(lines.next()?)?;
            lines.next().is_none().then_some(Err(failure))
        }
        "ok" => {
            let success = match lines.next()? {
                "opened" => PairingControlSuccess::Opened(decode_snapshot(&mut lines)?),
                "closed" => PairingControlSuccess::Closed,
                "approved" => PairingControlSuccess::Approved,
                "rejected" => PairingControlSuccess::Rejected,
                "status-closed" => PairingControlSuccess::Status(PairingStatus::Closed),
                "status-open" => {
                    PairingControlSuccess::Status(PairingStatus::Open(decode_snapshot(&mut lines)?))
                }
                _ => return None,
            };
            lines.next().is_none().then_some(Ok(success))
        }
        _ => None,
    }
}

fn decode_snapshot<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Option<PairingWindowSnapshot> {
    Some(PairingWindowSnapshot {
        invitation_code: lines.next()?.to_owned(),
        endpoint_hash: lines.next()?.to_owned(),
        expires_at_millis: lines.next()?.parse().ok()?,
        confirmation_digits: match lines.next()? {
            "none" | "pending" => None,
            digits
                if digits.len() == 6
                    && digits.chars().all(|character| character.is_ascii_digit()) =>
            {
                Some(digits.to_owned())
            }
            _ => return None,
        },
    })
}

fn decode_request(path: &Path, prefix: &str) -> Option<DecodedRequest> {
    let id = control_file_id(path, prefix)?;
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()? != CONTROL_VERSION || u128::from_str_radix(lines.next()?, 16).ok()? != id {
        return None;
    }
    let kind = PairingControlKind::from_str(lines.next()?)?;
    lines
        .next()
        .is_none()
        .then_some(DecodedRequest { id, kind })
}

fn control_root(config_dir: &Path) -> PathBuf {
    config_dir.join(CONTROL_DIRECTORY_NAME)
}

fn control_path(root: &Path, prefix: &str, id: u128) -> PathBuf {
    root.join(format!("{prefix}{id:032x}"))
}

fn control_file_id(path: &Path, prefix: &str) -> Option<u128> {
    let encoded = path.file_name()?.to_str()?.strip_prefix(prefix)?;
    (encoded.len() == 32)
        .then(|| u128::from_str_radix(encoded, 16).ok())
        .flatten()
}

fn next_control_id() -> u128 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let process = u128::from(std::process::id()) << 64;
    let sequence = u128::from(CONTROL_SEQUENCE.fetch_add(1, Ordering::Relaxed));
    time ^ process ^ sequence
}

fn prepare_control_root(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        CONTROL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_control_result_round_trips() {
        let request = DecodedRequest {
            id: 7,
            kind: PairingControlKind::Status,
        };
        let snapshot = PairingWindowSnapshot {
            invitation_code: String::from("12ABCDEF"),
            endpoint_hash: String::from("00112233445566778899aabbccddeeff"),
            expires_at_millis: 42,
            confirmation_digits: Some(String::from("843863")),
        };
        for result in [
            Ok(PairingControlSuccess::Opened(snapshot.clone())),
            Ok(PairingControlSuccess::Closed),
            Ok(PairingControlSuccess::Approved),
            Ok(PairingControlSuccess::Rejected),
            Ok(PairingControlSuccess::Status(PairingStatus::Closed)),
            Ok(PairingControlSuccess::Status(PairingStatus::Open(
                snapshot.clone(),
            ))),
            Err(PairingControlFailure::NoPendingConfirmation),
        ] {
            assert_eq!(
                decode_result(request, &encode_result(request, result.clone())),
                Some(result)
            );
        }
    }

    #[tokio::test]
    async fn request_is_claimed_once_and_returns_its_result() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_dir = directory.path().to_path_buf();
        let client_dir = config_dir.clone();
        let client =
            tokio::spawn(
                async move { request_control(&client_dir, PairingControlKind::Close).await },
            );
        let request =
            tokio::time::timeout(Duration::from_secs(2), next_control_request(&config_dir))
                .await
                .expect("request arrives")
                .expect("request decodes");
        assert_eq!(request.kind(), PairingControlKind::Close);
        request.finish(Ok(PairingControlSuccess::Closed));
        assert_eq!(
            client
                .await
                .expect("client joins")
                .expect("control succeeds"),
            PairingControlSuccess::Closed
        );
    }
}
