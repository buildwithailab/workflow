//! Only the application backend can access these native credentials.
use anyhow::{bail, Result};
pub(crate) const ERROR: &str = "Güvenli token deposuna erişilemedi. Sistem izinlerini kontrol et.";
pub(crate) const MAX_BYTES: usize = 2500;
fn validate(target: &str) -> Result<()> {
    let suffix = target
        .strip_prefix("local.workflow.desktop.atlassian.")
        .unwrap_or("");
    let Some((profile, kind)) = suffix.split_once('.') else {
        bail!(ERROR);
    };
    if profile.len() != 64
        || !profile.bytes().all(|b| b.is_ascii_hexdigit())
        || !matches!(kind, "jira" | "confluence")
    {
        bail!(ERROR);
    }
    Ok(())
}
pub(crate) fn read(target: &str) -> Result<Option<String>> {
    validate(target)?;
    native::read(target).map_err(|_|anyhow::anyhow!(ERROR))
}
pub(crate) fn write(target: &str, value: &str) -> Result<()> {
    validate(target)?;
    if value.is_empty() || value.len() > MAX_BYTES || value.contains('\0') {
        bail!(ERROR);
    }
    native::write(target, value).map_err(|_|anyhow::anyhow!(ERROR))
}
pub(crate) fn remove(target: &str) -> Result<()> {
    validate(target)?;
    native::remove(target).map_err(|_|anyhow::anyhow!(ERROR))
}

#[cfg(target_os = "macos")]
#[path = "platform/macos/credential_store.rs"]
mod native;
#[cfg(windows)]
mod native {
    use super::*;
    use windows::{
        core::{HRESULT, PCWSTR, PWSTR},
        Win32::{Foundation::ERROR_NOT_FOUND, Security::Credentials::*},
    };
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    fn missing(error: &windows::core::Error) -> bool {
        error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0)
    }
    pub(super) fn read(target: &str) -> Result<Option<String>> {
        let target = wide(target);
        let mut pointer = std::ptr::null_mut();
        unsafe {
            if let Err(error) = CredReadW(
                PCWSTR(target.as_ptr()),
                CRED_TYPE_GENERIC,
                None,
                &mut pointer,
            ) {
                if missing(&error) {
                    return Ok(None);
                }
                bail!(ERROR);
            }
            if pointer.is_null() {
                bail!(ERROR);
            }
            struct Allocation(*mut CREDENTIALW);
            impl Drop for Allocation {
                fn drop(&mut self) {
                    unsafe {
                        let c = &*self.0;
                        if !c.CredentialBlob.is_null() && c.CredentialBlobSize <= MAX_BYTES as u32 {
                            std::ptr::write_bytes(
                                c.CredentialBlob,
                                0,
                                c.CredentialBlobSize as usize,
                            );
                        }
                        CredFree(self.0.cast());
                    }
                }
            }
            let _allocation = Allocation(pointer);
            let value = &*pointer;
            if value.CredentialBlobSize == 0
                || value.CredentialBlobSize as usize > MAX_BYTES
                || value.CredentialBlob.is_null()
            {
                bail!(ERROR);
            }
            let bytes =
                std::slice::from_raw_parts(value.CredentialBlob, value.CredentialBlobSize as usize);
            Ok(Some(
                std::str::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!(ERROR))?
                    .to_owned(),
            ))
        }
    }
    pub(super) fn write(target: &str, value: &str) -> Result<()> {
        let mut target = wide(target);
        let mut username = wide("Workflow");
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            CredentialBlobSize: value.len() as u32,
            CredentialBlob: value.as_ptr() as *mut u8,
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(username.as_mut_ptr()),
            ..Default::default()
        };
        unsafe { CredWriteW(&credential, 0) }.map_err(|_| anyhow::anyhow!(ERROR))
    }
    pub(super) fn remove(target: &str) -> Result<()> {
        let target = wide(target);
        match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(error) if missing(&error) => Ok(()),
            Err(_) => bail!(ERROR),
        }
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
mod native {
    use super::*;
    pub(super) fn read(_: &str) -> Result<Option<String>> {
        bail!(ERROR)
    }
    pub(super) fn write(_: &str, _: &str) -> Result<()> {
        bail!(ERROR)
    }
    pub(super) fn remove(_: &str) -> Result<()> {
        bail!(ERROR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_store_roundtrip_is_isolated_and_never_writes_project_files() {
        let target = format!(
            "local.workflow.desktop.atlassian.{}.jira",
            crate::core::hash(crate::core::id().as_bytes())
        );
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = remove(&self.0);
            }
        }
        let _cleanup = Cleanup(target.clone());
        let first = crate::core::id();
        let second = crate::core::id();
        assert!(read(&target).unwrap().is_none());
        write(&target, &first).unwrap();
        assert!(read(&target).unwrap().as_deref() == Some(&first));
        write(&target, &second).unwrap();
        assert!(read(&target).unwrap().as_deref() == Some(&second));
        remove(&target).unwrap();
        assert!(read(&target).unwrap().is_none());
        remove(&target).unwrap();
        assert!(write("unrelated-target", &first).is_err());
    }
}
