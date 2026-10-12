//! Opening a volume and accessing MFT records.
//!
//! # Bootstrapping
//!
//! 1. The boot sector is taken from sector 0, or from the backup in the last
//!    sector of the volume when sector 0 does not validate (or first, when
//!    [`NtfsOptions::use_backup_boot_sector`] is set).
//! 2. MFT record 0 (`$MFT` itself) is read at the boot sector's MFT LCN; if
//!    it is unreadable or invalid, the copy in `$MFTMirr` is used. Its
//!    unnamed `$DATA` runs locate every other record (the MFT is often
//!    fragmented); an `$ATTRIBUTE_LIST` in record 0 is followed so a heavily
//!    fragmented MFT whose runs spill into extension records is mapped
//!    completely.
//! 3. If neither copy of record 0 is usable, the MFT is assumed to be one
//!    contiguous run starting at the MFT LCN and its length is estimated by
//!    probing for `FILE` signatures. Records in later fragments are lost.
//! 4. `$Volume` (record 3) supplies the label and dirty flag, `$Bitmap`
//!    (record 6) the cluster allocation bitmap.
//!
//! Every problem along the way becomes a [`VolumeInfo`] warning; only an
//! unusable boot sector or an MFT that cannot be located at all is fatal.
//!
//! # Records
//!
//! Record `n` is read through the MFT's extent map (never linearly), fixed
//! up and decoded. A record with any unreadable byte is reported as
//! unreadable rather than parsed with zeros. Extension records are folded
//! into their base record by following `$ATTRIBUTE_LIST` entries: each
//! extension record is loaded at most once per fold (cycle protection), its
//! base reference must point back at the base record, and at most
//! [`MAX_EXTENSION_RECORDS`] are followed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use diskrescue_core::fs::{FsKind, VolumeInfo};
use diskrescue_core::source::{ReadError, ReadErrorKind};
use diskrescue_core::{Error, Range, SharedSource};

use crate::attr::{
    mref, parse_attribute_list, types, Attribute, FileName, StandardInformation, VolumeInformation,
    FILE_ATTR_ENCRYPTED,
};
use crate::bitmap::ClusterBitmap;
use crate::boot::BootSector;
use crate::io::Disk;
use crate::record::{MftRecord, RecordError};
use crate::runs::Run;
use crate::stream::{
    assemble, read_all, read_plain, Annotations, Extent, ExtentKind, NonResStream, Stream,
    StreamData,
};
use crate::{NtfsOptions, Result};

/// Upper bound on extension records followed for one base record.
pub const MAX_EXTENSION_RECORDS: usize = 4096;
/// Upper bound on the size of an $ATTRIBUTE_LIST value.
const MAX_ATTRIBUTE_LIST: u64 = 4 << 20;
/// Consecutive non-FILE records that end the MFT length probe.
const PROBE_GIVE_UP: u64 = 1024;

/// Outcome of reading one record.
pub(crate) enum RecordRead {
    Ok(MftRecord),
    Unreadable,
    Invalid(RecordError),
}

/// A parsed NTFS volume. Implements [`diskrescue_core::fs::FileSystem`].
pub struct NtfsFs {
    pub(crate) src: SharedSource,
    pub(crate) disk: Disk,
    pub(crate) boot: BootSector,
    pub(crate) opts: NtfsOptions,
    pub(crate) info: VolumeInfo,
    pub(crate) cluster: u64,
    pub(crate) rsize: u64,
    pub(crate) total_clusters: u64,
    pub(crate) mft: NonResStream,
    pub(crate) record_count: u64,
    pub(crate) bitmap: Option<ClusterBitmap>,
    /// Extension records by base record number (built by the first full
    /// MFT scan; see [`NtfsFs::extension_map`]).
    pub(crate) ext_map: OnceLock<HashMap<u64, Vec<u64>>>,
}

/// Result of folding a base record's extension records.
pub(crate) struct Folded {
    pub attrs: Vec<Attribute>,
    pub warnings: Vec<String>,
    /// The record has an $ATTRIBUTE_LIST.
    pub has_list: bool,
    /// The list was unreadable or named records that could not be used.
    pub list_damaged: bool,
    /// Extension records already examined.
    pub visited: HashSet<u64>,
    /// `(record, offset)` of every attribute taken.
    pub taken: HashSet<(u64, usize)>,
    /// Some attributes came from extension records found by scanning the
    /// MFT rather than from the attribute list (a guess).
    pub guessed: bool,
    /// An extension record that supplied attributes had a torn write.
    pub torn: bool,
}

