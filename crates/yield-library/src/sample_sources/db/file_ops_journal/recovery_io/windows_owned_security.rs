//! Conservative original-directory security observations, without effect authority.

const SECURITY_BYTES: usize = 16 * 1024;
const TOKEN_BYTES: usize = 512;
const MAX_ACES: usize = 64;

#[repr(C, align(16))]
struct SnapshotBuffers {
    token: [u8; TOKEN_BYTES],
    security: [u8; SECURITY_BYTES],
}

pub(super) const BUFFER_BYTES: usize = std::mem::size_of::<SnapshotBuffers>();
const _: () = assert!(BUFFER_BYTES == TOKEN_BYTES + SECURITY_BYTES);
// FILE_ALL_ACCESS plus the four generic access bits. Native SDK parity is below.
const KNOWN_RIGHTS: u32 = 0xf01f01ff;
// ADD_FILE, ADD_SUBDIRECTORY, WRITE_EA, DELETE_CHILD, WRITE_ATTRIBUTES;
// DELETE, WRITE_DAC, WRITE_OWNER; GENERIC_WRITE and GENERIC_ALL.
const MUTATION_RIGHTS: u32 = 0x500d0156;
const KNOWN_ACE_FLAGS: u8 = 0x1f;
const INHERIT_ONLY: u8 = 0x08;
// Only ownership/DACL control fields represented by this snapshot grammar.
const KNOWN_CONTROL: u16 = 0x950f;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SnapshotError {
    Malformed,
    ForeignOwner,
    ForeignMutation,
    Unavailable,
    NotDirectory,
    Changed,
}

fn sid(bytes: &[u8], offset: usize) -> Option<&[u8]> {
    if offset % 4 != 0 {
        return None;
    }
    let header = bytes.get(offset..offset.checked_add(8)?)?;
    if header[0] != 1 || header[1] > 15 {
        return None;
    }
    bytes.get(offset..offset.checked_add(8 + usize::from(header[1]) * 4)?)
}

fn token_sid_offset(base: usize, length: usize, address: usize, header: usize) -> Option<usize> {
    base.checked_add(length)?;
    let offset = address.checked_sub(base)?;
    (offset >= header && offset % 4 == 0 && offset.checked_add(8)? <= length).then_some(offset)
}

