use crate::DaemonService;
use pithos_agent_api::{JsonRpcError, JsonRpcResponse, PublicErrorKind};
use serde_json::Value;
use std::io;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, oneshot};

const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const LISTENER_RETRY_DELAY: Duration = Duration::from_millis(10);
const SERVICE_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
pub const DEFAULT_IPC_FRAME_BYTES: usize = 1024 * 1024;
pub const DEFAULT_IPC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcEndpoint {
    state_dir: PathBuf,
    #[cfg(windows)]
    pipe_name: String,
    #[cfg(unix)]
    socket_path: PathBuf,
}

impl IpcEndpoint {
    pub fn for_state_dir(state_dir: PathBuf) -> Self {
        let state_dir = normalize_state_dir(state_dir);
        #[cfg(windows)]
        let pipe_name = {
            let digest = blake3::hash(state_dir.to_string_lossy().as_bytes());
            let suffix = digest.as_bytes()[..12]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            format!(r"\\.\pipe\pithosd-{suffix}")
        };
        #[cfg(unix)]
        let socket_path = state_dir.join("pithosd.sock");
        Self {
            state_dir,
            #[cfg(windows)]
            pipe_name,
            #[cfg(unix)]
            socket_path,
        }
    }

    #[cfg(windows)]
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    #[cfg(unix)]
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    pub fn display_name(&self) -> String {
        #[cfg(windows)]
        {
            self.pipe_name.clone()
        }
        #[cfg(unix)]
        {
            self.socket_path.to_string_lossy().into_owned()
        }
        #[cfg(not(any(windows, unix)))]
        {
            "unsupported".to_owned()
        }
    }
}

fn normalize_state_dir(path: PathBuf) -> PathBuf {
    let resolved = std::fs::canonicalize(&path)
        .or_else(|_| std::path::absolute(&path).map(lexically_normalize_absolute))
        .unwrap_or(path);
    stable_state_dir_path(resolved)
}

#[cfg(unix)]
fn lexically_normalize_absolute(path: PathBuf) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
            Component::Prefix(_) => unreachable!("Unix paths have no prefix component"),
        }
    }
    normalized
}

#[cfg(not(unix))]
fn lexically_normalize_absolute(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
fn stable_state_dir_path(path: PathBuf) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    const VERBATIM_PREFIX: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const VERBATIM_UNC_PREFIX: &[u16] = &[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];

    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let stable = if let Some(rest) = encoded.strip_prefix(VERBATIM_UNC_PREFIX) {
        let mut value = vec![b'\\' as u16, b'\\' as u16];
        value.extend_from_slice(rest);
        value
    } else if let Some(rest) = encoded.strip_prefix(VERBATIM_PREFIX) {
        rest.to_vec()
    } else {
        encoded
    };
    PathBuf::from(OsString::from_wide(&stable))
}

#[cfg(not(windows))]
fn stable_state_dir_path(path: PathBuf) -> PathBuf {
    path
}

pub(crate) fn prepare_private_state_dir(path: &std::path::Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    protect_windows_state_dir(path)?;
    #[cfg(unix)]
    fs_create_private_dir(path)?;
    #[cfg(not(any(windows, unix)))]
    {
        std::fs::create_dir_all(path)?;
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe IPC state directory",
            ));
        }
    }
    std::fs::canonicalize(path)
}

pub struct IpcServer {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

impl IpcServer {
    pub async fn spawn(service: DaemonService, endpoint: IpcEndpoint) -> io::Result<Self> {
        #[cfg(windows)]
        {
            spawn_windows(service, endpoint).await
        }
        #[cfg(unix)]
        {
            spawn_unix(service, endpoint).await
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = service;
            let _ = endpoint;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local IPC is unsupported on this platform",
            ))
        }
    }

    pub async fn shutdown(mut self) -> io::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task
            .await
            .map_err(|_| io::Error::other("IPC task failed"))?
    }
}

#[cfg(unix)]
pub struct IpcClient {
    stream: tokio::net::UnixStream,
    maximum_frame: usize,
}

#[cfg(windows)]
pub struct IpcClient {
    stream: tokio::net::windows::named_pipe::NamedPipeClient,
    maximum_frame: usize,
}

#[cfg(not(any(windows, unix)))]
pub struct IpcClient;