/// Everything the walker and reader need about one base record.
pub(crate) struct FileInfo {
    pub record: MftRecord,
    pub attrs: Vec<Attribute>,
    /// Attribute pieces were recovered without the attribute list.
    pub guessed: bool,
    /// The base record or an extension record it uses had a torn write:
    /// some of its bytes may be stale.
    pub torn: bool,
    pub warnings: Vec<String>,
    pub si: Option<StandardInformation>,
    pub names: Vec<FileName>,
    pub data: Option<Stream>,
    pub ads: Vec<Stream>,
    pub reparse_tag: Option<u32>,
    pub encrypted: bool,
}

impl NtfsFs {
    pub(crate) fn open_impl(src: SharedSource, opts: &NtfsOptions) -> Result<NtfsFs> {
        let disk = Disk::new(src.clone());
        let mut warnings = Vec::new();
        let boot = select_boot(&disk, opts, &mut warnings)?;
        for w in &boot.warnings {
            warnings.push(format!("boot sector: {w}"));
        }
        let cluster = boot.cluster_size as u64;
        let rsize = boot.mft_record_size as u64;
        let total_clusters = boot.total_clusters();
        let vol_size = boot.volume_size();
        if vol_size > disk.size() {
            warnings.push(format!(
                "source is {} bytes but the volume claims {} bytes; data past the end is unreadable",
                disk.size(),
                vol_size
            ));
        }
        let mft = bootstrap_mft(&disk, &boot, opts, &mut warnings)?;
        let mut record_count = mft.data_size / rsize;
        // Records past the last real cluster of the MFT cannot be read; a
        // forged size must not make the walk iterate over them.
        let mapped_end = mft
            .extents
            .iter()
            .rev()
            .find(|e| matches!(e.kind, ExtentKind::Real(_)))
            .map(|e| e.end().saturating_mul(cluster) / rsize)
            .unwrap_or(0);
        if record_count > mapped_end {
            warnings.push(format!(
                "$MFT claims {record_count} records but only {mapped_end} are mapped to clusters"
            ));
            record_count = mapped_end;
        }
        if record_count > opts.max_records {
            warnings.push(format!(
                "MFT holds {record_count} records; only the first {} are examined (max_records)",
                opts.max_records
            ));
            record_count = opts.max_records;
        }
        let mut fs = NtfsFs {
            src,
            disk,
            info: VolumeInfo {
                kind: FsKind::Ntfs,
                label: None,
                cluster_size: boot.cluster_size,
                total_size: vol_size,
                data_start: 0,
                serial: Some(boot.serial_string()),
                warnings: Vec::new(),
            },
            boot,
            opts: opts.clone(),
            cluster,
            rsize,
            total_clusters,
            mft,
            record_count,
            bitmap: None,
            ext_map: OnceLock::new(),
        };
        // $Volume: label and dirty flag.
        match fs.file_info(3) {
            Ok(Some(v)) => {
                let (attrs, _) = v.record.own_attributes(Some(total_clusters));
                if let Some(name) = attrs
                    .iter()
                    .find(|a| a.type_code == types::VOLUME_NAME)
                    .and_then(|a| a.value())
                {
                    let label = diskrescue_core::util::bytes::utf16le_exact(name, name.len() / 2);
                    if !label.is_empty() {
                        fs.info.label = Some(label);
                    }
                }
                if let Some(vi) = attrs
                    .iter()
                    .find(|a| a.type_code == types::VOLUME_INFORMATION)
                    .and_then(|a| a.value())
                    .and_then(VolumeInformation::parse)
                {
                    if vi.is_dirty() {
                        warnings.push(
                            "volume is marked dirty (unclean shutdown): expect torn metadata"
                                .into(),
                        );
                    }
                }
            }
            Ok(None) => warnings.push("$Volume (record 3) is unreadable or invalid".into()),
            Err(e) if is_fatal(&e) => return Err(e),
            Err(e) => warnings.push(format!("$Volume (record 3): {e}")),
        }
        // $Bitmap: cluster allocation.
        match fs.file_info(6) {
            Ok(Some(b)) => match b.data {
                Some(stream) => {
                    let need = total_clusters.div_ceil(8);
                    if stream.size() < need {
                        warnings.push(format!(
                            "$Bitmap holds {} bytes but the volume needs {need}; the rest is treated as unknown",
                            stream.size()
                        ));
                    }
                    if let Some(n) = stream.non_resident() {
                        if !n.complete {
                            warnings.push("$Bitmap is only partly mapped".into());
                        }
                    }
                    fs.bitmap = Some(ClusterBitmap::new(stream, total_clusters, cluster));
                }
                None => warnings.push("$Bitmap has no data attribute".into()),
            },
            Ok(None) => warnings.push(
                "$Bitmap (record 6) is unreadable: free space and deleted-file overwrite checks are unavailable"
                    .into(),
            ),
            Err(e) if is_fatal(&e) => return Err(e),
            Err(e) => warnings.push(format!("$Bitmap (record 6): {e}")),
        }
        if fs.bitmap.is_none() {
            log::warn!("NTFS: no usable $Bitmap");
        }
        // Root directory sanity.
        match fs.read_record(5)? {
            RecordRead::Ok(r) if r.is_directory() => {}
            RecordRead::Ok(_) => warnings.push("record 5 (root) is not a directory".into()),
            _ => warnings.push(
                "root directory (record 5) is unreadable; entries are placed by parent references only"
                    .into(),
            ),
        }
        for w in &warnings {
            log::warn!("NTFS: {w}");
        }
        fs.info.warnings = warnings;
        Ok(fs)
    }

