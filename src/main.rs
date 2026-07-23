use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const BLOCK_SIZE: u32 = 2048;
const SYSTEM_AREA_BLOCKS: u32 = 16;
const PVD_BLOCK: u32 = 16;
const TERMINATOR_BLOCK: u32 = 17;
const FIRST_METADATA_BLOCK: u32 = 18;
const CD001: &[u8; 5] = b"CD001";

#[derive(Debug)]
struct IsoFile {
    source_path: PathBuf,
    iso_name: Vec<u8>,
    byte_len: u32,
    extent_lba: u32,
}

#[derive(Debug)]
struct IsoDirectory {
    source_path: PathBuf,
    iso_name: Vec<u8>,
    parent: usize,
    children_dirs: Vec<usize>,
    children_files: Vec<usize>,
    extent_lba: u32,
    extent_size: u32,
    path_table_number: u16,
}

#[derive(Debug)]
struct IsoLayout {
    directories: Vec<IsoDirectory>,
    files: Vec<IsoFile>,
    directory_order: Vec<usize>,
    path_table_l_lba: u32,
    path_table_m_lba: u32,
    path_table_size: u32,
    total_blocks: u32,
    total_file_bytes: u64,
}

fn blocks_for(bytes: u64) -> io::Result<u32> {
    let blocks = bytes
        .checked_add(u64::from(BLOCK_SIZE) - 1)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "size overflow"))?
        / u64::from(BLOCK_SIZE);

    u32::try_from(blocks)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "ISO exceeds 32-bit LBA range"))
}

fn seek_to_lba<W: Seek>(writer: &mut W, lba: u32) -> io::Result<()> {
    writer.seek(SeekFrom::Start(u64::from(lba) * u64::from(BLOCK_SIZE)))?;
    Ok(())
}

fn write_zeros<W: Write>(writer: &mut W, mut count: u64) -> io::Result<()> {
    const ZEROES: [u8; 8192] = [0; 8192];
    while count > 0 {
        let chunk = usize::try_from(count.min(ZEROES.len() as u64)).unwrap();
        writer.write_all(&ZEROES[..chunk])?;
        count -= chunk as u64;
    }
    Ok(())
}

fn pad_to_block<W: Write + Seek>(writer: &mut W) -> io::Result<()> {
    let position = writer.stream_position()?;
    let block = u64::from(BLOCK_SIZE);
    let padding = (block - (position % block)) % block;
    write_zeros(writer, padding)
}

fn write_u16_both(dst: &mut [u8], value: u16) {
    dst[..2].copy_from_slice(&value.to_le_bytes());
    dst[2..4].copy_from_slice(&value.to_be_bytes());
}

fn write_u32_both(dst: &mut [u8], value: u32) {
    dst[..4].copy_from_slice(&value.to_le_bytes());
    dst[4..8].copy_from_slice(&value.to_be_bytes());
}

fn fill_ascii_field(dst: &mut [u8], value: &str) {
    dst.fill(b' ');
    let bytes = value.as_bytes();
    let len = bytes.len().min(dst.len());
    dst[..len].copy_from_slice(&bytes[..len]);
}

fn normalize_component(name: &str, is_directory: bool) -> Vec<u8> {
    let uppercase = name.to_ascii_uppercase();
    let mut output = String::with_capacity(uppercase.len());

    for ch in uppercase.chars() {
        let valid = ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_' || (!is_directory && ch == '.');
        output.push(if valid { ch } else { '_' });
    }

    if output.is_empty() {
        output.push('_');
    }

    if is_directory {
        output.truncate(31);
        output.into_bytes()
    } else {
        let (stem, extension) = match output.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, Some(ext)),
            _ => (output.as_str(), None),
        };

        let mut stem = stem.to_string();
        stem.truncate(24);

        let mut result = stem;
        if let Some(extension) = extension {
            let mut extension = extension.to_string();
            extension.truncate(3);
            if !extension.is_empty() {
                result.push('.');
                result.push_str(&extension);
            }
        }
        result.push_str(";1");
        result.into_bytes()
    }
}

