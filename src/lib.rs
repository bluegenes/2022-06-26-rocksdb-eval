pub mod color_revindex;
pub mod revindex;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use byteorder::{LittleEndian, WriteBytesExt};
use histogram::Histogram;
use log::info;
use rayon::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use rocksdb::Options;

use sourmash::signature::{Signature, SigsTrait};
use sourmash::sketch::minhash::{max_hash_for_scaled, KmerMinHash};
use sourmash::sketch::Sketch;

//type DB = rocksdb::DBWithThreadMode<rocksdb::SingleThreaded>;
pub type DB = rocksdb::DBWithThreadMode<rocksdb::MultiThreaded>;

pub type DatasetID = u64;
type SigCounter = counter::Counter<DatasetID>;

pub const HASHES: &str = "hashes";
pub const SIGS: &str = "signatures";
pub const COLORS: &str = "colors";

pub fn open_db(path: &Path, read_only: bool, colors: bool) -> Arc<DB> {
    let mut opts = Options::default();
    opts.set_max_open_files(1000);

    // Updated defaults from
    // https://github.com/facebook/rocksdb/wiki/Setup-Options-and-Basic-Tuning#other-general-options
    opts.set_bytes_per_sync(1048576);
    let mut block_opts = rocksdb::BlockBasedOptions::default();
    block_opts.set_block_size(16 * 1024);
    block_opts.set_cache_index_and_filter_blocks(true);
    block_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
    block_opts.set_format_version(5);
    opts.set_block_based_table_factory(&block_opts);
    // End of updated defaults

    if !read_only {
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
    }

    // prepare column family descriptors
    let cfs = if colors {
        color_revindex::cf_descriptors()
    } else {
        revindex::cf_descriptors()
    };

    if read_only {
        //TODO: error_if_log_file_exists set to false, is that an issue?
        Arc::new(DB::open_cf_descriptors_read_only(&opts, path, cfs, false).unwrap())
    } else {
        Arc::new(DB::open_cf_descriptors(&opts, path, cfs).unwrap())
    }
}

#[derive(Debug, PartialEq, Clone, Archive, Serialize, Deserialize)]
pub enum SignatureData {
    Empty,
    Internal(Signature),
    External(String),
}

impl Default for SignatureData {
    fn default() -> Self {
        SignatureData::Empty
    }
}

impl SignatureData {
    pub fn from_slice(slice: &[u8]) -> Option<Self> {
        // TODO: avoid the aligned vec allocation here
        let mut vec = rkyv::AlignedVec::new();
        vec.extend_from_slice(slice);
        let archived_value = unsafe { rkyv::archived_root::<Self>(vec.as_ref()) };
        let inner = archived_value.deserialize(&mut rkyv::Infallible).unwrap();
        Some(inner)
    }

    pub fn as_bytes(&self) -> Option<Vec<u8>> {
        let bytes = rkyv::to_bytes::<_, 256>(self).unwrap();
        Some(bytes.into_vec())

        /*
        let mut serializer = DefaultSerializer::default();
        let v = serializer.serialize_value(self).unwrap();
        debug_assert_eq!(v, 0);
        let buf = serializer.into_serializer().into_inner();
        debug_assert!(Datasets::from_slice(&buf.to_vec()).is_some());
        Some(buf.to_vec())
        */
    }
}

pub fn check_compatible_downsample(
    me: &KmerMinHash,
    other: &KmerMinHash,
) -> Result<(), sourmash::Error> {
    /*
    if self.num != other.num {
        return Err(Error::MismatchNum {
            n1: self.num,
            n2: other.num,
        }
        .into());
    }
    */
    use sourmash::Error;

    if me.ksize() != other.ksize() {
        return Err(Error::MismatchKSizes);
    }
    if me.hash_function() != other.hash_function() {
        // TODO: fix this error
        return Err(Error::MismatchDNAProt);
    }
    if me.max_hash() < other.max_hash() {
        return Err(Error::MismatchScaled);
    }
    if me.seed() != other.seed() {
        return Err(Error::MismatchSeed);
    }
    Ok(())
}

