//! MeshTower V2 microSD, bit-banged on the socket pins.
#![cfg_attr(feature = "image-battery-sense", allow(dead_code, unused))]
//!
//! The socket nets are P1.00 chip select, P0.06 clock, P1.01 MOSI, and P0.26 MISO.
//! P0.07 is the GPS power gate, not the card. Blocks are 512 bytes. exFAT is accepted as a
//! super-floppy volume, an MBR partition, or a GPT partition. A write can replace bytes in an
//! existing file, or create a new root file that fits in one sector. Creation takes one free
//! cluster, marks it in the allocation bitmap, and appends one directory set. It does not grow
//! a file past one sector. A directory set may cross a sector boundary, and the root gains one
//! cluster when the current one has no room left.

use core::fmt::Write;

use embassy_nrf::gpio::{Input, Output};
use embassy_time::{Duration, Instant, Timer};
use static_cell::StaticCell;

const BLOCK: usize = 512;
#[cfg(not(feature = "image-battery-sense"))]
const SETTLE: Duration = Duration::from_secs(5);
const MAX_NAME: usize = 64;
const MAX_DIR_SECTORS: u32 = 64;
const GPT_ENTRIES: usize = 32;
const CMD0_ATTEMPTS: usize = 32;
const ACMD41_ATTEMPTS: usize = 200;
const ACMD41_PAUSE: Duration = Duration::from_millis(10);
const DATA_WAIT: Duration = Duration::from_millis(250);
const BUSY_WAIT: Duration = Duration::from_millis(500);

const GO_IDLE: u8 = 0;
const SEND_IF_COND: u8 = 8;
const SET_BLOCKLEN: u8 = 16;
const READ_SINGLE: u8 = 17;
const WRITE_SINGLE: u8 = 24;
const APP_CMD: u8 = 55;
const READ_OCR: u8 = 58;
const SD_SEND_OP_COND: u8 = 41;
const IF_COND_ARG: u32 = 0x1AA;
const HCS: u32 = 0x4000_0000;
const IDLE: u8 = 0x01;
const ILLEGAL: u8 = 0x04;
const DATA_TOKEN: u8 = 0xFE;
const DATA_ACCEPTED: u8 = 0x05;
const FILE_ENTRY: u8 = 0x85;
const STREAM_ENTRY: u8 = 0xC0;
const NAME_ENTRY: u8 = 0xC1;
const BITMAP_ENTRY: u8 = 0x81;
const UPCASE_ENTRY: u8 = 0x82;
const DIRECTORY: u16 = 0x10;
const ARCHIVE: u8 = 0x20;
const NO_FAT_CHAIN: u8 = 0x02;
const ALLOCATION_POSSIBLE: u8 = 0x01;
const NAME_ENTRY_CHARS: usize = 15;
const WRITE_NAME: &[u8] = b"write.txt";
const WRITE_BODY: &[u8] = b"This file was written";
const FIRMWARE_NAME: &[u8] = b"firmware.bin";
const FIRMWARE_DONE: &[u8] = b"firmware.old";
#[cfg(all(feature = "image-spruce-101", not(feature = "image-battery-sense")))]
const BOOT_ID: &str = "spruce-101";
#[cfg(all(
    not(feature = "image-spruce-101"),
    not(feature = "image-battery-sense")
))]
const BOOT_ID: &str = "alder-747";
const _: () = assert!(FIRMWARE_NAME.len() == FIRMWARE_DONE.len());
const APP_BASE: u32 = 0x26000;
const APP_END: u32 = 0xE2000;
const FAT_END: u32 = 0xFFFF_FFF8;
const MBR_BASE: usize = 446;
const PROTECTIVE_MBR: u8 = 0xEE;
const EXFAT_MAGIC: &[u8] = b"EXFAT   ";

/// Half of a 100 kHz bit at the 64 MHz CPU clock. Slow enough for card initialization.
const HALF_BIT_CYCLES: u32 = 320;

const CMD0_CRC: u8 = crc7(&[0x40, 0, 0, 0, 0]);
const CMD8_CRC: u8 = crc7(&[0x48, 0, 0, 1, 0xAA]);
const _: () = assert!(CMD0_CRC == 0x95);
const _: () = assert!(CMD8_CRC == 0x87);

pub(crate) struct SdCard {
    sck: Output<'static>,
    mosi: Output<'static>,
    miso: Input<'static>,
    cs: Output<'static>,
    state: CardState,
}

enum CardState {
    Down,
    Ready(Addressing),
    Mounted {
        addressing: Addressing,
        volume: Volume,
    },
}

#[derive(Clone, Copy)]
enum Addressing {
    Byte,
    Block,
}

#[derive(Clone, Copy)]
struct Volume {
    volume_lba: u32,
    fat_lba: u32,
    fat_length: u32,
    fat_count: u8,
    heap_lba: u32,
    sectors_per_cluster: u32,
    cluster_count: u32,
    root_cluster: u32,
}

#[derive(Clone, Copy)]
struct RootFile {
    first_cluster: u32,
    valid_bytes: u64,
    chain: ClusterChain,
}

#[derive(Clone, Copy)]
enum ClusterChain {
    Contiguous,
    Fat,
}

enum CardGeneration {
    V1,
    V2,
}

enum ReadyWait {
    Immediate,
    Brief,
    UntilIdle,
}

enum FatLink {
    Next(u32),
    End,
}

enum CardError {
    IdleTimeout,
    InitTimeout,
    /// MISO produced no response byte. `sample` is the first four bytes clocked in.
    ResponseTimeout {
        command: u8,
        sample: u32,
    },
    /// CMD17 was accepted and MISO then stayed high until the data wait expired.
    DataTimeout(u32),
    DataRejected,
    WriteRejected,
    Busy,
    NotReady,
    Address,
    Interface,
    Status(u8),
}

enum VolumeError {
    Card(CardError),
    NotExfat,
    Geometry,
    DirectoryLimit,
    FileNotFound,
    NameRejected,
    OutOfRange,
    Fat,
    NotMounted,
    NoSpace,
    DirectoryFull,
    ImageRejected,
}

impl From<CardError> for VolumeError {
    fn from(error: CardError) -> Self {
        Self::Card(error)
    }
}

impl core::fmt::Display for CardError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::IdleTimeout => formatter.write_str("card idle timeout"),
            Self::InitTimeout => formatter.write_str("card init timeout"),
            Self::ResponseTimeout { command, sample } => {
                write!(formatter, "cmd{command} saw {sample:08x}")
            }
            Self::DataTimeout(lba) => write!(formatter, "cmd17 lba {lba} no data"),
            Self::DataRejected => formatter.write_str("data rejected"),
            Self::WriteRejected => formatter.write_str("write rejected"),
            Self::Busy => formatter.write_str("card busy timeout"),
            Self::NotReady => formatter.write_str("card not ready"),
            Self::Address => formatter.write_str("block address overflow"),
            Self::Interface => formatter.write_str("interface condition rejected"),
            Self::Status(status) => write!(formatter, "card status {status:02x}"),
        }
    }
}

impl core::fmt::Display for VolumeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Card(error) => write!(formatter, "{error}"),
            Self::NotExfat => formatter.write_str("not exfat"),
            Self::Geometry => formatter.write_str("exfat geometry rejected"),
            Self::DirectoryLimit => formatter.write_str("root directory too large"),
            Self::FileNotFound => formatter.write_str("file not found"),
            Self::NameRejected => formatter.write_str("file name rejected"),
            Self::OutOfRange => formatter.write_str("write past end of file"),
            Self::Fat => formatter.write_str("fat chain broken"),
            Self::NotMounted => formatter.write_str("volume not mounted"),
            Self::NoSpace => formatter.write_str("no free cluster"),
            Self::DirectoryFull => formatter.write_str("root directory full"),
            Self::ImageRejected => formatter.write_str("firmware image rejected"),
        }
    }
}

enum PartitionKind {
    Empty,
    Protective,
    Probe(u32),
}

enum ScanControl {
    Continue,
    Found(RootFile),
    End,
    Broken,
}

struct FileScan<'a> {
    target: &'a [u8],
    phase: ScanPhase,
}

enum ScanPhase {
    Searching,
    InFile(PartialFile),
}

struct PartialFile {
    remaining: u8,
    attributes: u16,
    name: [u16; MAX_NAME],
    stored: usize,
    declared: usize,
    stream: StreamSlot,
}

