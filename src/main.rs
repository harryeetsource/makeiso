use std::fs::File;
use std::io::{self, Write, stdin};
use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use windows::core::{PCWSTR, Error as WinError};
use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, GENERIC_READ, HANDLE, BOOL, ERROR_CRC};
use windows::Win32::Storage::FileSystem::{CreateFileW, ReadFile, FILE_SHARE_READ, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::System::Ioctl::{IOCTL_DISK_GET_DRIVE_GEOMETRY, DISK_GEOMETRY};
use windows::Win32::System::IO::DeviceIoControl;
use std::fs::rename;
use core::time::Duration;
use core::error::Error;
fn main() -> io::Result<()> {
    // Prompt the user for a drive letter
    println!("Enter the drive letter (e.g., A):");
    let mut drive_letter = String::new();
    stdin().read_line(&mut drive_letter)
        .expect("Failed to read line");

    // Trim the input and format it into the physical drive path syntax
    let drive_letter = drive_letter.trim().to_uppercase();
    if drive_letter.len() != 1 || !drive_letter.chars().next().unwrap().is_alphabetic() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid drive letter"));
    }
    let drive_path = format!(r#"\\.\{}:"# , drive_letter);
    let drive_path_wide: Vec<u16> = std::ffi::OsStr::new(&drive_path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let drive_path_pcwstr = PCWSTR(drive_path_wide.as_ptr());

    unsafe {
        let disk_handle = CreateFileW(
            drive_path_pcwstr,
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE(0 as *mut _),
        ).unwrap();

        if disk_handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let mut disk_geometry: DISK_GEOMETRY = std::mem::zeroed();
        let mut returned_bytes: u32 = 0;

        if DeviceIoControl(
            disk_handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY,
            None,
            0,
            Some(&mut disk_geometry as *mut _ as *mut _),
            std::mem::size_of::<DISK_GEOMETRY>() as u32,
            Some(&mut returned_bytes as *mut _ as *mut _),
            None,
        ).is_err()
        {
            CloseHandle(disk_handle).unwrap();
            return Err(io::Error::last_os_error());
        }

        let total_size = disk_geometry.Cylinders
            * disk_geometry.TracksPerCylinder as i64
            * disk_geometry.SectorsPerTrack as i64
            * disk_geometry.BytesPerSector as i64;

        let mut iso_file = File::create("output.iso")?;
        // Dynamically determine buffer size based on the sector size
        let sector_size = disk_geometry.BytesPerSector as usize;
        // Example: Setting the buffer size to be 1024 times the sector size
        let buffer_size = sector_size * 1024;
        let mut buffer = vec![0u8; buffer_size];
        let mut bytes_read: u32 = 0;
        let mut total_bytes_read: i64 = 0;

        let result = (|| -> io::Result<()> {
            while total_bytes_read < total_size {
                let success: Result<(), WinError> = ReadFile(
                    disk_handle,
                    Some(&mut buffer),
                    Some(&mut bytes_read),
                    None,
                );

                match success {
                    Ok(()) => {
                        if bytes_read == 0 {
                            break;
                        }

                        iso_file.write_all(&buffer[..bytes_read as usize])?;
                        total_bytes_read += bytes_read as i64;

                        // Ensure progress doesn't exceed 100%
                        let progress = (total_bytes_read.min(total_size) as f64 / total_size as f64) * 100.0;
                        println!("Progress: {:.2}%", progress);
                    }
                    Err(e) => {
                        if e.code() == ERROR_CRC.into() {
                            eprintln!("Data error (cyclic redundancy check).");
                        } else {
                            return Err(io::Error::new(io::ErrorKind::Other, e));
                        }
                    }
                }
            }
            Ok(())
        })();

        // Close the file explicitly
        iso_file.flush()?;
        drop(iso_file);

        CloseHandle(disk_handle).unwrap();

        result?; // Check the result of the read operation
    }

    // Add a short delay to ensure the system releases the file handle
    std::thread::sleep(Duration::from_secs(1));

    // Rename the file after it is closed
    rename("output.iso", "new_output.iso")?;

    Ok(())
}
