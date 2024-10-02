use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write, Read};
use std::path::{Path, PathBuf};
use std::io::ErrorKind;

// Constants for the ISO 9660 format
const BLOCK_SIZE: usize = 2048; // ISO 9660 uses 2KB blocks
const PRIMARY_VOLUME_DESCRIPTOR: u8 = 1;
const CD001: &[u8] = b"CD001";

// Helper function to pad data to the block size
fn pad_to_block<W: Write>(writer: &mut W, current_size: usize) -> io::Result<()> {
    let padding_size = (BLOCK_SIZE - (current_size % BLOCK_SIZE)) % BLOCK_SIZE;
    if padding_size > 0 {
        writer.write_all(&vec![0u8; padding_size])?;
    }
    Ok(())
}

// Write a valid Primary Volume Descriptor (PVD)
fn write_primary_volume_descriptor<W: Write>(writer: &mut W, total_blocks: u32) -> io::Result<()> {
    let mut volume_descriptor = vec![0u8; BLOCK_SIZE];

    // Set the descriptor type (Primary Volume Descriptor)
    volume_descriptor[0] = PRIMARY_VOLUME_DESCRIPTOR;

    // Set the standard identifier ("CD001")
    volume_descriptor[1..6].copy_from_slice(CD001);

    // Set the version number (1)
    volume_descriptor[6] = 1;

    // Set system identifier (can be 32 characters, padded with spaces)
    volume_descriptor[8..40].copy_from_slice(b"RUST_SYSTEM_GENERATED         ");

    // Set volume identifier (can be 32 characters, padded with spaces)
    volume_descriptor[40..72].copy_from_slice(b"RUST_ISO_VOLUME               ");

    // Volume space size (in logical blocks, which are 2048 bytes each)
    volume_descriptor[80..84].copy_from_slice(&total_blocks.to_le_bytes());

    // Logical block size (2048 bytes per block)
    volume_descriptor[128..130].copy_from_slice(&(BLOCK_SIZE as u16).to_le_bytes());

    // Volume set size and volume sequence number
    volume_descriptor[120..122].copy_from_slice(&1u16.to_le_bytes());
    volume_descriptor[124..126].copy_from_slice(&1u16.to_le_bytes());

    // Write the volume descriptor
    writer.write_all(&volume_descriptor)?;

    Ok(())
}

// Helper function to write directory records
fn write_directory_record<W: Write>(writer: &mut W, file_name: &str, start_block: u32, file_size: u32, is_directory: bool) -> io::Result<()> {
    let mut record = vec![0u8; 34 + file_name.len()];

    // Length of the directory record
    record[0] = record.len() as u8;

    // Location of the extent (start block)
    record[2..6].copy_from_slice(&start_block.to_le_bytes());

    // Data length (file size)
    record[10..14].copy_from_slice(&file_size.to_le_bytes());

    // Set file flags
    record[25] = if is_directory { 0x02 } else { 0x00 };

    // File identifier (file name)
    record[32] = file_name.len() as u8;
    record[33..33 + file_name.len()].copy_from_slice(file_name.as_bytes());

    // Write the directory record
    writer.write_all(&record)?;

    Ok(())
}

// Add file contents to the ISO image, handle permission errors, and return the size in blocks
fn add_file<W: Write + Seek>(writer: &mut W, file_path: &Path, bytes_processed: &mut u64, total_size: u64) -> io::Result<u32> {
    match File::open(file_path) {
        Ok(mut file) => {
            let file_size = fs::metadata(file_path)?.len() as u32;
            let mut buffer = vec![0u8; BLOCK_SIZE];
            let mut total_written = 0;

            // Read and write the file contents
            loop {
                let bytes_read = file.read(&mut buffer)?;
                if bytes_read == 0 {
                    break;
                }
                writer.write_all(&buffer[..bytes_read])?;
                total_written += bytes_read as u32;

                // Update progress
                *bytes_processed += bytes_read as u64;
                let progress = (*bytes_processed as f64 / total_size as f64) * 100.0;
                println!("Progress: {:.2}%", progress);
            }

            // Align to the next block
            pad_to_block(writer, total_written as usize)?;

            // Return the number of blocks written
            let blocks_written = (file_size + BLOCK_SIZE as u32 - 1) / BLOCK_SIZE as u32;
            Ok(blocks_written)
        }
        Err(e) => {
            if e.kind() == ErrorKind::PermissionDenied {
                eprintln!("Permission denied while accessing file: {}", file_path.display());
                Ok(0) // Skip file and continue
            } else {
                Err(e)
            }
        }
    }
}