    /// The boot sector in use.
    pub fn boot_sector(&self) -> &BootSector {
        &self.boot
    }

    /// Options the volume was opened with.
    pub fn options(&self) -> &NtfsOptions {
        &self.opts
    }

    /// Number of MFT records examined (MFT size / record size, capped by
    /// [`NtfsOptions::max_records`]).
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Cluster size in bytes.
    pub fn cluster_size(&self) -> u64 {
        self.cluster
    }

    /// MFT record size in bytes.
    pub fn record_size(&self) -> u64 {
        self.rsize
    }

    /// Volume-relative byte ranges occupied by the MFT, in MFT order.
    pub fn mft_extents(&self) -> Vec<Range> {
        self.mft.byte_extents(self.cluster, self.mft.data_size)
    }

    /// Read and decode MFT record `n` (no attribute-list folding).
    pub fn record(&self, n: u64) -> Result<MftRecord> {
        match self.read_record(n)? {
            RecordRead::Ok(r) => Ok(r),
            RecordRead::Unreadable => Err(Error::Read(ReadError::new(
                self.record_offset(n).unwrap_or(0),
                self.rsize as usize,
                ReadErrorKind::Io,
                format!("MFT record {n} is unreadable"),
            ))),
            RecordRead::Invalid(e) => Err(Error::corrupt("MFT record", format!("record {n}: {e}"))),
        }
    }

    /// All attributes of a base record, including those stored in
    /// extension records listed by its `$ATTRIBUTE_LIST`. Problems with
    /// extension records are logged and skipped.
    pub fn attributes(&self, record: &MftRecord) -> Vec<Attribute> {
        let f = self.fold(record, true);
        for w in f.warnings {
            log::debug!("NTFS: {w}");
        }
        f.attrs
    }

    /// Data runs of a non-resident attribute (empty for resident ones).
    pub fn data_runs(&self, attr: &Attribute) -> Vec<Run> {
        attr.non_resident()
            .map(|n| n.runs.clone())
            .unwrap_or_default()
    }

    /// Volume byte offset of record `n`, when mapped.
    pub(crate) fn record_offset(&self, n: u64) -> Option<u64> {
        let off = n.checked_mul(self.rsize)?;
        let i = self.mft.find(off / self.cluster)?;
        let e = self.mft.extents[i];
        match e.kind {
            ExtentKind::Real(lcn) => Some(lcn * self.cluster + (off - e.vcn * self.cluster)),
            _ => None,
        }
    }

    pub(crate) fn read_record(&self, n: u64) -> Result<RecordRead> {
        if n >= self.record_count {
            return Ok(RecordRead::Invalid(RecordError::Corrupt(format!(
                "record {n} is beyond the MFT ({} records)",
                self.record_count
            ))));
        }
        let mut buf = vec![0u8; self.rsize as usize];
        let mut ann = Annotations::default();
        read_plain(
            &self.disk,
            &self.mft,
            self.cluster,
            n * self.rsize,
            &mut buf,
            &mut ann,
            None,
            true,
        )?;
        Ok(classify_record(n, buf, &ann, n * self.rsize))
    }