enum StreamSlot {
    Absent,
    Present(StreamInfo),
}

struct StreamInfo {
    first_cluster: u32,
    valid_bytes: u64,
    chain: ClusterChain,
}

struct StagedSector {
    lba: u32,
    start: usize,
    len: usize,
}

#[derive(Clone, Copy)]
struct BitmapLoc {
    cluster: u32,
    bytes: u64,
}

struct ExistingFile {
    file: RootFile,
    set_lba: u32,
    set_cluster: u32,
    set_sector: u32,
    set_index: usize,
    secondary_count: u8,
}

struct DirSlot {
    lba: u32,
    index: usize,
    cluster: u32,
    sector: u32,
}

struct RootPlan {
    bitmap: Option<BitmapLoc>,
    upcase_cluster: Option<u32>,
    existing: Option<ExistingFile>,
    slot: Option<DirSlot>,
}

impl SdCard {
    pub(crate) fn new(
        sck: Output<'static>,
        mosi: Output<'static>,
        miso: Input<'static>,
        cs: Output<'static>,
    ) -> Self {
        Self {
            sck,
            mosi,
            miso,
            cs,
            state: CardState::Down,
        }
    }

    async fn open_root_file(
        &mut self,
        name: &[u8],
        scratch: &mut [u8; BLOCK],
    ) -> Result<RootFile, VolumeError> {
        accept_root_name(name)?;
        self.mount(scratch).await?;
        self.find_root_file(name, scratch).await
    }

    async fn mount(&mut self, scratch: &mut [u8; BLOCK]) -> Result<(), VolumeError> {
        self.initialize().await?;
        if !matches!(self.state, CardState::Mounted { .. }) {
            let addressing = self.addressing()?;
            let volume = self.find_volume(scratch).await?;
            self.state = CardState::Mounted { addressing, volume };
        }
        Ok(())
    }

    /// Create `name` in the root, or replace it when it already exists.
    ///
    /// The payload must fit in one sector. A new file uses one contiguous cluster and does not
    /// update the FAT. An existing file keeps its cluster and only its stored length changes.
    async fn store_root_file(
        &mut self,
        name: &[u8],
        data: &[u8],
        scratch: &mut [u8; BLOCK],
    ) -> Result<(), VolumeError> {
        accept_root_name(name)?;
        if name.len() > NAME_ENTRY_CHARS || data.len() > BLOCK {
            return Err(VolumeError::OutOfRange);
        }
        self.mount(scratch).await?;
        let volume = self.volume()?;
        write_console(format_args!(
            "sd: root {} spc {}",
            volume.root_cluster, volume.sectors_per_cluster
        ));
        let plan = self.scan_root(name, scratch).await?;
        if let Some(existing) = plan.existing {
            return self.replace_existing(existing, data, scratch).await;
        }
        let Some(slot) = plan.slot else {
            write_console(format_args!("sd: no dir end"));
            return Err(VolumeError::DirectoryFull);
        };
        write_console(format_args!(
            "sd: slot {} sector {}",
            slot.index, slot.sector
        ));
        let bitmap = plan.bitmap.ok_or(VolumeError::Geometry)?;
        if bitmap.cluster == volume.root_cluster {
            return Err(VolumeError::Geometry);
        }
        self.ensure_directory_room(&slot, bitmap, scratch).await?;
        let cluster = self
            .claim_cluster(bitmap, volume.root_cluster, plan.upcase_cluster, scratch)
            .await?;
        write_console(format_args!("sd: cluster {cluster}"));
        self.write_cluster_prefix(cluster, data, scratch).await?;
        self.insert_directory_set(slot, name, cluster, data.len() as u64, scratch)
            .await
    }

    async fn prepare_firmware(
        &mut self,
        scratch: &mut [u8; BLOCK],
    ) -> Result<super::update::FirmwareImage, VolumeError> {
        let file = self.open_root_file(FIRMWARE_NAME, scratch).await?;
        let bytes = u32::try_from(file.valid_bytes).map_err(|_| VolumeError::ImageRejected)?;
        if bytes < 8 || bytes > APP_END - APP_BASE {
            return Err(VolumeError::ImageRejected);
        }
        let header = self.load_file_slice(&file, 0, scratch).await?;
        if header < 8 || !firmware_vector_ok(scratch) {
            return Err(VolumeError::ImageRejected);
        }
        let volume = self.volume()?;
        let cluster_bytes = u64::from(volume.sectors_per_cluster) * BLOCK as u64;
        let mut image = super::update::FirmwareImage {
            clusters: [0; super::update::MAX_FIRMWARE_CLUSTERS],
            cluster_count: 0,
            sectors_per_cluster: volume.sectors_per_cluster,
            heap_lba: volume.heap_lba,
            bytes,
        };
        let mut cluster = file.first_cluster;
        let mut left = file.valid_bytes;
        while left > 0 {
            let index = image.cluster_count as usize;
            if index >= image.clusters.len() || cluster < 2 {
                return Err(VolumeError::ImageRejected);
            }
            image.clusters[index] = cluster;
            image.cluster_count += 1;
            if left <= cluster_bytes {
                break;
            }
            left -= cluster_bytes;
            cluster = match file.chain {
                ClusterChain::Contiguous => cluster.saturating_add(1),
                ClusterChain::Fat => match self.fat_next(volume, cluster, scratch).await? {
                    FatLink::Next(next) => next,
                    FatLink::End => return Err(VolumeError::Fat),
                },
            };
        }
        Ok(image)
    }

    /// Rename `/firmware.bin` to `/firmware.old` in place. Both names are 12 characters, so the
    /// directory set keeps its size. This runs before the copier resets the chip.
    async fn rename_firmware(&mut self, scratch: &mut [u8; BLOCK]) -> Result<(), VolumeError> {
        self.mount(scratch).await?;
        let plan = self.scan_root(FIRMWARE_NAME, scratch).await?;
        let Some(existing) = plan.existing else {
            return Err(VolumeError::FileNotFound);
        };
        let entries = 1 + existing.secondary_count as usize;
        if existing.secondary_count == 0 || entries > 4 {
            return Err(VolumeError::Geometry);
        }
        let volume = self.volume()?;
        let mut set = [0u8; 128];
        let mut index = 0;
        while index < entries {
            let (lba, offset) = directory_slot(&volume, &existing, index)?;
            self.read_block(lba, scratch).await?;
            set[index * 32..index * 32 + 32].copy_from_slice(&scratch[offset..offset + 32]);
            index += 1;
        }
        if set[0] != FILE_ENTRY {
            return Err(VolumeError::Geometry);
        }
        write_firmware_name(&mut set[..entries * 32], FIRMWARE_DONE);
        let sum = entry_checksum(&set[..entries * 32]).to_le_bytes();
        set[2] = sum[0];
        set[3] = sum[1];
        index = 0;
        while index < entries {
            let (lba, offset) = directory_slot(&volume, &existing, index)?;
            self.read_block(lba, scratch).await?;
            scratch[offset..offset + 32].copy_from_slice(&set[index * 32..index * 32 + 32]);
            self.write_block(lba, scratch).await?;
            index += 1;
        }
        Ok(())
    }

    async fn load_file_slice(
        &mut self,
        file: &RootFile,
        offset: u64,
        scratch: &mut [u8; BLOCK],
    ) -> Result<usize, VolumeError> {
        let staged = self.stage_file_sector(file, offset, scratch).await?;
        if staged.len == 0 {
            return Ok(0);
        }
        if staged.start != 0 {
            scratch.copy_within(staged.start..staged.start + staged.len, 0);
        }
        Ok(staged.len)
    }

    /// Replace bytes already stored in `file`. The directory entry, valid length, and cluster
    /// allocation stay as they are.
    async fn overwrite_root_file(
        &mut self,
        file: &RootFile,
        offset: u64,
        data: &[u8],
        scratch: &mut [u8; BLOCK],
    ) -> Result<(), VolumeError> {
        let Some(end) = offset.checked_add(data.len() as u64) else {
            return Err(VolumeError::OutOfRange);
        };
        if end > file.valid_bytes {
            return Err(VolumeError::OutOfRange);
        }
        let mut cursor = 0;
        while cursor < data.len() {
            let staged = self
                .stage_file_sector(file, offset + cursor as u64, scratch)
                .await?;
            if staged.len == 0 {
                return Err(VolumeError::Fat);
            }
            let take = staged.len.min(data.len() - cursor);
            let span = staged.start..staged.start + take;
            scratch[span].copy_from_slice(&data[cursor..cursor + take]);
            self.write_block(staged.lba, scratch).await?;
            cursor += take;
        }
        Ok(())
    }

