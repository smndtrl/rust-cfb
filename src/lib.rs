//! A library for reading/writing [Compound File Binary](
//! https://en.wikipedia.org/wiki/Compound_File_Binary_Format) (structured
//! storage) files.  See [MS-CFB](
//! https://msdn.microsoft.com/en-us/library/dd942138.aspx) for the format
//! specification.

#![warn(missing_docs)]

extern crate byteorder;

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::cmp::{self, Ordering};
use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[macro_use]
mod internal;

// ========================================================================= //

const HEADER_LEN: usize = 512; // length of CFB file header, in bytes
const DIR_ENTRY_LEN: usize = 128; // length of directory entry, in bytes
const NUM_DIFAT_ENTRIES_IN_HEADER: usize = 109;

// Constants for CFB file header values:
const MAGIC_NUMBER: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
const MINOR_VERSION: u16 = 0x3e;
const BYTE_ORDER_MARK: u16 = 0xfffe;
const MINI_SECTOR_SHIFT: u16 = 6; // 64-byte mini sectors
const MINI_SECTOR_LEN: usize = 1 << (MINI_SECTOR_SHIFT as usize);
const MINI_STREAM_CUTOFF: u32 = 4096;

// Constants for FAT entries:
const MAX_REGULAR_SECTOR: u32 = 0xfffffffa;
const FAT_SECTOR: u32 = 0xfffffffd;
const END_OF_CHAIN: u32 = 0xfffffffe;
const FREE_SECTOR: u32 = 0xffffffff;

// Constants for directory entries:
const ROOT_DIR_NAME: &'static str = "Root Entry";
const OBJ_TYPE_UNALLOCATED: u8 = 0;
const OBJ_TYPE_STORAGE: u8 = 1;
const OBJ_TYPE_STREAM: u8 = 2;
const OBJ_TYPE_ROOT: u8 = 5;
const COLOR_RED: u8 = 0;
const COLOR_BLACK: u8 = 1;
const ROOT_STREAM_ID: u32 = 0;
const MAX_REGULAR_STREAM_ID: u32 = 0xfffffffa;
const NO_STREAM: u32 = 0xffffffff;

// ========================================================================= //

/// A compound file, backed by an underlying reader/writer (such as a
/// [`File`](https://doc.rust-lang.org/std/fs/struct.File.html) or
/// [`Cursor`](https://doc.rust-lang.org/std/io/struct.Cursor.html)).
pub struct CompoundFile<F> {
    inner: F,
    version: Version,
    difat: Vec<u32>,
    fat: Vec<u32>,
    minifat: Vec<u32>,
    minifat_start_sector: u32,
    directory: Vec<DirEntry>,
    directory_start_sector: u32,
}

impl<F> CompoundFile<F> {
    /// Returns the CFB format version used for this compound file.
    pub fn version(&self) -> Version { self.version }

    fn root_dir_entry(&self) -> &DirEntry {
        &self.directory[ROOT_STREAM_ID as usize]
    }

    fn root_dir_entry_mut(&mut self) -> &mut DirEntry {
        &mut self.directory[ROOT_STREAM_ID as usize]
    }

    fn stream_id_for_path(&self, path: &Path) -> io::Result<u32> {
        let names = internal::path::name_chain_from_path(path)?;
        let mut stream_id = ROOT_STREAM_ID;
        for name in names.into_iter() {
            stream_id = self.directory[stream_id as usize].child;
            loop {
                if stream_id == NO_STREAM {
                    not_found!("No such object: {:?}", path);
                }
                let dir_entry = &self.directory[stream_id as usize];
                match internal::path::compare_names(&name, &dir_entry.name) {
                    Ordering::Equal => break,
                    Ordering::Less => stream_id = dir_entry.left_sibling,
                    Ordering::Greater => stream_id = dir_entry.right_sibling,
                }
            }
        }
        Ok(stream_id)
    }

    /// Given a path within the compound file, get information about that
    /// stream or storage object.
    pub fn entry<P: AsRef<Path>>(&self, path: P) -> io::Result<StorageEntry> {
        self.entry_for_path(path.as_ref())
    }

    fn entry_for_path(&self, path: &Path) -> io::Result<StorageEntry> {
        let stream_id = self.stream_id_for_path(path)?;
        let dir_entry = &self.directory[stream_id as usize];
        Ok(StorageEntry::new(dir_entry, path.to_path_buf()))
    }

    /// Returns an iterator over the entries within a storage object.
    pub fn read_storage<P: AsRef<Path>>(&self, path: P)
                                        -> io::Result<ReadStorage> {
        self.read_storage_for_path(path.as_ref())
    }

    fn read_storage_for_path(&self, path: &Path) -> io::Result<ReadStorage> {
        let stream_id = self.stream_id_for_path(path)?;
        Ok(ReadStorage {
            directory: &self.directory,
            path: internal::path::canonicalize_path(path)?,
            stack: Vec::new(),
            current: self.directory[stream_id as usize].child,
        })
    }

    // TODO: pub fn walk_storage

    // TODO: pub fn create_storage

    // TODO: pub fn remove_storage

    // TODO: pub fn create_stream

    // TODO: pub fn remove_stream

    // TODO: pub fn copy_stream

    // TODO: pub fn rename

    /// Consumes the `CompoundFile`, returning the underlying reader/writer.
    pub fn into_inner(self) -> F { self.inner }

    fn validate_difat_and_fat(&self) -> io::Result<()> {
        for &fat_sector in self.difat.iter() {
            if fat_sector as usize >= self.fat.len() {
                invalid_data!("Malformed DIFAT (index {} out of bounds)",
                              fat_sector);
            }
            if self.fat[fat_sector as usize] != FAT_SECTOR {
                invalid_data!("Malformed FAT (sector {} not marked as FAT)",
                              fat_sector);
            }
        }
        let mut pointees = HashSet::new();
        for (from_sector, &to_sector) in self.fat.iter().enumerate() {
            if to_sector <= MAX_REGULAR_SECTOR {
                if to_sector as usize >= self.fat.len() {
                    invalid_data!("Malformed FAT (sector {} points to {}, \
                                   which is out of bounds)",
                                  from_sector,
                                  to_sector);
                }
                if pointees.contains(&to_sector) {
                    invalid_data!("Malformed FAT (sector {} pointed to twice)",
                                  to_sector);
                }
                pointees.insert(to_sector);
            }
        }
        Ok(())
    }

