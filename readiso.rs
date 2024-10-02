use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::str;

const BLOCK_SIZE: usize = 2048; // ISO 9660 block size
const PRIMARY_VOLUME_DESCRIPTOR: u8 = 1;
const VOLUME_DESCRIPTOR_TERMINATOR: u8 = 255;

/// Primary Volume Descriptor structure
#[derive(Debug)]
struct PrimaryVolumeDescriptor {
    root_directory_extent: u32,
    root_directory_size: u32,
}

impl PrimaryVolumeDescriptor {
    fn from_bytes(data: &[u8]) -> Option<PrimaryVolumeDescriptor> {
        if data[0] != PRIMARY_VOLUME_DESCRIPTOR {
            return None; // Not a Primary Volume Descriptor
        }

        let root_directory_extent = u32::from_le_bytes([data[158], data[159], data[160], data[161]]);
        let root_directory_size = u32::from_le_bytes([data[166], data[167], data[168], data[169]]);

        Some(PrimaryVolumeDescriptor {
            root_directory_extent,
            root_directory_size,
        })
    }
}

/// Directory Record structure
#[derive(Debug)]
struct DirectoryRecord {
    file_name: String,
    extent_location: u32,  // Logical block where the file starts
    data_length: u32,      // Size of the file in bytes
    is_directory: bool,    // Whether this is a directory
}

impl DirectoryRecord {
    fn from_bytes(data: &[u8]) -> Option<DirectoryRecord> {
        let length_of_directory_record = data[0] as usize;
        if length_of_directory_record == 0 {
            return None; // No more records
        }

        let extent_location = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);
        let data_length = u32::from_le_bytes([data[10], data[11], data[12], data[13]]);
        let file_name_length = data[32] as usize;

        let file_name = str::from_utf8(&data[33..33 + file_name_length])
            .ok()?
            .trim_end_matches(";1") // Remove the ISO versioning info
            .to_string();

        let is_directory = data[25] & 0x02 != 0; // Directory flag is bit 1 of flags

        Some(DirectoryRecord {
            file_name,
            extent_location,
            data_length,
            is_directory,
        })
    }
}

/// Read the directory contents and list files
fn read_directory(iso_file: &mut File, start_block: u32, size: u32, indent: usize) -> io::Result<()> {
    // Calculate the number of blocks to read (size is in bytes)
    let num_blocks = (size as usize + BLOCK_SIZE - 1) / BLOCK_SIZE;
    
    for block_num in 0..num_blocks {
        // Seek to the block's position in the ISO file
        let block_offset = (start_block as u64 + block_num as u64) * BLOCK_SIZE as u64;
        iso_file.seek(SeekFrom::Start(block_offset))?;

        // Read the block
        let mut buffer = [0u8; BLOCK_SIZE];
        iso_file.read_exact(&mut buffer)?;

        let mut offset = 0;
        while offset < BLOCK_SIZE {
            if let Some(record) = DirectoryRecord::from_bytes(&buffer[offset..]) {
                // Print the file or directory name with indentation
                let indent_str = " ".repeat(indent);
                println!("{}{}{}", indent_str, if record.is_directory { "[DIR] " } else { "" }, record.file_name);

                // If it's a directory, recursively read its contents
                if record.is_directory && record.file_name != "." && record.file_name != ".." {
                    read_directory(iso_file, record.extent_location, record.data_length, indent + 4)?;
                }

                // Move the offset by the length of the directory record
                offset += buffer[offset] as usize;
            } else {
                break; // No more records
            }
        }
    }

    Ok(())
}

fn main() -> io::Result<()> {
    // Ask the user for the ISO file path
    println!("Enter the path to the ISO file:");
    let mut iso_path = String::new();
    io::stdin().read_line(&mut iso_path)?;
    let iso_path = iso_path.trim(); // Remove any trailing whitespace or newline

    // Open the ISO file
    let mut iso_file = File::open(PathBuf::from(iso_path))?;

    // Seek to the start of the Primary Volume Descriptor (sector 16)
    iso_file.seek(SeekFrom::Start((16 * BLOCK_SIZE) as u64))?;

    // Read the 2048 bytes that represent the Primary Volume Descriptor
    let mut buffer = [0u8; BLOCK_SIZE];
    iso_file.read_exact(&mut buffer)?;

    // Parse the Primary Volume Descriptor
    if let Some(pvd) = PrimaryVolumeDescriptor::from_bytes(&buffer) {
        println!("Primary Volume Descriptor: {:?}", pvd);

        // Read the root directory starting from the root_directory_extent
        read_directory(&mut iso_file, pvd.root_directory_extent, pvd.root_directory_size, 0)?;
    } else {
        println!("Could not read the Primary Volume Descriptor");
    }

    Ok(())
}