    async fn scan_root(
        &mut self,
        name: &[u8],
        scratch: &mut [u8; BLOCK],
    ) -> Result<RootPlan, VolumeError> {
        let volume = self.volume()?;
        let mut scan = FileScan {
            target: name,
            phase: ScanPhase::Searching,
        };
        let mut plan = RootPlan {
            bitmap: None,
            upcase_cluster: None,
            existing: None,
            slot: None,
        };
        let mut set_lba = 0u32;
        let mut set_cluster = 0u32;
        let mut set_sector = 0u32;
        let mut set_index = 0usize;
        let mut secondary_count = 0u8;
        let mut cluster = volume.root_cluster;
        let mut sectors = 0u32;
        loop {
            for sector in 0..volume.sectors_per_cluster {
                if sectors >= MAX_DIR_SECTORS {
                    return Err(VolumeError::DirectoryLimit);
                }
                let lba = volume.lba(cluster, sector)?;
                self.read_block(lba, scratch).await?;
                sectors = sectors.saturating_add(1);
                for index in 0..BLOCK / 32 {
                    let start = index * 32;
                    let mut entry = [0u8; 32];
                    entry.copy_from_slice(&scratch[start..start + 32]);
                    if entry[0] == BITMAP_ENTRY && entry[1] & 1 == 0 && plan.bitmap.is_none() {
                        plan.bitmap = Some(BitmapLoc {
                            cluster: le_u32_at(&entry, 20),
                            bytes: le_u64_at(&entry, 24),
                        });
                    }
                    if entry[0] == UPCASE_ENTRY && plan.upcase_cluster.is_none() {
                        plan.upcase_cluster = Some(le_u32_at(&entry, 20));
                    }
                    if entry[0] == FILE_ENTRY && matches!(scan.phase, ScanPhase::Searching) {
                        set_lba = lba;
                        set_cluster = cluster;
                        set_sector = sector;
                        set_index = index;
                        secondary_count = entry[1];
                    }
                    match scan.push(entry) {
                        ScanControl::Continue => {}
                        ScanControl::Found(file) => {
                            plan.existing = Some(ExistingFile {
                                file,
                                set_lba,
                                set_cluster,
                                set_sector,
                                set_index,
                                secondary_count,
                            });
                            return Ok(plan);
                        }
                        ScanControl::End => {
                            log_dir_types(scratch);
                            plan.slot = Some(DirSlot {
                                lba,
                                index,
                                cluster,
                                sector,
                            });
                            return Ok(plan);
                        }
                        ScanControl::Broken => return Err(VolumeError::Fat),
                    }
                }
            }
            cluster = match self.fat_next(volume, cluster, scratch).await? {
                FatLink::Next(next) => next,
                FatLink::End => return Ok(plan),
            };
        }
    }

    async fn claim_cluster(
        &mut self,
        bitmap: BitmapLoc,
        root_cluster: u32,
        upcase_cluster: Option<u32>,
        scratch: &mut [u8; BLOCK],
    ) -> Result<u32, VolumeError> {
        let volume = self.volume()?;
        if bitmap.cluster < 2 || bitmap.bytes == 0 {
            return Err(VolumeError::Geometry);
        }
        let mut bit = 0u32;
        while bit < volume.cluster_count {
            let byte_index = bit / 8;
            if u64::from(byte_index) >= bitmap.bytes {
                return Err(VolumeError::NoSpace);
            }
            let sector_index = byte_index / BLOCK as u32;
            let cluster_step = sector_index / volume.sectors_per_cluster;
            let sector = sector_index % volume.sectors_per_cluster;
            let cluster = bitmap
                .cluster
                .checked_add(cluster_step)
                .ok_or(VolumeError::Fat)?;
            let lba = volume.lba(cluster, sector)?;
            self.read_block(lba, scratch).await?;
            let sector_bit = sector_index.saturating_mul((BLOCK as u32).saturating_mul(8));
            let mut offset = ((bit / 8) as usize) % BLOCK;
            while offset < BLOCK {
                let mut mask = 1u8;
                let mut shift = 0u32;
                while shift < 8 {
                    let candidate = sector_bit + (offset as u32) * 8 + shift;
                    if candidate >= volume.cluster_count {
                        return Err(VolumeError::NoSpace);
                    }
                    let cluster_no = candidate + 2;
                    let reserved = cluster_no == root_cluster
                        || cluster_no == bitmap.cluster
                        || Some(cluster_no) == upcase_cluster;
                    if scratch[offset] & mask == 0 && !reserved {
                        scratch[offset] |= mask;
                        self.write_block(lba, scratch).await?;
                        return Ok(cluster_no);
                    }
                    mask <<= 1;
                    shift += 1;
                }
                offset += 1;
            }
            let next = sector_bit + (BLOCK as u32) * 8;
            if next <= bit {
                return Err(VolumeError::NoSpace);
            }
            bit = next;
        }
        Err(VolumeError::NoSpace)
    }

    async fn write_cluster_prefix(
        &mut self,
        cluster: u32,
        data: &[u8],
        scratch: &mut [u8; BLOCK],
    ) -> Result<(), VolumeError> {
        if data.len() > BLOCK {
            return Err(VolumeError::OutOfRange);
        }
        let lba = self.volume()?.lba(cluster, 0)?;
        scratch.fill(0);
        scratch[..data.len()].copy_from_slice(data);
        self.write_block(lba, scratch).await?;
        Ok(())
    }

    async fn replace_existing(
        &mut self,
        existing: ExistingFile,
        data: &[u8],
        scratch: &mut [u8; BLOCK],
    ) -> Result<(), VolumeError> {
        if existing.file.first_cluster < 2 {
            return Err(VolumeError::Fat);
        }
        if data.len() as u64 == existing.file.valid_bytes {
            return self
                .overwrite_root_file(&existing.file, 0, data, scratch)
                .await;
        }
        self.write_cluster_prefix(existing.file.first_cluster, data, scratch)
            .await?;
        self.rewrite_stream_length(existing, data.len() as u64, scratch)
            .await
    }

    async fn rewrite_stream_length(
        &mut self,
        existing: ExistingFile,
        bytes: u64,
        scratch: &mut [u8; BLOCK],
    ) -> Result<(), VolumeError> {
        let entries = 1 + usize::from(existing.secondary_count);
        if existing.set_index + entries > BLOCK / 32 || entries < 2 {
            return Err(VolumeError::DirectoryFull);
        }
        self.read_block(existing.set_lba, scratch).await?;
        let start = existing.set_index * 32;
        let end = start + entries * 32;
        if scratch[start] != FILE_ENTRY {
            return Err(VolumeError::Fat);
        }
        let mut stream_at = None;
        for entry in 1..entries {
            let at = start + entry * 32;
            if scratch[at] == STREAM_ENTRY {
                stream_at = Some(at);
                break;
            }
        }
        let Some(stream_at) = stream_at else {
            return Err(VolumeError::Fat);
        };
        scratch[stream_at + 8..stream_at + 16].copy_from_slice(&bytes.to_le_bytes());
        scratch[stream_at + 24..stream_at + 32].copy_from_slice(&bytes.to_le_bytes());
        let sum = entry_checksum(&scratch[start..end]);
        scratch[start + 2..start + 4].copy_from_slice(&sum.to_le_bytes());
        self.write_block(existing.set_lba, scratch).await?;
        Ok(())
    }

    async fn ensure_directory_room(
        &mut self,
        slot: &DirSlot,
        bitmap: BitmapLoc,
        scratch: &mut [u8; BLOCK],
    ) -> Result<(), VolumeError> {
        let volume = self.volume()?;
        let later = volume.sectors_per_cluster.saturating_sub(slot.sector + 1) as usize;
        let room = (BLOCK / 32 - slot.index) + later * (BLOCK / 32);
        if room >= 3 {
            return Ok(());
        }
        match self.fat_next(volume, slot.cluster, scratch).await? {
            FatLink::Next(_) => Ok(()),
            FatLink::End => {
                let fresh = self
                    .claim_cluster(bitmap, slot.cluster, None, scratch)
                    .await?;
                self.write_fat(slot.cluster, fresh, scratch).await?;
                self.write_fat(fresh, u32::MAX, scratch).await
            }
        }
    }