    fn validate_minifat(&self) -> io::Result<()> {
        let mut pointees = HashSet::new();
        for (from_mini_sector, &to_mini_sector) in
            self.minifat.iter().enumerate() {
            if to_mini_sector <= MAX_REGULAR_SECTOR {
                if to_mini_sector as usize >= self.minifat.len() {
                    invalid_data!("Malformed MiniFAT (mini sector {} points \
                                   to {}, which is out of bounds)",
                                  from_mini_sector,
                                  to_mini_sector);
                }
                if pointees.contains(&to_mini_sector) {
                    invalid_data!("Malformed MiniFAT (mini sector {} pointed \
                                   to twice)",
                                  to_mini_sector);
                }
                pointees.insert(to_mini_sector);
            }
        }
        Ok(())
    }

    fn validate_directory(&self) -> io::Result<()> {
        let root_entry = self.root_dir_entry();
        if root_entry.name != ROOT_DIR_NAME {
            invalid_data!("Malformed directory (root name)");
        }
        let expected_root_stream_len = MINI_SECTOR_LEN as u64 *
                                       self.minifat.len() as u64;
        if root_entry.stream_len != expected_root_stream_len {
            invalid_data!("Malformed directory (root stream len is {}, but \
                           should be {})",
                          root_entry.stream_len,
                          expected_root_stream_len);
        }
        if root_entry.stream_len % MINI_SECTOR_LEN as u64 != 0 {
            invalid_data!("Malformed directory (root stream len is {}, but \
                           should be multiple of {})",
                          root_entry.stream_len,
                          MINI_SECTOR_LEN);
        }
        let mut visited = HashSet::new();
        let mut stack = vec![ROOT_STREAM_ID];
        while let Some(stream_id) = stack.pop() {
            if visited.contains(&stream_id) {
                invalid_data!("Malformed directory (loop in tree)");
            }
            visited.insert(stream_id);
            let dir_entry = &self.directory[stream_id as usize];
            let left_sibling = dir_entry.left_sibling;
            if left_sibling != NO_STREAM {
                if left_sibling as usize >= self.directory.len() {
                    invalid_data!("Malformed directory (sibling index)");
                }
                let entry = &self.directory[left_sibling as usize];
                if internal::path::compare_names(&dir_entry.name,
                                                 &entry.name) !=
                   Ordering::Less {
                    invalid_data!("Malformed directory (name ordering)");
                }
                stack.push(left_sibling);
            }
            let right_sibling = dir_entry.right_sibling;
            if right_sibling != NO_STREAM {
                if right_sibling as usize >= self.directory.len() {
                    invalid_data!("Malformed directory (sibling index)");
                }
                let entry = &self.directory[right_sibling as usize];
                if internal::path::compare_names(&dir_entry.name,
                                                 &entry.name) !=
                   Ordering::Less {
                    invalid_data!("Malformed directory (name ordering)");
                }
                stack.push(right_sibling);
            }
            if dir_entry.obj_type != OBJ_TYPE_ROOT {
                let child = dir_entry.child;
                if child != NO_STREAM {
                    if child as usize >= self.directory.len() {
                        invalid_data!("Malformed directory (child index)");
                    }
                    stack.push(child);
                }
            }
        }
        Ok(())
    }
}

impl<F: Seek> CompoundFile<F> {
    fn seek_to_sector(&mut self, sector: u32) -> io::Result<()> {
        self.seek_within_sector(sector, 0)
    }

    fn seek_within_sector(&mut self, sector: u32, offset_within_sector: u64)
                          -> io::Result<()> {
        self.inner
            .seek(SeekFrom::Start(offset_within_sector +
                                  self.version.sector_len() as u64 *
                                  (1 + sector as u64)))?;
        Ok(())
    }

    fn seek_within_mini_sector(&mut self, mini_sector: u32,
                               offset_within_mini_sector: u64)
                               -> io::Result<()> {
        let sector_len = self.version.sector_len() as u64;
        let offset_within_mini_stream = offset_within_mini_sector +
                                        MINI_SECTOR_LEN as u64 *
                                        mini_sector as u64;
        let mini_stream_start_sector = self.root_dir_entry().start_sector;
        let mut mini_stream_sector = mini_stream_start_sector;
        for _ in 0..(offset_within_mini_stream / sector_len) {
            debug_assert_ne!(mini_stream_sector, END_OF_CHAIN);
            mini_stream_sector = self.fat[mini_stream_sector as usize];
        }
        let offset_within_sector = offset_within_mini_stream % sector_len;
        self.seek_within_sector(mini_stream_sector, offset_within_sector)
    }

    fn seek_within_dir_entry(&mut self, stream_id: u32,
                             offset_within_dir_entry: usize)
                             -> io::Result<()> {
        let dir_entries_per_sector = self.version.sector_len() / DIR_ENTRY_LEN;
        let index_within_sector = stream_id as usize % dir_entries_per_sector;
        let offset_within_sector = index_within_sector * DIR_ENTRY_LEN +
                                   offset_within_dir_entry;
        let mut directory_sector = self.directory_start_sector;
        for _ in 0..(stream_id as usize / dir_entries_per_sector) {
            debug_assert_ne!(directory_sector, END_OF_CHAIN);
            directory_sector = self.fat[directory_sector as usize];
        }
        self.seek_within_sector(directory_sector, offset_within_sector as u64)
    }

    /// Opens an existing stream in the compound file for reading and/or
    /// writing (depending on what the underlying file supports).
    pub fn open_stream<P: AsRef<Path>>(&mut self, path: P)
                                       -> io::Result<Stream<F>> {
        self.open_stream_for_path(path.as_ref())
    }

    fn open_stream_for_path(&mut self, path: &Path) -> io::Result<Stream<F>> {
        let stream_id = self.stream_id_for_path(path)?;
        if self.directory[stream_id as usize].obj_type != OBJ_TYPE_STREAM {
            invalid_input!("not a stream: {:?}", path);
        }
        Stream::new(self, stream_id)
    }
}