    /// Fold extension records into `rec`'s attributes by following its
    /// attribute list. With `use_map`, a deleted record (whose list ntfs-3g
    /// and Windows truncate while freeing it) or a record with a damaged
    /// list also gets the attributes of every extension record that still
    /// points back at it.
    pub(crate) fn fold(&self, rec: &MftRecord, use_map: bool) -> Folded {
        let mut f = fold_with(
            rec,
            self.total_clusters,
            self.cluster,
            &self.disk,
            &mut |n| self.read_record(n),
        );
        if !(use_map && f.has_list && (!rec.in_use() || f.list_damaged)) {
            return f;
        }
        let map = match self.extension_map() {
            Ok(m) => m,
            Err(e) => {
                f.warnings
                    .push(format!("cannot index extension records: {e}"));
                return f;
            }
        };
        let Some(exts) = map.get(&rec.number) else {
            return f;
        };
        let mut added = 0usize;
        for &x in exts.iter().take(MAX_EXTENSION_RECORDS) {
            if !f.visited.insert(x) {
                continue;
            }
            let Ok(RecordRead::Ok(ext)) = self.read_record(x) else {
                continue;
            };
            let belongs = ext.base_record() == Some(rec.number)
                && crate::record::sequence_matches(rec.sequence, rec.in_use(), ext.base_sequence())
                && (ext.in_use() || !rec.in_use());
            if !belongs {
                continue;
            }
            let (attrs, _) = ext.own_attributes(Some(self.total_clusters));
            let before = added;
            for a in attrs {
                if f.taken.insert((a.record, a.offset)) {
                    f.attrs.push(a);
                    added += 1;
                }
            }
            if added > before && ext.is_torn() {
                f.torn = true;
            }
        }
        if added > 0 {
            f.guessed = true;
            f.warnings.push(format!(
                "record {}: {added} attribute(s) recovered from extension records its attribute list no longer names",
                rec.number
            ));
        }
        f
    }

    /// Extension records grouped by base record number. Built from one
    /// sequential pass over the MFT (shared with the walk's first pass).
    pub(crate) fn extension_map(&self) -> Result<&HashMap<u64, Vec<u64>>> {
        if let Some(m) = self.ext_map.get() {
            return Ok(m);
        }
        let mut map: HashMap<u64, Vec<u64>> = HashMap::new();
        let per_chunk = ((256 * 1024) / self.rsize).max(1);
        let mut n = 0u64;
        while n < self.record_count {
            let k = per_chunk.min(self.record_count - n);
            let mut buf = vec![0u8; (k * self.rsize) as usize];
            let mut ann = Annotations::default();
            read_plain(
                &self.disk,
                &self.mft,
                self.cluster,
                n * self.rsize,
                &mut buf,
                &mut ann,
                None,
                false,
            )?;
            for i in 0..k {
                let a = (i * self.rsize) as usize;
                let r = &buf[a..a + self.rsize as usize];
                // The base reference sits in the first 512-byte stride, away
                // from any fixup slot, so no fixup is needed to read it.
                if &r[0..4] == b"FILE" {
                    let base = mref(diskrescue_core::util::bytes::u64le(r, 0x20).unwrap_or(0));
                    if base != 0 {
                        map.entry(base).or_default().push(n + i);
                    }
                }
            }
            n += k;
        }
        let _ = self.ext_map.set(map);
        Ok(self.ext_map.get().expect("extension map was just set"))
    }

    /// Load a base record with everything the walker needs. `Ok(None)` when
    /// the record is unreadable, not a FILE record, or an extension record.
    pub(crate) fn file_info(&self, n: u64) -> Result<Option<FileInfo>> {
        let rec = match self.read_record(n)? {
            RecordRead::Ok(r) => r,
            RecordRead::Unreadable => {
                log::warn!("NTFS: MFT record {n} is unreadable; skipped");
                return Ok(None);
            }
            RecordRead::Invalid(e) => {
                log::debug!("NTFS: MFT record {n}: {e}");
                return Ok(None);
            }
        };
        if rec.is_extension() {
            return Ok(None);
        }
        Ok(Some(self.build_info(rec)))
    }

