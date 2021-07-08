use std::{convert::{TryFrom, TryInto}, fmt, fs::{create_dir_all, File}, io::{BufReader, Read, Write}, iter::once, mem, path::{Path, PathBuf}, str::FromStr};

use color_eyre::{Report, eyre::{eyre, Result}};
use deku::prelude::*;
use log::{debug, error, trace};
use zstd::{
    dict::{from_continuous, DecoderDictionary, EncoderDictionary},
    Decoder, Encoder,
};

pub struct Store {
    pub dir: PathBuf,
    pub dict_en: Option<EncoderDictionary<'static>>,
    pub dict_de: Option<DecoderDictionary<'static>>,
}

impl Store {
    pub fn commit(&mut self, block: &mut Block, n: usize) -> Result<()> {
        let block = mem::take(block);

        let dict = if let Some(ref d) = self.dict_en {
            d
        } else {
            // create dictionary from first block
            let sample_sizes: Vec<usize> = once(&0_u64)
                .chain(block.starts.iter())
                .zip(block.starts.iter())
                .fuse()
                .map(|(start, end)| end - start)
                .map(|n| usize::try_from(n).unwrap())
                .collect();

            let dict_data = from_continuous(&block.data, &sample_sizes, 150_000)?;
            let mut file = File::create(self.dir.join("zst.dictionary"))?;
            file.write_all(&dict_data)?;
            self.dict_en = Some(EncoderDictionary::copy(&dict_data, 3));
            self.dict_de = Some(DecoderDictionary::copy(&dict_data));
            self.dict_en.as_ref().unwrap()
        };

        let file = File::create(self.dir.join(format!("{}.zst", n)))?;
        let mut target = Encoder::with_prepared_dictionary(file, dict)?;

        let block_bytes = block.finish()?;
        target.write_all(&block_bytes)?;
        target.finish()?;

        Ok(())
    }

    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().into(),
            dict_en: None,
            dict_de: None,
        }
    }

    pub fn create(&self) -> Result<()> {
        if !self.dir.exists() {
            create_dir_all(&self.dir)?;
        }

        Ok(())
    }

    pub fn open(&mut self) -> Result<()> {
        let mut dict = File::open(self.dir.join("zst.dictionary"))?;
        let mut dict_bytes = Vec::with_capacity(dict.metadata()?.len().try_into()?);
        dict.read_to_end(&mut dict_bytes)?;
        self.dict_de = Some(DecoderDictionary::copy(&dict_bytes));
        debug!("loaded dictionary size={}", dict_bytes.len());

        Ok(())
    }

    pub fn blocks(&self) -> Result<Vec<PathBuf>> {
        let mut blocks = Vec::new();
        for d in self.dir.read_dir()? {
            let d = d?;
            if !d.file_type()?.is_file() {
                continue;
            }
            if !d.path().display().to_string().ends_with(".zst") {
                continue;
            }

            blocks.push(d.path());
        }

        Ok(blocks)
    }

    /// reads a block
    ///
    /// panics if decoder dictionary isn't ready (call `open()` first)
    pub fn read_block(&self, path: impl AsRef<Path>) -> Result<Block> {
        let path = path.as_ref();

        let file = File::open(path)?;
        let filelen: usize = file.metadata()?.len().try_into()?;
        let file = BufReader::new(file);
        let mut source = Decoder::with_prepared_dictionary(file, self.dict_de.as_ref().unwrap())?;
        let mut block_bytes = Vec::with_capacity(filelen * 2);
        source.read_to_end(&mut block_bytes)?;

        let id: u32 = path
            .file_stem()
            .ok_or_else(|| eyre!("no file stem for block filename {:?}", path))?
            .to_string_lossy()
            .parse()?;

        debug!("loaded block id={} size={}", id, block_bytes.len());
        let mut block = Block::from_bytes((&block_bytes, 0))?.1;
        block.id = id;
        Ok(block)
    }

    /// reads an entry directly from its ref
    ///
    /// panics if decoder dictionary isn't ready (call `open()` first)
    pub fn read_entry(&mut self, refid: Ref) -> Result<Entry> {
        let path = self.dir.join(format!("{}.zst", refid.block_id));
        let block = self.read_block(path)?;
        block.entry(refid.entry_id)
    }
}