pub fn prepare_query(search_sig: &Signature, template: &Sketch) -> Option<KmerMinHash> {
    let mut search_mh = None;
    if let Some(Sketch::MinHash(mh)) = search_sig.select_sketch(template) {
        search_mh = Some(mh.clone());
    } else {
        // try to find one that can be downsampled
        if let Sketch::MinHash(template_mh) = template {
            for sketch in search_sig.sketches() {
                if let Sketch::MinHash(ref_mh) = sketch {
                    if check_compatible_downsample(&ref_mh, template_mh).is_ok() {
                        let max_hash = max_hash_for_scaled(template_mh.scaled());
                        let mh = ref_mh.downsample_max_hash(max_hash).unwrap();
                        search_mh = Some(mh);
                    }
                }
            }
        }
    }
    search_mh
}

pub fn read_paths<P: AsRef<Path>>(
    paths_file: P,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let paths = BufReader::new(File::open(paths_file)?);
    Ok(paths
        .lines()
        .map(|line| {
            let mut path = PathBuf::new();
            path.push(line.unwrap());
            path
        })
        .collect())
}

#[derive(Debug, PartialEq, Clone, Archive, Serialize, Deserialize, Hash)]
pub enum Datasets {
    Empty,
    Unique(DatasetID),
    Many(BTreeSet<DatasetID>),
}

impl IntoIterator for Datasets {
    type Item = DatasetID;
    type IntoIter = Box<dyn Iterator<Item = Self::Item>>;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::Empty => Box::new(std::iter::empty()),
            Self::Unique(v) => Box::new(std::iter::once(v)),
            Self::Many(v) => Box::new(v.into_iter()),
        }
    }
}

impl Default for Datasets {
    fn default() -> Self {
        Datasets::Empty
    }
}

impl Extend<DatasetID> for Datasets {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = DatasetID>,
    {
        for value in iter {
            match self {
                Self::Empty => *self = Datasets::Unique(value),
                Self::Unique(v) => {
                    if *v != value {
                        *self = Self::Many([*v, value].into_iter().collect());
                    }
                }
                Self::Many(v) => {
                    v.insert(value);
                }
            }
        }
    }
}

impl Datasets {
    pub fn new(vals: &[DatasetID]) -> Self {
        if vals.len() == 0 {
            Self::Empty
        } else if vals.len() == 1 {
            Self::Unique(vals[0])
        } else {
            Self::Many(BTreeSet::from_iter(vals.into_iter().cloned()))
        }
    }

    pub fn from_slice(slice: &[u8]) -> Option<Self> {
        // TODO: avoid the aligned vec allocation here
        let mut vec = rkyv::AlignedVec::new();
        vec.extend_from_slice(slice);
        let archived_value = unsafe { rkyv::archived_root::<Datasets>(vec.as_ref()) };
        let inner = archived_value.deserialize(&mut rkyv::Infallible).unwrap();
        Some(inner)
    }

    pub fn as_bytes(&self) -> Option<Vec<u8>> {
        let bytes = rkyv::to_bytes::<_, 256>(self).unwrap();
        Some(bytes.into_vec())

        /*
        let mut serializer = DefaultSerializer::default();
        let v = serializer.serialize_value(self).unwrap();
        debug_assert_eq!(v, 0);
        let buf = serializer.into_serializer().into_inner();
        debug_assert!(Datasets::from_slice(&buf.to_vec()).is_some());
        Some(buf.to_vec())
        */
    }

