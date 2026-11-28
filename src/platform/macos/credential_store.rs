//! Profile-scoped Atlassian records in the local macOS Keychain.
//! The caller validates the JSON schema. No record is sent to a process,
//! environment variable, log, or plaintext fallback file by this module.
#![cfg(target_os = "macos")]

use anyhow::{anyhow, Result};
use std::{ffi::c_void, ptr};

const SERVICE: &str = "local.workflow.desktop.atlassian";
const PREFIX: &str = "local.workflow.desktop.atlassian.";
const MAX_BYTES: usize = 2500;
const FAILED: &str = "Güvenli kimlik bilgisi deposuna erişilemedi.";
const SUCCESS: i32 = 0;
const NOT_FOUND: i32 = -25300;
const DUPLICATE: i32 = -25299;
type Ref = *const c_void;

// These declarations follow Security/SecItem.h and CoreFoundation headers.
// CF containers retain their contents using the framework's own callbacks.
#[allow(non_snake_case, non_upper_case_globals)]
mod ffi {
    use super::Ref;
    type Retain = Option<unsafe extern "C" fn(Ref, Ref) -> Ref>;
    type Release = Option<unsafe extern "C" fn(Ref, Ref)>;
    type Description = Option<unsafe extern "C" fn(Ref) -> Ref>;
    type Equal = Option<unsafe extern "C" fn(Ref, Ref) -> u8>;
    #[repr(C)]
    pub(super) struct KeyCallbacks {
        version: isize,
        retain: Retain,
        release: Release,
        description: Description,
        equal: Equal,
        hash: Option<unsafe extern "C" fn(Ref) -> usize>,
    }
    #[repr(C)]
    pub(super) struct ValueCallbacks {
        version: isize,
        retain: Retain,
        release: Release,
        description: Description,
        equal: Equal,
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub(super) static kCFTypeDictionaryKeyCallBacks: KeyCallbacks;
        pub(super) static kCFTypeDictionaryValueCallBacks: ValueCallbacks;
        pub(super) static kCFBooleanTrue: Ref;
        pub(super) static kCFBooleanFalse: Ref;
        pub(super) fn CFStringCreateWithBytes(
            allocator: Ref, bytes: *const u8, length: isize,
            encoding: u32, external_representation: u8,
        ) -> Ref;
        pub(super) fn CFDataCreate(allocator: Ref, bytes: *const u8, length: isize) -> Ref;
        pub(super) fn CFDictionaryCreate(
            allocator: Ref, keys: *const Ref, values: *const Ref, count: isize,
            key_callbacks: *const KeyCallbacks, value_callbacks: *const ValueCallbacks,
        ) -> Ref;
        pub(super) fn CFGetTypeID(value: Ref) -> usize;
        pub(super) fn CFDataGetTypeID() -> usize;
        pub(super) fn CFDataGetLength(value: Ref) -> isize;
        pub(super) fn CFDataGetBytePtr(value: Ref) -> *const u8;
        pub(super) fn CFRelease(value: Ref);
    }
    #[link(name = "Security", kind = "framework")]
    extern "C" {
        pub(super) static kSecClass: Ref;
        pub(super) static kSecClassGenericPassword: Ref;
        pub(super) static kSecAttrService: Ref;
        pub(super) static kSecAttrAccount: Ref;
        pub(super) static kSecAttrSynchronizable: Ref;
        pub(super) static kSecUseAuthenticationUI: Ref;
        pub(super) static kSecUseAuthenticationUIFail: Ref;
        pub(super) static kSecReturnData: Ref;
        pub(super) static kSecMatchLimit: Ref;
        pub(super) static kSecMatchLimitOne: Ref;
        pub(super) static kSecValueData: Ref;
        pub(super) fn SecItemCopyMatching(query: Ref, result: *mut Ref) -> i32;
        pub(super) fn SecItemAdd(attributes: Ref, result: *mut Ref) -> i32;
        pub(super) fn SecItemUpdate(query: Ref, attributes: Ref) -> i32;
        pub(super) fn SecItemDelete(query: Ref) -> i32;
    }
}