impl IpcClient {
    pub async fn connect(endpoint: &IpcEndpoint) -> io::Result<Self> {
        Self::connect_with_limit(endpoint, DEFAULT_IPC_FRAME_BYTES).await
    }

    pub async fn connect_with_limit(
        endpoint: &IpcEndpoint,
        maximum_frame: usize,
    ) -> io::Result<Self> {
        validate_frame_limit(maximum_frame)?;
        #[cfg(unix)]
        {
            Ok(Self {
                stream: tokio::net::UnixStream::connect(&endpoint.socket_path).await?,
                maximum_frame,
            })
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ClientOptions;
            let expected_user_sid = current_user_sid_string()?;
            let mut last_error = None;
            for _ in 0..100 {
                match ClientOptions::new().open(&endpoint.pipe_name) {
                    Ok(stream) => {
                        verify_connected_server(&stream, &expected_user_sid)?;
                        return Ok(Self {
                            stream,
                            maximum_frame,
                        });
                    }
                    Err(error) if is_transient_pipe_client_connect_error(&error) => {
                        last_error = Some(error);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(last_error.unwrap_or_else(|| io::Error::other("named pipe unavailable")))
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = endpoint;
            let _ = maximum_frame;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local IPC is unsupported on this platform",
            ))
        }
    }

    pub async fn request(&mut self, request: &Value) -> io::Result<Value> {
        self.request_with_timeout(request, DEFAULT_IPC_RESPONSE_TIMEOUT)
            .await
    }

    pub async fn request_with_timeout(
        &mut self,
        request: &Value,
        response_timeout: Duration,
    ) -> io::Result<Value> {
        request_on_stream_with_timeout(
            &mut self.stream,
            request,
            self.maximum_frame,
            response_timeout,
        )
        .await
    }
}

#[cfg(test)]
async fn request_on_stream<S>(
    stream: &mut S,
    request: &Value,
    maximum_frame: usize,
) -> io::Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    request_on_stream_with_timeout(stream, request, maximum_frame, DEFAULT_IPC_RESPONSE_TIMEOUT)
        .await
}

async fn request_on_stream_with_timeout<S>(
    stream: &mut S,
    request: &Value,
    maximum_frame: usize,
    response_timeout: Duration,
) -> io::Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    validate_frame_limit(maximum_frame)?;
    if response_timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "response timeout must be positive",
        ));
    }
    let bytes = serde_json::to_vec(request)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid request"))?;
    write_frame(stream, &bytes, maximum_frame, FRAME_TIMEOUT).await?;
    let response = read_frame(stream, maximum_frame, response_timeout).await?;
    serde_json::from_slice(&response)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid response"))
}

fn validate_frame_limit(maximum_frame: usize) -> io::Result<()> {
    if maximum_frame == 0 || maximum_frame > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid frame limit",
        ));
    }
    Ok(())
}

fn is_transient_listener_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    ) || is_transient_listener_os_error(error)
}

#[cfg(windows)]
fn is_transient_listener_os_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(109 | 121 | 232 | 233 | 995))
}

#[cfg(not(windows))]
fn is_transient_listener_os_error(_error: &io::Error) -> bool {
    false
}

#[cfg(windows)]
fn is_transient_pipe_client_connect_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(2 | 109 | 121 | 231 | 232 | 233 | 536)
    ) || is_transient_listener_error(error)
}

pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    maximum: usize,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let mut length_bytes = [0_u8; 4];
    tokio::time::timeout(timeout, reader.read_exact(&mut length_bytes))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "frame header timeout"))??;
    let length = u32::from_le_bytes(length_bytes) as usize;
    if length == 0 || length > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame size exceeds limit",
        ));
    }
    let mut payload = vec![0_u8; length];
    tokio::time::timeout(timeout, reader.read_exact(&mut payload))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "frame payload timeout"))??;
    Ok(payload)
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
    maximum: usize,
    timeout: Duration,
) -> io::Result<()> {
    if payload.is_empty() || payload.len() > maximum || payload.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame size exceeds limit",
        ));
    }
    let length = (payload.len() as u32).to_le_bytes();
    tokio::time::timeout(timeout, async {
        writer.write_all(&length).await?;
        writer.write_all(payload).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "frame write timeout"))?
}