    pub fn union(&mut self, other: Datasets) {
        match self {
            Datasets::Empty => match other {
                Datasets::Empty => (),
                Datasets::Unique(_) | Datasets::Many(_) => *self = other,
            },
            Datasets::Unique(v) => match other {
                Datasets::Empty => (),
                Datasets::Unique(o) => {
                    if *v != o {
                        *self = Datasets::Many([*v, o].into_iter().collect())
                    }
                }
                Datasets::Many(o) => {
                    let mut new_hashset: BTreeSet<DatasetID> = [*v].into_iter().collect();
                    new_hashset.extend(o.into_iter());
                    *self = Datasets::Many(new_hashset);
                }
            },
            Datasets::Many(ref mut v) => v.extend(other.into_iter()),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Unique(_) => 1,
            Self::Many(ref v) => v.len(),
        }
    }

    pub fn contains(&self, value: &DatasetID) -> bool {
        match self {
            Self::Empty => false,
            Self::Unique(v) => v == value,
            Self::Many(ref v) => v.contains(value),
        }
    }
}

pub fn search<P: AsRef<Path>>(
    queries_file: P,
    index: P,
    template: Sketch,
    threshold_bp: usize,
    _output: Option<P>,
    colors: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let query_sig = Signature::from_path(queries_file)?;

    let mut query = None;
    for sig in &query_sig {
        if let Some(q) = prepare_query(sig, &template) {
            query = Some(q);
        }
    }
    let query = query.expect("Couldn't find a compatible MinHash");

    let threshold = threshold_bp / query.scaled() as usize;

    let db = open_db(index.as_ref(), true, colors);
    info!("Loaded DB");

    info!("Building counter");
    let counter = if colors {
        color_revindex::counter_for_query(db.clone(), &query)
    } else {
        revindex::counter_for_query(db.clone(), &query)
    };
    info!("Counter built");

    let cf_sigs = db.cf_handle(SIGS).unwrap();

    let matches_iter = counter
        .most_common()
        .into_iter()
        .filter_map(|(dataset_id, size)| {
            if size >= threshold {
                let mut v = vec![0_u8; 8];
                (&mut v[..])
                    .write_u64::<LittleEndian>(dataset_id)
                    .expect("error writing bytes");
                Some((&cf_sigs, v))
            } else {
                None
            }
        });

    info!("Multi get matches");
    let matches: Vec<String> = db
        .multi_get_cf(matches_iter)
        .into_iter()
        .filter_map(|r| r.ok().unwrap_or(None))
        .filter_map(
            |sigdata| match SignatureData::from_slice(&sigdata).unwrap() {
                SignatureData::Empty => None,
                SignatureData::External(p) => Some(p),
                SignatureData::Internal(sig) => Some(sig.name()),
            },
        )
        .collect();

    info!("matches: {}", matches.len());
    //info!("matches: {:?}", matches);

    Ok(())
}
pub fn index<P: AsRef<Path>>(
    siglist: P,
    template: Sketch,
    threshold: f64,
    output: P,
    save_paths: bool,
    colors: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Loading siglist");
    let index_sigs = read_paths(siglist)?;
    info!("Loaded {} sig paths in siglist", index_sigs.len());

    let db = open_db(output.as_ref(), false, colors);

    let processed_sigs = AtomicUsize::new(0);

    if colors {
        index_sigs
            .iter()
            .enumerate()
            .filter_map(|(dataset_id, filename)| {
                let i = processed_sigs.fetch_add(1, Ordering::SeqCst);
                if i % 1000 == 0 {
                    info!("Processed {} reference sigs", i);
                }

                color_revindex::map_hashes_colors(
                    db.clone(),
                    dataset_id as DatasetID,
                    filename,
                    threshold,
                    &template,
                    save_paths,
                );
                Some(true)
            })
            .count();
    } else {
        index_sigs
            .par_iter()
            .enumerate()
            .filter_map(|(dataset_id, filename)| {
                let i = processed_sigs.fetch_add(1, Ordering::SeqCst);
                if i % 1000 == 0 {
                    info!("Processed {} reference sigs", i);
                }

                revindex::map_hashes_colors(
                    db.clone(),
                    dataset_id as DatasetID,
                    filename,
                    threshold,
                    &template,
                    save_paths,
                );
                Some(true)
            })
            .count();
    };

    info!("Processed {} reference sigs", processed_sigs.into_inner());

    if colors {
        use crate::color_revindex::Colors;

        info!("Compressing colors");
        Colors::compress(db.clone());
        info!("Finished compressing colors");
    }
    db.compact_range(None::<&[u8]>, None::<&[u8]>);

    Ok(())
}