fn snapshot(bytes: &[u8], account: &[u8]) -> Result<(), SnapshotError> {
    use SnapshotError::*;
    if bytes.len() > SECURITY_BYTES || sid(account, 0) != Some(account) {
        return Err(Malformed);
    }
    let header = bytes.get(..20).ok_or(Malformed)?;
    let control = u16::from_le_bytes(header[2..4].try_into().unwrap());
    if header[0] != 1
        || header[1] != 0
        || control & !KNOWN_CONTROL != 0
        || control & 0x8004 != 0x8004
    {
        return Err(Malformed);
    }
    let offset = |start| u32::from_le_bytes(header[start..start + 4].try_into().unwrap()) as usize;
    let owner_offset = offset(4);
    if owner_offset < 20 {
        return Err(Malformed);
    }
    let owner = sid(bytes, owner_offset).ok_or(Malformed)?;
    let group = offset(8);
    if group != 0 && (group < 20 || sid(bytes, group).is_none()) {
        return Err(Malformed);
    }
    // No SACL was requested or interpreted by this original-handle boundary.
    if offset(12) != 0 {
        return Err(Malformed);
    }
    let acl_offset = offset(16);
    if acl_offset < 20 || acl_offset % 4 != 0 {
        return Err(Malformed);
    }
    let acl_header = bytes
        .get(acl_offset..acl_offset.checked_add(8).ok_or(Malformed)?)
        .ok_or(Malformed)?;
    if !matches!(acl_header[0], 2 | 4) || acl_header[1] != 0 || acl_header[6..8] != [0, 0] {
        return Err(Malformed);
    }
    let acl_length = u16::from_le_bytes(acl_header[2..4].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(acl_header[4..6].try_into().unwrap()) as usize;
    if acl_length < 8 || acl_length % 4 != 0 || count > MAX_ACES {
        return Err(Malformed);
    }
    let acl = bytes
        .get(acl_offset..acl_offset.checked_add(acl_length).ok_or(Malformed)?)
        .ok_or(Malformed)?;
    let mut cursor = 8usize;
    let mut foreign_mutation = false;
    for _ in 0..count {
        let ace_header = acl
            .get(cursor..cursor.checked_add(4).ok_or(Malformed)?)
            .ok_or(Malformed)?;
        if !matches!(ace_header[0], 0 | 1) || ace_header[1] & !KNOWN_ACE_FLAGS != 0 {
            return Err(Malformed);
        }
        let length = u16::from_le_bytes(ace_header[2..4].try_into().unwrap()) as usize;
        if length < 16 || length % 4 != 0 {
            return Err(Malformed);
        }
        let ace = acl
            .get(cursor..cursor.checked_add(length).ok_or(Malformed)?)
            .ok_or(Malformed)?;
        let mask = u32::from_le_bytes(ace[4..8].try_into().unwrap());
        let principal = sid(ace, 8).ok_or(Malformed)?;
        if mask & !KNOWN_RIGHTS != 0 || length != 8 + principal.len() {
            return Err(Malformed);
        }
        if ace_header[0] == 0
            && ace_header[1] & INHERIT_ONLY == 0
            && principal != account
            && mask & MUTATION_RIGHTS != 0
        {
            foreign_mutation = true;
        }
        cursor += length;
    }
    if owner != account {
        Err(ForeignOwner)
    } else if foreign_mutation {
        Err(ForeignMutation)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{
        fs::File,
        os::windows::io::{AsRawHandle, FromRawHandle},
    };
    use windows::Win32::{
        Foundation::{
            CloseHandle, GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, HANDLE,
        },
        Security::{
            CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, GetKernelObjectSecurity,
            GetTokenInformation, INHERIT_ONLY_ACE, INHERITED_ACE, NO_PROPAGATE_INHERIT_ACE,
            OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            SE_DACL_AUTO_INHERIT_REQ, SE_DACL_AUTO_INHERITED, SE_DACL_DEFAULTED, SE_DACL_PRESENT,
            SE_DACL_PROTECTED, SE_GROUP_DEFAULTED, SE_OWNER_DEFAULTED, SE_SELF_RELATIVE,
            TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA,
            FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
            FILE_WRITE_DATA, FILE_WRITE_EA, GetFileInformationByHandle, GetFileType, READ_CONTROL,
            ReOpenFile, WRITE_DAC, WRITE_OWNER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    const _: () = {
        assert!(
            KNOWN_RIGHTS
                == FILE_ALL_ACCESS.0
                    | GENERIC_ALL.0
                    | GENERIC_EXECUTE.0
                    | GENERIC_READ.0
                    | GENERIC_WRITE.0
        );
        assert!(
            MUTATION_RIGHTS
                == FILE_WRITE_DATA.0
                    | FILE_APPEND_DATA.0
                    | FILE_WRITE_EA.0
                    | FILE_DELETE_CHILD.0
                    | FILE_WRITE_ATTRIBUTES.0
                    | DELETE.0
                    | WRITE_DAC.0
                    | WRITE_OWNER.0
                    | GENERIC_WRITE.0
                    | GENERIC_ALL.0
        );
        assert!(
            KNOWN_ACE_FLAGS as u32
                == OBJECT_INHERIT_ACE.0
                    | CONTAINER_INHERIT_ACE.0
                    | NO_PROPAGATE_INHERIT_ACE.0
                    | INHERIT_ONLY_ACE.0
                    | INHERITED_ACE.0
        );
        assert!(
            KNOWN_CONTROL
                == SE_OWNER_DEFAULTED.0
                    | SE_GROUP_DEFAULTED.0
                    | SE_DACL_PRESENT.0
                    | SE_DACL_DEFAULTED.0
                    | SE_DACL_AUTO_INHERIT_REQ.0
                    | SE_DACL_AUTO_INHERITED.0
                    | SE_DACL_PROTECTED.0
                    | SE_SELF_RELATIVE.0
        );
    };

    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Identity {
        serial: u32,
        index: u64,
        created: u64,
    }

    fn identity(file: &File) -> Result<Identity, SnapshotError> {
        let handle = HANDLE(file.as_raw_handle());
        if unsafe { GetFileType(handle) }.0 != 1 {
            return Err(SnapshotError::NotDirectory);
        }
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(handle, &mut info) }
            .map_err(|_| SnapshotError::Unavailable)?;
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        if info.dwFileAttributes & 0x10 == 0
            || info.dwFileAttributes & (0x40 | 0x400) != 0
            || info.dwVolumeSerialNumber == 0
            || index == 0
        {
            return Err(SnapshotError::NotDirectory);
        }
        Ok(Identity {
            serial: info.dwVolumeSerialNumber,
            index,
            created: (u64::from(info.ftCreationTime.dwHighDateTime) << 32)
                | u64::from(info.ftCreationTime.dwLowDateTime),
        })
    }

    pub(in crate::sample_sources::db::file_ops_journal::recovery_io) fn validate(
        file: &File,
    ) -> Result<(), SnapshotError> {
        let original = identity(file)?;
        // The original production root was opened for attributes only. Acquire
        // READ_CONTROL on that exact held object, never by a pathname. Both this
        // read-only handle and the still-live original preserve the delete barrier.
        let reopened = unsafe {
            ReOpenFile(
                HANDLE(file.as_raw_handle()),
                (READ_CONTROL | FILE_READ_ATTRIBUTES).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        }
        .map_err(|_| SnapshotError::Unavailable)?;
        // SAFETY: successful ReOpenFile transfers this owned handle; File closes
        // it on all returns, including token/security/identity refusal.
        let security_file = unsafe { File::from_raw_handle(reopened.0) };
        if identity(&security_file)? != original || identity(file)? != original {
            return Err(SnapshotError::Changed);
        }
        let mut raw = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) }
            .map_err(|_| SnapshotError::Unavailable)?;
        let token = Token(raw);
        // Fixed-size original profile backing is reserved before production
        // construction. Initialize this aligned heap object in place, avoiding
        // a descriptor-sized stack temporary and hint-sized allocation.
        let mut allocation = Box::<SnapshotBuffers>::new_uninit();
        // SAFETY: SnapshotBuffers contains only byte arrays; all-zero initializes
        // every byte, including any padding, before constructing a reference.
        unsafe { allocation.as_mut_ptr().write_bytes(0, 1) };
        let mut allocation = unsafe { allocation.assume_init() };
        let buffers = &mut *allocation;
        let mut token_length = 0u32;
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffers.token.as_mut_ptr().cast()),
                TOKEN_BYTES as u32,
                &mut token_length,
            )
        }
        .map_err(|_| SnapshotError::Unavailable)?;
        let length = token_length as usize;
        if !(std::mem::size_of::<TOKEN_USER>()..=TOKEN_BYTES).contains(&length) {
            return Err(SnapshotError::Malformed);
        }
        // The aligned, complete native header is copied; its SID pointer is then
        // checked against the returned span before any SID bytes are interpreted.
        let user = unsafe { std::ptr::read(buffers.token.as_ptr().cast::<TOKEN_USER>()) };
        let offset = token_sid_offset(
            buffers.token.as_ptr() as usize,
            length,
            user.User.Sid.0 as usize,
            std::mem::size_of::<TOKEN_USER>(),
        )
        .ok_or(SnapshotError::Malformed)?;
        let account = sid(&buffers.token[..length], offset).ok_or(SnapshotError::Malformed)?;
        let mut security_length = 0u32;
        unsafe {
            GetKernelObjectSecurity(
                HANDLE(security_file.as_raw_handle()),
                (OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION).0,
                Some(PSECURITY_DESCRIPTOR(buffers.security.as_mut_ptr().cast())),
                SECURITY_BYTES as u32,
                &mut security_length,
            )
        }
        .map_err(|_| SnapshotError::Unavailable)?;
        let bytes = buffers
            .security
            .get(..security_length as usize)
            .ok_or(SnapshotError::Malformed)?;
        snapshot(bytes, account)?;
        if identity(&security_file)? != original || identity(file)? != original {
            return Err(SnapshotError::Changed);
        }
        Ok(())
    }
}