async fn handle_connection<S>(
    mut stream: S,
    service: DaemonService,
    connection_id: u64,
    maximum_frame: usize,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _registration = ActiveConnection {
        service: service.clone(),
        connection_id,
    };
    loop {
        let frame = match read_frame(&mut stream, maximum_frame, FRAME_TIMEOUT).await {
            Ok(frame) => frame,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                break;
            }
            Err(_) => break,
        };
        let mut response = service.handle_frame(connection_id, &frame).await;
        if response.len() > maximum_frame {
            response = oversized_response();
        }
        if write_frame(&mut stream, &response, maximum_frame, FRAME_TIMEOUT)
            .await
            .is_err()
        {
            break;
        }
    }
}

struct ActiveConnection {
    service: DaemonService,
    connection_id: u64,
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.service.disconnect(self.connection_id);
    }
}

fn oversized_response() -> Vec<u8> {
    serde_json::to_vec(&JsonRpcResponse::<Value>::error(
        None,
        JsonRpcError::domain(
            PublicErrorKind::ResourceLimit,
            "response exceeds frame limit",
        ),
    ))
    .unwrap_or_default()
}

#[cfg(unix)]
async fn spawn_unix(service: DaemonService, endpoint: IpcEndpoint) -> io::Result<IpcServer> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use tokio::net::UnixListener;

    fs_create_private_dir(&endpoint.state_dir)?;
    let owner_uid = std::fs::metadata(&endpoint.state_dir)?.uid();
    if let Ok(metadata) = std::fs::symlink_metadata(&endpoint.socket_path) {
        if !metadata.file_type().is_socket() || metadata.uid() != owner_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe existing IPC endpoint",
            ));
        }
        match std::os::unix::net::UnixStream::connect(&endpoint.socket_path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "daemon is running",
                ));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(&endpoint.socket_path)?;
            }
            Err(error) => return Err(error),
        }
    }
    let listener = UnixListener::bind(&endpoint.socket_path)?;
    std::fs::set_permissions(
        &endpoint.socket_path,
        std::fs::Permissions::from_mode(0o600),
    )?;
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let max_connections = service.max_connections();
    let maximum_frame = service.max_frame_bytes();
    let socket_path = endpoint.socket_path.clone();
    let connection_ids = Arc::new(AtomicU64::new(1));
    let connection_slots = Arc::new(Semaphore::new(max_connections));
    let task = tokio::spawn(async move {
        let mut connection_tasks = tokio::task::JoinSet::new();
        let listener_result = loop {
            tokio::select! {
                _ = &mut shutdown_rx => break Ok(()),
                completed = connection_tasks.join_next(), if !connection_tasks.is_empty() => {
                    let _ = completed;
                }
                accepted = listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) if is_transient_listener_error(&error) => {
                            tokio::time::sleep(LISTENER_RETRY_DELAY).await;
                            continue;
                        }
                        Err(error) => break Err(error),
                    };
                    let peer_uid = match stream.peer_cred() {
                        Ok(credentials) => credentials.uid(),
                        Err(_) => continue,
                    };
                    if peer_uid != owner_uid {
                        continue;
                    }
                    let permit = match connection_slots.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => continue,
                    };
                    let service = service.clone();
                    let connection_id = connection_ids.fetch_add(1, Ordering::Relaxed);
                    connection_tasks.spawn(async move {
                        handle_connection(stream, service, connection_id, maximum_frame).await;
                        drop(permit);
                    });
                }
            }
        };
        connection_tasks.abort_all();
        while connection_tasks.join_next().await.is_some() {}
        let cleanup_result = match std::fs::symlink_metadata(&socket_path) {
            Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == owner_uid => {
                std::fs::remove_file(socket_path)
            }
            _ => Ok(()),
        };
        let shutdown_result = service
            .shutdown(SERVICE_SHUTDOWN_GRACE)
            .await
            .map_err(|error| io::Error::other(error.message));
        listener_result?;
        cleanup_result?;
        shutdown_result
    });
    Ok(IpcServer {
        shutdown: Some(shutdown_tx),
        task,
    })
}

#[cfg(unix)]
fn fs_create_private_dir(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe IPC state directory",
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(windows)]
mod windows_security {
    use super::*;
    use std::ffi::{OsStr, c_void};
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr::null_mut;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
    #[cfg(test)]
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    #[cfg(test)]
    use windows_sys::Win32::Security::GetFileSecurityW;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetTokenInformation, IsWellKnownSid, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub(super) struct WindowsSecurityDescriptor {
        pointer: PSECURITY_DESCRIPTOR,
    }

