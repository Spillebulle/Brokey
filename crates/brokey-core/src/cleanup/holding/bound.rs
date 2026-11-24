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
//!   so that while it runs none of them can be renamed or deleted, and so
//!   none can be swapped for another folder by name.
//! - A held folder can still be **turned into a junction in place** by
//!   anyone who may write its attributes: an empty folder Brokey has just
//!   made inherits its parent's permissions, and share modes do not govern
//!   `FILE_WRITE_ATTRIBUTES` or setting a reparse point. So nothing is
//!   created or renamed by a whole path, which the kernel would resolve
//!   again through such a junction: every new file and folder is created,
//!   and every rename made, relative to the held handle of the folder it
//!   goes in (`NtCreateFile` and `FileRenameInfo` with a root directory),
//!   with only its own name. Afterwards that folder is asked again, and
//!   what was made or moved is asked where it is; should either be anywhere
//!   but where its name says, a new file or folder is deleted through its
//!   own handle and a rename is undone, and the step is refused.
//! - A rename is made through the item's own handle, so it moves the object
//!   that was checked, whatever its name leads to by then.
//! - A copy across drives keeps each folder it copies from open the same
//!   way while it copies, and reads each file through a handle opened as
//!   itself.

use super::Disk;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT,
    FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, NtCreateFile,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GENERIC_READ, GENERIC_WRITE, HANDLE, OBJ_CASE_INSENSITIVE,
    RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, DELETE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    FileAttributeTagInfo, FileBasicInfo, FileDispositionInfo, FileDispositionInfoEx,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    OPEN_EXISTING, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

const ATTRIBUTES: u32 = FILE_READ_ATTRIBUTES | SYNCHRONIZE;
/// Shared for reading and writing, never for deleting or renaming: what
/// keeps a folder in its place while the handle is open. Not without
/// `FILE_SHARE_WRITE`: a rename into a folder opens that folder for writing
/// itself, and would be refused by its own pin.
const PINNED: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
const ANY: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
/// `STATUS_OBJECT_NAME_COLLISION`.
const NAME_COLLISION: i32 = 0xC000_0035_u32 as i32;

fn text_of(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .trim_end_matches('\\')
        .to_string()
}

fn key_of(path: &Path) -> String {
    text_of(path).to_lowercase()
}

fn wide(path: &Path) -> Vec<u16> {
    std::ffi::OsStr::new(&format!(r"\\?\{}", text_of(path)))
        .encode_wide()
        .chain([0])
        .collect()
}

/// The last part of a path, as UTF-16 without a terminator.
fn leaf(path: &Path) -> io::Result<Vec<u16>> {
    path.file_name()
        .map(|n| n.encode_wide().collect())
        .ok_or_else(|| io::Error::other(format!("{} has no name of its own", text_of(path))))
}

fn parent_of(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| io::Error::other(format!("{} has no folder above it", text_of(path))))
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