    pub(crate) fn build_info(&self, rec: MftRecord) -> FileInfo {
        let Folded {
            attrs,
            mut warnings,
            guessed,
            torn: ext_torn,
            ..
        } = self.fold(&rec, true);
        let torn = ext_torn || rec.is_torn();
        warnings.extend(rec.warnings.iter().cloned());
        let mut si = None;
        let mut names = Vec::new();
        let mut reparse_tag = None;
        let mut encrypted = false;
        let mut data_groups: BTreeMap<String, Vec<&Attribute>> = BTreeMap::new();
        for a in &attrs {
            match a.type_code {
                types::STANDARD_INFORMATION => {
                    if si.is_none() {
                        si = a.value().and_then(StandardInformation::parse);
                    }
                }
                types::FILE_NAME => {
                    if let Some(f) = a.value().and_then(FileName::parse) {
                        names.push(f);
                    }
                }
                types::DATA => {
                    if a.is_encrypted() {
                        encrypted = true;
                    }
                    data_groups.entry(a.name.clone()).or_default().push(a);
                }
                types::REPARSE_POINT => {
                    reparse_tag = Some(
                        a.value()
                            .and_then(|v| diskrescue_core::util::bytes::u32le(v, 0))
                            .unwrap_or(0),
                    );
                }
                types::LOGGED_UTILITY_STREAM if a.name == "$EFS" => encrypted = true,
                _ => {}
            }
        }
        if si.is_some_and(|s| s.file_attributes & FILE_ATTR_ENCRYPTED != 0) {
            encrypted = true;
        }
        let mut data = None;
        let mut ads = Vec::new();
        for (name, pieces) in data_groups {
            if let Some(mut s) = assemble(&pieces, self.cluster, Some(self.total_clusters)) {
                warnings.append(&mut s.warnings);
                if name.is_empty() {
                    data = Some(s);
                } else {
                    ads.push(s);
                }
            }
        }
        FileInfo {
            record: rec,
            attrs,
            guessed,
            torn,
            warnings,
            si,
            names,
            data,
            ads,
            reparse_tag,
            encrypted,
        }
    }

    /// All `$FILE_NAME` values of a base record, including those stored in
    /// extension records.
    pub(crate) fn record_names(&self, rec: &MftRecord) -> Vec<FileName> {
        let (own, _) = rec.own_attributes(Some(self.total_clusters));
        let attrs = if own.iter().any(|a| a.type_code == types::ATTRIBUTE_LIST) {
            self.fold(rec, false).attrs
        } else {
            own
        };
        attrs
            .iter()
            .filter(|a| a.type_code == types::FILE_NAME)
            .filter_map(|a| a.value().and_then(FileName::parse))
            .collect()
    }

    /// Assemble the stream `(type, name)` of an already folded attribute set.
    pub(crate) fn stream_of(&self, attrs: &[Attribute], t: u32, name: &str) -> Option<Stream> {
        let pieces: Vec<&Attribute> = attrs
            .iter()
            .filter(|a| a.type_code == t && a.name == name)
            .collect();
        assemble(&pieces, self.cluster, Some(self.total_clusters))
    }
}

/// True for errors that must abort the whole operation (device gone).
pub(crate) fn is_fatal(e: &Error) -> bool {
    matches!(e, Error::Read(r) if r.kind == ReadErrorKind::DeviceGone)
}

/// Turn raw record bytes plus read annotations into a [`RecordRead`].
/// `base` is the logical MFT offset of `buf` (annotations are expressed in
/// logical MFT offsets).
pub(crate) fn classify_record(n: u64, buf: Vec<u8>, ann: &Annotations, base: u64) -> RecordRead {
    let window = Range::with_len(base, buf.len() as u64);
    if ann.unreadable.intersects(window) {
        return RecordRead::Unreadable;
    }
    if ann.holes.intersects(window) {
        return RecordRead::Invalid(RecordError::NotARecord);
    }
    match MftRecord::parse(n, buf) {
        Ok(r) => RecordRead::Ok(r),
        Err(e) => RecordRead::Invalid(e),
    }
}

