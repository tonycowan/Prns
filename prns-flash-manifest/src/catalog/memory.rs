use personal_hopspot_memory::{
    esp_partition_table, memory_profile_named, AddressRange, AddressSpaceGeometry,
    AddressSpaceKind, AddressSpaceKindLookupError, MemoryProfile, MemoryProfileId,
    ProcessorArchitecture, RegionRetention, RegionRole, RegionRoleLookupError, ValidationError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    BoardBuild, BoardCatalogEntry, EspBuild, NrfSerialDfuBuild, NrfSerialDfuCompatibility,
    Uf2BuildVariant,
};
use crate::ApplicationAddressRange;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryProfileReference(String);

impl MemoryProfileReference {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn resolve(&self) -> Result<ResolvedMemoryProfile, MemoryProfileReferenceError> {
        let profile = memory_profile_named(&self.0).ok_or_else(|| {
            MemoryProfileReferenceError::UnknownProfile {
                profile: self.0.clone(),
            }
        })?;
        profile
            .validate()
            .map_err(|error| MemoryProfileReferenceError::InvalidProfile {
                profile: profile.id,
                error,
            })?;
        let firmware = profile
            .region(profile.firmware.firmware_owned_region)
            .ok_or(MemoryProfileReferenceError::MissingFirmwareRegion {
                profile: profile.id,
            })?;
        let internal_flash = profile
            .unique_address_space_for_kind(AddressSpaceKind::InternalFlash)
            .map_err(|error| MemoryProfileReferenceError::InvalidInternalFlash {
                profile: profile.id,
                error,
            })?;
        let flash_geometry = internal_flash.geometry;
        let AddressSpaceGeometry::Fixed(flash_range) = flash_geometry else {
            return Err(MemoryProfileReferenceError::UnboundedInternalFlash {
                profile: profile.id,
                address_space: internal_flash.id,
            });
        };
        if flash_range.start() != 0 {
            return Err(MemoryProfileReferenceError::NonzeroInternalFlashOrigin {
                profile: profile.id,
                address_space: internal_flash.id,
                origin: flash_range.start(),
            });
        }
        let flash_capacity = u32::try_from(flash_range.byte_len()).map_err(|_| {
            MemoryProfileReferenceError::AddressExceedsU32 {
                profile: profile.id,
                address: flash_range.byte_len(),
            }
        })?;
        Ok(ResolvedMemoryProfile {
            profile,
            id: profile.id,
            architecture: profile.architecture,
            internal_flash_capacity: flash_capacity,
            firmware_owned: application_range(profile.id, firmware.range)?,
            transport_envelope: application_range(
                profile.id,
                profile.firmware.transport_envelope.range,
            )?,
        })
    }
}

/// One preserved partition an operator can choose to erase during a flash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErasablePartition {
    /// Partition-table name passed to the flasher, such as `wifi_cfg`.
    pub name: &'static str,
    /// Short label for the flash screen.
    pub label: &'static str,
    pub offset: u32,
    pub size: u32,
}

pub(super) fn erasable_partitions(board: &BoardCatalogEntry) -> Vec<ErasablePartition> {
    let BoardBuild::Esp(build) = &board.build else {
        return Vec::new();
    };
    let Some(profile) = memory_profile_named(build.memory_profile.as_str()) else {
        return Vec::new();
    };
    let Some(table) = esp_partition_table(profile.id) else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    for binding in table.partitions {
        let Some(region) = profile.region(binding.region) else {
            continue;
        };
        if region.retention != RegionRetention::PreserveAcrossFirmwareUpdate {
            continue;
        }
        let Ok(offset) = u32::try_from(region.range.start()) else {
            continue;
        };
        let Ok(size) = u32::try_from(region.range.byte_len()) else {
            continue;
        };
        parts.push(ErasablePartition {
            name: binding.name,
            label: erasable_partition_label(region.role),
            offset,
            size,
        });
    }
    parts
}

fn erasable_partition_label(role: RegionRole) -> &'static str {
    match role {
        RegionRole::PlatformData => "NVS",
        RegionRole::BleIdentity => "BLE identity",
        RegionRole::Provisioning => "Provisioned Wi-Fi",
        RegionRole::NodeIdentity => "Node identity",
        RegionRole::PhyInitialization => "PHY calibration",
        RegionRole::RemoteControlIdentity => "Remote Control identity",
        RegionRole::RadioProfile => "Radio profile",
        RegionRole::WifiConfiguration => "Sealed Wi-Fi",
        RegionRole::Journal => "Learned state",
        _ => "Preserved region",
    }
}

impl EspBuild {
    pub fn memory_layout(&self) -> Result<ResolvedMemoryProfile, MemoryProfileReferenceError> {
        self.memory_profile.resolve()
    }
}

