//! Firmware-update link for MeshTower V2.
//!
//! The controller speaks the same digest, length, and byte stream it uses for a Heltec slot.
//! Bytes land in `/firmware.new`. When the digest matches, the file is renamed to
//! `/firmware.old` and the RAM copier writes it into the single application region.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, Ordering};

use super::sd::{self, SdCard, StagedFirmware, VolumeError, BLOCK};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel::Channel;
use personal_rns::identity::IdentityHash;
use personal_rns::routing::links::channel::byte_stream;
use personal_rns::routing::links::LinkId;
use personal_rns::runtime::{Diagnostic, Message, PrnsEvent};
use personal_rns::wire::DestinationHash;
use prns_core::crypto::Sha256PrefixState;

/// One broadcast stream frame. The controller never sends a larger chunk on this link.
const CHUNK: usize = byte_stream::max_stream_payload(personal_rns::wire::BROADCAST_MTU);
/// One frame can sit here while the previous frame is written. The controller waits
/// for each `resume` before sending another, so a second frame means it ran ahead.
const QUEUE: usize = 1;

static DESTINATION: Mutex<CriticalSectionRawMutex, Cell<Option<DestinationHash>>> =
    Mutex::new(Cell::new(None));
static EVENTS: Channel<CriticalSectionRawMutex, OtaEvent, QUEUE> = Channel::new();
static DROPPED: AtomicBool = AtomicBool::new(false);

enum OtaEvent {
    Link {
        link_id: LinkId,
        destination: DestinationHash,
    },
    Identified {
        link_id: LinkId,
        identity: IdentityHash,
    },
    Closed {
        link_id: LinkId,
    },
    Chunk {
        link_id: LinkId,
        bytes: heapless::Vec<u8, CHUNK>,
        eof: bool,
    },
}

enum Phase {
    Digest,
    Length {
        digest: [u8; 32],
        resume_at: u32,
    },
    Data {
        digest: [u8; 32],
        staged: StagedFirmware,
        hasher: Sha256PrefixState,
    },
}

enum Feed {
    OfferResume(u32),
    Install(super::update::FirmwareImage),
}

pub(crate) fn set_firmware_update_destination(destination: DestinationHash) {
    ota_log("update: listening");
    DESTINATION.lock(|slot| slot.set(Some(destination)));
}

pub(crate) fn observe_firmware_update(event: PrnsEvent<'_>) {
    let queued = match event {
        PrnsEvent::Diagnostic(Diagnostic::LinkEstablished(established)) => Some(OtaEvent::Link {
            link_id: established.link_id,
            destination: established.destination,
        }),
        PrnsEvent::Diagnostic(Diagnostic::PeerIdentified { link_id, identity }) => {
            Some(OtaEvent::Identified { link_id, identity })
        }
        PrnsEvent::Diagnostic(Diagnostic::LinkClosed { link_id, .. }) => {
            Some(OtaEvent::Closed { link_id })
        }
        PrnsEvent::Message(Message::ChannelMessage {
            link_id,
            message_type,
            data,
        }) if message_type == byte_stream::STREAM_DATA_TYPE => {
            let Ok(frame) = byte_stream::parse(data) else {
                return;
            };
            if frame.header.compressed || frame.payload.len() > CHUNK {
                return;
            }
            let mut bytes = heapless::Vec::new();
            if bytes.extend_from_slice(frame.payload).is_err() {
                return;
            }
            Some(OtaEvent::Chunk {
                link_id,
                bytes,
                eof: frame.header.eof,
            })
        }
        _ => None,
    };
    if let Some(event) = queued {
        if EVENTS.try_send(event).is_err() {
            DROPPED.store(true, Ordering::Relaxed);
        }
    }
}