/// Attribute-list folding with an arbitrary record loader (used both by an
/// open volume and while bootstrapping the MFT).
pub(crate) fn fold_with(
    rec: &MftRecord,
    total_clusters: u64,
    cluster: u64,
    disk: &Disk,
    load: &mut dyn FnMut(u64) -> Result<RecordRead>,
) -> Folded {
    let (attrs, warnings) = rec.own_attributes(Some(total_clusters));
    let mut f = Folded {
        taken: attrs.iter().map(|a| (a.record, a.offset)).collect(),
        attrs,
        warnings,
        has_list: false,
        list_damaged: false,
        visited: HashSet::new(),
        guessed: false,
        torn: false,
    };
    let Some(list_attr) = f
        .attrs
        .iter()
        .find(|a| a.type_code == types::ATTRIBUTE_LIST)
        .cloned()
    else {
        return f;
    };
    f.has_list = true;
    let Some(list_stream) = assemble(&[&list_attr], cluster, Some(total_clusters)) else {
        f.list_damaged = true;
        return f;
    };
    let bytes = match read_all(disk, &list_stream, cluster, MAX_ATTRIBUTE_LIST) {
        Ok((b, ann)) => {
            if !ann.unreadable.is_empty() {
                f.list_damaged = true;
                f.warnings.push(format!(
                    "record {}: attribute list partly unreadable",
                    rec.number
                ));
            }
            b
        }
        Err(e) => {
            f.list_damaged = true;
            f.warnings.push(format!(
                "record {}: attribute list unreadable: {e}",
                rec.number
            ));
            return f;
        }
    };
    let entries = parse_attribute_list(&bytes);
    if entries.is_empty() {
        f.list_damaged = true;
    }
    let mut loaded: HashMap<u64, Option<Vec<Attribute>>> = HashMap::new();
    let (attrs, warnings, taken) = (&mut f.attrs, &mut f.warnings, &mut f.taken);
    let mut damaged = false;
    let mut ext_torn = false;
    for e in entries {
        let r = mref(e.reference);
        if r == rec.number {
            continue;
        }
        if !loaded.contains_key(&r) {
            if loaded.len() >= MAX_EXTENSION_RECORDS {
                warnings.push(format!(
                    "record {}: more than {MAX_EXTENSION_RECORDS} extension records; rest ignored",
                    rec.number
                ));
                break;
            }
            let ext_attrs = match load(r) {
                Ok(RecordRead::Ok(ext)) => {
                    let base_ok = ext.base_record() == Some(rec.number)
                        && crate::record::sequence_matches(
                            rec.sequence,
                            rec.in_use(),
                            ext.base_sequence(),
                        );
                    if base_ok {
                        let (a, w) = ext.own_attributes(Some(total_clusters));
                        warnings.extend(w);
                        if ext.is_torn() {
                            ext_torn = true;
                            warnings.push(format!(
                                "extension record {r} of record {}: torn write",
                                rec.number
                            ));
                        }
                        Some(a)
                    } else {
                        warnings.push(format!(
                            "record {}: extension record {r} belongs to another file (base {:?}); skipped",
                            rec.number,
                            ext.base_record()
                        ));
                        None
                    }
                }
                Ok(RecordRead::Unreadable) => {
                    damaged = true;
                    warnings.push(format!(
                        "record {}: extension record {r} is unreadable",
                        rec.number
                    ));
                    None
                }
                Ok(RecordRead::Invalid(err)) => {
                    damaged = true;
                    warnings.push(format!(
                        "record {}: extension record {r}: {err}",
                        rec.number
                    ));
                    None
                }
                Err(err) => {
                    damaged = true;
                    warnings.push(format!(
                        "record {}: extension record {r}: {err}",
                        rec.number
                    ));
                    None
                }
            };
            if ext_attrs.is_none() {
                damaged = true;
            }
            loaded.insert(r, ext_attrs);
        }
        let Some(Some(ext_attrs)) = loaded.get(&r) else {
            continue;
        };
        if let Some(a) = ext_attrs
            .iter()
            .find(|a| a.type_code == e.type_code && a.instance == e.instance && a.name == e.name)
        {
            if taken.insert((a.record, a.offset)) {
                attrs.push(a.clone());
            }
        }
    }
    f.list_damaged |= damaged;
    f.torn |= ext_torn;
    f.visited = loaded.into_keys().collect();
    f
}

