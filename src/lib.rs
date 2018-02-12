extern crate byteorder;
extern crate fst;
extern crate libc;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

use std::io;
use std::sync::Arc;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::prelude::*;
use std::ops::Range;
use std::cmp::{Ordering, PartialOrd};
use std::collections::{HashMap, HashSet};

use byteorder::{ByteOrder, LittleEndian};
use fst::Map;
use fst::raw::{Fst, MmapReadOnly};
use std::io::SeekFrom;

use fst::Error;

#[macro_use]
pub mod util;
pub mod config;
pub mod ngrams;
pub mod merge;
pub mod shard;
pub mod builder;
pub mod stopwords;

use shard::QueryType;

macro_rules! make_static_var_and_getter {
    ($fn_name:ident, $var_name:ident, $t:ty) => (
    static mut $var_name: Option<$t> = None;
    #[inline]
    fn $fn_name() -> &'static $t {
        unsafe {
            match $var_name {
                Some(ref n) => n,
                None => std::process::exit(1),
            }
       }
    })
}

make_static_var_and_getter!(get_id_size, ID_SIZE, usize);
make_static_var_and_getter!(get_bucket_size, BUCKET_SIZE, usize);
make_static_var_and_getter!(get_nr_shards, NR_SHARDS, usize);
make_static_var_and_getter!(get_shard_size, SHARD_SIZE, usize);

fn read_bucket(mut file: &File, addr: u64, len: u64) -> Vec<(u32, u8, u8, u8)> {
    let id_size = get_id_size();
    let bk_size = get_bucket_size();
    file.seek(SeekFrom::Start(addr)).unwrap();
    let mut handle = file.take((bk_size * id_size) as u64);
    let mut buf = vec![0u8; bk_size * id_size];

    let vlen = len as usize;
    let mut vector = Vec::<(u32, u8, u8, u8)>::with_capacity(vlen);

    // failure to read returns 0
    let n = handle.read(&mut buf).unwrap_or(0);

    if n > 0 {
        for i in 0..vlen {
            let j = i * id_size;
            vector.push((
                LittleEndian::read_u32(&buf[j..j + 4]),
                buf[j + 4],
                buf[j + 5],
                buf[j + 6],
            ));
        }
    }

    vector
}

// reading part
#[inline]
fn get_addr_and_len(ngram: &str, map: &fst::Map) -> Option<(u64, u64)> {
    match map.get(ngram) {
        Some(val) => return Some(util::elegant_pair_inv(val)),
        None => return None,
    }
}