fn make_unique_name(base: Vec<u8>, used: &mut HashSet<Vec<u8>>, is_directory: bool) -> Vec<u8> {
    if used.insert(base.clone()) {
        return base;
    }

    let base_string = String::from_utf8_lossy(&base);
    let version_suffix = if is_directory { "" } else { ";1" };
    let without_version = base_string.strip_suffix(version_suffix).unwrap_or(&base_string);
    let (stem, extension) = without_version
        .rsplit_once('.')
        .map_or((without_version, ""), |(stem, ext)| (stem, ext));

    for counter in 1u32.. {
        let suffix = format!("_{counter}");
        let max_stem: usize = if is_directory { 31 } else { 24 };
        let keep = max_stem.saturating_sub(suffix.len());
        let mut candidate_stem = stem.chars().take(keep).collect::<String>();
        candidate_stem.push_str(&suffix);

        let candidate = if is_directory {
            candidate_stem
        } else if extension.is_empty() {
            format!("{candidate_stem};1")
        } else {
            format!("{candidate_stem}.{};1", extension.chars().take(3).collect::<String>())
        };

        let bytes = candidate.into_bytes();
        if used.insert(bytes.clone()) {
            return bytes;
        }
    }

    unreachable!()
}

fn record_length(identifier_len: usize) -> usize {
    let padding = if identifier_len % 2 == 0 { 1 } else { 0 };
    33 + identifier_len + padding
}

fn directory_data_size(record_lengths: impl IntoIterator<Item = usize>) -> io::Result<u32> {
    let mut offset = 0usize;

    for length in record_lengths {
        if length > BLOCK_SIZE as usize {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "directory record exceeds one logical block",
            ));
        }

        let remaining = BLOCK_SIZE as usize - (offset % BLOCK_SIZE as usize);
        if length > remaining {
            offset += remaining;
        }
        offset += length;
    }

    let blocks = blocks_for(offset as u64)?;
    blocks
        .checked_mul(BLOCK_SIZE)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "directory size overflow"))
}

fn scan_tree(source_dir: &Path) -> io::Result<(Vec<IsoDirectory>, Vec<IsoFile>)> {
    if !source_dir.is_dir() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("source is not a directory: {}", source_dir.display()),
        ));
    }

    let mut directories = vec![IsoDirectory {
        source_path: source_dir.to_path_buf(),
        iso_name: vec![0],
        parent: 0,
        children_dirs: Vec::new(),
        children_files: Vec::new(),
        extent_lba: 0,
        extent_size: 0,
        path_table_number: 1,
    }];
    let mut files = Vec::new();
    let mut pending = vec![0usize];

    while let Some(dir_index) = pending.pop() {
        let source_path = directories[dir_index].source_path.clone();
        let read_dir = match fs::read_dir(&source_path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                eprintln!("Skipping inaccessible directory: {}", source_path.display());
                continue;
            }
            Err(error) => return Err(error),
        };

        let mut entries = Vec::new();
        for entry in read_dir {
            match entry {
                Ok(entry) => entries.push(entry),
                Err(error) => eprintln!("Skipping unreadable directory entry: {error}"),
            }
        }
        entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_ascii_uppercase());

        let mut used_names = HashSet::new();

        for entry in entries {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(value) => value,
                Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                    eprintln!("Skipping inaccessible entry: {}", path.display());
                    continue;
                }
                Err(error) => return Err(error),
            };

            if file_type.is_symlink() {
                eprintln!("Skipping symbolic link: {}", path.display());
                continue;
            }

            let raw_name = entry.file_name().to_string_lossy().into_owned();

            if file_type.is_dir() {
                let name = make_unique_name(normalize_component(&raw_name, true), &mut used_names, true);
                let child_index = directories.len();
                directories.push(IsoDirectory {
                    source_path: path,
                    iso_name: name,
                    parent: dir_index,
                    children_dirs: Vec::new(),
                    children_files: Vec::new(),
                    extent_lba: 0,
                    extent_size: 0,
                    path_table_number: 0,
                });
                directories[dir_index].children_dirs.push(child_index);
                pending.push(child_index);
            } else if file_type.is_file() {
                let metadata = match entry.metadata() {
                    Ok(value) => value,
                    Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                        eprintln!("Skipping inaccessible file: {}", path.display());
                        continue;
                    }
                    Err(error) => return Err(error),
                };

                let byte_len = match u32::try_from(metadata.len()) {
                    Ok(value) => value,
                    Err(_) => {
                        eprintln!(
                            "Skipping file larger than the ISO 9660 single-extent limit: {}",
                            path.display()
                        );
                        continue;
                    }
                };

                let name = make_unique_name(normalize_component(&raw_name, false), &mut used_names, false);
                let file_index = files.len();
                files.push(IsoFile {
                    source_path: path,
                    iso_name: name,
                    byte_len,
                    extent_lba: 0,
                });
                directories[dir_index].children_files.push(file_index);
            }
        }
    }

    Ok((directories, files))
}