    async fn write_fat(
        &mut self,
        cluster: u32,
        value: u32,
        scratch: &mut [u8; BLOCK],
    ) -> Result<(), VolumeError> {
        let volume = self.volume()?;
        let byte_index = cluster.checked_mul(4).ok_or(VolumeError::Fat)?;
        let sector_index = byte_index / BLOCK as u32;
        let offset = (byte_index % BLOCK as u32) as usize;
        if sector_index >= volume.fat_length {
            return Err(VolumeError::Fat);
        }
        let fats = u32::from(volume.fat_count.max(1).min(2));
        for fat in 0..fats {
            let lba = volume
                .fat_lba
                .checked_add(fat.saturating_mul(volume.fat_length))
                .and_then(|base| base.checked_add(sector_index))
                .ok_or(VolumeError::Fat)?;
            self.read_block(lba, scratch).await?;
            scratch[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            self.write_block(lba, scratch).await?;
        }
        Ok(())
    }

    async fn insert_directory_set(
        &mut self,
        slot: DirSlot,
        name: &[u8],
        cluster: u32,
        bytes: u64,
        scratch: &mut [u8; BLOCK],
    ) -> Result<(), VolumeError> {
        let set = directory_set(name, cluster, bytes);
        let mut cursor = 0usize;
        let mut lba = slot.lba;
        let mut index = slot.index;
        let mut dir_cluster = slot.cluster;
        let mut sector = slot.sector;
        let mut first = true;
        while cursor < set.len() {
            let room = BLOCK / 32 - index;
            if room == 0 {
                return Err(VolumeError::DirectoryFull);
            }
            let take = room.min((set.len() - cursor) / 32);
            let bytes = take * 32;
            self.read_block(lba, scratch).await?;
            if first && scratch[index * 32] != 0 {
                return Err(VolumeError::DirectoryFull);
            }
            let start = index * 32;
            scratch[start..start + bytes].copy_from_slice(&set[cursor..cursor + bytes]);
            cursor += bytes;
            if cursor == set.len() {
                scratch[start + bytes..].fill(0);
            }
            self.write_block(lba, scratch).await?;
            first = false;
            if cursor == set.len() {
                break;
            }
            let volume = self.volume()?;
            if sector + 1 < volume.sectors_per_cluster {
                sector += 1;
                lba = volume.lba(dir_cluster, sector)?;
            } else {
                dir_cluster = match self.fat_next(volume, dir_cluster, scratch).await? {
                    FatLink::Next(next) => next,
                    FatLink::End => return Err(VolumeError::DirectoryFull),
                };
                sector = 0;
                lba = volume.lba(dir_cluster, 0)?;
            }
            index = 0;
        }
        Ok(())
    }

    fn link_label(&self) -> &'static str {
        match self.addressing() {
            Ok(Addressing::Block) => "block",
            Ok(Addressing::Byte) => "byte",
            Err(_) => "down",
        }
    }

    fn volume_lba(&self) -> u32 {
        match self.state {
            CardState::Mounted { volume, .. } => volume.volume_lba,
            _ => 0,
        }
    }

    async fn initialize(&mut self) -> Result<(), CardError> {
        if !matches!(self.state, CardState::Down) {
            return Ok(());
        }
        Timer::after_millis(10).await;
        self.probe_hardware_spi();
        let idle = u8::from(self.miso.is_high());
        write_console(format_args!("sd: bitbang miso {idle}"));
        self.cs.set_high();
        self.write_clocks(10).await?;
        self.enter_idle().await?;
        write_console(format_args!("sd: idle"));
        let generation = self.interface_condition().await?;
        let hcs = match generation {
            CardGeneration::V2 => {
                write_console(format_args!("sd: v2"));
                HCS
            }
            CardGeneration::V1 => {
                write_console(format_args!("sd: v1"));
                0
            }
        };
        self.leave_idle(hcs).await?;
        write_console(format_args!("sd: ready"));
        let addressing = match generation {
            CardGeneration::V2 => self.read_addressing().await?,
            CardGeneration::V1 => Addressing::Byte,
        };
        if matches!(addressing, Addressing::Byte) {
            self.command_r1(SET_BLOCKLEN, BLOCK as u32, ReadyWait::Brief)
                .await?;
        }
        // The socket has 22 ohm series resistors. Stay at the init clock until a block read works.
        match addressing {
            Addressing::Block => write_console(format_args!("sd: block")),
            Addressing::Byte => write_console(format_args!("sd: byte")),
        }
        self.state = CardState::Ready(addressing);
        Ok(())
    }

    async fn enter_idle(&mut self) -> Result<(), CardError> {
        for _ in 0..CMD0_ATTEMPTS {
            let status = self.command_r1(GO_IDLE, 0, ReadyWait::Immediate).await?;
            if status == IDLE {
                return Ok(());
            }
            Timer::after_millis(1).await;
        }
        Err(CardError::IdleTimeout)
    }

    async fn interface_condition(&mut self) -> Result<CardGeneration, CardError> {
        let selected = self
            .command_while_selected(SEND_IF_COND, IF_COND_ARG, ReadyWait::Brief)
            .await;
        let generation = match selected {
            Ok(status) if status == IDLE => match self.read_n::<4>().await {
                Ok(tail) if tail == [0x00, 0x00, 0x01, 0xAA] => Ok(CardGeneration::V2),
                Ok(_) => Err(CardError::Interface),
                Err(error) => Err(error),
            },
            Ok(status) if status == IDLE | ILLEGAL => Ok(CardGeneration::V1),
            Ok(status) => Err(CardError::Status(status)),
            Err(error) => Err(error),
        };
        self.abandon(generation).await
    }

    async fn leave_idle(&mut self, hcs: u32) -> Result<(), CardError> {
        for _ in 0..ACMD41_ATTEMPTS {
            let app = self.command_r1(APP_CMD, 0, ReadyWait::Brief).await?;
            if app & !IDLE != 0 {
                return Err(CardError::Status(app));
            }
            let status = self
                .command_r1(SD_SEND_OP_COND, hcs, ReadyWait::Brief)
                .await?;
            if status == 0 {
                return Ok(());
            }
            if status != IDLE {
                return Err(CardError::Status(status));
            }
            Timer::after(ACMD41_PAUSE).await;
        }
        Err(CardError::InitTimeout)
    }

    async fn read_addressing(&mut self) -> Result<Addressing, CardError> {
        let (status, ocr) = self.command_tail(READ_OCR, 0, ReadyWait::Brief).await?;
        if status != 0 {
            return Err(CardError::Status(status));
        }
        if ocr[0] & 0x40 == 0 {
            Ok(Addressing::Byte)
        } else {
            Ok(Addressing::Block)
        }
    }

    async fn find_volume(&mut self, scratch: &mut [u8; BLOCK]) -> Result<Volume, VolumeError> {
        self.read_block(0, scratch).await?;
        if is_exfat(scratch) {
            return volume_from_boot(scratch, 0);
        }
        if !has_boot_signature(scratch) {
            return Err(VolumeError::NotExfat);
        }
        let mut records = [
            PartitionKind::Empty,
            PartitionKind::Empty,
            PartitionKind::Empty,
            PartitionKind::Empty,
        ];
        for (index, record) in records.iter_mut().enumerate() {
            *record = mbr_partition(scratch, index);
        }
        let mut protective = false;
        for record in records {
            match record {
                PartitionKind::Empty => {}
                PartitionKind::Protective => protective = true,
                PartitionKind::Probe(lba) => {
                    if self.read_block(lba, scratch).await.is_ok() && is_exfat(scratch) {
                        return volume_from_boot(scratch, lba);
                    }
                }
            }
        }
        if protective {
            self.find_gpt(scratch).await
        } else {
            Err(VolumeError::NotExfat)
        }
    }