    impl WindowsSecurityDescriptor {
        pub(super) fn from_sddl(sddl: &str) -> io::Result<Self> {
            let encoded = wide(OsStr::new(sddl));
            let mut pointer = null_mut();
            let converted = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    encoded.as_ptr(),
                    SDDL_REVISION_1,
                    &mut pointer,
                    null_mut(),
                )
            };
            if converted == 0 || pointer.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { pointer })
        }

        fn attributes(&mut self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.pointer.cast(),
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for WindowsSecurityDescriptor {
        fn drop(&mut self) {
            if !self.pointer.is_null() {
                unsafe {
                    LocalFree(self.pointer as HLOCAL);
                }
            }
        }
    }

    pub(super) fn restricted_security_sddl(user_sid: &str, directory: bool) -> String {
        if directory {
            format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{user_sid})")
        } else {
            format!("D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})")
        }
    }

    pub(super) fn current_user_sid_string() -> io::Result<String> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        token_user_identity(&token).map(|identity| identity.0)
    }

    pub(super) fn protect_windows_state_dir(path: &std::path::Path) -> io::Result<()> {
        let path = std::path::absolute(path)?;
        validate_no_reparse_points(&path)?;
        std::fs::create_dir_all(&path)?;
        validate_no_reparse_points(&path)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || is_reparse_point(&metadata) {
            return Err(unsafe_state_directory());
        }

        let user_sid = current_user_sid_string()?;
        validate_path_owner(&path, &user_sid)?;
        apply_restricted_path_dacl(&path, true, &user_sid)?;
        let mut pending = vec![path];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory)? {
                let entry = entry?;
                let child = entry.path();
                let metadata = std::fs::symlink_metadata(&child)?;
                if is_reparse_point(&metadata) {
                    return Err(unsafe_state_directory());
                }
                validate_path_owner(&child, &user_sid)?;
                apply_restricted_path_dacl(&child, metadata.is_dir(), &user_sid)?;
                if metadata.is_dir() {
                    pending.push(child);
                }
            }
        }
        Ok(())
    }

    fn validate_no_reparse_points(path: &std::path::Path) -> io::Result<()> {
        for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            match std::fs::symlink_metadata(ancestor) {
                Ok(metadata) if is_reparse_point(&metadata) => {
                    return Err(unsafe_state_directory());
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    fn validate_path_owner(path: &std::path::Path, expected_user_sid: &str) -> io::Result<()> {
        let encoded_path = wide(path.as_os_str());
        let mut owner = null_mut();
        let mut descriptor = null_mut();
        let result = unsafe {
            GetNamedSecurityInfoW(
                encoded_path.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        if descriptor.is_null() || owner.is_null() {
            if !descriptor.is_null() {
                unsafe {
                    LocalFree(descriptor as HLOCAL);
                }
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "state path has no security owner",
            ));
        }
        let _descriptor = WindowsSecurityDescriptor {
            pointer: descriptor,
        };
        let owner_is_system = unsafe { IsWellKnownSid(owner, WinLocalSystemSid) } != 0;
        let owner_sid = sid_to_string(owner)?;
        if !owner_identity_is_allowed(&owner_sid, owner_is_system, expected_user_sid) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state path is owned by an unauthorized SID",
            ));
        }
        Ok(())
    }

    pub(super) fn owner_identity_is_allowed(
        owner_sid: &str,
        owner_is_system: bool,
        expected_user_sid: &str,
    ) -> bool {
        owner_is_system || owner_sid == expected_user_sid
    }

    fn apply_restricted_path_dacl(
        path: &std::path::Path,
        directory: bool,
        user_sid: &str,
    ) -> io::Result<()> {
        let descriptor =
            WindowsSecurityDescriptor::from_sddl(&restricted_security_sddl(user_sid, directory))?;
        let encoded_path = wide(path.as_os_str());
        let applied = unsafe {
            SetFileSecurityW(
                encoded_path.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor.pointer,
            )
        };
        if applied == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn create_restricted_pipe(
        options: &ServerOptions,
        pipe_name: &str,
        user_sid: &str,
    ) -> io::Result<NamedPipeServer> {
        let mut descriptor =
            WindowsSecurityDescriptor::from_sddl(&restricted_security_sddl(user_sid, false))?;
        let mut attributes = descriptor.attributes();
        unsafe {
            options.create_with_security_attributes_raw(
                pipe_name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
            )
        }
    }

    pub(super) fn verify_connected_client(
        pipe: &NamedPipeServer,
        expected_user_sid: &str,
    ) -> io::Result<()> {
        let mut client_pid = 0_u32;
        let handle = pipe.as_raw_handle() as HANDLE;
        if unsafe { GetNamedPipeClientProcessId(handle, &mut client_pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        verify_process_identity(client_pid, expected_user_sid, "named-pipe client")
    }

    pub(super) fn verify_connected_server(
        pipe: &tokio::net::windows::named_pipe::NamedPipeClient,
        expected_user_sid: &str,
    ) -> io::Result<()> {
        let mut server_pid = 0_u32;
        let handle = pipe.as_raw_handle() as HANDLE;
        if unsafe { GetNamedPipeServerProcessId(handle, &mut server_pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        verify_process_identity(server_pid, expected_user_sid, "named-pipe server")
    }

    fn verify_process_identity(
        process_id: u32,
        expected_user_sid: &str,
        peer_label: &'static str,
    ) -> io::Result<()> {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        let mut token = null_mut();
        if unsafe { OpenProcessToken(process.as_raw_handle() as HANDLE, TOKEN_QUERY, &mut token) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let (client_sid, client_is_system) = token_user_identity(&token)?;
        if client_sid != expected_user_sid && !client_is_system {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{peer_label} has an unauthorized SID"),
            ));
        }
        Ok(())
    }

    fn token_user_identity(token: &OwnedHandle) -> io::Result<(String, bool)> {
        let mut required = 0_u32;
        unsafe {
            GetTokenInformation(
                token.as_raw_handle() as HANDLE,
                TokenUser,
                null_mut(),
                0,
                &mut required,
            );
        }
        if required == 0 {
            return Err(io::Error::last_os_error());
        }
        let words = (required as usize).div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle() as HANDLE,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let token_user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
        let sid = token_user.User.Sid;
        if sid.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "token has no user SID",
            ));
        }
        let is_system = unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0;
        Ok((sid_to_string(sid)?, is_system))
    }

    fn sid_to_string(sid: windows_sys::Win32::Security::PSID) -> io::Result<String> {
        let mut encoded = null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut encoded) } == 0 || encoded.is_null() {
            return Err(io::Error::last_os_error());
        }
        wide_local_string(encoded)
    }

    fn wide_local_string(pointer: *mut u16) -> io::Result<String> {
        let mut length = 0_usize;
        while unsafe { *pointer.add(length) } != 0 {
            length = length.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "wide string overflow")
            })?;
            if length > 64 * 1024 {
                unsafe {
                    LocalFree(pointer as HLOCAL);
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "wide string exceeds limit",
                ));
            }
        }
        let value = String::from_utf16(unsafe { std::slice::from_raw_parts(pointer, length) })
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-16"));
        unsafe {
            LocalFree(pointer as HLOCAL);
        }
        value
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn unsafe_state_directory() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows state directory contains a reparse point",
        )
    }

    #[cfg(test)]
    pub(super) fn windows_path_dacl_sddl(path: &std::path::Path) -> io::Result<String> {
        let encoded_path = wide(path.as_os_str());
        let mut required = 0_u32;
        unsafe {
            GetFileSecurityW(
                encoded_path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                null_mut(),
                0,
                &mut required,
            );
        }
        if required == 0 {
            return Err(io::Error::last_os_error());
        }
        let words = (required as usize).div_ceil(size_of::<usize>());
        let mut descriptor = vec![0_usize; words];
        if unsafe {
            GetFileSecurityW(
                encoded_path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut sddl = null_mut();
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.as_mut_ptr().cast(),
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut sddl,
                null_mut(),
            )
        } == 0
            || sddl.is_null()
        {
            return Err(io::Error::last_os_error());
        }
        wide_local_string(sddl)
    }
}