struct Owned(Ref);
impl Owned {
    fn new(value: Ref) -> Result<Self> {
        if value.is_null() { Err(anyhow!(FAILED)) } else { Ok(Self(value)) }
    }
    fn string(value: &str) -> Result<Self> {
        let length = isize::try_from(value.len()).map_err(|_| anyhow!(FAILED))?;
        // UTF-8 is length-delimited; no secret is converted to a C string.
        Self::new(unsafe { ffi::CFStringCreateWithBytes(ptr::null(), value.as_ptr(), length, 0x0800_0100, 0) })
    }
    fn data(value: &[u8]) -> Result<Self> {
        let length = isize::try_from(value.len()).map_err(|_| anyhow!(FAILED))?;
        Self::new(unsafe { ffi::CFDataCreate(ptr::null(), value.as_ptr(), length) })
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        // Every non-null Create/Copy result is owned and released exactly once.
        unsafe { ffi::CFRelease(self.0); }
    }
}

fn dictionary(entries: &[(Ref, Ref)]) -> Result<Owned> {
    let keys: Vec<_> = entries.iter().map(|entry| entry.0).collect();
    let values: Vec<_> = entries.iter().map(|entry| entry.1).collect();
    if keys.iter().chain(&values).any(|value| value.is_null()) { return Err(anyhow!(FAILED)); }
    let count = isize::try_from(entries.len()).map_err(|_| anyhow!(FAILED))?;
    // Each argument is a valid CF object. The CFType callbacks retain keys and
    // values, so the dictionary never borrows a dropped account/data handle.
    Owned::new(unsafe {
        ffi::CFDictionaryCreate(ptr::null(), keys.as_ptr(), values.as_ptr(), count,
            ptr::addr_of!(ffi::kCFTypeDictionaryKeyCallBacks),
            ptr::addr_of!(ffi::kCFTypeDictionaryValueCallBacks))
    })
}

fn validate_target(target: &str) -> Result<()> {
    let suffix = target.strip_prefix(PREFIX).ok_or_else(|| anyhow!(FAILED))?;
    let profile = suffix.strip_suffix(".jira").or_else(|| suffix.strip_suffix(".confluence"))
        .ok_or_else(|| anyhow!(FAILED))?;
    if profile.len() != 64 || !profile.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!(FAILED));
    }
    Ok(())
}

fn base_entries(target: &str) -> Result<(Owned, Owned, Vec<(Ref, Ref)>)> {
    validate_target(target)?;
    let service = Owned::string(SERVICE)?;
    let account = Owned::string(target)?;
    let entries = unsafe { vec![
        (ffi::kSecClass, ffi::kSecClassGenericPassword),
        (ffi::kSecAttrService, service.0),
        (ffi::kSecAttrAccount, account.0),
        (ffi::kSecAttrSynchronizable, ffi::kCFBooleanFalse),
        // Background reads must fail rather than opening an authentication UI.
        (ffi::kSecUseAuthenticationUI, ffi::kSecUseAuthenticationUIFail),
    ] };
    Ok((service, account, entries))
}

fn decode(value: &Owned) -> Result<String> {
    if unsafe { ffi::CFGetTypeID(value.0) != ffi::CFDataGetTypeID() } { return Err(anyhow!(FAILED)); }
    let length = usize::try_from(unsafe { ffi::CFDataGetLength(value.0) }).map_err(|_| anyhow!(FAILED))?;
    if !(1..=MAX_BYTES).contains(&length) { return Err(anyhow!(FAILED)); }
    let bytes = unsafe { ffi::CFDataGetBytePtr(value.0) };
    if bytes.is_null() { return Err(anyhow!(FAILED)); }
    // The checked CFData remains alive while its bounded bytes are copied.
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(bytes, length) })
        .map(str::to_owned).map_err(|_| anyhow!(FAILED))
}