    async fn find_gpt(&mut self, scratch: &mut [u8; BLOCK]) -> Result<Volume, VolumeError> {
        self.read_block(1, scratch).await?;
        if &scratch[..8] != b"EFI PART" {
            return Err(VolumeError::NotExfat);
        }
        let Ok(entry_lba) = u32::try_from(le_u64_at(scratch, 72)) else {
            return Err(VolumeError::Geometry);
        };
        let entry_count = usize::try_from(le_u32_at(scratch, 80)).unwrap_or(GPT_ENTRIES);
        let entry_count = entry_count.min(GPT_ENTRIES);
        let entry_size = usize::try_from(le_u32_at(scratch, 84)).unwrap_or(0);
        if !(40..=128).contains(&entry_size) {
            return Err(VolumeError::NotExfat);
        }
        for index in 0..entry_count {
            let byte = index * entry_size;
            let sector = byte / BLOCK;
            let offset = byte % BLOCK;
            if offset + 40 > BLOCK {
                continue;
            }
            let Some(lba) = entry_lba.checked_add(sector as u32) else {
                continue;
            };
            if self.read_block(lba, scratch).await.is_err() {
                continue;
            }
            if scratch[offset..offset + 16].iter().all(|byte| *byte == 0) {
                continue;
            }
            let Ok(start) = u32::try_from(le_u64_at(scratch, offset + 32)) else {
                continue;
            };
            if start == 0 || self.read_block(start, scratch).await.is_err() {
                continue;
            }
            if is_exfat(scratch) {
                return volume_from_boot(scratch, start);
            }
        }
        Err(VolumeError::NotExfat)
    }

    async fn find_root_file(
        &mut self,
        name: &[u8],
        scratch: &mut [u8; BLOCK],
    ) -> Result<RootFile, VolumeError> {
        let volume = self.volume()?;
        let mut scan = FileScan {
            target: name,
            phase: ScanPhase::Searching,
        };
        let mut cluster = volume.root_cluster;
        let mut sectors = 0u32;
        loop {
            for sector in 0..volume.sectors_per_cluster {
                if sectors >= MAX_DIR_SECTORS {
                    return Err(VolumeError::DirectoryLimit);
                }
                let lba = volume.lba(cluster, sector)?;
                self.read_block(lba, scratch).await?;
                sectors = sectors.saturating_add(1);
                for index in 0..BLOCK / 32 {
                    let start = index * 32;
                    let mut entry = [0u8; 32];
                    entry.copy_from_slice(&scratch[start..start + 32]);
                    match scan.push(entry) {
                        ScanControl::Continue => {}
                        ScanControl::Found(file) => return Ok(file),
                        ScanControl::End => return Err(VolumeError::FileNotFound),
                        ScanControl::Broken => return Err(VolumeError::Fat),
                    }
                }
            }
            cluster = match self.fat_next(volume, cluster, scratch).await? {
                FatLink::Next(next) => next,
                FatLink::End => return Err(VolumeError::FileNotFound),
            };
        }
    }

    async fn stage_file_sector(
        &mut self,
        file: &RootFile,
        offset: u64,
        scratch: &mut [u8; BLOCK],
    ) -> Result<StagedSector, VolumeError> {
        if offset >= file.valid_bytes {
            return Ok(StagedSector {
                lba: 0,
                start: 0,
                len: 0,
            });
        }
        let volume = self.volume()?;
        let cluster_bytes = u64::from(volume.sectors_per_cluster) * BLOCK as u64;
        let steps = u32::try_from(offset / cluster_bytes).map_err(|_| VolumeError::Fat)?;
        if steps > volume.cluster_count {
            return Err(VolumeError::Fat);
        }
        let within = (offset % cluster_bytes) as u32;
        let cluster = self.cluster_at(volume, file, steps, scratch).await?;
        let sector = within / BLOCK as u32;
        let start = (within % BLOCK as u32) as usize;
        let lba = volume.lba(cluster, sector)?;
        self.read_block(lba, scratch).await?;
        let available = BLOCK - start;
        let remaining = usize::try_from(file.valid_bytes - offset).unwrap_or(available);
        Ok(StagedSector {
            lba,
            start,
            len: available.min(remaining),
        })
    }

    async fn cluster_at(
        &mut self,
        volume: Volume,
        file: &RootFile,
        steps: u32,
        scratch: &mut [u8; BLOCK],
    ) -> Result<u32, VolumeError> {
        if file.valid_bytes > 0 && file.first_cluster < 2 {
            return Err(VolumeError::Fat);
        }
        match file.chain {
            ClusterChain::Contiguous => file
                .first_cluster
                .checked_add(steps)
                .ok_or(VolumeError::Fat),
            ClusterChain::Fat => {
                let mut cluster = file.first_cluster;
                for _ in 0..steps {
                    cluster = match self.fat_next(volume, cluster, scratch).await? {
                        FatLink::Next(next) => next,
                        FatLink::End => return Err(VolumeError::Fat),
                    };
                }
                Ok(cluster)
            }
        }
    }

    async fn fat_next(
        &mut self,
        volume: Volume,
        cluster: u32,
        scratch: &mut [u8; BLOCK],
    ) -> Result<FatLink, VolumeError> {
        let byte_index = cluster.checked_mul(4).ok_or(VolumeError::Fat)?;
        let sector_index = byte_index / BLOCK as u32;
        let offset = (byte_index % BLOCK as u32) as usize;
        let lba = volume
            .fat_lba
            .checked_add(sector_index)
            .ok_or(VolumeError::Fat)?;
        self.read_block(lba, scratch).await?;
        let entry = le_u32_at(scratch, offset);
        let link = classify_fat(entry)?;
        if let FatLink::Next(next) = link {
            if next == cluster || next > volume.cluster_count.saturating_add(1) {
                return Err(VolumeError::Fat);
            }
        }
        Ok(link)
    }

    fn addressing(&self) -> Result<Addressing, CardError> {
        match self.state {
            CardState::Ready(addressing) | CardState::Mounted { addressing, .. } => Ok(addressing),
            CardState::Down => Err(CardError::NotReady),
        }
    }

    fn volume(&self) -> Result<Volume, VolumeError> {
        match self.state {
            CardState::Mounted { volume, .. } => Ok(volume),
            _ => Err(VolumeError::NotMounted),
        }
    }

    async fn read_block(&mut self, lba: u32, block: &mut [u8; BLOCK]) -> Result<(), CardError> {
        let result = self.read_block_selected(lba, block).await;
        self.abandon(result).await
    }

    async fn read_block_selected(
        &mut self,
        lba: u32,
        block: &mut [u8; BLOCK],
    ) -> Result<(), CardError> {
        let status = self
            .command_while_selected(READ_SINGLE, self.data_arg(lba)?, ReadyWait::UntilIdle)
            .await?;
        if status != 0 {
            return Err(CardError::Status(status));
        }
        self.wait_token(lba).await?;
        self.read_bytes(block).await?;
        let _crc = self.read_n::<2>().await?;
        Ok(())
    }

    async fn write_block(&mut self, lba: u32, block: &[u8; BLOCK]) -> Result<(), CardError> {
        let result = self.write_block_selected(lba, block).await;
        self.abandon(result).await
    }

    async fn write_block_selected(
        &mut self,
        lba: u32,
        block: &[u8; BLOCK],
    ) -> Result<(), CardError> {
        let status = self
            .command_while_selected(WRITE_SINGLE, self.data_arg(lba)?, ReadyWait::UntilIdle)
            .await?;
        if status != 0 {
            return Err(CardError::Status(status));
        }
        self.clock().await?;
        self.write_ram(&[DATA_TOKEN]).await?;
        self.write_ram(block).await?;
        self.write_ram(&[0xFF, 0xFF]).await?;
        let response = self.clock().await?;
        if response & 0x1F != DATA_ACCEPTED {
            return Err(CardError::WriteRejected);
        }
        self.wait_not_busy().await
    }

    fn data_arg(&self, lba: u32) -> Result<u32, CardError> {
        match self.addressing()? {
            Addressing::Block => Ok(lba),
            Addressing::Byte => lba.checked_mul(BLOCK as u32).ok_or(CardError::Address),
        }
    }

    async fn command_r1(&mut self, index: u8, arg: u32, ready: ReadyWait) -> Result<u8, CardError> {
        let status = self.command_while_selected(index, arg, ready).await;
        self.abandon(status).await
    }

