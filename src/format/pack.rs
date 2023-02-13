use std::{
    borrow::Cow,
    collections::HashMap,
    io::{Cursor, Seek, SeekFrom, Write},
};

use anyhow::{bail, ensure, Result};
use binrw::{binrw, BinReaderExt, BinWriterExt, Endian};
use uuid::Uuid;

use crate::{
    format::{chunk::ChunkDescriptor, rfrm::FormDescriptor, FourCC},
    util::lzss::decompress,
};

// Package file
pub const K_FORM_PACK: FourCC = FourCC(*b"PACK");
// Table of contents
pub const K_FORM_TOCC: FourCC = FourCC(*b"TOCC");
// Metadata
pub const K_CHUNK_META: FourCC = FourCC(*b"META");
// String table
pub const K_CHUNK_STRG: FourCC = FourCC(*b"STRG");
// Asset directory
pub const K_CHUNK_ADIR: FourCC = FourCC(*b"ADIR");

// Custom footer for extracted files
pub const K_FORM_FOOT: FourCC = FourCC(*b"FOOT");
// Custom footer asset information
pub const K_CHUNK_AINF: FourCC = FourCC(*b"AINF");
// Custom footer asset name
pub const K_CHUNK_NAME: FourCC = FourCC(*b"NAME");

/// PACK::TOCC::ADIR chunk
#[binrw]
#[derive(Clone, Debug, Default)]
pub struct AssetDirectory {
    #[bw(try_calc = entries.len().try_into())]
    pub entry_count: u32,
    #[br(count = entry_count)]
    pub entries: Vec<AssetDirectoryEntry>,
}

/// PACK::TOCC::ADIR chunk entry
#[binrw]
#[derive(Clone, Debug)]
pub struct AssetDirectoryEntry {
    pub asset_type: FourCC,
    #[br(map = Uuid::from_u128)]
    #[bw(map = Uuid::as_u128)]
    pub asset_id: Uuid,
    pub version: u32,
    pub other_version: u32,
    pub offset: u64,
    pub decompressed_size: u64,
    pub size: u64,
}

/// PACK::TOCC::META chunk
#[binrw]
#[derive(Clone, Debug, Default)]
pub struct MetadataTable {
    #[bw(try_calc = entries.len().try_into())]
    pub entry_count: u32,
    #[br(count = entry_count)]
    pub entries: Vec<MetadataTableEntry>,
}

/// PACK::TOCC::META chunk entry
#[binrw]
#[derive(Clone, Debug)]
pub struct MetadataTableEntry {
    #[br(map = Uuid::from_u128)]
    #[bw(map = Uuid::as_u128)]
    pub asset_id: Uuid,
    pub offset: u32,
}

/// PACK::TOCC::STRG chunk
#[binrw]
#[derive(Clone, Debug, Default)]
pub struct StringTable {
    #[bw(try_calc = entries.len().try_into())]
    pub entry_count: u32,
    #[br(count = entry_count)]
    pub entries: Vec<StringTableEntry>,
}

/// PACK::TOCC::STRG chunk entry
#[binrw]
#[derive(Clone, Debug)]
pub struct StringTableEntry {
    #[br(map = FourCC::swap)]
    #[bw(map = |&f| f.swap())]
    pub kind: FourCC,
    #[br(map = Uuid::from_u128)]
    #[bw(map = Uuid::as_u128)]
    pub asset_id: Uuid,
    #[bw(try_calc = name.len().try_into())]
    pub name_length: u32,
    #[br(count = name_length)]
    pub name: Vec<u8>,
}

/// Custom AINF chunk
#[binrw]
#[derive(Clone, Debug)]
pub struct AssetInfo {
    #[br(map = Uuid::from_u128)]
    #[bw(map = Uuid::as_u128)]
    pub id: Uuid,
    pub compression_mode: u32,
    pub entry_idx: u32,
    pub orig_offset: u64,
}

/// Combined asset representation
#[derive(Debug, Clone)]
pub struct Asset<'a> {
    pub id: Uuid,
    pub kind: FourCC,
    pub name: Option<String>,
    // TODO lazy decompression?
    pub data: Cow<'a, [u8]>,
    pub meta: Option<Cow<'a, [u8]>>,
    pub info: AssetInfo,
    pub version: u32,
    pub other_version: u32,
}