fn breadth_first_directory_order(directories: &[IsoDirectory]) -> Vec<usize> {
    let mut order = Vec::with_capacity(directories.len());
    let mut queue = VecDeque::from([0usize]);

    while let Some(index) = queue.pop_front() {
        order.push(index);
        queue.extend(directories[index].children_dirs.iter().copied());
    }

    order
}

fn path_table_entry_length(identifier_len: usize) -> usize {
    8 + identifier_len + (identifier_len % 2)
}

fn plan_layout(source_dir: &Path) -> io::Result<IsoLayout> {
    let (mut directories, mut files) = scan_tree(source_dir)?;
    let directory_order = breadth_first_directory_order(&directories);

    if directory_order.len() > u16::MAX as usize {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "too many directories for ISO 9660 path table numbering",
        ));
    }

    for (position, &dir_index) in directory_order.iter().enumerate() {
        directories[dir_index].path_table_number = (position + 1) as u16;
    }

    for dir_index in 0..directories.len() {
        let child_dirs = directories[dir_index].children_dirs.clone();
        let child_files = directories[dir_index].children_files.clone();
        let mut lengths = vec![record_length(1), record_length(1)];
        lengths.extend(
            child_dirs
                .iter()
                .map(|&index| record_length(directories[index].iso_name.len())),
        );
        lengths.extend(
            child_files
                .iter()
                .map(|&index| record_length(files[index].iso_name.len())),
        );
        directories[dir_index].extent_size = directory_data_size(lengths)?;
    }

    let path_table_size_usize: usize = directory_order
        .iter()
        .map(|&index| path_table_entry_length(directories[index].iso_name.len()))
        .sum();
    let path_table_size = u32::try_from(path_table_size_usize)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "path table too large"))?;
    let path_table_blocks = blocks_for(u64::from(path_table_size))?;

    let path_table_l_lba = FIRST_METADATA_BLOCK;
    let path_table_m_lba = path_table_l_lba
        .checked_add(path_table_blocks)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "LBA overflow"))?;
    let mut next_lba = path_table_m_lba
        .checked_add(path_table_blocks)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "LBA overflow"))?;

    for &dir_index in &directory_order {
        directories[dir_index].extent_lba = next_lba;
        next_lba = next_lba
            .checked_add(blocks_for(u64::from(directories[dir_index].extent_size))?)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "LBA overflow"))?;
    }

    let mut total_file_bytes = 0u64;
    for file in &mut files {
        file.extent_lba = next_lba;
        next_lba = next_lba
            .checked_add(blocks_for(u64::from(file.byte_len))?)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "LBA overflow"))?;
        total_file_bytes = total_file_bytes
            .checked_add(u64::from(file.byte_len))
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "file byte total overflow"))?;
    }

    Ok(IsoLayout {
        directories,
        files,
        directory_order,
        path_table_l_lba,
        path_table_m_lba,
        path_table_size,
        total_blocks: next_lba,
        total_file_bytes,
    })
}

fn directory_record(identifier: &[u8], extent_lba: u32, data_len: u32, is_directory: bool) -> Vec<u8> {
    let len = record_length(identifier.len());
    let mut record = vec![0u8; len];

    record[0] = len as u8;
    record[1] = 0;
    write_u32_both(&mut record[2..10], extent_lba);
    write_u32_both(&mut record[10..18], data_len);
    // Recording date/time is left as zero when no portable std-only calendar conversion is available.
    record[25] = if is_directory { 0x02 } else { 0x00 };
    record[26] = 0;
    record[27] = 0;
    write_u16_both(&mut record[28..32], 1);
    record[32] = identifier.len() as u8;
    record[33..33 + identifier.len()].copy_from_slice(identifier);
    record
}