impl<F: Read + Seek> CompoundFile<F> {
    /// Opens a existing compound file, using the underlying reader.
    pub fn open(mut inner: F) -> io::Result<CompoundFile<F>> {
        // Read basic header information.
        inner.seek(SeekFrom::Start(0))?;
        let mut magic = [0u8; 8];
        inner.read_exact(&mut magic)?;
        if magic != MAGIC_NUMBER {
            invalid_data!("Invalid CFB file (wrong magic number)");
        }
        inner.seek(SeekFrom::Start(26))?;
        let version_number = inner.read_u16::<LittleEndian>()?;
        let version = match Version::from_number(version_number) {
            Some(version) => version,
            None => {
                invalid_data!("CFB version {} is not supported",
                              version_number);
            }
        };
        if inner.read_u16::<LittleEndian>()? != BYTE_ORDER_MARK {
            invalid_data!("Invalid CFB byte order mark");
        }
        let sector_shift = inner.read_u16::<LittleEndian>()?;
        if sector_shift != version.sector_shift() {
            invalid_data!("Incorrect sector shift for CFB version {} \
                           (is {}, but must be {})",
                          version.number(),
                          sector_shift,
                          version.sector_shift());
        }
        let sector_len = version.sector_len();
        let mini_sector_shift = inner.read_u16::<LittleEndian>()?;
        if mini_sector_shift != MINI_SECTOR_SHIFT {
            invalid_data!("Incorrect mini sector shift \
                           (is {}, but must be {})",
                          mini_sector_shift,
                          MINI_SECTOR_SHIFT);
        }
        inner.seek(SeekFrom::Start(44))?;
        let num_fat_sectors = inner.read_u32::<LittleEndian>()?;
        let first_dir_sector = inner.read_u32::<LittleEndian>()?;
        let _transaction_signature = inner.read_u32::<LittleEndian>()?;
        let mini_stream_cutoff = inner.read_u32::<LittleEndian>()?;
        if mini_stream_cutoff != MINI_STREAM_CUTOFF {
            invalid_data!("Invalid mini stream cutoff value \
                           (is {}, but must be {})",
                          mini_stream_cutoff,
                          MINI_STREAM_CUTOFF);
        }
        let first_minifat_sector = inner.read_u32::<LittleEndian>()?;
        let num_minifat_sectors = inner.read_u32::<LittleEndian>()?;
        let first_difat_sector = inner.read_u32::<LittleEndian>()?;
        let num_difat_sectors = inner.read_u32::<LittleEndian>()?;
        let mut comp = CompoundFile {
            inner: inner,
            version: version,
            difat: Vec::new(),
            fat: Vec::new(),
            minifat: Vec::new(),
            minifat_start_sector: first_minifat_sector,
            directory: Vec::new(),
            directory_start_sector: first_dir_sector,
        };

        // Read in DIFAT.
        for _ in 0..NUM_DIFAT_ENTRIES_IN_HEADER {
            let next = comp.inner.read_u32::<LittleEndian>()?;
            if next == FREE_SECTOR {
                break;
            } else if next > MAX_REGULAR_SECTOR {
                invalid_data!("Invalid sector index ({}) in DIFAT", next);
            }
            comp.difat.push(next);
        }
        let mut difat_sectors = Vec::new();
        let mut current_difat_sector = first_difat_sector;
        while current_difat_sector != END_OF_CHAIN {
            difat_sectors.push(current_difat_sector);
            comp.seek_to_sector(current_difat_sector)?;
            for _ in 0..(sector_len / 4 - 1) {
                let next = comp.inner.read_u32::<LittleEndian>()?;
                if next != FREE_SECTOR && next > MAX_REGULAR_SECTOR {
                    invalid_data!("Invalid sector index ({}) in DIFAT", next);
                }
                comp.difat.push(next);
            }
            current_difat_sector = comp.inner.read_u32::<LittleEndian>()?;
        }
        if num_difat_sectors as usize != difat_sectors.len() {
            invalid_data!("Incorrect DIFAT chain length \
                           (header says {}, actual is {})",
                          num_difat_sectors,
                          difat_sectors.len());
        }
        while comp.difat.last() == Some(&FREE_SECTOR) {
            comp.difat.pop();
        }
        if num_fat_sectors as usize != comp.difat.len() {
            invalid_data!("Incorrect number of FAT sectors \
                           (header says {}, DIFAT says {})",
                          num_fat_sectors,
                          comp.difat.len());
        }

        // Read in FAT.
        for index in 0..comp.difat.len() {
            let current_fat_sector = comp.difat[index];
            comp.seek_to_sector(current_fat_sector)?;
            for _ in 0..(sector_len / 4) {
                comp.fat.push(comp.inner.read_u32::<LittleEndian>()?);
            }
        }
        while comp.fat.last() == Some(&FREE_SECTOR) {
            comp.fat.pop();
        }
        comp.validate_difat_and_fat()?;

        // Read in MiniFAT.
        let mut minifat_sectors = Vec::new();
        let mut current_minifat_sector = first_minifat_sector;
        while current_minifat_sector != END_OF_CHAIN {
            minifat_sectors.push(current_minifat_sector);
            comp.seek_to_sector(current_minifat_sector)?;
            for _ in 0..(sector_len / 4) {
                comp.minifat.push(comp.inner.read_u32::<LittleEndian>()?);
            }
            current_minifat_sector = comp.fat[current_minifat_sector as usize];
        }
        if num_minifat_sectors as usize != minifat_sectors.len() {
            invalid_data!("Incorrect MiniFAT chain length \
                           (header says {}, actual is {})",
                          num_minifat_sectors,
                          minifat_sectors.len());
        }
        while comp.minifat.last() == Some(&FREE_SECTOR) {
            comp.minifat.pop();
        }
        comp.validate_minifat()?;

        // Read in directory.
        let mut current_dir_sector = first_dir_sector;
        while current_dir_sector != END_OF_CHAIN {
            comp.seek_to_sector(current_dir_sector)?;
            for _ in 0..(sector_len / DIR_ENTRY_LEN) {
                comp.directory.push(DirEntry::read(&mut comp.inner, version)?);
            }
            current_dir_sector = comp.fat[current_dir_sector as usize];
        }
        comp.validate_directory()?;
        Ok(comp)
    }
}

impl<F: Write + Seek> CompoundFile<F> {
    /// Creates a new compound file with no contents, using the underlying
    /// writer.  The writer should be initially empty.
    pub fn create(inner: F) -> io::Result<CompoundFile<F>> {
        CompoundFile::create_with_version(inner, Version::V4)
    }