#[derive(Debug, Default, DekuRead, DekuWrite)]
#[deku(endian = "little")]
pub struct Block {
    #[deku(skip)]
    pub id: u32,

    #[deku(update = "self.starts.len()", pad_bytes_after = "4")] // FIXME: regenerate the store with 4-byte n!
    pub n: u32,
    #[deku(count = "n")]
    pub starts: Vec<u64>,
    #[deku(bits_read = "deku::rest.len()")]
    pub data: Vec<u8>,
}

impl Block {
    pub fn add(&mut self, entry: Entry) -> Result<()> {
        let data = entry.to_bytes()?;
        self.n += 1;
        self.starts.push(u64::try_from(self.data.len())?);
        self.data.extend(data);
        Ok(())
    }

    pub fn finish(self) -> Result<Vec<u8>> {
        Ok(self.to_bytes()?)
    }

    pub fn entry(&self, n: u32) -> Result<Entry> {
        let start = *self
            .starts
            .get(usize::try_from(n)?)
            .ok_or_else(|| eyre!("no such entry: {}", n))?;
        let start: usize = start.try_into()?;
        debug!("[block={}] reading entry {}/{} start={}", self.id, n, self.n, start);

        let store_ref = Ref::new(self.id, n);

        let title_len =
            usize::try_from(u32::from_le_bytes(self.data[start..start + 4].try_into()?))?;
        let body_len = usize::try_from(u32::from_le_bytes(
            self.data[start + 4..start + 8].try_into()?,
        ))?;
        debug!("[{}] entry title len={} body len={}", store_ref, title_len, body_len);

        let entry_slice = &self.data[start..(start + 8 + title_len + body_len)];
        trace!("[{}] entry slice = {:?}", store_ref, entry_slice);

        if body_len == 0 {
            // this really should work with deku but whatever
            Ok(Entry {
                store_ref,
                title_len: title_len.try_into()?,
                body_len: body_len.try_into()?,
                title: entry_slice[8..].to_vec(),
                body: Vec::new(),
            })
        } else {
            let mut entry = Entry::from_bytes((entry_slice, 0))
                .map_err(|err| {
                    error!(
                        "entry {} t={} b={} data={:?}",
                        n, title_len, body_len, entry_slice
                    );
                    err
                })?
                .1;
            entry.store_ref = Ref::new(self.id, n);
            Ok(entry)
        }
    }
}

#[derive(Debug, DekuRead, DekuWrite)]
#[deku(endian = "little")]
pub struct Entry {
    #[deku(skip)]
    pub store_ref: Ref,

    pub title_len: u32,
    pub body_len: u32,
    #[deku(bytes_read = "title_len")]
    pub title: Vec<u8>,
    #[deku(bytes_read = "body_len")]
    pub body: Vec<u8>,
}

impl Entry {
    pub fn new(title: &str, body: &str) -> Self {
        let title = title.as_bytes();
        let body = body.as_bytes();

        Self {
            store_ref: Ref::default(),
            title_len: u32::try_from(title.len()).unwrap(),
            body_len: u32::try_from(body.len()).unwrap(),
            title: title.into(),
            body: body.into(),
        }
    }

    pub fn open(self) -> (String, String, Ref) {
        let title = String::from_utf8(self.title).unwrap();
        let body = String::from_utf8(self.body).unwrap();
        (title, body, self.store_ref)
    }
}

#[derive(Clone, Copy, Debug, Default, DekuRead, DekuWrite)]
#[deku(endian = "little")]
pub struct Ref {
    pub block_id: u32,
    pub entry_id: u32,
}

impl Ref {
    pub fn new(block_id: u32, entry_id: u32) -> Self {
        Self { block_id, entry_id }
    }

    pub fn as_u64(self) -> u64 {
        u64::from_le_bytes(self.to_bytes().unwrap().try_into().unwrap())
    }

    pub fn from_u64(r: u64) -> Self {
        Self::from_bytes((&r.to_le_bytes(), 0)).unwrap().1
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.block_id, self.entry_id)
    }
}

impl FromStr for Ref {
    type Err = Report;

    fn from_str(s: &str) -> Result<Self> {
        let (b, e) = s.split_once('/').ok_or_else(|| eyre!("missing / in refid"))?;
        Ok(Self {
            block_id: b.parse()?,
            entry_id: e.parse()?,
        })
    }
}