pub(crate) fn read(target: &str) -> Result<Option<String>> {
    let (_service, _account, mut entries) = base_entries(target)?;
    entries.extend(unsafe { [
        (ffi::kSecReturnData, ffi::kCFBooleanTrue),
        (ffi::kSecMatchLimit, ffi::kSecMatchLimitOne),
    ] });
    let query = dictionary(&entries)?;
    let mut result = ptr::null();
    let status = unsafe { ffi::SecItemCopyMatching(query.0, &mut result) };
    // Retain a guard even on an unexpected error/result combination.
    let value = if result.is_null() { None } else { Some(Owned(result)) };
    match status {
        NOT_FOUND => Ok(None),
        SUCCESS => decode(value.as_ref().ok_or_else(|| anyhow!(FAILED))?).map(Some),
        _ => Err(anyhow!(FAILED)),
    }
}

pub(crate) fn write(target: &str, value: &str) -> Result<()> {
    validate_target(target)?;
    if !(1..=MAX_BYTES).contains(&value.len()) { return Err(anyhow!(FAILED)); }
    let (_service, _account, mut entries) = base_entries(target)?;
    let query = dictionary(&entries)?;
    let data = Owned::data(value.as_bytes())?;
    let changes = dictionary(&[(unsafe { ffi::kSecValueData }, data.0)])?;
    let status = unsafe { ffi::SecItemUpdate(query.0, changes.0) };
    if status == SUCCESS { return Ok(()); }
    if status != NOT_FOUND { return Err(anyhow!(FAILED)); }
    entries.push((unsafe { ffi::kSecValueData }, data.0));
    let attributes = dictionary(&entries)?;
    match unsafe { ffi::SecItemAdd(attributes.0, ptr::null_mut()) } {
        SUCCESS => Ok(()),
        // A concurrent creator won. One atomic update is enough; never delete
        // first, retry indefinitely, or expose an intermediate empty record.
        DUPLICATE if unsafe { ffi::SecItemUpdate(query.0, changes.0) } == SUCCESS => Ok(()),
        _ => Err(anyhow!(FAILED)),
    }
}