    /// Creates a new compound file of the given version with no contents,
    /// using the underlying writer.  The writer should be initially empty.
    pub fn create_with_version(mut inner: F, version: Version)
                               -> io::Result<CompoundFile<F>> {
        // Write file header:
        inner.write_all(&MAGIC_NUMBER)?;
        inner.write_all(&[0; 16])?; // reserved field
        inner.write_u16::<LittleEndian>(MINOR_VERSION)?;
        inner.write_u16::<LittleEndian>(version.number())?;
        inner.write_u16::<LittleEndian>(BYTE_ORDER_MARK)?;
        inner.write_u16::<LittleEndian>(version.sector_shift())?;
        inner.write_u16::<LittleEndian>(MINI_SECTOR_SHIFT)?;
        inner.write_all(&[0; 6])?; // reserved field
        inner.write_u32::<LittleEndian>(1)?; // num dir sectors
        inner.write_u32::<LittleEndian>(1)?; // num FAT sectors
        inner.write_u32::<LittleEndian>(1)?; // first dir sector
        inner.write_u32::<LittleEndian>(0)?; // transaction signature (unused)
        inner.write_u32::<LittleEndian>(MINI_STREAM_CUTOFF)?;
        inner.write_u32::<LittleEndian>(END_OF_CHAIN)?; // first MiniFAT sector
        inner.write_u32::<LittleEndian>(0)?; // num MiniFAT sectors
        inner.write_u32::<LittleEndian>(END_OF_CHAIN)?; // first DIFAT sector
        inner.write_u32::<LittleEndian>(0)?; // num DIFAT sectors
        // First 109 DIFAT entries:
        inner.write_u32::<LittleEndian>(0)?;
        for _ in 1..NUM_DIFAT_ENTRIES_IN_HEADER {
            inner.write_u32::<LittleEndian>(FREE_SECTOR)?;
        }
        // Pad the header with zeroes so it's the length of a sector.
        let sector_len = version.sector_len();
        debug_assert!(sector_len >= HEADER_LEN);
        if sector_len > HEADER_LEN {
            inner.write_all(&vec![0; sector_len - HEADER_LEN])?;
        }

        // Write FAT sector:
        let fat = vec![FAT_SECTOR, END_OF_CHAIN];
        for &entry in fat.iter() {
            inner.write_u32::<LittleEndian>(entry)?;
        }
        for _ in fat.len()..(sector_len / 4) {
            inner.write_u32::<LittleEndian>(FREE_SECTOR)?;
        }

        // Write directory sector:
        let root_dir_entry = DirEntry {
            name: ROOT_DIR_NAME.to_string(),
            obj_type: OBJ_TYPE_ROOT,
            color: COLOR_BLACK,
            left_sibling: NO_STREAM,
            right_sibling: NO_STREAM,
            child: NO_STREAM,
            creation_time: 0,
            modified_time: 0,
            start_sector: END_OF_CHAIN,
            stream_len: 0,
        };
        root_dir_entry.write(&mut inner)?;
        for _ in 1..(sector_len / DIR_ENTRY_LEN) {
            DirEntry::unallocated().write(&mut inner)?;
        }

        Ok(CompoundFile {
            inner: inner,
            version: version,
            difat: vec![0],
            fat: fat,
            minifat: vec![],
            minifat_start_sector: END_OF_CHAIN,
            directory: vec![root_dir_entry],
            directory_start_sector: 1,
        })
    }