pub(crate) async fn serve(mut card: SdCard) {
    let scratch = sd::firmware_scratch();
    let mut link: Option<LinkId> = None;
    let mut peer: Option<IdentityHash> = None;
    let mut phase = Phase::Digest;
    loop {
        match EVENTS.receive().await {
            OtaEvent::Link {
                link_id,
                destination,
            } => {
                let wanted = DESTINATION.lock(|slot| slot.get());
                if wanted == Some(destination) && link.is_none() {
                    DROPPED.store(false, Ordering::Relaxed);
                    link = Some(link_id);
                    peer = None;
                    phase = Phase::Digest;
                    ota_log("update: link");
                }
            }
            OtaEvent::Identified { link_id, identity } => {
                if link == Some(link_id) {
                    peer = Some(identity);
                }
            }
            OtaEvent::Closed { link_id } => {
                if link == Some(link_id) {
                    link = None;
                    peer = None;
                    phase = Phase::Digest;
                    ota_log("update: link closed");
                }
            }
            OtaEvent::Chunk {
                link_id,
                bytes,
                eof,
            } => {
                if link != Some(link_id) {
                    continue;
                }
                let Some(identity) = peer else {
                    continue;
                };
                if !personal_rns::runtime::firmware_update_permitted(&identity) {
                    ota_log("update: grant refused");
                    continue;
                }
                match feed(&mut card, scratch, &mut phase, &bytes, eof).await {
                    Ok(Feed::OfferResume(offset)) => {
                        crate::runtime::reply_firmware_resume(link_id, offset).await;
                    }
                    Ok(Feed::Install(image)) => {
                        crate::runtime::reply_firmware_resume(link_id, image.bytes).await;
                        ota_log("update: copying into flash");
                        embassy_time::Timer::after_millis(300).await;
                        if super::update::commit(&image).is_err() {
                            ota_log("update: copy refused");
                            crate::runtime::close_firmware_link(link_id);
                        }
                        return;
                    }
                    Err(error) => {
                        ota_log_error(error);
                        crate::runtime::close_firmware_link(link_id);
                        link = None;
                        peer = None;
                        phase = Phase::Digest;
                    }
                }
            }
        }
    }
}

async fn feed(
    card: &mut SdCard,
    scratch: &mut [u8; BLOCK],
    phase: &mut Phase,
    bytes: &[u8],
    eof: bool,
) -> Result<Feed, VolumeError> {
    let dropped = DROPPED.swap(false, Ordering::Relaxed);
    let stored = match core::mem::replace(phase, Phase::Digest) {
        Phase::Digest => {
            let digest: [u8; 32] = bytes.try_into().map_err(|_| VolumeError::ImageRejected)?;
            let resume_at = card.firmware_resume_offset(&digest, scratch).await?;
            *phase = Phase::Length { digest, resume_at };
            Ok(Feed::OfferResume(resume_at))
        }
        Phase::Length { digest, resume_at } => {
            if bytes.len() != 4 {
                return Err(VolumeError::ImageRejected);
            }
            let declared = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if resume_at > declared {
                return Err(VolumeError::ImageRejected);
            }
            let staged = card
                .begin_firmware_stream(&digest, declared, resume_at > 0, scratch)
                .await?;
            if staged.valid != resume_at {
                return Err(VolumeError::ImageRejected);
            }
            let hasher = seed_hash(card, &staged, scratch).await?;
            let valid = staged.valid;
            *phase = Phase::Data {
                digest,
                staged,
                hasher,
            };
            Ok(Feed::OfferResume(valid))
        }
        Phase::Data {
            digest,
            mut staged,
            mut hasher,
        } => {
            if !bytes.is_empty() {
                card.write_firmware_stream(&mut staged, bytes, scratch)
                    .await?;
                hasher.update(bytes);
            }
            if !eof {
                let valid = staged.valid;
                *phase = Phase::Data {
                    digest,
                    staged,
                    hasher,
                };
                Ok(Feed::OfferResume(valid))
            } else if staged.valid != staged.declared {
                Err(VolumeError::ImageRejected)
            } else {
                let got = hasher.digest_with_suffix(&[]);
                if got != digest {
                    ota_log("update: digest mismatch");
                    Err(VolumeError::ImageRejected)
                } else {
                    let image = card.finish_firmware_stream(&mut staged, scratch).await?;
                    Ok(Feed::Install(image))
                }
            }
        }
    };
    if dropped && !matches!(stored, Ok(Feed::Install(_))) {
        ota_log("update: stream fell behind");
        return Err(VolumeError::ImageRejected);
    }
    stored
}

async fn seed_hash(
    card: &mut SdCard,
    staged: &StagedFirmware,
    scratch: &mut [u8; BLOCK],
) -> Result<Sha256PrefixState, VolumeError> {
    let mut hasher = Sha256PrefixState::absorb(&[]);
    let mut offset = 0u32;
    while offset < staged.valid {
        let read = card.read_firmware_stream(staged, offset, scratch).await?;
        if read == 0 {
            return Err(VolumeError::Fat);
        }
        hasher.update(&scratch[..read]);
        offset = offset.saturating_add(read as u32);
    }
    Ok(hasher)
}

fn ota_log(message: &str) {
    #[cfg(feature = "usb-debug-log")]
    log::info!("{message}");
    #[cfg(not(feature = "usb-debug-log"))]
    let _ = message;
}

fn ota_log_error(error: VolumeError) {
    #[cfg(feature = "usb-debug-log")]
    log::warn!("update: {error}");
    #[cfg(not(feature = "usb-debug-log"))]
    let _ = error;
}