#[cfg(all(windows, test))]
use windows_security::{
    WindowsSecurityDescriptor, owner_identity_is_allowed, restricted_security_sddl,
    windows_path_dacl_sddl,
};
#[cfg(windows)]
use windows_security::{
    create_restricted_pipe, current_user_sid_string, protect_windows_state_dir,
    verify_connected_client, verify_connected_server,
};

#[cfg(windows)]
async fn spawn_windows(service: DaemonService, endpoint: IpcEndpoint) -> io::Result<IpcServer> {
    use tokio::net::windows::named_pipe::ServerOptions;

    protect_windows_state_dir(&endpoint.state_dir)?;
    let current_user_sid = current_user_sid_string()?;
    let mut first_options = ServerOptions::new();
    first_options
        .first_pipe_instance(true)
        .reject_remote_clients(true);
    let mut pending =
        create_restricted_pipe(&first_options, &endpoint.pipe_name, &current_user_sid)?;
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let maximum_frame = service.max_frame_bytes();
    let connection_ids = Arc::new(AtomicU64::new(1));
    let connection_slots = Arc::new(Semaphore::new(service.max_connections()));
    let pipe_name = endpoint.pipe_name.clone();
    let task = tokio::spawn(async move {
        let mut connection_tasks = tokio::task::JoinSet::new();
        let listener_result = loop {
            tokio::select! {
                _ = &mut shutdown_rx => break Ok(()),
                completed = connection_tasks.join_next(), if !connection_tasks.is_empty() => {
                    let _ = completed;
                }
                connected = pending.connect() => {
                    if let Err(error) = connected {
                        if !is_transient_listener_error(&error) {
                            break Err(error);
                        }
                        tokio::time::sleep(LISTENER_RETRY_DELAY).await;
                        let mut replacement_options = ServerOptions::new();
                        replacement_options.reject_remote_clients(true);
                        pending = match create_restricted_pipe(
                            &replacement_options,
                            &pipe_name,
                            &current_user_sid,
                        ) {
                            Ok(replacement) => replacement,
                            Err(error) => break Err(error),
                        };
                        continue;
                    }
                    let mut next_options = ServerOptions::new();
                    next_options.reject_remote_clients(true);
                    let next = match create_restricted_pipe(
                        &next_options,
                        &pipe_name,
                        &current_user_sid,
                    ) {
                        Ok(next) => next,
                        Err(error) => break Err(error),
                    };
                    let connected_pipe = std::mem::replace(&mut pending, next);
                    if verify_connected_client(&connected_pipe, &current_user_sid).is_err() {
                        continue;
                    }
                    let permit = match connection_slots.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => continue,
                    };
                    let service = service.clone();
                    let connection_id = connection_ids.fetch_add(1, Ordering::Relaxed);
                    connection_tasks.spawn(async move {
                        handle_connection(connected_pipe, service, connection_id, maximum_frame).await;
                        drop(permit);
                    });
                }
            }
        };
        drop(pending);
        connection_tasks.abort_all();
        while connection_tasks.join_next().await.is_some() {}
        let shutdown_result = service
            .shutdown(SERVICE_SHUTDOWN_GRACE)
            .await
            .map_err(|error| io::Error::other(error.message));
        listener_result?;
        shutdown_result
    });
    Ok(IpcServer {
        shutdown: Some(shutdown_tx),
        task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn bounded_client_exchange_rejects_oversized_request_before_writing() {
        let (mut client, mut server) = tokio::io::duplex(512);
        let request = json!({"payload": "x".repeat(128)});

        let error = request_on_stream(&mut client, &request, 32)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let mut byte = [0_u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), server.read_exact(&mut byte))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bounded_client_exchange_rejects_oversized_response_before_payload_allocation() {
        let (mut client, mut server) = tokio::io::duplex(512);
        let peer = tokio::spawn(async move {
            let request = read_frame(&mut server, 64, Duration::from_secs(1))
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&request).unwrap(),
                json!({"ok": true})
            );
            server.write_all(&(65_u32).to_le_bytes()).await.unwrap();
        });

        let error = request_on_stream(&mut client, &json!({"ok": true}), 64)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn configurable_response_timeout_allows_long_poll_responses() {
        let (mut client, mut server) = tokio::io::duplex(512);
        let peer = tokio::spawn(async move {
            read_frame(&mut server, 64, Duration::from_secs(1))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            write_frame(&mut server, br#"{"ok":true}"#, 64, Duration::from_secs(1))
                .await
                .unwrap();
        });

        let response = request_on_stream_with_timeout(
            &mut client,
            &json!({"poll": true}),
            64,
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        assert_eq!(response, json!({"ok": true}));
        peer.await.unwrap();
    }

    #[test]
    fn endpoint_derivation_normalizes_lexical_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let before_creation = IpcEndpoint::for_state_dir(state_dir.clone());
        let aliased_before_creation =
            IpcEndpoint::for_state_dir(state_dir.join("..").join("state"));
        std::fs::create_dir(&state_dir).unwrap();
        let direct = IpcEndpoint::for_state_dir(state_dir.clone());
        let aliased = IpcEndpoint::for_state_dir(state_dir.join("..").join("state"));
        assert_eq!(before_creation, aliased_before_creation);
        assert_eq!(before_creation, direct);
        assert_eq!(direct, aliased);
    }

    #[tokio::test]
    async fn shutdown_terminates_open_connection_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let service =
            DaemonService::open(crate::DaemonConfig::for_test(state_dir.clone())).unwrap();
        let endpoint = IpcEndpoint::for_state_dir(state_dir);
        let server = IpcServer::spawn(service, endpoint.clone()).await.unwrap();
        let mut client = IpcClient::connect(&endpoint).await.unwrap();
        client
            .request(&json!({
                "jsonrpc": "2.0",
                "method": "capabilities",
                "params": {
                    "client_name": "shutdown-test",
                    "protocol_version": 1,
                    "requested_scope": {
                        "read_roots": [temp.path()],
                        "write_roots": [temp.path()]
                    }
                },
                "id": 1
            }))
            .await
            .unwrap();

        server.shutdown().await.unwrap();
        let after_shutdown = tokio::time::timeout(
            Duration::from_secs(1),
            client.request(&json!({
                "jsonrpc": "2.0",
                "method": "capabilities",
                "params": {
                    "client_name": "shutdown-test",
                    "protocol_version": 1,
                    "requested_scope": {
                        "read_roots": [temp.path()],
                        "write_roots": [temp.path()]
                    }
                },
                "id": 2
            })),
        )
        .await;
        assert!(matches!(after_shutdown, Ok(Err(_))));
    }

    #[test]
    fn listener_retries_only_errors_classified_as_transient() {
        assert!(is_transient_listener_error(&io::Error::from(
            io::ErrorKind::Interrupted
        )));
        assert!(is_transient_listener_error(&io::Error::from(
            io::ErrorKind::ConnectionAborted
        )));
        assert!(!is_transient_listener_error(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[cfg(windows)]
    #[test]
    fn windows_security_descriptor_allows_only_current_user_and_system() {
        let user_sid = current_user_sid_string().unwrap();
        let sddl = restricted_security_sddl(&user_sid, false);
        let _descriptor = WindowsSecurityDescriptor::from_sddl(&sddl).unwrap();

        assert!(owner_identity_is_allowed(&user_sid, false, &user_sid));
        assert!(owner_identity_is_allowed("S-1-5-999", true, &user_sid));
        assert!(!owner_identity_is_allowed("S-1-5-21-999", false, &user_sid));
        assert!(sddl.contains(&format!(";;;{user_sid})")));
        assert!(sddl.contains(";;;SY)"));
        for forbidden in [";;;WD)", ";;;AN)", ";;;AU)", ";;;BU)"] {
            assert!(!sddl.contains(forbidden));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_state_directory_gets_restricted_acl_and_rejects_reparse_points() {
        use std::os::windows::fs::symlink_dir;

        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        protect_windows_state_dir(&state_dir).unwrap();
        let user_sid = current_user_sid_string().unwrap();
        let dacl = windows_path_dacl_sddl(&state_dir).unwrap();
        assert!(dacl.contains(&user_sid));
        assert!(dacl.contains("SY") || dacl.contains("S-1-5-18"));
        for forbidden in [";;;WD)", ";;;AN)", ";;;AU)", ";;;BU)"] {
            assert!(!dacl.contains(forbidden));
        }

        let real = temp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = temp.path().join("reparse");
        match symlink_dir(&real, &link) {
            Ok(()) => {
                assert!(
                    DaemonService::open(crate::DaemonConfig::for_test(link.clone())).is_err(),
                    "service must reject a reparse state path before opening stores"
                );
                let error = protect_windows_state_dir(&link).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            }
            Err(error) if error.raw_os_error() == Some(1314) => {
                // Windows without Developer Mode cannot create an unprivileged test symlink.
            }
            Err(error) => panic!("cannot create reparse-point fixture: {error}"),
        }
    }
}