    /// Sets the modified time for the object at the given path to now.  Has no
    /// effect when called on the root storage.
    pub fn touch<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        self.touch_with_path(path.as_ref())
    }

    fn touch_with_path(&mut self, path: &Path) -> io::Result<()> {
        let stream_id = self.stream_id_for_path(path)?;
        if stream_id != ROOT_STREAM_ID {
            debug_assert_ne!(self.directory[stream_id as usize].obj_type,
                             OBJ_TYPE_ROOT);
            self.seek_within_dir_entry(stream_id, 108)?;
            let now = internal::time::current_timestamp();
            self.inner.write_u64::<LittleEndian>(now)?;
            self.directory[stream_id as usize].modified_time = now;
        }
        Ok(())
    }

    /// Flushes all changes to the underlying file.
    pub fn flush(&mut self) -> io::Result<()> { self.inner.flush() }

    /// Given the starting sector (or any internal sector) of a chain, extends
    /// the end of that chain by one sector and returns the new sector number,
    /// updating the FAT as necessary.
    fn extend_chain(&mut self, start_sector: u32) -> io::Result<u32> {
        debug_assert_ne!(start_sector, END_OF_CHAIN);
        let mut last_sector = start_sector;
        loop {
            let next = self.fat[last_sector as usize];
            if next == END_OF_CHAIN {
                break;
            }
            last_sector = next;
        }
        let new_sector = self.allocate_sector(END_OF_CHAIN)?;
        self.set_fat(last_sector, new_sector)?;
        Ok(new_sector)
    }

    /// Given the starting sector (or any internal sector) of a mini chain,
    /// extends the end of that chain by one mini sector and returns the new
    /// mini sector number, updating the MiniFAT as necessary.
    fn extend_mini_chain(&mut self, start_mini_sector: u32)
                         -> io::Result<u32> {
        debug_assert_ne!(start_mini_sector, END_OF_CHAIN);
        let mut last_mini_sector = start_mini_sector;
        loop {
            let next = self.minifat[last_mini_sector as usize];
            if next == END_OF_CHAIN {
                break;
            }
            last_mini_sector = next;
        }
        let new_mini_sector = self.allocate_mini_sector(END_OF_CHAIN)?;
        self.set_minifat(last_mini_sector, new_mini_sector)?;
        Ok(new_mini_sector)
    }

    /// Allocates a new entry in the FAT, sets its value to `value`, and
    /// returns the new sector number.
    fn allocate_sector(&mut self, value: u32) -> io::Result<u32> {
        // If there's an existing free sector, use that.
        for sector in 0..self.fat.len() {
            if self.fat[sector] == FREE_SECTOR {
                let sector = sector as u32;
                self.set_fat(sector, value)?;
                return Ok(sector);
            }
        }
        // Otherwise, we need a new sector; if there's not room in the FAT to
        // add it, then first we need to allocate a new FAT sector.
        let sector_len = self.version.sector_len();
        if self.fat.len() % (sector_len / 4) == 0 {
            self.append_fat_sector()?;
        }
        // Add a new sector to the end of the file and return it.
        let new_sector = self.fat.len() as u32;
        self.set_fat(new_sector, value)?;
        self.seek_to_sector(new_sector)?;
        io::copy(&mut io::repeat(0).take(sector_len as u64), &mut self.inner)?;
        Ok(new_sector)
    }

    /// Allocates a new entry in the MiniFAT, sets its value to `value`, and
    /// returns the new mini sector number.
    fn allocate_mini_sector(&mut self, value: u32) -> io::Result<u32> {
        // If there's an existing free mini sector, use that.
        for mini_sector in 0..self.minifat.len() {
            if self.minifat[mini_sector] == FREE_SECTOR {
                let mini_sector = mini_sector as u32;
                self.set_minifat(mini_sector, value)?;
                return Ok(mini_sector);
            }
        }
        // Otherwise, we need a new mini sector; if there's not room in the
        // MiniFAT to add it, then first we need to allocate a new MiniFAT
        // sector.
        let minifat_entries_per_sector = self.version.sector_len() / 4;
        let num_minifat_sectors =
            (self.minifat.len() / minifat_entries_per_sector) as u32;
        if self.minifat_start_sector == END_OF_CHAIN {
            debug_assert!(self.minifat.is_empty());
            debug_assert_eq!(num_minifat_sectors, 0);
            let sector = self.allocate_sector(END_OF_CHAIN)?;
            self.minifat_start_sector = sector;
            self.inner.seek(SeekFrom::Start(60))?;
            self.inner.write_u32::<LittleEndian>(sector)?;
        } else if self.minifat.len() % minifat_entries_per_sector == 0 {
            let start = self.minifat_start_sector;
            self.extend_chain(start)?;
            self.inner.seek(SeekFrom::Start(64))?;
        }
        self.inner.write_u32::<LittleEndian>(num_minifat_sectors + 1)?;
        // Add a new mini sector to the end of the mini stream and return it.
        let new_mini_sector = self.minifat.len() as u32;
        self.set_minifat(new_mini_sector, value)?;
        self.append_mini_sector()?;
        Ok(new_mini_sector)
    }

    /// Adds a new sector to the FAT chain at the end of the file, and updates
    /// the FAT and DIFAT accordingly.
    fn append_fat_sector(&mut self) -> io::Result<()> {
        // Add a new sector to the end of the file.
        let new_fat_sector = self.fat.len() as u32;
        self.seek_to_sector(new_fat_sector)?;
        io::copy(&mut io::repeat(0).take(self.version.sector_len() as u64),
                 &mut self.inner)?;

        // Record this new FAT sector in the DIFAT and in the FAT itself.
        let difat_index = self.difat.len();
        self.difat.push(new_fat_sector);
        if difat_index < NUM_DIFAT_ENTRIES_IN_HEADER {
            self.inner.seek(SeekFrom::Start(76 + 4 * difat_index as u64))?;
            self.inner.write_u32::<LittleEndian>(new_fat_sector)?;
        } else {
            // TODO: write into a DIFAT sector, possibly allocating a new one
            panic!("more than {} DIFAT entries not yet supported");
        }
        self.set_fat(new_fat_sector, FAT_SECTOR)?;
        debug_assert_eq!(self.fat.len(), new_fat_sector as usize + 1);

        // Update length of FAT chain in header.
        self.inner.seek(SeekFrom::Start(44))?;
        self.inner.write_u32::<LittleEndian>(self.difat.len() as u32)?;
        Ok(())
    }

    /// Adds a new mini sector to the end of the mini stream.
    fn append_mini_sector(&mut self) -> io::Result<()> {
        let directory_start_sector = self.directory_start_sector;
        let mini_stream_start_sector = self.root_dir_entry().start_sector;
        let mini_stream_len = self.root_dir_entry().stream_len;
        debug_assert_eq!(mini_stream_len % MINI_SECTOR_LEN as u64, 0);
        let sector_len = self.version.sector_len();

        // If the mini stream doesn't have room for new mini sector, add
        // another regular sector to its chain.
        if mini_stream_start_sector == END_OF_CHAIN {
            debug_assert_eq!(mini_stream_len, 0);
            let sector = self.allocate_sector(END_OF_CHAIN)?;
            self.root_dir_entry_mut().start_sector = sector;
            self.seek_within_sector(directory_start_sector, 116)?;
            self.inner.write_u32::<LittleEndian>(sector)?;
        } else if mini_stream_len % sector_len as u64 == 0 {
            self.extend_chain(mini_stream_start_sector)?;
        }

        // Update length of mini stream in root directory entry.
        self.root_dir_entry_mut().stream_len += MINI_SECTOR_LEN as u64;
        let mini_stream_len = self.root_dir_entry().stream_len;
        self.seek_within_sector(directory_start_sector, 120)?;
        self.inner.write_u64::<LittleEndian>(mini_stream_len)?;
        Ok(())
    }

    /// Sets `self.fat[index] = value`, and also writes that change to the
    /// underlying file.  The `index` must be <= `self.fat.len()`.
    fn set_fat(&mut self, index: u32, value: u32) -> io::Result<()> {
        let index = index as usize;
        debug_assert!(index <= self.fat.len());
        let fat_entries_per_sector = self.version.sector_len() / 4;
        let fat_sector = self.difat[index / fat_entries_per_sector];
        let offset = 4 * (index % fat_entries_per_sector) as u64;
        self.seek_within_sector(fat_sector, offset)?;
        self.inner.write_u32::<LittleEndian>(value)?;
        if index == self.fat.len() {
            self.fat.push(value);
        } else {
            self.fat[index] = value;
        }
        Ok(())
    }

    /// Sets `self.minifat[index] = value`, and also writes that change to the
    /// underlying file.  The `index` must be <= `self.minifat.len()`.
    fn set_minifat(&mut self, index: u32, value: u32) -> io::Result<()> {
        let index = index as usize;
        debug_assert!(index <= self.minifat.len());
        let minifat_entries_per_sector = self.version.sector_len() / 4;
        let mut minifat_sector = self.minifat_start_sector;
        for _ in 0..(index / minifat_entries_per_sector) {
            debug_assert_ne!(minifat_sector, END_OF_CHAIN);
            minifat_sector = self.fat[minifat_sector as usize];
        }
        let offset = 4 * (index % minifat_entries_per_sector) as u64;
        self.seek_within_sector(minifat_sector, offset)?;
        self.inner.write_u32::<LittleEndian>(value)?;
        if index == self.minifat.len() {
            self.minifat.push(value);
        } else {
            self.minifat[index] = value;
        }
        Ok(())
    }
}