pub fn check<P: AsRef<Path>>(
    output: P,
    quick: bool,
    colors: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use byteorder::ReadBytesExt;
    use numsep::{separate, Locale};

    let db = open_db(output.as_ref(), true, colors);

    let deep_check = if colors { COLORS } else { HASHES };

    let stats_for_cf = |cf_name| {
        let cf = db.cf_handle(cf_name).unwrap();

        let iter = db.iterator_cf(&cf, rocksdb::IteratorMode::Start);
        let mut kcount = 0;
        let mut vcount = 0;
        let mut vcounts = Histogram::new();
        let mut datasets: Datasets = Default::default();

        for (key, value) in iter {
            let _k = (&key[..]).read_u64::<LittleEndian>().unwrap();
            kcount += key.len();

            //println!("Saw {} {:?}", k, Datasets::from_slice(&value));
            vcount += value.len();

            if !quick && cf_name == deep_check {
                let v = Datasets::from_slice(&value).expect("Error with value");
                vcounts.increment(v.len() as u64).unwrap();
                datasets.union(v);
            }
            //println!("Saw {} {:?}", k, value);
        }

        info!("*** {} ***", cf_name);
        use size::Size;
        let ksize = Size::from_bytes(kcount);
        let vsize = Size::from_bytes(vcount);
        if !quick && cf_name == COLORS {
            info!(
                "total datasets: {}",
                separate(datasets.len(), Locale::English)
            );
        }
        info!("total keys: {}", separate(kcount / 8, Locale::English));

        info!("k: {}", ksize.to_string());
        info!("v: {}", vsize.to_string());

        if !quick && kcount > 0 && cf_name == deep_check {
            info!("max v: {}", vcounts.maximum().unwrap());
            info!("mean v: {}", vcounts.mean().unwrap());
            info!("stddev: {}", vcounts.stddev().unwrap());
            info!("median v: {}", vcounts.percentile(50.0).unwrap());
            info!("p25 v: {}", vcounts.percentile(25.0).unwrap());
            info!("p75 v: {}", vcounts.percentile(75.0).unwrap());
        }
    };

    stats_for_cf(HASHES);
    if colors {
        info!("");
        stats_for_cf(COLORS);
    }
    info!("");
    stats_for_cf(SIGS);

    Ok(())
}

pub fn sig_save_to_db(
    db: Arc<DB>,
    mut search_sig: Signature,
    search_mh: KmerMinHash,
    size: u64,
    threshold: f64,
    save_paths: bool,
    filename: &PathBuf,
    dataset_id: u64,
) {
    // Save signature to DB
    let sig = if search_mh.is_empty() || size < threshold as u64 {
        SignatureData::Empty
    } else if save_paths {
        SignatureData::External(filename.to_str().unwrap().to_string())
    } else {
        search_sig.reset_sketches();
        search_sig.push(Sketch::MinHash(search_mh));
        SignatureData::Internal(search_sig)
    };

    let sig_bytes = sig.as_bytes().unwrap();
    let cf_sigs = db.cf_handle(SIGS).unwrap();
    let mut hash_bytes = [0u8; 8];
    (&mut hash_bytes[..])
        .write_u64::<LittleEndian>(dataset_id)
        .expect("error writing bytes");
    db.put_cf(&cf_sigs, &hash_bytes[..], sig_bytes.as_slice())
        .expect("error saving sig");
}
