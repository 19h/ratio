use std::{
    ffi::c_void,
    io,
    mem::size_of,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::Path,
    process::Command,
    ptr::null_mut,
};

use anyhow::{Context, Result};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::Threading::{
        CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, GetCurrentProcess, OpenProcessToken,
    },
};

use super::workspace_key;

pub(crate) struct Listener {
    server: NamedPipeServer,
    name: String,
    descriptor: SecurityDescriptor,
}

impl Listener {
    pub(crate) fn bind(workspace: &Path) -> Result<Self> {
        let name = endpoint(workspace);
        let descriptor = SecurityDescriptor::for_current_user()?;
        let server = descriptor
            .open(&name, true)
            .context("bind private Ratio named pipe")?;
        Ok(Self {
            server,
            name,
            descriptor,
        })
    }

    pub(crate) async fn accept(&mut self) -> Result<NamedPipeServer> {
        self.server.connect().await?;
        let next = self.descriptor.open(&self.name, false)?;
        Ok(std::mem::replace(&mut self.server, next))
    }
}

fn endpoint(workspace: &Path) -> String {
    format!(r"\\.\pipe\ratio-{}", workspace_key(workspace))
}

pub(super) async fn connect(workspace: &Path) -> Result<NamedPipeClient> {
    Ok(ClientOptions::new().open(endpoint(workspace))?)
}

pub(super) fn detach(command: &mut Command) {
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

struct SecurityDescriptor(*mut c_void);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: this pointer originates from a Win32 LocalAlloc API.
        unsafe { LocalFree(self.0) };
    }
}

impl SecurityDescriptor {
    fn for_current_user() -> Result<Self> {
        let mut token: HANDLE = null_mut();
        // SAFETY: the output is one writable HANDLE. GetCurrentProcess returns a pseudo-handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error()).context("open Windows user token");
        }
        // SAFETY: OpenProcessToken returned a new handle owned by this call.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut length = 0;
        // SAFETY: the null buffer and zero length request the required size.
        unsafe {
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length)
        };
        if (length as usize) < size_of::<TOKEN_USER>() {
            return Err(io::Error::last_os_error()).context("size Windows user token");
        }
        let mut buffer = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        // SAFETY: buffer is aligned and at least length bytes.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("read Windows user token");
        }
        // SAFETY: a successful TokenUser query writes TOKEN_USER with a SID pointing into buffer.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut sid = null_mut();
        // SAFETY: user.User.Sid is live until buffer is dropped and sid is an output parameter.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
            return Err(io::Error::last_os_error()).context("encode Windows user SID");
        }
        let sid_owner = Self(sid.cast());
        let mut sid_len = 0;
        // SAFETY: ConvertSidToStringSidW returns a NUL-terminated wide string.
        unsafe {
            while *sid.add(sid_len) != 0 {
                sid_len += 1;
            }
        }
        // SAFETY: sid_len was read within the allocation above.
        let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(sid, sid_len) })?;
        drop(sid_owner);
        let sddl = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)")
            .encode_utf16()
            .chain([0])
            .collect::<Vec<_>>();
        let mut descriptor = null_mut();
        // SAFETY: sddl is NUL-terminated and descriptor is a writable output pointer.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error())
                .context("create Windows pipe security descriptor");
        }
        Ok(Self(descriptor))
    }

    fn open(&self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        };
        // SAFETY: the initialized security attributes and descriptor are live for the call.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                )
        }
    }
}