// ========================================================================= //

struct DirEntry {
    name: String,
    obj_type: u8,
    color: u8,
    left_sibling: u32,
    right_sibling: u32,
    child: u32,
    creation_time: u64,
    modified_time: u64,
    start_sector: u32,
    stream_len: u64,
}

impl DirEntry {
    fn unallocated() -> DirEntry {
        DirEntry {
            name: String::new(),
            obj_type: OBJ_TYPE_UNALLOCATED,
            // Values of fields below don't really matter:
            color: 0,
            left_sibling: 0,
            right_sibling: 0,
            child: 0,
            creation_time: 0,
            modified_time: 0,
            start_sector: 0,
            stream_len: 0,
        }
    }

    fn read<R: Read>(reader: &mut R, version: Version)
                     -> io::Result<DirEntry> {
        let name: String = {
            let mut name_chars: Vec<u16> = Vec::with_capacity(32);
            for _ in 0..32 {
                name_chars.push(reader.read_u16::<LittleEndian>()?);
            }
            let name_len_bytes = reader.read_u16::<LittleEndian>()?;
            if name_len_bytes > 64 || name_len_bytes % 2 != 0 {
                invalid_data!("Invalid name length ({}) in directory entry",
                              name_len_bytes);
            }
            let name_len_chars = if name_len_bytes > 0 {
                (name_len_bytes / 2 - 1) as usize
            } else {
                0
            };
            debug_assert!(name_len_chars < name_chars.len());
            if name_chars[name_len_chars] != 0 {
                invalid_data!("Directory entry name must be null-terminated");
            }
            String::from_utf16_lossy(&name_chars[0..name_len_chars])
        };
        internal::path::validate_name(&name)?;
        let obj_type = reader.read_u8()?;
        let color = reader.read_u8()?;
        if color != COLOR_RED && color != COLOR_BLACK {
            invalid_data!("Invalid color in directory entry ({})", color);
        }
        let left_sibling = reader.read_u32::<LittleEndian>()?;
        if left_sibling != NO_STREAM && left_sibling > MAX_REGULAR_STREAM_ID {
            invalid_data!("Invalid left sibling in directory entry ({})",
                          left_sibling);
        }
        let right_sibling = reader.read_u32::<LittleEndian>()?;
        if right_sibling != NO_STREAM &&
           right_sibling > MAX_REGULAR_STREAM_ID {
            invalid_data!("Invalid right sibling in directory entry ({})",
                          right_sibling);
        }
        let child = reader.read_u32::<LittleEndian>()?;
        if child != NO_STREAM && child > MAX_REGULAR_STREAM_ID {
            invalid_data!("Invalid child in directory entry ({})", child);
        }
        let mut clsid = [0u8; 16];
        reader.read_exact(&mut clsid)?;
        let _state_bits = reader.read_u32::<LittleEndian>()?;
        let creation_time = reader.read_u64::<LittleEndian>()?;
        let modified_time = reader.read_u64::<LittleEndian>()?;
        let start_sector = reader.read_u32::<LittleEndian>()?;
        let stream_len = reader.read_u64::<LittleEndian>()? &
                         version.stream_len_mask();
        Ok(DirEntry {
            name: name,
            obj_type: obj_type,
            color: color,
            left_sibling: left_sibling,
            right_sibling: right_sibling,
            child: child,
            creation_time: creation_time,
            modified_time: modified_time,
            start_sector: start_sector,
            stream_len: stream_len,
        })
    }

    fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        debug_assert!(internal::path::validate_name(&self.name).is_ok());
        let name_utf16: Vec<u16> = self.name.encode_utf16().collect();
        debug_assert!(name_utf16.len() < 32);
        for &chr in name_utf16.iter() {
            writer.write_u16::<LittleEndian>(chr)?;
        }
        for _ in name_utf16.len()..32 {
            writer.write_u16::<LittleEndian>(0)?;
        }
        writer.write_u16::<LittleEndian>((name_utf16.len() as u16 + 1) * 2)?;
        writer.write_u8(self.obj_type)?;
        writer.write_u8(self.color)?;
        writer.write_u32::<LittleEndian>(self.left_sibling)?;
        writer.write_u32::<LittleEndian>(self.right_sibling)?;
        writer.write_u32::<LittleEndian>(self.child)?;
        writer.write_all(&[0; 16])?; // TODO: CLSID
        writer.write_u32::<LittleEndian>(0)?; // TODO: state bits
        writer.write_u64::<LittleEndian>(self.creation_time)?;
        writer.write_u64::<LittleEndian>(self.modified_time)?;
        writer.write_u32::<LittleEndian>(self.start_sector)?;
        writer.write_u64::<LittleEndian>(self.stream_len)?;
        Ok(())
    }
}

// ========================================================================= //

/// Metadata about a single object (storage or stream) in a compound file.
#[derive(Clone)]
pub struct StorageEntry {
    name: String,
    path: PathBuf,
    obj_type: u8,
    creation_time: u64,
    modified_time: u64,
    stream_len: u64,
}

impl StorageEntry {
    fn new(dir_entry: &DirEntry, path: PathBuf) -> StorageEntry {
        StorageEntry {
            name: dir_entry.name.clone(),
            path: path,
            obj_type: dir_entry.obj_type,
            creation_time: dir_entry.creation_time,
            modified_time: dir_entry.modified_time,
            stream_len: dir_entry.stream_len,
        }
    }

    /// Returns the name of the object that this entry represents.
    pub fn name(&self) -> &str { &self.name }

    /// Returns the full path to the object that this entry represents.
    pub fn path(&self) -> &Path { &self.path }

    /// Returns whether this entry is for a stream object (i.e. a "file" within
    /// the compound file).
    pub fn is_stream(&self) -> bool { self.obj_type == OBJ_TYPE_STREAM }