    async fn command_tail(
        &mut self,
        index: u8,
        arg: u32,
        ready: ReadyWait,
    ) -> Result<(u8, [u8; 4]), CardError> {
        let selected = self.command_while_selected(index, arg, ready).await;
        let paired = match selected {
            Ok(status) => match self.read_n::<4>().await {
                Ok(tail) => Ok((status, tail)),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        self.abandon(paired).await
    }

    async fn command_while_selected(
        &mut self,
        index: u8,
        arg: u32,
        ready: ReadyWait,
    ) -> Result<u8, CardError> {
        self.cs.set_high();
        self.clock().await?;
        self.cs.set_low();
        self.wait_ready(ready).await?;
        let mut frame = [0u8; 6];
        frame[0] = 0x40 | index;
        frame[1..5].copy_from_slice(&arg.to_be_bytes());
        frame[5] = crc7(&frame[..5]);
        self.write_ram(&frame).await?;
        self.read_r1(index).await
    }

    async fn wait_token(&mut self, lba: u32) -> Result<(), CardError> {
        let deadline = Instant::now() + DATA_WAIT;
        loop {
            let byte = self.clock().await?;
            if byte == DATA_TOKEN {
                return Ok(());
            }
            if byte != 0xFF {
                return Err(CardError::DataRejected);
            }
            if Instant::now() >= deadline {
                return Err(CardError::DataTimeout(lba));
            }
        }
    }

    async fn wait_not_busy(&mut self) -> Result<(), CardError> {
        let deadline = Instant::now() + BUSY_WAIT;
        loop {
            if self.clock().await? == 0xFF {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(CardError::Busy);
            }
        }
    }

    async fn wait_ready(&mut self, ready: ReadyWait) -> Result<(), CardError> {
        match ready {
            ReadyWait::Immediate => Ok(()),
            ReadyWait::Brief => {
                for _ in 0..16 {
                    if self.clock().await? == 0xFF {
                        return Ok(());
                    }
                }
                Ok(())
            }
            ReadyWait::UntilIdle => {
                let deadline = Instant::now() + BUSY_WAIT;
                loop {
                    if self.clock().await? == 0xFF {
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        return Err(CardError::Busy);
                    }
                }
            }
        }
    }

    async fn read_r1(&mut self, command: u8) -> Result<u8, CardError> {
        let mut sample = 0u32;
        for index in 0..16 {
            let byte = self.clock().await?;
            if index < 4 {
                sample = (sample << 8) | u32::from(byte);
            }
            if byte != 0xFF {
                return Ok(byte);
            }
        }
        Err(CardError::ResponseTimeout { command, sample })
    }

    async fn clock(&mut self) -> Result<u8, CardError> {
        embassy_futures::yield_now().await;
        Ok(self.transfer(0xFF))
    }

    async fn read_n<const N: usize>(&mut self) -> Result<[u8; N], CardError> {
        let mut buffer = [0xFF; N];
        self.read_bytes(&mut buffer).await?;
        Ok(buffer)
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> Result<(), CardError> {
        for byte in buffer {
            *byte = self.clock().await?;
        }
        Ok(())
    }

    async fn write_ram(&mut self, data: &[u8]) -> Result<(), CardError> {
        for &byte in data {
            embassy_futures::yield_now().await;
            let _ = self.transfer(byte);
        }
        Ok(())
    }

    fn probe_hardware_spi(&mut self) {
        let clocks = [0xFFu8; 10];
        let mut ignored = [0u8; 10];
        self.cs.set_high();
        let primed = self.hardware_spi_exchange(&clocks, &mut ignored);
        self.cs.set_low();
        let mut frame = [0xFFu8; 14];
        frame[..6].copy_from_slice(&[0x40, 0, 0, 0, 0, CMD0_CRC]);
        let mut received = [0u8; 14];
        let finished = self.hardware_spi_exchange(&frame, &mut received);
        self.cs.set_high();
        let _ = self.hardware_spi_exchange(&clocks[..1], &mut ignored[..1]);
        if !primed || !finished {
            write_console(format_args!("sd: spim timeout"));
        }
        let mut hex = heapless::String::<28>::new();
        for byte in received {
            let _ = write!(hex, "{byte:02x}");
        }
        write_console(format_args!("sd: spim {hex}"));
    }

    fn hardware_spi_exchange(&mut self, tx: &[u8], rx: &mut [u8]) -> bool {
        use embassy_nrf::pac::shared::regs::Psel;
        use embassy_nrf::pac::spim::vals::{Cpha, Cpol, Enable, Frequency, Order};

        let spi = embassy_nrf::pac::SPIM2;
        let connected = |pin: u32| Psel(pin);
        let disconnected = Psel(1 << 31);
        spi.psel().sck().write_value(connected(6));
        spi.psel().mosi().write_value(connected(33));
        spi.psel().miso().write_value(connected(26));
        spi.enable().write(|w| w.set_enable(Enable::ENABLED));
        spi.config().write(|w| {
            w.set_order(Order::MSB_FIRST);
            w.set_cpol(Cpol::ACTIVE_HIGH);
            w.set_cpha(Cpha::LEADING);
        });
        spi.frequency().write(|w| w.set_frequency(Frequency::K250));
        spi.orc().write(|w| w.set_orc(0xFF));
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        spi.dma().rx().ptr().write_value(rx.as_mut_ptr() as u32);
        spi.dma()
            .rx()
            .maxcnt()
            .write(|w| w.set_maxcnt(rx.len() as _));
        spi.dma().tx().ptr().write_value(tx.as_ptr() as u32);
        spi.dma()
            .tx()
            .maxcnt()
            .write(|w| w.set_maxcnt(tx.len() as _));
        spi.events_end().write_value(0);
        spi.tasks_start().write_value(1);
        let mut spins = 0u32;
        while spi.events_end().read() == 0 {
            spins = spins.saturating_add(1);
            if spins > 2_000_000 {
                break;
            }
        }
        let finished = spi.events_end().read() != 0;
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        spi.psel().sck().write_value(disconnected);
        spi.psel().mosi().write_value(disconnected);
        spi.psel().miso().write_value(disconnected);
        spi.enable().write(|w| w.set_enable(Enable::DISABLED));
        self.sck.set_low();
        self.mosi.set_high();
        finished
    }

    fn transfer(&mut self, out: u8) -> u8 {
        let mut input = 0u8;
        for bit in (0..8).rev() {
            if out & (1 << bit) == 0 {
                self.mosi.set_low();
            } else {
                self.mosi.set_high();
            }
            cortex_m::asm::delay(HALF_BIT_CYCLES);
            self.sck.set_high();
            cortex_m::asm::delay(HALF_BIT_CYCLES);
            input <<= 1;
            if self.miso.is_high() {
                input |= 1;
            }
            self.sck.set_low();
        }
        self.mosi.set_high();
        input
    }

    async fn write_clocks(&mut self, count: usize) -> Result<(), CardError> {
        let clocks = [0xFFu8; 16];
        let mut sent = 0;
        while sent < count {
            let take = (count - sent).min(clocks.len());
            self.write_ram(&clocks[..take]).await?;
            sent += take;
        }
        Ok(())
    }

    async fn abandon<T>(&mut self, result: Result<T, CardError>) -> Result<T, CardError> {
        self.cs.set_high();
        let released = self.clock().await;
        match (result, released) {
            (Ok(value), Ok(_)) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }
}

impl Volume {
    fn lba(&self, cluster: u32, sector: u32) -> Result<u32, VolumeError> {
        if cluster < 2
            || cluster > self.cluster_count.saturating_add(1)
            || sector >= self.sectors_per_cluster
        {
            return Err(VolumeError::Fat);
        }
        let sectors = (cluster - 2)
            .checked_mul(self.sectors_per_cluster)
            .ok_or(VolumeError::Fat)?;
        self.heap_lba
            .checked_add(sectors)
            .and_then(|lba| lba.checked_add(sector))
            .ok_or(VolumeError::Fat)
    }
}

impl FileScan<'_> {
    fn push(&mut self, entry: [u8; 32]) -> ScanControl {
        if matches!(self.phase, ScanPhase::InFile(_)) {
            return self.push_secondary(entry);
        }
        if entry[0] == 0 {
            return ScanControl::End;
        }
        if entry[0] != FILE_ENTRY {
            return ScanControl::Continue;
        }
        let remaining = entry[1];
        let partial = PartialFile {
            remaining,
            attributes: u16::from_le_bytes([entry[4], entry[5]]),
            name: [0; MAX_NAME],
            stored: 0,
            declared: 0,
            stream: StreamSlot::Absent,
        };
        if remaining == 0 {
            return ScanControl::Continue;
        }
        self.phase = ScanPhase::InFile(partial);
        ScanControl::Continue
    }

    fn push_secondary(&mut self, entry: [u8; 32]) -> ScanControl {
        if entry[0] == 0 {
            return ScanControl::End;
        }
        let ScanPhase::InFile(partial) = &mut self.phase else {
            return ScanControl::Continue;
        };
        partial.remaining = partial.remaining.saturating_sub(1);
        match entry[0] {
            STREAM_ENTRY => partial.note_stream(entry),
            NAME_ENTRY => partial.note_name(entry),
            _ => {}
        }
        if partial.remaining > 0 {
            return ScanControl::Continue;
        }
        let target = self.target;
        let finished = match &self.phase {
            ScanPhase::InFile(partial) => partial.finish(target),
            ScanPhase::Searching => ScanControl::Continue,
        };
        self.phase = ScanPhase::Searching;
        finished
    }
}

impl PartialFile {
    fn note_stream(&mut self, entry: [u8; 32]) {
        let chain = if entry[1] & NO_FAT_CHAIN == 0 {
            ClusterChain::Fat
        } else {
            ClusterChain::Contiguous
        };
        self.declared = usize::from(entry[3]);
        self.stream = StreamSlot::Present(StreamInfo {
            first_cluster: le_u32_at(&entry, 20),
            valid_bytes: le_u64_at(&entry, 8),
            chain,
        });
    }

    fn note_name(&mut self, entry: [u8; 32]) {
        let limit = if self.declared == 0 {
            MAX_NAME
        } else {
            self.declared.min(MAX_NAME)
        };
        for pair in entry[2..].chunks_exact(2) {
            if self.stored >= limit {
                break;
            }
            self.name[self.stored] = u16::from_le_bytes([pair[0], pair[1]]);
            self.stored += 1;
        }
    }

    fn finish(&self, target: &[u8]) -> ScanControl {
        if self.attributes & DIRECTORY != 0 {
            return ScanControl::Continue;
        }
        let StreamSlot::Present(stream) = &self.stream else {
            return ScanControl::Continue;
        };
        if self.declared != target.len() || self.stored < self.declared {
            return ScanControl::Continue;
        }
        if !ascii_case_eq(&self.name[..self.declared], target) {
            return ScanControl::Continue;
        }
        if stream.valid_bytes > 0 && stream.first_cluster < 2 {
            return ScanControl::Broken;
        }
        ScanControl::Found(RootFile {
            first_cluster: stream.first_cluster,
            valid_bytes: stream.valid_bytes,
            chain: stream.chain,
        })
    }
}

fn accept_root_name(name: &[u8]) -> Result<(), VolumeError> {
    if name.is_empty()
        || name.len() > MAX_NAME
        || name.iter().any(|byte| matches!(byte, 0 | b'/' | b'\\'))
    {
        return Err(VolumeError::NameRejected);
    }
    Ok(())
}

fn ascii_case_eq(stored: &[u16], target: &[u8]) -> bool {
    if stored.len() != target.len() {
        return false;
    }
    stored
        .iter()
        .zip(target.iter())
        .all(|(&unit, &byte)| unit <= 0x7F && fold_ascii(unit as u8) == fold_ascii(byte))
}

fn fold_ascii(byte: u8) -> u8 {
    match byte {
        b'A'..=b'Z' => byte + 32,
        other => other,
    }
}

fn classify_fat(entry: u32) -> Result<FatLink, VolumeError> {
    if entry >= FAT_END {
        return Ok(FatLink::End);
    }
    if entry < 2 || entry == 0xFFFF_FFF7 {
        return Err(VolumeError::Fat);
    }
    Ok(FatLink::Next(entry))
}

fn is_exfat(block: &[u8; BLOCK]) -> bool {
    &block[3..11] == EXFAT_MAGIC && has_boot_signature(block)
}

fn has_boot_signature(block: &[u8; BLOCK]) -> bool {
    block[510] == 0x55 && block[511] == 0xAA
}

fn mbr_partition(block: &[u8; BLOCK], index: usize) -> PartitionKind {
    let base = MBR_BASE + index * 16;
    let kind = block[base + 4];
    let lba = le_u32_at(block, base + 8);
    if kind == 0 || lba == 0 {
        PartitionKind::Empty
    } else if kind == PROTECTIVE_MBR {
        PartitionKind::Protective
    } else {
        PartitionKind::Probe(lba)
    }
}

fn volume_from_boot(block: &[u8; BLOCK], volume_lba: u32) -> Result<Volume, VolumeError> {
    if !is_exfat(block) || block[108] != 9 || block[109] > 16 {
        return Err(VolumeError::Geometry);
    }
    let sectors_per_cluster = 1u32 << block[109];
    let fat_lba = volume_lba
        .checked_add(le_u32_at(block, 80))
        .ok_or(VolumeError::Geometry)?;
    let fat_length = le_u32_at(block, 84);
    let fat_count = block[110].clamp(1, 2);
    let heap_lba = volume_lba
        .checked_add(le_u32_at(block, 88))
        .ok_or(VolumeError::Geometry)?;
    let cluster_count = le_u32_at(block, 92);
    let root_cluster = le_u32_at(block, 96);
    if cluster_count == 0 || root_cluster < 2 || fat_length == 0 {
        return Err(VolumeError::Geometry);
    }
    Ok(Volume {
        volume_lba,
        fat_lba,
        fat_length,
        fat_count,
        heap_lba,
        sectors_per_cluster,
        cluster_count,
        root_cluster,
    })
}

const fn name_hash(name: &[u8]) -> u16 {
    let mut hash = 0u16;
    let mut index = 0;
    while index < name.len() {
        let byte = name[index];
        let upper = if byte >= b'a' && byte <= b'z' {
            byte - 32
        } else {
            byte
        };
        hash = hash.rotate_right(1).wrapping_add(upper as u16);
        hash = hash.rotate_right(1);
        index += 1;
    }
    hash
}

const fn entry_checksum(bytes: &[u8]) -> u16 {
    let mut hash = 0u16;
    let mut index = 0;
    while index < bytes.len() {
        if index != 2 && index != 3 {
            hash = hash.rotate_right(1).wrapping_add(bytes[index] as u16);
        }
        index += 1;
    }
    hash
}

const fn directory_set(name: &[u8], cluster: u32, bytes: u64) -> [u8; 96] {
    let mut set = [0u8; 96];
    set[0] = FILE_ENTRY;
    set[1] = 2;
    set[4] = ARCHIVE;
    set[32] = STREAM_ENTRY;
    set[33] = ALLOCATION_POSSIBLE | NO_FAT_CHAIN;
    set[35] = name.len() as u8;
    let hash = name_hash(name).to_le_bytes();
    set[36] = hash[0];
    set[37] = hash[1];
    let length = bytes.to_le_bytes();
    let mut index = 0;
    while index < length.len() {
        set[40 + index] = length[index];
        set[56 + index] = length[index];
        index += 1;
    }
    let cluster_bytes = cluster.to_le_bytes();
    index = 0;
    while index < cluster_bytes.len() {
        set[52 + index] = cluster_bytes[index];
        index += 1;
    }
    set[64] = NAME_ENTRY;
    index = 0;
    while index < name.len() && index < NAME_ENTRY_CHARS {
        let unit = (name[index] as u16).to_le_bytes();
        set[66 + index * 2] = unit[0];
        set[67 + index * 2] = unit[1];
        index += 1;
    }
    let sum = entry_checksum(&set).to_le_bytes();
    set[2] = sum[0];
    set[3] = sum[1];
    set
}

const _: () = assert!(name_hash(WRITE_NAME) == 0xAC4F);
const SAMPLE_SET: [u8; 96] = directory_set(WRITE_NAME, 2, WRITE_BODY.len() as u64);
const _: () = assert!(SAMPLE_SET[2] == 0x52 && SAMPLE_SET[3] == 0xB0);

fn directory_slot(
    volume: &Volume,
    existing: &ExistingFile,
    entry: usize,
) -> Result<(u32, usize), VolumeError> {
    let slot = existing.set_index + entry;
    let sector = existing
        .set_sector
        .checked_add((slot / (BLOCK / 32)) as u32)
        .ok_or(VolumeError::Geometry)?;
    if sector >= volume.sectors_per_cluster {
        return Err(VolumeError::Geometry);
    }
    let lba = volume.lba(existing.set_cluster, sector)?;
    Ok((lba, (slot % (BLOCK / 32)) * 32))
}

fn write_firmware_name(set: &mut [u8], name: &[u8]) {
    let entries = set.len() / 32;
    let mut written = 0;
    let mut index = 1;
    while index < entries {
        let entry = &mut set[index * 32..index * 32 + 32];
        if entry[0] == STREAM_ENTRY {
            entry[3] = name.len() as u8;
            let hash = name_hash(name).to_le_bytes();
            entry[4] = hash[0];
            entry[5] = hash[1];
        } else if entry[0] == NAME_ENTRY {
            let mut char_index = 0;
            while char_index < NAME_ENTRY_CHARS {
                let pos = 2 + char_index * 2;
                if written < name.len() {
                    let unit = (name[written] as u16).to_le_bytes();
                    entry[pos] = unit[0];
                    entry[pos + 1] = unit[1];
                    written += 1;
                } else {
                    entry[pos] = 0;
                    entry[pos + 1] = 0;
                }
                char_index += 1;
            }
        }
        index += 1;
    }
}

fn le_u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn le_u64_at(bytes: &[u8], offset: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(raw)
}

const fn crc7(message: &[u8]) -> u8 {
    let mut crc = 0u8;
    let mut index = 0;
    while index < message.len() {
        crc ^= message[index];
        let mut bit = 0;
        while bit < 8 {
            if crc & 0x80 != 0 {
                crc = crc.wrapping_shl(1) ^ 0x12;
            } else {
                crc = crc.wrapping_shl(1);
            }
            bit += 1;
        }
        index += 1;
    }
    crc | 1
}

fn user_button_held() -> bool {
    // P1.10, active-low. The button task owns the pin; the input register is still readable.
    let value = unsafe { core::ptr::read_volatile(0x5000_0810u32 as *const u32) };
    value & (1 << 10) == 0
}

fn firmware_vector_ok(header: &[u8]) -> bool {
    let sp = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let reset = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let stack_ok = (0x2000_0000..=0x2004_0000).contains(&sp);
    let target = reset & !1;
    stack_ok && reset & 1 == 1 && (APP_BASE..APP_END).contains(&target)
}

fn log_dir_types(sector: &[u8]) {
    let mut types = heapless::String::<32>::new();
    for index in 0..BLOCK / 32 {
        let _ = write!(types, "{:02x}", sector[index * 32]);
    }
    write_console(format_args!("sd: dir {types}"));
}

fn write_console(args: core::fmt::Arguments<'_>) {
    #[cfg(feature = "usb-debug-log")]
    log::info!("{args}");
    #[cfg(not(feature = "usb-debug-log"))]
    let _ = args;
}

macro_rules! sd_log {
    ($($arg:tt)*) => {
        write_console(format_args!($($arg)*))
    };
}

fn block_buffer() -> &'static mut [u8; BLOCK] {
    static BLOCK_BUFFER: StaticCell<[u8; BLOCK]> = StaticCell::new();
    BLOCK_BUFFER.init([0; BLOCK])
}

pub(crate) async fn publish_test_file(card: SdCard) {
    #[cfg(feature = "image-battery-sense")]
    {
        let _card = card;
    }
    #[cfg(all(not(feature = "image-battery-sense"), feature = "image-spruce-101"))]
    {
        let _card = card;
        Timer::after(SETTLE).await;
        sd_log!("{BOOT_ID}");
    }
    #[cfg(all(
        not(feature = "image-battery-sense"),
        not(feature = "image-spruce-101")
    ))]
    alder_boot(card).await;
}