#[cfg(windows)]
pub(super) use native::validate;

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: u32) -> Vec<u8> {
        let mut value = vec![1, 1, 0, 0, 0, 0, 0, 5];
        value.extend_from_slice(&id.to_le_bytes());
        value
    }
    fn descriptor(owner: &[u8], entries: &[(u8, u8, u32, Vec<u8>)]) -> Vec<u8> {
        let mut value = vec![0; 20];
        value[0] = 1;
        value[2..4].copy_from_slice(&0x8004u16.to_le_bytes());
        value[4..8].copy_from_slice(&20u32.to_le_bytes());
        value.extend_from_slice(owner);
        let acl_offset = value.len();
        value[16..20].copy_from_slice(&(acl_offset as u32).to_le_bytes());
        let mut acl = vec![2, 0, 0, 0, 0, 0, 0, 0];
        acl[4..6].copy_from_slice(&(entries.len() as u16).to_le_bytes());
        for (kind, flags, rights, principal) in entries {
            acl.extend_from_slice(&[*kind, *flags]);
            acl.extend_from_slice(&((8 + principal.len()) as u16).to_le_bytes());
            acl.extend_from_slice(&rights.to_le_bytes());
            acl.extend_from_slice(principal);
        }
        let length = acl.len() as u16;
        acl[2..4].copy_from_slice(&length.to_le_bytes());
        value.extend_from_slice(&acl);
        value
    }

    #[test]
    fn windows_owned_security_owner_only_policy_has_no_privileged_sid_exemption() {
        let user = account(1001);
        let entries = [(0, 0, KNOWN_RIGHTS, user.clone())];
        assert_eq!(snapshot(&descriptor(&user, &entries), &user), Ok(()));
        // SYSTEM (S-1-5-18), another user, and Builtin Administrators.
        let mut administrators = vec![1, 2, 0, 0, 0, 0, 0, 5];
        administrators.extend_from_slice(&32u32.to_le_bytes());
        administrators.extend_from_slice(&544u32.to_le_bytes());
        for foreign in [account(18), account(1002), administrators] {
            for bit in 0..32 {
                let right = 1u32 << bit;
                if MUTATION_RIGHTS & right != 0 {
                    assert_eq!(
                        snapshot(&descriptor(&user, &[(0, 0, right, foreign.clone())]), &user),
                        Err(SnapshotError::ForeignMutation)
                    );
                } else if KNOWN_RIGHTS & right != 0 {
                    assert_eq!(
                        snapshot(&descriptor(&user, &[(0, 0, right, foreign.clone())]), &user),
                        Ok(())
                    );
                }
            }
            assert_eq!(
                snapshot(&descriptor(&foreign, &entries), &user),
                Err(SnapshotError::ForeignOwner)
            );
        }
    }

    #[test]
    fn windows_owned_security_inheritance_and_denies_never_cancel_foreign_allow() {
        let user = account(1001);
        let other = account(1002);
        assert_eq!(
            snapshot(
                &descriptor(&user, &[(0, INHERIT_ONLY, MUTATION_RIGHTS, other.clone())]),
                &user
            ),
            Ok(())
        );
        assert_eq!(
            snapshot(
                &descriptor(&user, &[(1, 0, MUTATION_RIGHTS, other.clone())]),
                &user
            ),
            Ok(())
        );
        for flags in [0, 1, 2, 4, 0x10, 0x17] {
            assert_eq!(
                snapshot(
                    &descriptor(
                        &user,
                        &[
                            (1, 0, MUTATION_RIGHTS, other.clone()),
                            (0, flags, MUTATION_RIGHTS, other.clone())
                        ]
                    ),
                    &user
                ),
                Err(SnapshotError::ForeignMutation)
            );
        }
    }

    #[test]
    fn windows_owned_security_malformed_unknown_and_unbounded_descriptors_refuse() {
        let user = account(1001);
        let good = descriptor(&user, &[(0, 0, KNOWN_RIGHTS, user.clone())]);
        for length in 0..good.len() {
            assert_eq!(
                snapshot(&good[..length], &user),
                Err(SnapshotError::Malformed)
            );
        }
        for (offset, value) in [
            (0, 2),
            (1, 1),
            (2, 0),
            (3, 0x88),
            (4, 21),
            (12, 20),
            (16, 0),
            (32, 3),
            (33, 1),
            (38, 1),
            (40, 2),
            (41, 0x20),
            (42, 15),
            (48, 2),
            (49, 16),
        ] {
            let mut bad = good.clone();
            bad[offset] = value;
            assert_eq!(
                snapshot(&bad, &user),
                Err(SnapshotError::Malformed),
                "offset {offset}"
            );
        }
        assert_eq!(
            snapshot(&descriptor(&user, &[(0, 0, 0x200, user.clone())]), &user),
            Err(SnapshotError::Malformed)
        );
        let entries = vec![(0, 0, 1, user.clone()); MAX_ACES + 1];
        assert_eq!(
            snapshot(&descriptor(&user, &entries), &user),
            Err(SnapshotError::Malformed)
        );
        let mut huge = good.clone();
        huge.resize(SECURITY_BYTES + 1, 0);
        assert_eq!(snapshot(&huge, &user), Err(SnapshotError::Malformed));
        assert_eq!(snapshot(&descriptor(&user, &[]), &user), Ok(()));
    }

    #[test]
    fn windows_owned_security_token_pointer_must_be_aligned_inside_returned_span() {
        assert_eq!(token_sid_offset(1000, 512, 1016, 16), Some(16));
        for address in [999, 1000, 1012, 1017, 1508, usize::MAX] {
            assert_eq!(token_sid_offset(1000, 512, address, 16), None);
        }
        assert_eq!(
            token_sid_offset(usize::MAX - 7, 16, usize::MAX - 3, 0),
            None
        );
    }
}