    /// Returns whether this entry is for a storage object (i.e. a "directory"
    /// within the compound file), either the root or a nested storage.
    pub fn is_storage(&self) -> bool {
        self.obj_type == OBJ_TYPE_STORAGE || self.obj_type == OBJ_TYPE_ROOT
    }

    /// Returns whether this entry is specifically for the root storage object
    /// of the compound file.
    pub fn is_root(&self) -> bool { self.obj_type == OBJ_TYPE_ROOT }

    /// Returns the size, in bytes, of the stream that this metadata is for.
    pub fn len(&self) -> u64 { self.stream_len }

    /// Returns the time when the object that this entry represents was
    /// created.
    pub fn created(&self) -> SystemTime {
        internal::time::system_time_from_timestamp(self.creation_time)
    }

    /// Returns the time when the object that this entry represents was last
    /// modified.
    pub fn modified(&self) -> SystemTime {
        internal::time::system_time_from_timestamp(self.modified_time)
    }

    // TODO: CLSID
    // TODO: state bits
}

// ========================================================================= //

/// Iterator over the entries in a storage object.
pub struct ReadStorage<'a> {
    directory: &'a Vec<DirEntry>,
    path: PathBuf,
    stack: Vec<u32>,
    current: u32,
}

impl<'a> Iterator for ReadStorage<'a> {
    type Item = StorageEntry;

    fn next(&mut self) -> Option<StorageEntry> {
        while self.current != NO_STREAM {
            self.stack.push(self.current);
            self.current = self.directory[self.current as usize].left_sibling;
        }
        if let Some(parent) = self.stack.pop() {
            let dir_entry = &self.directory[parent as usize];
            self.current = dir_entry.right_sibling;
            Some(StorageEntry::new(dir_entry, self.path.join(&dir_entry.name)))
        } else {
            None
        }
    }
}

// ========================================================================= //

/// A stream entry in a compound file, much like a filesystem file.
pub struct Stream<'a, F: 'a> {
    comp: &'a mut CompoundFile<F>,
    stream_id: u32,
    offset_from_start: u64,
    offset_within_sector: usize,
    current_sector: u32,
    finisher: Option<Box<Finish<F>>>,
}

impl<'a, F> Stream<'a, F> {
    fn dir_entry(&self) -> &DirEntry {
        &self.comp.directory[self.stream_id as usize]
    }

    fn dir_entry_mut(&mut self) -> &mut DirEntry {
        &mut self.comp.directory[self.stream_id as usize]
    }

    /// Returns the current length of the stream, in bytes.
    pub fn len(&self) -> u64 { self.dir_entry().stream_len }

    fn is_in_mini_stream(&self) -> bool {
        self.len() < MINI_STREAM_CUTOFF as u64
    }

    fn sector_len(&self) -> usize {
        if self.is_in_mini_stream() {
            MINI_SECTOR_LEN
        } else {
            self.comp.version.sector_len()
        }
    }
}

impl<'a, F: Seek> Stream<'a, F> {
    fn new(comp: &'a mut CompoundFile<F>, stream_id: u32)
           -> io::Result<Stream<'a, F>> {
        let start_sector = comp.directory[stream_id as usize].start_sector;
        let mut stream = Stream {
            comp: comp,
            stream_id: stream_id,
            offset_from_start: 0,
            offset_within_sector: 0,
            current_sector: start_sector,
            finisher: None,
        };
        stream.seek_to_current_position()?;
        Ok(stream)
    }

    fn seek_to_current_position(&mut self) -> io::Result<()> {
        if self.current_sector == END_OF_CHAIN {
            debug_assert_eq!(self.offset_from_start, self.len());
        } else if self.is_in_mini_stream() {
            self.comp
                .seek_within_mini_sector(self.current_sector,
                                         self.offset_within_sector as u64)?;
        } else {
            self.comp
                .seek_within_sector(self.current_sector,
                                    self.offset_within_sector as u64)?;
        }
        Ok(())
    }

    fn advance_to_next_sector(&mut self) -> io::Result<()> {
        debug_assert_ne!(self.current_sector, END_OF_CHAIN);
        debug_assert_eq!(self.offset_within_sector, self.sector_len());
        self.offset_within_sector = 0;
        let is_mini = self.is_in_mini_stream();
        self.current_sector = if is_mini {
            self.comp.minifat[self.current_sector as usize]
        } else {
            self.comp.fat[self.current_sector as usize]
        };
        self.seek_to_current_position()
    }
}

impl<'a, F: Write + Seek> Stream<'a, F> {
    // TODO: pub fn set_len(&mut self, size: u64) -> io::Result<()>

    fn mark_modified(&mut self) {
        if self.finisher.is_none() {
            let finisher: Box<Finish<F>> =
                Box::new(UpdateDirEntry::new(self.stream_id));
            self.finisher = Some(finisher);
        }
    }
}

impl<'a, F: Seek> Seek for Stream<'a, F> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let total_len = self.len();
        let new_pos = match pos {
            SeekFrom::Start(delta) => delta as i64,
            SeekFrom::End(delta) => delta + total_len as i64,
            SeekFrom::Current(delta) => delta + self.offset_from_start as i64,
        };
        if new_pos < 0 || (new_pos as u64) > total_len {
            invalid_input!("Cannot seek to {}, stream length is {}",
                           new_pos,
                           total_len);
        }
        let old_pos = self.offset_from_start as u64;
        let new_pos = new_pos as u64;
        if new_pos != self.offset_from_start {
            let is_mini = self.is_in_mini_stream();
            let sector_len = self.sector_len() as u64;
            let mut offset = new_pos;
            let mut sector = self.dir_entry().start_sector;
            while offset >= sector_len {
                sector = if is_mini {
                    self.comp.minifat[sector as usize]
                } else {
                    self.comp.fat[sector as usize]
                };
                offset -= sector_len;
            }
            self.current_sector = sector;
            self.offset_within_sector = offset as usize;
            self.offset_from_start = new_pos;
            self.seek_to_current_position()?;
        }
        Ok(old_pos)
    }
}