// Card read/write proof, kept out of the boot path. It read /Test.txt, then wrote and
// read back /write.txt, before the firmware.bin check.
//
// const ROOT_FILE: &[u8] = b"Test.txt";
// const PUBLISH_LIMIT: u64 = 2048;
// const LINE_BYTES: usize = 96;
// const LINE_PAUSE: Duration = Duration::from_millis(20);
//
// sd_log!("sd: /Test.txt in 5s");
// match card.open_root_file(ROOT_FILE, scratch).await {
//     Ok(file) => {
//         sd_log!(
//             "sd: {} lba {} /Test.txt {} bytes",
//             card.link_label(),
//             card.volume_lba(),
//             file.valid_bytes
//         );
//         if let Err(error) = publish_contents(&mut card, &file, scratch).await {
//             sd_log!("sd: {error}");
//         }
//         sd_log!("sd: /Test.txt end");
//     }
//     Err(error) => {
//         sd_log!("sd: {error}");
//         if matches!(error, VolumeError::NotExfat) && card.read_block(0, scratch).await.is_ok() {
//             log_sector_prefix(scratch);
//         }
//     }
// }
// sd_log!("sd: writing /write.txt");
// match card.store_root_file(WRITE_NAME, WRITE_BODY, scratch).await {
//     Ok(()) => {
//         sd_log!("sd: wrote /write.txt {} bytes", WRITE_BODY.len());
//         match card.open_root_file(WRITE_NAME, scratch).await {
//             Ok(file) => {
//                 sd_log!("sd: read /write.txt {} bytes", file.valid_bytes);
//                 if let Err(error) = publish_contents(&mut card, &file, scratch).await {
//                     sd_log!("sd: {error}");
//                 }
//                 sd_log!("sd: /write.txt end");
//             }
//             Err(error) => sd_log!("sd: read /write.txt {error}"),
//         }
//     }
//     Err(error) => sd_log!("sd: write /write.txt {error}"),
// }
//
// async fn publish_contents(
//     card: &mut SdCard,
//     file: &RootFile,
//     scratch: &mut [u8; BLOCK],
// ) -> Result<(), VolumeError> {
//     let mut shown = 0u64;
//     while shown < file.valid_bytes && shown < PUBLISH_LIMIT {
//         let len = card.load_file_slice(file, shown, scratch).await?;
//         if len == 0 {
//             return Err(VolumeError::Fat);
//         }
//         let room = usize::try_from(PUBLISH_LIMIT - shown).unwrap_or(len);
//         let take = len.min(room);
//         publish_text(&scratch[..take]).await;
//         shown += take as u64;
//     }
//     if shown < file.valid_bytes {
//         sd_log!("sd: truncated");
//         Timer::after(LINE_PAUSE).await;
//     }
//     Ok(())
// }
//
// async fn publish_text(bytes: &[u8]) {
//     let mut line = heapless::String::<LINE_BYTES>::new();
//     for &byte in bytes {
//         if byte == b'\n' {
//             sd_log!("{line}");
//             line.clear();
//             Timer::after(LINE_PAUSE).await;
//             continue;
//         }
//         if byte == b'\r' {
//             continue;
//         }
//         let ch = if byte.is_ascii_graphic() || byte == b' ' || byte == b'\t' {
//             byte as char
//         } else {
//             '.'
//         };
//         if line.push(ch).is_err() {
//             sd_log!("{line}");
//             line.clear();
//             Timer::after(LINE_PAUSE).await;
//             let _ = line.push(ch);
//         }
//     }
//     if !line.is_empty() {
//         sd_log!("{line}");
//         Timer::after(LINE_PAUSE).await;
//     }
// }
//
// fn log_sector_prefix(block: &[u8]) {
//     let mut hex = heapless::String::<32>::new();
//     for byte in block.iter().take(8) {
//         let _ = write!(hex, "{byte:02x}");
//     }
//     sd_log!("sd: sector0 {hex}");
// }