// Recursively process directories and add them to the ISO, handle permission errors and progress
fn process_directory<W: Write + Seek>(writer: &mut W, dir: &Path, start_block: u32, root: bool, total_size: u64, bytes_processed: &mut u64) -> io::Result<u32> {
    let mut block_counter = start_block;

    // Write root directory record
    if root {
        write_directory_record(writer, ".", start_block, 0, true)?;
        write_directory_record(writer, "..", start_block, 0, true)?;
    }

    for entry in fs::read_dir(dir)? {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                let file_name = path.file_name().unwrap().to_str().unwrap();

                if path.is_dir() {
                    // Handle permission errors when entering directories
                    match process_directory(writer, &path, block_counter, false, total_size, bytes_processed) {
                        Ok(dir_size) => {
                            write_directory_record(writer, file_name, block_counter, dir_size * BLOCK_SIZE as u32, true)?;
                            block_counter += dir_size;
                        }
                        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                            eprintln!("Permission denied while accessing directory: {}", path.display());
                            continue; // Skip this directory
                        }
                        Err(e) => return Err(e),
                    }
                } else if path.is_file() {
                    match add_file(writer, &path, bytes_processed, total_size) {
                        Ok(blocks_written) => {
                            let file_size = fs::metadata(&path)?.len() as u32;
                            write_directory_record(writer, file_name, block_counter, file_size, false)?;
                            block_counter += blocks_written;
                        }
                        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                            eprintln!("Permission denied while accessing file: {}", path.display());
                            continue; // Skip this file
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            Err(e) => {
                eprintln!("Error reading directory entry: {}", e);
                continue; // Skip unreadable entries
            }
        }
    }

    Ok(block_counter - start_block)
}

// Calculate the total number of bytes (size) required for the files in the directory
fn calculate_total_size(dir: &Path) -> io::Result<u64> {
    let mut total_size = 0;

    for entry in fs::read_dir(dir)? {
        match entry {
            Ok(entry) => {
                let path = entry.path();

                if path.is_dir() {
                    match calculate_total_size(&path) {
                        Ok(size) => total_size += size,
                        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                            eprintln!("Permission denied while accessing directory: {}", path.display());
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                } else if path.is_file() {
                    match fs::metadata(&path) {
                        Ok(metadata) => total_size += metadata.len(),
                        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                            eprintln!("Permission denied while accessing file: {}", path.display());
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            Err(e) => {
                eprintln!("Error reading directory entry: {}", e);
                continue; // Skip unreadable entries
            }
        }
    }

    Ok(total_size)
}

// Create the ISO from the given source directory with progress tracking and error handling
fn create_iso(source_dir: &Path, iso_file_path: &Path) -> io::Result<()> {
    let mut iso_file = File::create(iso_file_path)?;

    // Calculate the total size of all files in the directory
    let total_size = calculate_total_size(source_dir)?;
    println!("Total size to process: {} bytes", total_size);

    // Calculate total blocks as u64 and cast to u32
    let total_blocks = ((total_size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64) as u32;

    // Write the Primary Volume Descriptor (PVD)
    write_primary_volume_descriptor(&mut iso_file, total_blocks)?;

    // Process the source directory
    let mut bytes_processed = 0u64;
    process_directory(&mut iso_file, source_dir, 20, true, total_size, &mut bytes_processed)?;

    // Add padding and finalize
    let current_len = iso_file.metadata()?.len() as usize;
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