impl<'a, F: Read + Seek> Read for Stream<'a, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let total_len = self.len();
        debug_assert!(self.offset_from_start <= total_len);
        let remaining_in_file = total_len - self.offset_from_start;
        let sector_len = self.sector_len();
        debug_assert!(self.offset_within_sector <= sector_len);
        if self.offset_within_sector == sector_len {
            self.advance_to_next_sector()?;
        }
        debug_assert!(self.offset_within_sector < sector_len);
        let remaining_in_sector = sector_len - self.offset_within_sector;
        let max_len = cmp::min(buf.len() as u64,
                               cmp::min(remaining_in_file,
                                        remaining_in_sector as u64)) as
                      usize;
        if max_len == 0 {
            return Ok(0);
        }
        let bytes_read = self.comp.inner.read(&mut buf[0..max_len])?;
        self.offset_within_sector += bytes_read;
        debug_assert!(self.offset_within_sector <= sector_len);
        self.offset_from_start += bytes_read as u64;
        debug_assert!(self.offset_from_start <= total_len);
        Ok(bytes_read)
    }
}

impl<'a, F: Write + Seek> Write for Stream<'a, F> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.mark_modified();
        let sector_len = self.sector_len();
        debug_assert!(self.offset_within_sector <= sector_len);
        if self.offset_within_sector == sector_len {
            self.advance_to_next_sector()?;
        }
        if self.current_sector == END_OF_CHAIN {
            debug_assert_eq!(self.offset_from_start, self.len());
            debug_assert_eq!(self.offset_within_sector, 0);
            let start_sector = self.dir_entry().start_sector;
            self.current_sector = if start_sector == END_OF_CHAIN {
                debug_assert!(self.is_in_mini_stream());
                let sector = self.comp.allocate_mini_sector(END_OF_CHAIN)?;
                self.dir_entry_mut().start_sector = sector;
                sector
            } else if self.is_in_mini_stream() {
                self.comp.extend_mini_chain(start_sector)?
            } else {
                self.comp.extend_chain(start_sector)?
            };
            self.seek_to_current_position()?;
        }
        debug_assert_ne!(self.current_sector, END_OF_CHAIN);
        debug_assert!(self.offset_within_sector < sector_len);
        let remaining_in_sector = sector_len - self.offset_within_sector;
        let max_len = cmp::min(buf.len() as u64, remaining_in_sector as u64) as
                      usize;
        debug_assert!(max_len > 0);
        let bytes_written = self.comp.inner.write(&buf[0..max_len])?;
        self.offset_within_sector += bytes_written;
        debug_assert!(self.offset_within_sector <= sector_len);
        self.offset_from_start += bytes_written as u64;
        if self.offset_from_start > self.len() {
            let was_mini = self.is_in_mini_stream();
            self.dir_entry_mut().stream_len = self.offset_from_start;
            if was_mini && !self.is_in_mini_stream() {
                // TODO: migrate out of mini stream
                panic!("migrating out of mini stream not supported");
            }
        }
        debug_assert!(self.offset_from_start <= self.len());
        Ok(bytes_written)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(finisher) = self.finisher.take() {
            finisher.finish(self.comp)?;
            self.seek_to_current_position()?;
        }
        self.comp.inner.flush()
    }
}

impl<'a, F> Drop for Stream<'a, F> {
    fn drop(&mut self) {
        if let Some(finisher) = self.finisher.take() {
            let _ = finisher.finish(self.comp);
        }
    }
}

// ========================================================================= //

trait Finish<F> {
    fn finish(&self, comp: &mut CompoundFile<F>) -> io::Result<()>;
}

struct UpdateDirEntry {
    stream_id: u32,
}

impl UpdateDirEntry {
    fn new(stream_id: u32) -> UpdateDirEntry {
        UpdateDirEntry { stream_id: stream_id }
    }
}

impl<F: Write + Seek> Finish<F> for UpdateDirEntry {
    fn finish(&self, comp: &mut CompoundFile<F>) -> io::Result<()> {
        comp.seek_within_dir_entry(self.stream_id, 0)?;
        let dir_entry = &mut comp.directory[self.stream_id as usize];
        dir_entry.modified_time = internal::time::current_timestamp();
        dir_entry.write(&mut comp.inner)?;
        Ok(())
    }
}

// ========================================================================= //

/// The CFB format version to use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Version {
    /// Version 3, which uses 512-byte sectors.
    V3,
    /// Version 4, which uses 4096-byte sectors.
    V4,
}

impl Version {
    fn from_number(number: u16) -> Option<Version> {
        match number {
            3 => Some(Version::V3),
            4 => Some(Version::V4),
            _ => None,
        }
    }

    fn number(self) -> u16 {
        match self {
            Version::V3 => 3,
            Version::V4 => 4,
        }
    }

    fn sector_shift(self) -> u16 {
        match self {
            Version::V3 => 9, // 512-byte sectors
            Version::V4 => 12, // 4096-byte sectors
        }
    }

    fn sector_len(self) -> usize { 1 << (self.sector_shift() as usize) }

    fn stream_len_mask(self) -> u64 {
        match self {
            Version::V3 => 0xffffffff,
            Version::V4 => 0xffffffffffffffff,
        }
    }
}

// ========================================================================= //

#[cfg(test)]
mod tests {
    use super::{CompoundFile, ROOT_DIR_NAME, Version};
    use std::io::Cursor;

    #[test]
    #[should_panic(expected = "Invalid CFB file (wrong magic number)")]
    fn wrong_magic_number() {
        let cursor = Cursor::new([1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        CompoundFile::open(cursor).unwrap();
    }

    #[test]
    fn write_and_read_empty_compound_file() {
        let version = Version::V3;

        let cursor = Cursor::new(Vec::new());
        let comp = CompoundFile::create_with_version(cursor, version)
            .expect("create");
        assert_eq!(comp.version(), version);
        assert_eq!(comp.entry("/").unwrap().name(), ROOT_DIR_NAME);

        let cursor = comp.into_inner();
        assert_eq!(cursor.get_ref().len(), 3 * version.sector_len());
        let comp = CompoundFile::open(cursor).expect("open");
        assert_eq!(comp.version(), version);
        assert_eq!(comp.entry("/").unwrap().name(), ROOT_DIR_NAME);
    }

    #[test]
    fn empty_compound_file_has_no_children() {
        let cursor = Cursor::new(Vec::new());
        let comp = CompoundFile::create_with_version(cursor, Version::V4)
            .expect("create");
        assert!(comp.entry("/").unwrap().is_root());
        assert_eq!(comp.read_storage("/").unwrap().count(), 0);
    }
}

// ========================================================================= //