impl Uf2BuildVariant {
    pub fn memory_layout(&self) -> Result<ResolvedMemoryProfile, MemoryProfileReferenceError> {
        self.memory_profile.resolve()
    }
}

impl NrfSerialDfuBuild {
    pub fn memory_layout(&self) -> Result<ResolvedMemoryProfile, MemoryProfileReferenceError> {
        self.memory_profile.resolve()
    }

    pub fn manifest_compatibility(
        &self,
    ) -> Result<NrfSerialDfuCompatibility, MemoryProfileReferenceError> {
        let application = self.memory_layout()?.transport_envelope();
        Ok(NrfSerialDfuCompatibility {
            softdevice_family: self.compatibility.softdevice_family.clone(),
            softdevice_version: self.compatibility.softdevice_version.clone(),
            fwid: self.compatibility.fwid.clone(),
            device_type: self.compatibility.device_type.clone(),
            device_revision: self.compatibility.device_revision,
            application_version: self.compatibility.application_version,
            application_base: format!("0x{:08x}", application.start()),
            application_end_exclusive: format!("0x{:08x}", application.end_exclusive()),
            bank_layout: self.compatibility.bank_layout,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedMemoryProfile {
    profile: &'static MemoryProfile,
    id: MemoryProfileId,
    architecture: ProcessorArchitecture,
    internal_flash_capacity: u32,
    firmware_owned: ApplicationAddressRange,
    transport_envelope: ApplicationAddressRange,
}

impl ResolvedMemoryProfile {
    #[must_use]
    pub const fn profile(self) -> &'static MemoryProfile {
        self.profile
    }

    #[must_use]
    pub const fn id(self) -> MemoryProfileId {
        self.id
    }

    #[must_use]
    pub const fn architecture(self) -> ProcessorArchitecture {
        self.architecture
    }

    #[must_use]
    pub const fn internal_flash_capacity(self) -> u32 {
        self.internal_flash_capacity
    }

    #[must_use]
    pub const fn firmware_owned(self) -> ApplicationAddressRange {
        self.firmware_owned
    }

    #[must_use]
    pub const fn transport_envelope(self) -> ApplicationAddressRange {
        self.transport_envelope
    }

    pub fn region_for_role(
        self,
        role: RegionRole,
    ) -> Result<ApplicationAddressRange, MemoryProfileReferenceError> {
        let region = self.profile.unique_region_for_role(role).map_err(|error| {
            MemoryProfileReferenceError::InvalidRegionRole {
                profile: self.id,
                error,
            }
        })?;
        application_range(self.id, region.range)
    }
}

fn application_range(
    profile: MemoryProfileId,
    range: AddressRange,
) -> Result<ApplicationAddressRange, MemoryProfileReferenceError> {
    let start = u32::try_from(range.start()).map_err(|_| {
        MemoryProfileReferenceError::AddressExceedsU32 {
            profile,
            address: range.start(),
        }
    })?;
    let end_exclusive =
        u32::try_from(range.end()).map_err(|_| MemoryProfileReferenceError::AddressExceedsU32 {
            profile,
            address: range.end(),
        })?;
    Ok(ApplicationAddressRange::new(start, end_exclusive))
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MemoryProfileReferenceError {
    #[error("unknown embedded memory profile {profile:?}")]
    UnknownProfile { profile: String },
    #[error("embedded memory profile {profile} is invalid: {error:?}")]
    InvalidProfile {
        profile: MemoryProfileId,
        error: ValidationError,
    },
    #[error("embedded memory profile {profile} has no firmware-owned region")]
    MissingFirmwareRegion { profile: MemoryProfileId },
    #[error("embedded memory profile {profile} has invalid internal flash: {error}")]
    InvalidInternalFlash {
        profile: MemoryProfileId,
        error: AddressSpaceKindLookupError,
    },
    #[error("embedded memory profile {profile} has invalid region role: {error}")]
    InvalidRegionRole {
        profile: MemoryProfileId,
        error: RegionRoleLookupError,
    },
    #[error("embedded memory profile {profile} internal flash {address_space:?} is not fixed")]
    UnboundedInternalFlash {
        profile: MemoryProfileId,
        address_space: personal_hopspot_memory::AddressSpaceId,
    },
    #[error(
        "embedded memory profile {profile} internal flash {address_space:?} starts at 0x{origin:x}"
    )]
    NonzeroInternalFlashOrigin {
        profile: MemoryProfileId,
        address_space: personal_hopspot_memory::AddressSpaceId,
        origin: u64,
    },
    #[error("embedded memory profile {profile} address 0x{address:x} exceeds u32")]
    AddressExceedsU32 {
        profile: MemoryProfileId,
        address: u64,
    },
}