fn append_directory_record(buffer: &mut Vec<u8>, record: &[u8]) {
    let offset_in_block = buffer.len() % BLOCK_SIZE as usize;
    let remaining = BLOCK_SIZE as usize - offset_in_block;
    if record.len() > remaining {
        buffer.resize(buffer.len() + remaining, 0);
    }
    buffer.extend_from_slice(record);
}

fn build_directory_extent(layout: &IsoLayout, dir_index: usize) -> Vec<u8> {
    let dir = &layout.directories[dir_index];
    let parent = &layout.directories[dir.parent];
    let mut data = Vec::with_capacity(dir.extent_size as usize);

    append_directory_record(
        &mut data,
        &directory_record(&[0], dir.extent_lba, dir.extent_size, true),
    );
    append_directory_record(
        &mut data,
        &directory_record(&[1], parent.extent_lba, parent.extent_size, true),
    );

    for &child_index in &dir.children_dirs {
        let child = &layout.directories[child_index];
        append_directory_record(
            &mut data,
            &directory_record(&child.iso_name, child.extent_lba, child.extent_size, true),
        );
    }

    for &file_index in &dir.children_files {
        let file = &layout.files[file_index];
        append_directory_record(
            &mut data,
            &directory_record(&file.iso_name, file.extent_lba, file.byte_len, false),
        );
    }

    data.resize(dir.extent_size as usize, 0);
    data
}

fn build_path_table(layout: &IsoLayout, big_endian: bool) -> Vec<u8> {
    let mut table = Vec::with_capacity(layout.path_table_size as usize);

    for &dir_index in &layout.directory_order {
        let dir = &layout.directories[dir_index];
        let identifier = if dir_index == 0 { &[0u8][..] } else { &dir.iso_name };
        table.push(identifier.len() as u8);
        table.push(0);
        if big_endian {
            table.extend_from_slice(&dir.extent_lba.to_be_bytes());
        } else {
            table.extend_from_slice(&dir.extent_lba.to_le_bytes());
        }

        let parent_number = if dir_index == 0 {
            1
        } else {
            layout.directories[dir.parent].path_table_number
        };
        if big_endian {
            table.extend_from_slice(&parent_number.to_be_bytes());
        } else {
            table.extend_from_slice(&parent_number.to_le_bytes());
        }

        table.extend_from_slice(identifier);
        if identifier.len() % 2 == 1 {
            table.push(0);
        }
    }

    table
}

fn write_primary_volume_descriptor<W: Write + Seek>(writer: &mut W, layout: &IsoLayout) -> io::Result<()> {
    let root = &layout.directories[0];
    let mut descriptor = [0u8; BLOCK_SIZE as usize];

    descriptor[0] = 1;
    descriptor[1..6].copy_from_slice(CD001);
    descriptor[6] = 1;
    fill_ascii_field(&mut descriptor[8..40], "RUST ISO WRITER");
    fill_ascii_field(&mut descriptor[40..72], "RUST_ISO_VOLUME");
    write_u32_both(&mut descriptor[80..88], layout.total_blocks);
    write_u16_both(&mut descriptor[120..124], 1);
    write_u16_both(&mut descriptor[124..128], 1);
    write_u16_both(&mut descriptor[128..132], BLOCK_SIZE as u16);
    write_u32_both(&mut descriptor[132..140], layout.path_table_size);
    descriptor[140..144].copy_from_slice(&layout.path_table_l_lba.to_le_bytes());
    descriptor[144..148].copy_from_slice(&0u32.to_le_bytes());
    descriptor[148..152].copy_from_slice(&layout.path_table_m_lba.to_be_bytes());
    descriptor[152..156].copy_from_slice(&0u32.to_be_bytes());

    let root_record = directory_record(&[0], root.extent_lba, root.extent_size, true);
    descriptor[156..156 + 34].copy_from_slice(&root_record[..34]);

    descriptor[881] = 1;
    seek_to_lba(writer, PVD_BLOCK)?;
    writer.write_all(&descriptor)
}

