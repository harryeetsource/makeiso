use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write, Read};
use std::path::{Path, PathBuf};
use std::io::ErrorKind;

// Constants for the ISO 9660 format
const BLOCK_SIZE: usize = 2048; // ISO 9660 uses 2KB blocks
const PRIMARY_VOLUME_DESCRIPTOR_TYPE: u8 = 1;
const VOLUME_DESCRIPTOR_TERMINATOR: u8 = 255;

// A helper function to pad data to the next block
fn pad_to_block<W: Write>(writer: &mut W, current_size: usize) -> io::Result<()> {
    let padding_size = (BLOCK_SIZE - (current_size % BLOCK_SIZE)) % BLOCK_SIZE;
    if padding_size > 0 {
        writer.write_all(&vec![0u8; padding_size])?;
    }
    Ok(())
}

// Write a Primary Volume Descriptor (PVD)
fn write_primary_volume_descriptor<W: Write>(writer: &mut W, total_size: u32) -> io::Result<()> {
    let mut volume_descriptor = vec![0u8; BLOCK_SIZE];

    // Set the descriptor type
    volume_descriptor[0] = PRIMARY_VOLUME_DESCRIPTOR_TYPE;

    // Set the standard identifier (ASCII "CD001")
    volume_descriptor[1..6].copy_from_slice(b"CD001");

    // Set the version
    volume_descriptor[6] = 1;

    // Set volume size
    let total_blocks = total_size / BLOCK_SIZE as u32;
    volume_descriptor[80..84].copy_from_slice(&total_blocks.to_le_bytes());
    volume_descriptor[84..88].copy_from_slice(&total_blocks.to_be_bytes());

    // Volume descriptor terminator
    volume_descriptor[0x2C] = VOLUME_DESCRIPTOR_TERMINATOR;

    writer.write_all(&volume_descriptor)?;
    Ok(())
}

// Helper function to write directory records
fn write_directory_record<W: Write>(writer: &mut W, path: &Path, record_start: u32, size: u32) -> io::Result<()> {
    let mut record = vec![0u8; 33];

    // Directory Identifier: Could be "." for current or ".." for parent
    let identifier = path.file_name().unwrap_or_default().to_str().unwrap_or(".");
    let id_len = identifier.len().min(31);

    // Set directory size and location
    record[0] = id_len as u8;
    record[2..6].copy_from_slice(&record_start.to_le_bytes());
    record[10..14].copy_from_slice(&size.to_le_bytes());

    // Write identifier (directory name)
    writer.write_all(&record[..])?;

    Ok(())
}

// Add file contents to the ISO image, handling permission errors and tracking progress
fn add_file<W: Write + Seek>(
    writer: &mut W, 
    file_path: &Path, 
    relative_path: &Path, 
    total_size: u64, 
    bytes_processed: &mut u64
) -> io::Result<()> {
    match File::open(file_path) {
        Ok(mut file) => {
            let file_size = file.metadata()?.len();
            let file_data_start = writer.seek(SeekFrom::Current(0))?;
            let mut buffer = vec![0u8; BLOCK_SIZE];
            let mut total_written = 0;

            // Read and write file contents
            loop {
                let bytes_read = file.read(&mut buffer)?;
                if bytes_read == 0 {
                    break;
                }
                writer.write_all(&buffer[..bytes_read])?;
                total_written += bytes_read as u64;

                // Update the number of processed bytes and show progress
                *bytes_processed += bytes_read as u64;
                let progress = (*bytes_processed as f64 / total_size as f64) * 100.0;
                println!("Progress: {:.2}%", progress);
            }

            // Align to the next block
            pad_to_block(writer, total_written as usize)?;

            // Record file in the directory structure
            write_directory_record(writer, relative_path, file_data_start as u32 / BLOCK_SIZE as u32, file_size as u32)?;
        }
        Err(e) => {
            if e.kind() == io::ErrorKind::PermissionDenied {
                eprintln!("Permission denied while accessing file: {}", file_path.display());
            } else {
                return Err(e);
            }
        }
    }

    Ok(())
}

// Recursively process directories and add them to the ISO, handling permission errors and tracking progress
fn process_directory<W: Write + Seek>(
    writer: &mut W, 
    dir: &Path, 
    iso_root: &Path, 
    total_size: u64, 
    bytes_processed: &mut u64
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative_path = iso_root.join(entry.file_name());

        if path.is_dir() {
            // Handle permission errors when entering directories
            match process_directory(writer, &path, &relative_path, total_size, bytes_processed) {
                Ok(_) => {}
                Err(ref e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    eprintln!("Permission denied while accessing directory: {}", path.display());
                    continue;
                }
                Err(e) => return Err(e),
            }
        } else if path.is_file() {
            add_file(writer, &path, &relative_path, total_size, bytes_processed)?;
        }
    }
    Ok(())
}

// Calculate the total size of all files in the directory, skipping files that cause permission errors
fn calculate_total_size(dir: &Path) -> io::Result<u64> {
    let mut total_size = 0;

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            match calculate_total_size(&path) {
                Ok(size) => total_size += size,
                Err(ref e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    eprintln!("Permission denied while accessing directory: {}", path.display());
                    continue;
                }
                Err(e) => return Err(e),
            }
        } else if path.is_file() {
            match fs::metadata(&path) {
                Ok(metadata) => total_size += metadata.len(),
                Err(ref e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    eprintln!("Permission denied while accessing file: {}", path.display());
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(total_size)
}

fn create_iso(source_dir: &Path, iso_file_path: &Path) -> io::Result<()> {
    let mut iso_file = File::create(iso_file_path)?;

    // Calculate the total size of the source directory, skipping files/directories with permission issues
    let total_size = calculate_total_size(source_dir)?;
    println!("Total size to process: {} bytes", total_size);

    // Write Primary Volume Descriptor (PVD)
    write_primary_volume_descriptor(&mut iso_file, 0)?;

    // Initialize the bytes processed counter
    let mut bytes_processed: u64 = 0;

    // Process the source directory and add files
    process_directory(&mut iso_file, source_dir, Path::new("/"), total_size, &mut bytes_processed)?;

    // First, retrieve the current length of the file
    let current_len = iso_file.metadata()?.len() as usize;

    // Add padding to the end if needed
    pad_to_block(&mut iso_file, current_len)?;

    println!("ISO creation complete.");
    Ok(())
}

fn main() -> io::Result<()> {
    // Prompt the user for the directory to back up
    println!("Enter the directory path to back up:");
    let mut dir_path = String::new();
    io::stdin().read_line(&mut dir_path)?;
    let dir_path = PathBuf::from(dir_path.trim());

    // Prompt the user for the ISO output file
    println!("Enter the ISO output file path:");
    let mut iso_path = String::new();
    io::stdin().read_line(&mut iso_path)?;
    let iso_path = PathBuf::from(iso_path.trim());

    // Create the ISO
    create_iso(&dir_path, &iso_path)?;

    Ok(())
}