#[cfg(all(test, windows))]
mod native_tests {
    use super::*;
    use std::{
        fs::{File, OpenOptions},
        os::windows::fs::OpenOptionsExt,
    };

    fn fixture(name: &str) -> File {
        let path = std::env::var_os(name).expect("external Windows ACL fixture is required");
        OpenOptions::new()
            .read(true)
            .custom_flags(0x02000000 | 0x00200000)
            .share_mode(7)
            .open(path)
            .expect("open original no-follow fixture")
    }

    #[test]
    fn windows_owned_security_original_handle_survives_foreign_name_replacement() {
        let private_base =
            std::env::var_os("YIELD_WINDOWS_PRIVATE_DIRECTORY_FIXTURE").expect("private fixture");
        let mutable_base =
            std::env::var_os("YIELD_WINDOWS_MUTABLE_DIRECTORY_FIXTURE").expect("mutable fixture");
        let private_temp = tempfile::tempdir_in(private_base).unwrap();
        let mutable_temp = tempfile::tempdir_in(mutable_base).unwrap();
        let original = private_temp.path().join("original");
        let replacement = mutable_temp.path().join("replacement");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::write(original.join("bytes"), b"original private bytes").unwrap();
        std::fs::write(replacement.join("bytes"), b"foreign mutable bytes").unwrap();
        let open = |path: &std::path::Path| {
            OpenOptions::new()
                .read(true)
                .custom_flags(0x02000000 | 0x00200000)
                .share_mode(7)
                .open(path)
                .unwrap()
        };
        let held = open(&original);
        assert_eq!(validate(&held), Ok(()));
        let moved = private_temp.path().join("moved-original");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::rename(&replacement, &original).unwrap();
        assert_eq!(validate(&held), Ok(()));
        let foreign = open(&original);
        assert_eq!(validate(&foreign), Err(SnapshotError::ForeignMutation));
        assert_eq!(
            std::fs::read(moved.join("bytes")).unwrap(),
            b"original private bytes"
        );
        assert_eq!(
            std::fs::read(original.join("bytes")).unwrap(),
            b"foreign mutable bytes"
        );
        drop(foreign);
        drop(held);
    }

    #[test]
    fn windows_owned_security_actual_token_and_original_directory_fixtures() {
        let private = fixture("YIELD_WINDOWS_PRIVATE_DIRECTORY_FIXTURE");
        assert_eq!(validate(&private), Ok(()));
        assert_eq!(validate(&private), Ok(()));
        let readonly = fixture("YIELD_WINDOWS_READONLY_FOREIGN_DIRECTORY_FIXTURE");
        assert_eq!(validate(&readonly), Ok(()));
        let mutable = fixture("YIELD_WINDOWS_MUTABLE_DIRECTORY_FIXTURE");
        assert_eq!(validate(&mutable), Err(SnapshotError::ForeignMutation));
        let foreign = fixture("YIELD_WINDOWS_FOREIGN_DIRECTORY_FIXTURE");
        assert_eq!(validate(&foreign), Err(SnapshotError::ForeignOwner));
        let file = tempfile::tempfile().expect("ordinary file");
        assert_eq!(validate(&file), Err(SnapshotError::NotDirectory));
    }
}
