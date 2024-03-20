extern crate winapi;

use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use winapi::shared::minwindef::{DWORD, FALSE, LPVOID};
use winapi::um::fileapi::{CreateFileW, OPEN_EXISTING};
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::winioctl::{ DISK_GEOMETRY, IOCTL_DISK_GET_DRIVE_GEOMETRY};
use winapi::um::winnt::{FILE_SHARE_READ, GENERIC_READ};
use winapi::um::fileapi::ReadFile;
fn main() -> io::Result<()> {
    unsafe {
        let disk_handle = CreateFileW(
            std::ffi::OsStr::new("\\\\.\\E:")
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
                .as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            null_mut(),
            OPEN_EXISTING,
            0,
            null_mut(),
        );

        if disk_handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let mut disk_geometry: DISK_GEOMETRY = std::mem::zeroed();
        let mut returned_bytes: DWORD = 0;

        if DeviceIoControl(
            disk_handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY,
            null_mut(),
            0,
            &mut disk_geometry as *mut _ as LPVOID,
            std::mem::size_of::<DISK_GEOMETRY>() as DWORD,
            &mut returned_bytes,
            null_mut(),
        ) == FALSE
        {
            return Err(io::Error::last_os_error());
        }

        let total_size = disk_geometry.Cylinders.QuadPart()
            * disk_geometry.TracksPerCylinder as i64
            * disk_geometry.SectorsPerTrack as i64
            * disk_geometry.BytesPerSector as i64;

        let mut iso_file = File::create("output.iso")?;
        let mut buffer = vec![0u8; 512 * 1024]; // 256 KB buffer
        let mut bytes_read: DWORD = 0;
        let mut total_bytes_read: i64 = 0;

        while {
            let result = ReadFile(
                disk_handle,
                buffer.as_mut_ptr() as _,
                buffer.len() as u32,
                &mut bytes_read,
                null_mut(),
            );

            if result == FALSE {
                return Err(io::Error::last_os_error());
            }

            bytes_read > 0
        } {
            iso_file.write_all(&buffer[..bytes_read as usize])?;
            total_bytes_read += bytes_read as i64;
            println!("Progress: {:.2}%", (total_bytes_read as f64 / total_size as f64) * 100.0);
        }

        CloseHandle(disk_handle);
    }

    Ok(())
}