/// Combined package information
#[derive(Debug, Clone)]
pub struct Package<'a> {
    pub assets: Vec<Asset<'a>>,
}

impl Package<'_> {
    pub fn read(data: &[u8], e: Endian) -> Result<Package> {
        let (pack, pack_data, _) = FormDescriptor::slice(data, e)?;
        ensure!(pack.id == K_FORM_PACK);
        ensure!(pack.version == 1);
        log::debug!("PACK: {:?}", pack);
        let (tocc, mut tocc_data, _) = FormDescriptor::slice(pack_data, e)?;
        ensure!(tocc.id == K_FORM_TOCC);
        ensure!(tocc.version == 3);
        log::debug!("TOCC: {:?}", tocc);
        let mut adir: Option<AssetDirectory> = None;
        let mut meta: HashMap<Uuid, &[u8]> = HashMap::new();
        let mut strg: HashMap<Uuid, String> = HashMap::new();
        while !tocc_data.is_empty() {
            let (desc, chunk_data, remain) = ChunkDescriptor::slice(tocc_data, e)?;
            let mut reader = Cursor::new(chunk_data);
            log::debug!("{:?} data size {}", desc, chunk_data.len());
            match desc.id {
                K_CHUNK_ADIR => {
                    let chunk: AssetDirectory = reader.read_type(e)?;
                    for entry in &chunk.entries {
                        log::debug!("- {:?}", entry);
                    }
                    adir = Some(chunk);
                }
                K_CHUNK_META => {
                    let chunk: MetadataTable = reader.read_type(e)?;
                    let mut iter = chunk.entries.iter().peekable();
                    while let Some(entry) = iter.next() {
                        let size = if let Some(next) = iter.peek() {
                            (next.offset - entry.offset) as usize
                        } else {
                            chunk_data.len() - entry.offset as usize
                        };
                        log::debug!("- {:?}", entry);
                        meta.insert(
                            entry.asset_id,
                            &chunk_data[entry.offset as usize..entry.offset as usize + size],
                        );
                    }
                }
                K_CHUNK_STRG => {
                    let chunk: StringTable = reader.read_type(e)?;
                    for entry in &chunk.entries {
                        log::debug!("- {:?}", entry);
                        strg.insert(entry.asset_id, String::from_utf8(entry.name.clone())?);
                    }
                }
                kind => bail!("Unhandled TOCC chunk {:?}", kind),
            }
            tocc_data = remain;
        }

        let mut package = Package { assets: vec![] };
        if let Some(adir) = adir {
            for (entry_idx, asset_entry) in adir.entries.iter().enumerate() {
                let mut compression_mode = 0u32;
                let data: Cow<[u8]> = if asset_entry.size != asset_entry.decompressed_size {
                    let compressed_data = &data[asset_entry.offset as usize
                        ..(asset_entry.offset + asset_entry.size) as usize];
                    compression_mode =
                        u32::from_le_bytes(compressed_data[0..4].try_into().unwrap());
                    let mut out = vec![0u8; asset_entry.decompressed_size as usize];
                    let lzss_data = &compressed_data[4..];
                    match compression_mode {
                        1 => decompress::<1>(lzss_data, &mut out),
                        2 => decompress::<2>(lzss_data, &mut out),
                        3 => decompress::<3>(lzss_data, &mut out),
                        _ => bail!("Unsupported compression mode {}", compression_mode),
                    }
                    Cow::Owned(out)
                } else {
                    Cow::Borrowed(
                        &data[asset_entry.offset as usize
                            ..(asset_entry.offset + asset_entry.size) as usize],
                    )
                };

                // Validate RFRM
                {
                    let (form, _, _) = FormDescriptor::slice(&data, Endian::Little)?;
                    ensure!(asset_entry.asset_type == form.id);
                    ensure!(asset_entry.version == form.version);
                    ensure!(asset_entry.other_version == form.other_version);
                    ensure!(asset_entry.decompressed_size == form.size + 32 /* RFRM */);
                }

                package.assets.push(Asset {
                    id: asset_entry.asset_id,
                    kind: asset_entry.asset_type,
                    name: strg.get(&asset_entry.asset_id).cloned(),
                    data,
                    meta: meta.get(&asset_entry.asset_id).map(|data| Cow::Borrowed(*data)),
                    info: AssetInfo {
                        id: asset_entry.asset_id,
                        compression_mode,
                        entry_idx: entry_idx as u32,
                        orig_offset: asset_entry.offset,
                    },
                    version: asset_entry.version,
                    other_version: asset_entry.other_version,
                });
            }
        } else {
            bail!("Failed to locate asset directory");
        }
        Ok(package)
    }

    pub fn write<W: Write + Seek>(&self, w: &mut W, e: Endian) -> Result<()> {
        let mut asset_directory = AssetDirectory::default();
        let mut metadata = MetadataTable::default();
        let mut string_table = StringTable::default();
        for asset in &self.assets {
            asset_directory.entries.push(AssetDirectoryEntry {
                asset_type: asset.kind,
                asset_id: asset.id,
                version: asset.version,
                other_version: asset.other_version,
                offset: 0,
                decompressed_size: asset.data.len() as u64,
                size: asset.data.len() as u64,
            });
            if asset.meta.is_some() {
                metadata.entries.push(MetadataTableEntry { asset_id: asset.id, offset: 0 });
            }
            if let Some(name) = &asset.name {
                string_table.entries.push(StringTableEntry {
                    kind: asset.kind,
                    asset_id: asset.id,
                    name: name.as_bytes().to_vec(),
                });
            }
        }
        let mut adir_pos = 0;
        FormDescriptor { size: 0, unk1: 0, id: K_FORM_PACK, version: 1, other_version: 1 }.write(
            w,
            e,
            |w| {
                FormDescriptor { size: 0, unk1: 0, id: K_FORM_TOCC, version: 3, other_version: 3 }
                    .write(w, e, |w| {
                        ChunkDescriptor { id: K_CHUNK_ADIR, size: 0, unk: 1, skip: 0 }.write(
                            w,
                            e,
                            |w| {
                                adir_pos = w.stream_position()?;
                                w.write_type(&asset_directory, e)?;
                                Ok(())
                            },
                        )?;
                        ChunkDescriptor { id: K_CHUNK_META, size: 0, unk: 1, skip: 0 }.write(
                            w,
                            e,
                            |w| {
                                let start = w.stream_position()?;
                                w.write_type(&metadata, e)?;
                                for (asset, entry) in self
                                    .assets
                                    .iter()
                                    .filter(|a| a.meta.is_some())
                                    .zip(&mut metadata.entries)
                                {
                                    entry.offset = (w.stream_position()? - start) as u32;
                                    w.write_all(asset.meta.as_ref().unwrap())?;
                                }
                                let end = w.stream_position()?;
                                w.seek(SeekFrom::Start(start))?;
                                w.write_type(&metadata, e)?;
                                w.seek(SeekFrom::Start(end))?;
                                Ok(())
                            },
                        )?;
                        ChunkDescriptor { id: K_CHUNK_STRG, size: 0, unk: 1, skip: 0 }.write(
                            w,
                            e,
                            |w| {
                                w.write_type(&string_table, e)?;
                                Ok(())
                            },
                        )?;
                        Ok(())
                    })?;
                let mut entries: Vec<(&Asset, &mut AssetDirectoryEntry)> =
                    self.assets.iter().zip(&mut asset_directory.entries).collect();
                entries.sort_by_key(|(a, _)| a.info.orig_offset);
                for (asset, entry) in entries {
                    entry.offset = w.stream_position()?;
                    w.write_all(&asset.data)?;
                }
                Ok(())
            },
        )?;

        // Write updated ADIR offsets
        let pos = w.stream_position()?;
        w.seek(SeekFrom::Start(adir_pos))?;
        w.write_type(&asset_directory, e)?;
        w.seek(SeekFrom::Start(pos))?;

        // Align 16
        let aligned_end = (pos + 15) & !15;
        w.write_all(&vec![0u8; (aligned_end - pos) as usize])?;
        Ok(())
    }
}