/// Create `path` inside the folder `parent` is open on, by its own name
/// only: the kernel starts from that folder object, so a junction made of
/// it or above it since is not followed. Fails if anything is there; a
/// failure that comes of the folder no longer being itself says so.
fn create_in(parent: RawHandle, path: &Path, access: u32, dir: bool) -> io::Result<OwnedHandle> {
    let mut units = leaf(path)?;
    let object = UNICODE_STRING {
        Length: (units.len() * 2) as u16,
        MaximumLength: (units.len() * 2) as u16,
        Buffer: units.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent as HANDLE,
        ObjectName: &object,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    // SAFETY: plain data, filled in by the call.
    let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let mut handle: HANDLE = std::ptr::null_mut();
    let kind = if dir {
        FILE_DIRECTORY_FILE
    } else {
        FILE_NON_DIRECTORY_FILE
    };
    // SAFETY: `attributes` points at `object`, which points at `units`; all
    // outlive the call. No allocation size and no extended attributes.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            if dir { PINNED } else { 0 },
            FILE_CREATE,
            kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if let Err(e) = nt_result(status) {
        let above = parent_of(path)?;
        return Err(
            if e.kind() != io::ErrorKind::AlreadyExists && check(parent, above, Some(true)).is_err()
            {
                redirected(above)
            } else {
                e
            },
        );
    }
    // SAFETY: a handle just opened, owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
}

/// An NTSTATUS as the Win32 error it stands for.
fn nt_result(status: i32) -> io::Result<()> {
    if status == NAME_COLLISION {
        return Err(io::Error::from_raw_os_error(ERROR_ALREADY_EXISTS as i32));
    }
    if status < 0 {
        // SAFETY: a pure translation of a status code.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(())
}

fn tag_info(handle: RawHandle) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: an open handle and a plain-data out parameter of the size given.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle as _,
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
fn final_path(handle: RawHandle) -> io::Result<String> {
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: `buffer` has the length passed; the call writes at most
        // that many units, or says how many it needs.
        let n = unsafe {
            GetFinalPathNameByHandleW(handle as _, buffer.as_mut_ptr(), buffer.len() as u32, 0)
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
fn check(handle: RawHandle, path: &Path, dir: Option<bool>) -> io::Result<()> {
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
    if final_path(handle)?.to_lowercase() != key_of(path) {
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
    check(handle.as_raw_handle(), path, dir)?;
    Ok(handle)
}

/// A file opened to be copied that has more than one name: a hard link,
/// which anyone may make to a file they can read, a system file among
/// them. Copied as Administrator it would hand its content to whoever made
/// the link, and the removal after the copy would take a name from it.
fn refuse_hard_link(handle: &OwnedHandle, path: &Path) -> io::Result<()> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: an open handle and a plain-data out parameter.
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle() as _, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.nNumberOfLinks > 1 {
        return Err(io::Error::other(super::HardLink(text_of(path))));
    }
    Ok(())
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

/// Delete through the handle, now, and say why not: a folder with anything
/// in it is refused as not empty. With POSIX semantics where the volume has
/// them, so that a file another program holds open is gone from its folder
/// at once and does not keep that folder from going; read-only or not, as
/// `remove_dir_all` did.
fn delete_now(handle: &OwnedHandle) -> io::Result<()> {
    let ex = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: an open handle with DELETE access and a plain-data input of
    // the size given.
    let ok = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle() as _,
            FileDispositionInfoEx,
            (&ex as *const FILE_DISPOSITION_INFO_EX).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    // ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED: a volume without them.
    if !matches!(e.raw_os_error(), Some(87 | 50)) {
        return Err(e);
    }
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: as above.
    let ok = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle() as _,
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Make one folder that must not be there yet, by its whole name. Only for
/// the first folder under a drive, which has no held folder above it.
fn make_one(path: &Path) -> io::Result<()> {
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated; default security, which inherits.
    if unsafe { CreateDirectoryW(name.as_ptr(), std::ptr::null()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Rename through the handle into the folder `into` is open on, as `name`
/// there, never replacing what is there and never resolving a path. Through
/// `NtSetInformationFile`, since the Win32 call refuses a root directory.
fn rename_into(handle: &OwnedHandle, into: RawHandle, name: &[u16]) -> io::Result<()> {
    let bytes = size_of::<FILE_RENAME_INFORMATION>() + name.len() * 2;
    // u64 so the buffer is aligned for the handle inside the struct.
    let mut buffer = vec![0u64; bytes.div_ceil(8)];
    let info = buffer.as_mut_ptr() as *mut FILE_RENAME_INFORMATION;
    // SAFETY: plain data, filled in by the call.
    let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    // SAFETY: `buffer` is large enough and aligned for the struct and the
    // name written after it; nothing else refers to it.
    let status = unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = into as HANDLE;
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        NtSetInformationFile(
            handle.as_raw_handle() as _,
            &mut status_block,
            info.cast(),
            bytes as u32,
            FileRenameInformation,
        )
    };
    nt_result(status)
}

/// A test's hook, called with the item's path and where it goes once
/// everything is opened and checked and before it moves or is made: where
/// a swap would have to happen.
pub type Between = fn(&Path, &Path);

/// The handles one move holds, and so the places it holds still, each with
/// the place it was checked to be, in lower case.
#[derive(Default)]
pub struct Bound {
    /// The folders above the item and above where it goes, there before
    /// the move began. Kept until the move ends.
    kept: Vec<(String, OwnedHandle)>,
    /// Folders a merge writes into. Kept until the move ends.
    entered: Vec<(String, OwnedHandle)>,
    /// What a copy holds while it copies: the folders it copies from and
    /// the ones it made. Let go of before anything is removed.
    copying: Vec<(String, OwnedHandle)>,
    pub between: Option<Between>,
}

impl Bound {
    /// Every folder from the first under the drive down to `dir`, opened as
    /// itself, checked, and kept open without `FILE_SHARE_DELETE`. With
    /// `create`, one that is missing is made inside the one above it, then
    /// checked, before the next is made inside it.
    pub fn pin_dir(&mut self, dir: &Path, create: bool) -> io::Result<()> {
        let text = text_of(dir);
        let bytes = text.as_bytes();
        if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
            return Err(io::Error::other(format!("{text} is not a full path")));
        }
        let mut prefix = text[..2].to_string();
        let mut above: Option<RawHandle> = None;
        for name in text[2..].split('\\').filter(|n| !n.is_empty()) {
            prefix.push('\\');
            prefix.push_str(name);
            let path = Path::new(&prefix);
            let handle = match open_checked(path, ATTRIBUTES, PINNED, Some(true)) {
                Err(e) if create && not_found(&e) => match above {
                    Some(parent) => self.make_in(parent, path)?,
                    None => {
                        match make_one(path) {
                            Err(e) if e.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) => {
                                return Err(e);
                            }
                            _ => {}
                        }
                        open_checked(path, ATTRIBUTES, PINNED, Some(true))?
                    }
                },
                other => other?,
            };
            above = Some(handle.as_raw_handle());
            self.kept.push((key_of(path), handle));
        }
        Ok(())
    }

    /// A folder made inside `parent` and checked, along with `parent`; one
    /// that turns out to be there already, made since it was looked for, is
    /// opened and checked instead.
    fn make_in(&self, parent: RawHandle, path: &Path) -> io::Result<OwnedHandle> {
        let made = match create_in(parent, path, ATTRIBUTES | DELETE, true) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return open_checked(path, ATTRIBUTES, PINNED, Some(true));
            }
            other => other?,
        };
        if let Err(e) = check(parent, parent_of(path)?, Some(true))
            .and_then(|()| check(made.as_raw_handle(), path, Some(true)))
        {
            delete_on_close(&made);
            return Err(if not_found(&e) { redirected(path) } else { e });
        }
        Ok(made)
    }

    /// The held handle of the folder `dir`, the most recent first; one not
    /// held is opened, checked and held while the copy runs.
    fn folder(&mut self, dir: &Path) -> io::Result<RawHandle> {
        let key = key_of(dir);
        let held = self
            .copying
            .iter()
            .rev()
            .chain(self.entered.iter().rev())
            .chain(self.kept.iter().rev())
            .find(|(k, _)| *k == key);
        if let Some((_, handle)) = held {
            return Ok(handle.as_raw_handle());
        }
        let handle = open_checked(dir, ATTRIBUTES, PINNED, Some(true))?;
        let raw = handle.as_raw_handle();
        self.copying.push((key, handle));
        Ok(raw)
    }

    fn hook(&self, from: &Path, to: &Path) {
        if let Some(between) = self.between {
            between(from, to);
        }
    }
}

impl Disk for Bound {
    fn make_parent(&mut self, dir: &Path) -> io::Result<()> {
        self.pin_dir(dir, true)
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        let item = open_checked(from, DELETE | ATTRIBUTES, PINNED, None)?;
        let (from_dir, to_dir) = (parent_of(from)?, parent_of(to)?);
        let home = self.folder(from_dir)?;
        let into = self.folder(to_dir)?;
        self.hook(from, to);
        if let Err(e) = rename_into(&item, into, &leaf(to)?) {
            // Refused, perhaps because the folder is no longer what it was:
            // then that is what is said.
            return Err(match check(into, to_dir, Some(true)) {
                Err(_) => redirected(to_dir),
                Ok(()) => e,
            });
        }
        // Asked again: the folder it went into, as itself, and the item
        // where it now is. Anything else and the rename is undone.
        let landed = check(into, to_dir, Some(true)).and_then(|()| {
            if final_path(item.as_raw_handle())?.to_lowercase() == key_of(to) {
                Ok(())
            } else {
                Err(redirected(to))
            }
        });
        if let Err(e) = landed {
            let e = if not_found(&e) { redirected(to_dir) } else { e };
            return match rename_into(&item, home, &leaf(from)?) {
                Ok(()) => Err(e),
                Err(undo) => Err(io::Error::other(format!(
                    "{e}, and it could not be put back where it was ({undo}): look for it in \
                     {}",
                    text_of(to_dir)
                ))),
            };
        }
        Ok(())
    }

    fn keep_source(&mut self, dir: &Path) -> io::Result<()> {
        let handle = open_checked(dir, ATTRIBUTES, PINNED, Some(true))?;
        self.copying.push((key_of(dir), handle));
        Ok(())
    }

    fn make_dir(&mut self, dir: &Path) -> io::Result<()> {
        let parent = self.folder(parent_of(dir)?)?;
        self.hook(dir, dir);
        let made = self.make_in(parent, dir)?;
        self.copying.push((key_of(dir), made));
        Ok(())
    }

    fn copy_file(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        let source = open_checked(from, GENERIC_READ | SYNCHRONIZE, ANY, Some(false))?;
        refuse_hard_link(&source, from)?;
        let to_dir = parent_of(to)?;
        let parent = self.folder(to_dir)?;
        let target = create_in(
            parent,
            to,
            GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            false,
        )?;
        if let Err(e) = check(parent, to_dir, Some(true))
            .and_then(|()| check(target.as_raw_handle(), to, Some(false)))
        {
            delete_on_close(&target);
            return Err(if not_found(&e) { redirected(to_dir) } else { e });
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
        self.entered.push((key_of(dir), handle));
        Ok(())
    }

    fn release(&mut self) {
        self.copying.clear();
    }

    /// Through the item's own handle, opened as itself and checked to be
    /// where its name says, so that a folder above it swapped for a
    /// junction since the copy is refused, not followed.
    fn remove(&mut self, path: &Path, is_dir: bool) -> io::Result<()> {
        let item = open_checked(path, DELETE | ATTRIBUTES, PINNED, Some(is_dir))?;
        delete_now(&item)
    }
}
