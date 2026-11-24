//! A move bound to what was checked, for the live backend.
//!
//! Checking a path and then acting on its name leaves a gap: anyone who may
//! write a folder on the way can swap a part of it for a junction after the
//! check, and an Administrator's move then follows the junction. So nothing
//! here acts on a name it has not opened as itself
//! (`FILE_FLAG_OPEN_REPARSE_POINT`) and checked: not a name surrogate, and
//! `GetFinalPathNameByHandleW` naming exactly the place asked for.
//!
//! - The folders above an item, and above where it goes, are opened that
//!   way and **kept open without `FILE_SHARE_DELETE`** for the whole move,
//!   so that while it runs none of them can be renamed, deleted or
//!   replaced. One that is missing on the way back is made, opened and
//!   checked before the next is made inside it.
//! - A rename is made through the item's own handle (`FileRenameInfo`), so
//!   it moves the object that was checked, whatever its name leads to by
//!   then.
//! - A copy across drives keeps each folder it copies from open the same
//!   way while it copies, reads each file through a handle opened as itself,
//!   writes each new file and folder with `CREATE_NEW` and checks where it
//!   landed before anything is written into it.

use super::Disk;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    FileAttributeTagInfo, FileBasicInfo, FileDispositionInfo, FileRenameInfo,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, OPEN_EXISTING, SYNCHRONIZE,
    SetFileInformationByHandle,
};

const ATTRIBUTES: u32 = FILE_READ_ATTRIBUTES | SYNCHRONIZE;
/// Shared for reading and writing, never for deleting or renaming: what
/// keeps a folder in its place while the handle is open.
const PINNED: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
const ANY: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

fn text_of(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .trim_end_matches('\\')
        .to_string()
}

fn wide(path: &Path) -> Vec<u16> {
    std::ffi::OsStr::new(&format!(r"\\?\{}", text_of(path)))
        .encode_wide()
        .chain([0])
        .collect()
}

/// The sentence for a place that is not where its name says.
fn redirected(path: &Path) -> io::Error {
    io::Error::other(format!(
        "{} leads somewhere other than its name says, through a junction, a symbolic link or \
         a mount point, so Brokey did not go through it",
        text_of(path)
    ))
}

fn not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

fn open(path: &Path, access: u32, share: u32, disposition: u32) -> io::Result<OwnedHandle> {
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated and outlives the call; no security
    // attributes and no template.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            share,
            std::ptr::null(),
            disposition,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    crate::system::windows::owned_handle(raw).ok_or_else(io::Error::last_os_error)
}

fn tag_info(handle: &OwnedHandle) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: an open handle and a plain-data out parameter of the size given.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle() as _,
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

/// Where an open handle really is, as `X:\…`.
fn final_path(handle: &OwnedHandle) -> io::Result<String> {
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: `buffer` has the length passed; the call writes at most
        // that many units, or says how many it needs.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                handle.as_raw_handle() as _,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buffer.len() {
            let text = String::from_utf16_lossy(&buffer[..n]);
            return Ok(text_of(Path::new(&text)));
        }
        buffer.resize(n + 1, 0);
    }
}