fn select_boot(disk: &Disk, opts: &NtfsOptions, warnings: &mut Vec<String>) -> Result<BootSector> {
    let size = disk.size();
    let read_at = |off: u64| -> Result<Option<Vec<u8>>> {
        let mut b = vec![0u8; 512];
        let bad = disk.read(off, &mut b, false)?;
        Ok(bad.is_empty().then_some(b))
    };
    let primary = |w: &mut Vec<String>| -> Result<Option<BootSector>> {
        match read_at(0)? {
            None => {
                w.push("sector 0 is unreadable".into());
                Ok(None)
            }
            Some(b) => match BootSector::parse(&b, 0, false) {
                Ok(bs) => Ok(Some(bs)),
                Err(e) => {
                    w.push(format!("primary boot sector invalid: {e}"));
                    Ok(None)
                }
            },
        }
    };
    let backup = |w: &mut Vec<String>| -> Result<Option<BootSector>> {
        let mut tried = Vec::new();
        for ss in [512u64, 1024, 2048, 4096] {
            if size < ss * 2 {
                continue;
            }
            let off = (size / ss - 1) * ss;
            if tried.contains(&off) {
                continue;
            }
            tried.push(off);
            if let Some(b) = read_at(off)? {
                if let Ok(bs) = BootSector::parse(&b, off, true) {
                    if bs.bytes_per_sector as u64 == ss || off % bs.bytes_per_sector as u64 == 0 {
                        return Ok(Some(bs));
                    }
                }
            }
        }
        w.push("no valid backup boot sector at the end of the volume".into());
        Ok(None)
    };
    let mut notes = Vec::new();
    let chosen = if opts.use_backup_boot_sector {
        match backup(&mut notes)? {
            Some(b) => Some(b),
            None => primary(&mut notes)?,
        }
    } else {
        match primary(&mut notes)? {
            Some(b) => Some(b),
            None => backup(&mut notes)?,
        }
    };
    match chosen {
        Some(b) => {
            if b.is_backup {
                warnings.extend(notes);
                warnings.push(format!(
                    "using the backup boot sector at byte {} (sector 0 damaged or backup requested)",
                    b.offset
                ));
            }
            Ok(b)
        }
        None => Err(Error::corrupt(
            "NTFS boot sector",
            format!(
                "no valid primary or backup boot sector ({})",
                notes.join("; ")
            ),
        )),
    }
}

fn bootstrap_mft(
    disk: &Disk,
    boot: &BootSector,
    opts: &NtfsOptions,
    warnings: &mut Vec<String>,
) -> Result<NonResStream> {
    let cluster = boot.cluster_size as u64;
    let rsize = boot.mft_record_size as u64;
    let total = boot.total_clusters();
    let read_rec0 = |lcn: u64| -> Result<std::result::Result<MftRecord, String>> {
        let mut buf = vec![0u8; rsize as usize];
        let bad = disk.read(lcn * cluster, &mut buf, false)?;
        if !bad.is_empty() {
            return Ok(Err("unreadable".into()));
        }
        let rec = match MftRecord::parse(0, buf) {
            Ok(r) => r,
            Err(e) => return Ok(Err(e.to_string())),
        };
        let (attrs, _) = rec.own_attributes(Some(total));
        let has_data = attrs.iter().any(|a| {
            a.type_code == types::DATA
                && a.name.is_empty()
                && a.non_resident()
                    .is_some_and(|n| n.lowest_vcn == 0 && !n.runs.is_empty())
        });
        if has_data {
            Ok(Ok(rec))
        } else {
            Ok(Err("no $DATA runs".into()))
        }
    };
    let rec0 = match read_rec0(boot.mft_lcn)? {
        Ok(r) => Some(r),
        Err(e) => {
            warnings.push(format!("MFT record 0 at cluster {}: {e}", boot.mft_lcn));
            match read_rec0(boot.mftmirr_lcn)? {
                Ok(r) => {
                    warnings.push(format!(
                        "MFT record 0 taken from $MFTMirr at cluster {}",
                        boot.mftmirr_lcn
                    ));
                    Some(r)
                }
                Err(e2) => {
                    warnings.push(format!("$MFTMirr record 0: {e2}"));
                    None
                }
            }
        }
    };
    let Some(rec0) = rec0 else {
        return probe_contiguous_mft(disk, boot, opts, warnings);
    };
    if rec0.is_torn() {
        warnings.push("MFT record 0 has a torn write".into());
    }
    let (attrs, _) = rec0.own_attributes(Some(total));
    let pieces: Vec<&Attribute> = attrs
        .iter()
        .filter(|a| a.type_code == types::DATA && a.name.is_empty())
        .collect();
    let Some(Stream {
        data: StreamData::NonResident(mut mft),
        ..
    }) = assemble(&pieces, cluster, Some(total))
    else {
        return Err(Error::corrupt("$MFT", "record 0 has no non-resident $DATA"));
    };
    if attrs.iter().any(|a| a.type_code == types::ATTRIBUTE_LIST) {
        // The MFT's own runs continue in extension records; read those
        // through the partial map built from the base record.
        let partial = mft.clone();
        let mut load = |n: u64| -> Result<RecordRead> {
            let mut buf = vec![0u8; rsize as usize];
            let mut ann = Annotations::default();
            read_plain(
                disk,
                &partial,
                cluster,
                n * rsize,
                &mut buf,
                &mut ann,
                None,
                true,
            )?;
            Ok(classify_record(n, buf, &ann, n * rsize))
        };
        let folded = fold_with(&rec0, total, cluster, disk, &mut load);
        warnings.extend(folded.warnings.into_iter().map(|w| format!("$MFT: {w}")));
        let all = folded.attrs;
        let pieces: Vec<&Attribute> = all
            .iter()
            .filter(|a| a.type_code == types::DATA && a.name.is_empty())
            .collect();
        if let Some(Stream {
            data: StreamData::NonResident(full),
            ..
        }) = assemble(&pieces, cluster, Some(total))
        {
            mft = full;
        }
    }
    if !mft.complete {
        warnings.push("the $MFT run list is incomplete; some records cannot be located".into());
    }
    if let Some(Extent {
        kind: ExtentKind::Real(lcn),
        ..
    }) = mft.extents.first()
    {
        if *lcn != boot.mft_lcn {
            warnings.push(format!(
                "$MFT data starts at cluster {lcn} but the boot sector says {}",
                boot.mft_lcn
            ));
        }
    }
    if mft.data_size < rsize * 16 {
        warnings.push(format!(
            "$MFT is only {} bytes long; the volume looks damaged",
            mft.data_size
        ));
    }
    Ok(mft)
}