// Advise the OS on the random access pattern of data.
// Taken from https://docs.rs/crate/madvise/0.1.0
#[cfg(unix)]
fn advise_ram(data: &[u8]) -> io::Result<()> {
    unsafe {
        let result = libc::madvise(
            util::as_ptr(data) as *mut libc::c_void,
            data.len(),
            libc::MADV_RANDOM as libc::c_int,
        );

        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Sid {
    pub id: u64,
    pub sc: f32,
}

struct ShardIds {
    ids: Vec<Sid>,
    norm: f32,
}

impl PartialOrd for Sid {
    fn partial_cmp(&self, other: &Sid) -> Option<Ordering> {
        if self.eq(&other) {
            self.id.partial_cmp(&other.id)
        } else {
            self.sc.partial_cmp(&other.sc)
        }
    }
}

impl PartialEq for Sid {
    fn eq(&self, other: &Sid) -> bool {
        self.sc == other.sc
    }
}

fn get_query_ids(
    ngrams: &HashMap<String, f32>,
    map: &fst::Map,
    ifd: &File,
    count: usize,
) -> Result<ShardIds, Error> {
    let mut _ids = HashMap::new();
    let mut _norm: f32 = 0.0;
    let id_size = *get_id_size();
    let n = *get_shard_size() as f32;
    for (ngram, ntr) in ngrams {
        // IDF score for the ngram
        let mut _idf: f32 = 0.0;
        match get_addr_and_len(ngram, &map) {
            // returns physical memory address and length of the vector (not a number of bytes)
            Some((addr, len)) => {
                for pqid_rem_tr_f in read_bucket(&ifd, addr * id_size as u64, len).iter() {
                    let pqid = pqid_rem_tr_f.0;
                    let reminder = pqid_rem_tr_f.1;
                    let qid = util::pqid2qid(pqid as u64, reminder, *get_nr_shards());
                    // TODO cosine similarity, normalize ngrams relevance at indexing time
                    let f = pqid_rem_tr_f.3;
                    let tr = pqid_rem_tr_f.2;
                    let weight = util::min((tr as f32) / 100.0, *ntr) * (1.0 + f as f32 / 1000.0);
                    *_ids.entry(qid).or_insert(0.0) += weight * (n / len as f32).log(2.0);
                }
                // IDF for existing ngram
                _idf = (n / len as f32).log(2.0);
            }
            None => {
                // IDF for non existing ngram, occurs for the 1st time
                _idf = n.log(2.0);
            }
        }
        // compute the normalization score
        _norm += ntr * _idf;
    }

    let mut v: Vec<Sid> = _ids.iter()
        .map(|(id, sc)| Sid { id: *id, sc: *sc })
        .collect::<Vec<_>>();
    v.sort_by(|a, b| a.partial_cmp(&b).unwrap_or(Ordering::Less).reverse());
    v.truncate(count);

    Ok(ShardIds {
        ids: v,
        norm: _norm,
    })
}

pub struct Qpick {
    path: String,
    config: config::Config,
    stopwords: HashSet<String>,
    terms_relevance: fst::Map,
    shards: Arc<Vec<Shard>>,
    shard_range: Range<u32>,
}

pub struct Shard {
    map: fst::Map,
    shard: File,
}

#[derive(Debug)]
pub struct QpickResults {
    pub items_iter: std::vec::IntoIter<Sid>,
}

impl QpickResults {
    pub fn new(items_iter: std::vec::IntoIter<Sid>) -> QpickResults {
        QpickResults {
            items_iter: items_iter,
        }
    }

    pub fn next(&mut self) -> Option<Sid> {
        <std::vec::IntoIter<Sid> as std::iter::Iterator>::next(&mut self.items_iter)
    }
}

impl Qpick {
    fn new(path: String, shard_range_opt: Option<Range<u32>>) -> Qpick {
        let c = config::Config::init(path.clone());

        unsafe {
            // TODO set up globals, later should be available via self.config
            NR_SHARDS = Some(c.nr_shards);
            ID_SIZE = Some(c.id_size);
            BUCKET_SIZE = Some(c.bucket_size);
            SHARD_SIZE = Some(c.shard_size);
        }

        let shard_range = shard_range_opt.unwrap_or((0..c.nr_shards as u32));

        let stopwords = match stopwords::load(&c.stopwords_path) {
            Ok(stopwords) => stopwords,
            Err(_) => panic!("Failed to load stop-words!"),
        };

        let terms_relevance = match Map::from_path(&c.terms_relevance_path) {
            Ok(terms_relevance) => terms_relevance,
            Err(_) => panic!(
                "Failed to load terms rel. map: {}!",
                &c.terms_relevance_path
            ),
        };

        let mut shards = vec![];
        for i in shard_range.start..shard_range.end {
            let map_path = format!("{}/map.{}", path, i);

            // advice OS on random access to the map file and create Fst object from it
            let map_file = MmapReadOnly::open_path(&map_path).unwrap();
            unsafe { advise_ram(map_file.as_slice()).expect("Advisory failed") };
            let map = match Fst::from_mmap(map_file) {
                Ok(fst) => Map::from(fst),
                Err(_) => panic!("Failed to load index map: {}!", &map_path),
            };

            let shard = OpenOptions::new()
                .read(true)
                .open(format!("{}/shard.{}", path, i))
                .unwrap();
            shards.push(Shard {
                shard: shard,
                map: map,
            });
        }

        Qpick {
            config: c,
            path: path,
            stopwords: stopwords,
            terms_relevance: terms_relevance,
            shards: Arc::new(shards),
            shard_range: shard_range,
        }
    }

    pub fn from_path(path: String) -> Self {
        Qpick::new(path, None)
    }

    pub fn from_path_with_shard_range(path: String, shard_range: Range<u32>) -> Self {
        Qpick::new(path, Some(shard_range))
    }

    fn get_ids(
        &self,
        ngrams: &HashMap<String, f32>,
        count: Option<usize>,
    ) -> Result<Vec<Sid>, Error> {
        let shard_count = match count {
            Some(1...50) => 100,
            _ => count.unwrap(),
        };

        let ref mut shards_ngrams: HashMap<usize, HashMap<String, f32>> = HashMap::new();

        for (ngram, sc) in ngrams {
            let shard_id = util::jump_consistent_hash_str(ngram, self.config.nr_shards as u32);

            if shard_id >= self.shard_range.end || shard_id < self.shard_range.start {
                continue;
            }

            let sh_ngrams = shards_ngrams
                .entry(shard_id as usize)
                .or_insert(HashMap::new());
            sh_ngrams.insert(ngram.to_string(), *sc);
        }

        let shard_ids: Vec<ShardIds> = shards_ngrams
            .iter()
            .map(|sh_ng| {
                get_query_ids(
                    &sh_ng.1,
                    &self.shards[*sh_ng.0].map,
                    &self.shards[*sh_ng.0].shard,
                    shard_count,
                ).unwrap()
            })
            .collect();

        let mut hdata: HashMap<u64, f32> = HashMap::new();
        let mut norm: f32 = 0.0;
        for sh_id in shard_ids.iter() {
            for s in sh_id.ids.iter() {
                *hdata.entry(s.id).or_insert(0.0) += s.sc;
            }
            norm += sh_id.norm;
        }

        let mut vdata: Vec<Sid> = hdata
            .iter()
            .map(|(id, sc)| {
                Sid {
                    id: *id,
                    sc: *sc / norm,
                }
            })
            .collect();
        vdata.sort_by(|a, b| a.partial_cmp(&b).unwrap_or(Ordering::Less).reverse());
        vdata.truncate(count.unwrap_or(100)); //TODO put into config

        Ok(vdata)
    }

    pub fn get_str(&self, query: &str, count: u32) -> String {
        let mut res: Vec<(u64, f32)> = self.get(query, 30 * count)
            .into_iter()
            .map(|s| (s.id, s.sc))
            .collect();
        res.truncate(count as usize);

        serde_json::to_string(&res).unwrap()
    }

    pub fn nget_str(&self, queries: &str, count: u32) -> String {
        let qvec: Vec<String> = serde_json::from_str(queries).unwrap();
        let mut res: Vec<(u64, f32)> = self.nget(&qvec, 30 * count)
            .into_iter()
            .map(|s| (s.id, s.sc))
            .collect();
        res.truncate(count as usize);

        serde_json::to_string(&res).unwrap()
    }

    pub fn get_results(&self, query: &str, count: u32) -> QpickResults {
        QpickResults::new(self.get(query, count).into_iter())
    }

    pub fn nget_results(&self, qvec: &Vec<String>, count: u32) -> QpickResults {
        QpickResults::new(self.nget(qvec, count).into_iter())
    }

    pub fn get(&self, query: &str, count: u32) -> Vec<Sid> {
        if query == "" || count == 0 {
            return vec![];
        }

        let ref ngrams: HashMap<String, f32> =
            ngrams::parse(&query, &self.stopwords, &self.terms_relevance, QueryType::Q);

        match self.get_ids(ngrams, Some(count as usize)) {
            Ok(ids) => ids,
            Err(err) => panic!("Failed to get ids with: {message}", message = err),
        }
    }

    pub fn nget(&self, qvec: &Vec<String>, count: u32) -> Vec<Sid> {
        if qvec.len() == 0 || count == 0 {
            return vec![];
        }

        let ref mut ngrams: HashMap<String, f32> = HashMap::new();
        for query in qvec.iter() {
            for (ngram, sc) in
                ngrams::parse(&query, &self.stopwords, &self.terms_relevance, QueryType::Q)
            {
                ngrams.insert(ngram, sc);
            }
        }

        match self.get_ids(ngrams, Some(count as usize)) {
            Ok(ids) => ids,
            Err(err) => panic!("Failed to get ids with: {message}", message = err),
        }
    }

    pub fn merge(&self) -> Result<(), Error> {
        println!("Merging index maps from: {:?}", &self.path);
        merge::merge(&self.path, self.config.nr_shards as usize)
    }

    pub fn shard(
        file_path: String,
        nr_shards: usize,
        output_dir: String,
        concurrency: usize,
    ) -> Result<(), std::io::Error> {
        println!(
            "Creating {:?} shards from {:?} to {:?}",
            nr_shards,
            file_path,
            output_dir
        );
        shard::shard(&file_path, nr_shards, &output_dir, concurrency)
    }

    pub fn index(
        input_dir: String,
        first_shard: usize,
        last_shard: usize,
        output_dir: String,
    ) -> Result<(), Error> {
        println!(
            "Compiling {:?} shards from {:?} to {:?}",
            last_shard - first_shard,
            input_dir,
            output_dir
        );

        builder::index(&input_dir, first_shard, last_shard, &output_dir)
    }
}

#[allow(dead_code)]
fn main() {}