pub(crate) fn remove(target: &str) -> Result<()> {
    let (_service, _account, entries) = base_entries(target)?;
    let query = dictionary(&entries)?;
    match unsafe { ffi::SecItemDelete(query.0) } {
        SUCCESS | NOT_FOUND => Ok(()),
        _ => Err(anyhow!(FAILED)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Synthetic { target: String, cleaned: bool }
    fn target(provider: &str) -> String {
        let hash = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        format!("{PREFIX}{hash}.{provider}")
    }
    impl Synthetic {
        fn new(provider: &str) -> Self {
            Self { target: target(provider), cleaned: false }
        }
        fn cleanup(mut self) -> Result<()> {
            remove(&self.target)?;
            assert!(read(&self.target)?.is_none(), "synthetic cleanup not verified");
            self.cleaned = true;
            Ok(())
        }
    }
    impl Drop for Synthetic {
        fn drop(&mut self) {
            if !self.cleaned && remove(&self.target).is_err() {
                eprintln!("Synthetic Keychain cleanup failed; no record disclosed");
            }
        }
    }
    fn record(provider: &str) -> String {
        format!(r#"{{"baseUrl":"https://{provider}.example.invalid","pat":"{}","rev":"{}"}}"#,
            uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
    }
    #[test]
    fn target_validation_is_exact_and_data_checks_are_byte_bounded() {
        let target = target("jira");
        assert!(validate_target(&target).is_ok());
        for target in [target.replace(".jira", ".other"), target.replace('.', "/"),
            format!("{target}\0"), format!("{PREFIX}{}.jira", "g".repeat(64))] {
            assert!(validate_target(&target).is_err());
        }
        // No native lookup is needed to exercise CF result type/size/UTF-8 guards.
        assert!(decode(&Owned::string("synthetic non-data object").unwrap()).is_err());
        for bytes in [vec![], vec![0xff], vec![b'x'; MAX_BYTES + 1]] {
            assert!(decode(&Owned::data(&bytes).unwrap()).is_err());
        }
        let bounded = Owned::data(&vec![b'x'; MAX_BYTES]).unwrap();
        assert!(decode(&bounded).unwrap().len() == MAX_BYTES);
    }
    #[test]
    #[ignore = "Requires the real macOS Keychain; uses and removes only random synthetic production-namespace targets"]
    fn native_crud_overwrite_isolation_and_cleanup() -> Result<()> {
        for provider in ["jira", "confluence"] {
            let item = Synthetic::new(provider);
            let other = Synthetic::new(provider);
            assert!(read(&item.target)?.is_none(), "new synthetic target was not empty");
            remove(&item.target)?;
            let first = record(provider);
            let replacement = record(provider);
            let other_value = record(provider);
            write(&item.target, &first)?;
            write(&other.target, &other_value)?;
            assert!(read(&item.target)?.as_deref() == Some(first.as_str()), "synthetic read mismatch");
            write(&item.target, &replacement)?;
            assert!(read(&item.target)?.as_deref() == Some(replacement.as_str()), "synthetic overwrite mismatch");
            assert!(read(&other.target)?.as_deref() == Some(other_value.as_str()), "another synthetic target changed");
            assert!(write(&item.target, &"x".repeat(MAX_BYTES + 1)).is_err());
            assert!(write(&item.target, &"é".repeat(MAX_BYTES / 2 + 1)).is_err());
            assert!(write(&item.target, "").is_err());
            assert!(read(&item.target)?.as_deref() == Some(replacement.as_str()), "rejected write changed stored data");
            item.cleanup()?;
            assert!(read(&other.target)?.as_deref() == Some(other_value.as_str()), "deletion affected another target");
            other.cleanup()?;
        }
        Ok(())
    }
    #[test]
    #[ignore = "Requires the real macOS Keychain; races only one generated synthetic target"]
    fn native_concurrent_upserts_remain_complete() -> Result<()> {
        let item = Synthetic::new("jira");
        let values: Vec<_> = (0..4).map(|_| record("jira")).collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(values.len()));
        std::thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = values.iter().map(|value| {
                let barrier = barrier.clone();let target = &item.target;
                scope.spawn(move || { barrier.wait();write(target, value) })
            }).collect();
            for worker in workers { worker.join().map_err(|_| anyhow!(FAILED))??; }
            Ok(())
        })?;
        let stored = read(&item.target)?.ok_or_else(|| anyhow!(FAILED))?;
        assert!(values.contains(&stored), "concurrent upsert produced an incomplete record");
        item.cleanup()
    }
    #[test]
    #[ignore = "Requires the real macOS Keychain; malformed bytes are injected only at one generated synthetic target"]
    fn native_read_bounds_and_utf8_checks_preserve_safe_cleanup() -> Result<()> {
        let item = Synthetic::new("confluence");
        for bytes in [vec![], vec![0xff], vec![b'x'; MAX_BYTES + 1]] {
            let (_service, _account, mut entries) = base_entries(&item.target)?;
            let data = Owned::data(&bytes)?;
            entries.push((unsafe { ffi::kSecValueData }, data.0));
            let attributes = dictionary(&entries)?;
            assert!(unsafe { ffi::SecItemAdd(attributes.0, ptr::null_mut()) } == SUCCESS,
                "synthetic malformed-record setup failed");
            let error = read(&item.target).err().ok_or_else(|| anyhow!(FAILED))?;
            assert!(error.to_string() == FAILED, "storage error exposed unexpected details");
            remove(&item.target)?;
            assert!(read(&item.target)?.is_none(), "malformed synthetic record cleanup failed");
        }
        let prefix = "{\"padding\":\"";
        let suffix = "\"}";
        let boundary = format!("{prefix}{}{suffix}", "x".repeat(MAX_BYTES - prefix.len() - suffix.len()));
        assert!(boundary.len() == MAX_BYTES);
        write(&item.target, &boundary)?;
        assert!(read(&item.target)?.as_deref() == Some(boundary.as_str()), "maximum-size native record mismatch");
        item.cleanup()
    }
}