/// Last resort when record 0 and its mirror are both lost: assume the MFT
/// is contiguous from the boot sector's MFT LCN and find where it ends.
fn probe_contiguous_mft(
    disk: &Disk,
    boot: &BootSector,
    opts: &NtfsOptions,
    warnings: &mut Vec<String>,
) -> Result<NonResStream> {
    let cluster = boot.cluster_size as u64;
    let rsize = boot.mft_record_size as u64;
    let start = boot.mft_lcn * cluster;
    let vol_end = boot.volume_size().min(disk.size());
    let max_by_space = vol_end.saturating_sub(start) / rsize;
    let limit = max_by_space.min(opts.max_records);
    let per_chunk = (1u64 << 20) / rsize;
    let mut last_file: Option<u64> = None;
    let mut misses = 0u64;
    let mut n = 0u64;
    'outer: while n < limit {
        let count = per_chunk.min(limit - n);
        let mut buf = vec![0u8; (count * rsize) as usize];
        disk.read(start + n * rsize, &mut buf, false)?;
        for i in 0..count {
            let r = &buf[(i * rsize) as usize..((i + 1) * rsize) as usize];
            if &r[0..4] == b"FILE" {
                last_file = Some(n + i);
                misses = 0;
            } else {
                misses += 1;
                if last_file.is_some() && misses >= PROBE_GIVE_UP {
                    break 'outer;
                }
                if last_file.is_none() && n + i >= PROBE_GIVE_UP {
                    break 'outer;
                }
            }
        }
        n += count;
    }
    let Some(last) = last_file else {
        return Err(Error::corrupt(
            "$MFT",
            "record 0 and $MFTMirr are unusable and no FILE records were found at the MFT location",
        ));
    };
    let records = last + 1;
    let bytes = records * rsize;
    let clusters = bytes.div_ceil(cluster);
    warnings.push(format!(
        "MFT location reconstructed: assuming {records} contiguous records at cluster {}; records in other fragments are lost",
        boot.mft_lcn
    ));
    Ok(NonResStream {
        extents: vec![Extent {
            vcn: 0,
            len: clusters,
            kind: ExtentKind::Real(boot.mft_lcn),
        }],
        data_size: bytes,
        initialized_size: bytes,
        allocated_size: clusters * cluster,
        compression_unit: 0,
        complete: false,
        sizes_known: false,
        size_clamped: false,
    })
}