#[cfg(all(
    not(feature = "image-battery-sense"),
    not(feature = "image-spruce-101")
))]
async fn alder_boot(mut card: SdCard) {
    sd_log!("sd: boot {BOOT_ID}");
    Timer::after(SETTLE).await;
    let scratch = block_buffer();
    match card.prepare_firmware(scratch).await {
        Ok(image) => {
            sd_log!("sd: firmware.bin {} bytes", image.bytes);
            if !user_button_held() {
                sd_log!("sd: hold button to flash");
            } else {
                sd_log!("sd: flashing");
                match card.rename_firmware(scratch).await {
                    Ok(()) => sd_log!("sd: renamed firmware.old"),
                    Err(error) => {
                        sd_log!("sd: rename {error}");
                        sd_log!("sd: boot {BOOT_ID}");
                        return;
                    }
                }
                Timer::after(Duration::from_millis(300)).await;
                if super::update::commit(&image).is_err() {
                    sd_log!("sd: flash start failed");
                }
            }
        }
        Err(VolumeError::FileNotFound) => sd_log!("sd: no firmware.bin"),
        Err(error) => sd_log!("sd: firmware.bin {error}"),
    }
    sd_log!("sd: boot {BOOT_ID}");
}

#[embassy_executor::task]
pub(crate) async fn publish_root_test_file(card: SdCard) {
    publish_test_file(card).await;
}