fn write_volume_descriptor_terminator<W: Write + Seek>(writer: &mut W) -> io::Result<()> {
    let mut descriptor = [0u8; BLOCK_SIZE as usize];
    descriptor[0] = 255;
    descriptor[1..6].copy_from_slice(CD001);
    descriptor[6] = 1;
    seek_to_lba(writer, TERMINATOR_BLOCK)?;
    writer.write_all(&descriptor)
}

fn copy_file_extent<W: Write + Seek>(
    writer: &mut W,
    file: &IsoFile,
    bytes_processed: &mut u64,
    total_file_bytes: u64,
) -> io::Result<()> {
    let mut source = File::open(&file.source_path)?;
    seek_to_lba(writer, file.extent_lba)?;

    let mut buffer = [0u8; 64 * 1024];
    let mut remaining = u64::from(file.byte_len);

    while remaining > 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        let read = source.read(&mut buffer[..requested])?;
        if read == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                format!("file changed while being copied: {}", file.source_path.display()),
            ));
        }
        writer.write_all(&buffer[..read])?;
        remaining -= read as u64;
        *bytes_processed += read as u64;

        let progress = if total_file_bytes == 0 {
            100.0
        } else {
            (*bytes_processed as f64 / total_file_bytes as f64) * 100.0
        };
        print!("\rWriting file data: {progress:6.2}%");
        io::stdout().flush()?;
    }

    pad_to_block(writer)
}

fn create_iso(source_dir: &Path, iso_file_path: &Path) -> io::Result<()> {
    let layout = plan_layout(source_dir)?;
    println!("Directories: {}", layout.directories.len());
    println!("Files: {}", layout.files.len());
    println!("File data: {} bytes", layout.total_file_bytes);
    println!("Image size: {} blocks ({} bytes)", layout.total_blocks, u64::from(layout.total_blocks) * u64::from(BLOCK_SIZE));

    let mut iso = File::create(iso_file_path)?;
    iso.set_len(u64::from(layout.total_blocks) * u64::from(BLOCK_SIZE))?;

    seek_to_lba(&mut iso, 0)?;
    write_zeros(&mut iso, u64::from(SYSTEM_AREA_BLOCKS) * u64::from(BLOCK_SIZE))?;
    write_primary_volume_descriptor(&mut iso, &layout)?;
    write_volume_descriptor_terminator(&mut iso)?;

    let path_table_l = build_path_table(&layout, false);
    seek_to_lba(&mut iso, layout.path_table_l_lba)?;
    iso.write_all(&path_table_l)?;

    let path_table_m = build_path_table(&layout, true);
    seek_to_lba(&mut iso, layout.path_table_m_lba)?;
    iso.write_all(&path_table_m)?;

    for &dir_index in &layout.directory_order {
        let extent = build_directory_extent(&layout, dir_index);
        seek_to_lba(&mut iso, layout.directories[dir_index].extent_lba)?;
        iso.write_all(&extent)?;
    }

    let mut bytes_processed = 0u64;
    for file in &layout.files {
        copy_file_extent(&mut iso, file, &mut bytes_processed, layout.total_file_bytes)?;
    }

    if !layout.files.is_empty() {
        println!();
    }
    iso.flush()?;
    println!("ISO creation complete: {}", iso_file_path.display());
    Ok(())
}

fn prompt_path(prompt: &str) -> io::Result<PathBuf> {
    println!("{prompt}");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().trim_matches('"');
    if trimmed.is_empty() {
        return Err(io::Error::new(ErrorKind::InvalidInput, "path cannot be empty"));
    }
    Ok(PathBuf::from(trimmed))
}

fn main() -> io::Result<()> {
    let source_dir = prompt_path("Enter the directory path to place in the ISO:")?;
    let iso_path = prompt_path("Enter the output ISO path:")?;

    if source_dir == iso_path {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "source directory and output path cannot be the same",
        ));
    }

    create_iso(&source_dir, &iso_path)
}