/// That an open handle is the place `path` names, as itself: not a name
/// surrogate, a folder or a file as `dir` says, and nowhere else.
fn check(handle: &OwnedHandle, path: &Path, dir: Option<bool>) -> io::Result<()> {
    let info = tag_info(handle)?;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && info.ReparseTag & 0x2000_0000 != 0
    {
        return Err(redirected(path));
    }
    if let Some(dir) = dir
        && (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != dir
    {
        return Err(io::Error::other(format!(
            "{} is not the {} it was",
            text_of(path),
            if dir { "folder" } else { "file" }
        )));
    }
    if final_path(handle)?.to_lowercase() != text_of(path).to_lowercase() {
        return Err(redirected(path));
    }
    Ok(())
}

fn open_checked(
    path: &Path,
    access: u32,
    share: u32,
    dir: Option<bool>,
) -> io::Result<OwnedHandle> {
    let handle = open(path, access, share, OPEN_EXISTING)?;
    check(&handle, path, dir)?;
    Ok(handle)
}

fn delete_on_close(handle: &OwnedHandle) {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: an open handle with DELETE access and a plain-data input.
    unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle() as _,
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
}

/// Make one folder that must not be there yet.
fn make_one(path: &Path) -> io::Result<()> {
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated; default security, which inherits.
    if unsafe { CreateDirectoryW(name.as_ptr(), std::ptr::null()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Rename through the handle, never replacing what is at `to`.
fn rename_handle(handle: &OwnedHandle, to: &Path) -> io::Result<()> {
    let name: Vec<u16> = wide(to);
    let name = &name[..name.len() - 1];
    let bytes = size_of::<FILE_RENAME_INFO>() + name.len() * 2;
    // u64 so the buffer is aligned for the handle inside the struct.
    let mut buffer = vec![0u64; bytes.div_ceil(8)];
    let info = buffer.as_mut_ptr() as *mut FILE_RENAME_INFO;
    // SAFETY: `buffer` is large enough and aligned for the struct and the
    // name written after it; nothing else refers to it.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        if SetFileInformationByHandle(
            handle.as_raw_handle() as _,
            FileRenameInfo,
            info.cast(),
            bytes as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A test's hook, called with the item's path once everything is opened
/// and checked and before it moves: where a swap would have to happen.
pub type Between = fn(&Path);

/// The handles one move holds, and so the places it holds still.
#[derive(Default)]
pub struct Bound {
    /// The folders above the item and above where it goes, there before
    /// the move began. Kept until the move ends.
    kept: Vec<OwnedHandle>,
    /// Folders a merge writes into. Kept until the move ends.
    entered: Vec<OwnedHandle>,
    /// What a copy holds while it copies: the folders it copies from and
    /// the ones it made. Let go of before anything is removed.
    copying: Vec<OwnedHandle>,
    pub between: Option<Between>,
}

impl Bound {
    /// Every folder from the first under the drive down to `dir`, opened as
    /// itself, checked, and kept open without `FILE_SHARE_DELETE`. With
    /// `create`, one that is missing is made, then opened and checked, before
    /// the next is made inside it.
    pub fn pin_dir(&mut self, dir: &Path, create: bool) -> io::Result<()> {
        let text = text_of(dir);
        let bytes = text.as_bytes();
        if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
            return Err(io::Error::other(format!("{text} is not a full path")));
        }
        let mut prefix = text[..2].to_string();
        for name in text[2..].split('\\').filter(|n| !n.is_empty()) {
            prefix.push('\\');
            prefix.push_str(name);
            let path = Path::new(&prefix);
            let handle = match open_checked(path, ATTRIBUTES, PINNED, Some(true)) {
                Err(e) if create && not_found(&e) => {
                    match make_one(path) {
                        Err(e) if e.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) => {
                            return Err(e);
                        }
                        _ => {}
                    }
                    open_checked(path, ATTRIBUTES, PINNED, Some(true))?
                }
                other => other?,
            };
            self.kept.push(handle);
        }
        Ok(())
    }
}

impl Disk for Bound {
    fn make_parent(&mut self, dir: &Path) -> io::Result<()> {
        self.pin_dir(dir, true)
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        let item = open_checked(from, DELETE | ATTRIBUTES, PINNED, None)?;
        if let Some(between) = self.between {
            between(from);
        }
        rename_handle(&item, to)
    }

    fn keep_source(&mut self, dir: &Path) -> io::Result<()> {
        let handle = open_checked(dir, ATTRIBUTES, PINNED, Some(true))?;
        self.copying.push(handle);
        Ok(())
    }

    fn make_dir(&mut self, dir: &Path) -> io::Result<()> {
        make_one(dir)?;
        let handle = open_checked(dir, ATTRIBUTES, PINNED, Some(true))?;
        self.copying.push(handle);
        Ok(())
    }

    fn copy_file(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        let source = open_checked(from, GENERIC_READ | SYNCHRONIZE, ANY, Some(false))?;
        let target = open(
            to,
            GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE,
            0,
            CREATE_NEW,
        )?;
        if let Err(e) = check(&target, to, Some(false)) {
            delete_on_close(&target);
            return Err(e);
        }
        let mut basic = FILE_BASIC_INFO::default();
        // SAFETY: an open handle and a plain-data out parameter of the size
        // given.
        let have_basic = unsafe {
            GetFileInformationByHandleEx(
                source.as_raw_handle() as _,
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        } != 0;
        let target_raw = target.as_raw_handle();
        let (mut reader, mut writer) = (File::from(source), File::from(target));
        io::copy(&mut reader, &mut writer)?;
        if have_basic {
            // SAFETY: `writer` owns the handle for the call; the input is
            // plain data of the size given. Times and attributes only.
            unsafe {
                SetFileInformationByHandle(
                    target_raw as _,
                    FileBasicInfo,
                    (&basic as *const FILE_BASIC_INFO).cast(),
                    size_of::<FILE_BASIC_INFO>() as u32,
                )
            };
        }
        drop(writer);
        Ok(())
    }

    fn enter(&mut self, dir: &Path) -> io::Result<()> {
        let handle = open_checked(dir, ATTRIBUTES, PINNED, Some(true))?;
        self.entered.push(handle);
        Ok(())
    }

    fn release(&mut self) {
        self.copying.clear();
    }
}
