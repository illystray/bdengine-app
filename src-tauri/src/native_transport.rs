//! Page-owned file registrations and bounded, cancellable HTTP uploads.
use std::{
  collections::{HashMap, HashSet},
  fs::{File, OpenOptions},
  path::PathBuf,
  sync::{Arc, Mutex},
  time::{Duration, Instant, UNIX_EPOCH},
};

use futures_util::stream;
use reqwest::{header, Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, Manager, Webview};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

mod file_watch;
mod http_plugin;
#[cfg(test)]
mod tests;
mod webview;
#[cfg(all(test, feature = "transport-webview-tests"))]
mod webview_tests;

const BUFFER_SIZE: usize = 64 * 1024;
const RESPONSE_LIMIT: usize = 1024 * 1024;
const MAX_REGISTRATIONS: usize = 256;
const MAX_CANCELLED_IDS: usize = 4096;
const PROBE_SCRIPT: &str = r#"if (window === window.top && window.chrome?.webview) {
  try {
    window.chrome.webview.postMessageWithAdditionalObjects({type:'bde:transport-probe',supported:true}, []);
  } catch (_) {
    window.chrome.webview.postMessage({type:'bde:transport-probe',supported:false});
  }
}"#;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TransportError {
  pub code: &'static str,
  pub message: &'static str,
}

pub type Result<T> = std::result::Result<T, TransportError>;

fn error(code: &'static str) -> TransportError {
  TransportError {
    code,
    message: match code {
      "CANCELLED" => "The request was cancelled.",
      "TIMEOUT" => "The request timed out.",
      "FILE_NOT_LOCAL" => "The file has no local filesystem path.",
      "FILE_NOT_FOUND" => "The file could not be opened for reading.",
      "FILE_CHANGED" => "The selected file has changed or is being modified.",
      "FILE_RELEASED" => "The file registration is no longer available.",
      "UNSUPPORTED_RUNTIME" => "This WebView2 runtime does not support browser file registration.",
      "RESPONSE_TOO_LARGE" => "The response exceeds the 1 MiB limit.",
      "ACCESS_DENIED" => "Native transport is only available to the main editor page.",
      "INVALID_REQUEST" => {
        "The request parameters are invalid or the request ID is already in use."
      }
      "BUSY" => "Too many files or requests are registered on this page.",
      _ => "The network request failed.",
    },
  }
}

fn network_error(err: reqwest::Error) -> TransportError {
  // reqwest errors can contain signed URLs. Never expose their Display/Debug text.
  error(if err.is_timeout() {
    "TIMEOUT"
  } else {
    "NETWORK_ERROR"
  })
}

pub fn trusted_url(url: &Url) -> bool {
  url.scheme() == "https"
    && url.port_or_known_default() == Some(443)
    && matches!(url.host_str(), Some("bdengine.app" | "beta.bdengine.app"))
    && url.username().is_empty()
    && url.password().is_none()
}

fn require_editor(webview: &Webview) -> Result<()> {
  if webview.label() == crate::MAIN_WINDOW_LABEL
    && webview.url().map(|url| trusted_url(&url)).unwrap_or(false)
  {
    Ok(())
  } else {
    Err(error("ACCESS_DENIED"))
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
  size: u64,
  modified_ms: u64,
  identity: (u32, u64, u64, u64),
  change_time: i64,
}

fn fingerprint(file: &File) -> Result<Fingerprint> {
  use std::os::windows::io::AsRawHandle;
  use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
      FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
      BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO,
    },
  };
  let meta = file.metadata().map_err(|_| error("FILE_NOT_FOUND"))?;
  if !meta.is_file() {
    return Err(error("FILE_NOT_LOCAL"));
  }
  let modified_ms = meta
    .modified()
    .ok()
    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
    .map(|d| d.as_millis() as u64)
    .ok_or_else(|| error("FILE_NOT_FOUND"))?;
  let mut info = BY_HANDLE_FILE_INFORMATION::default();
  unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
    .map_err(|_| error("FILE_NOT_FOUND"))?;
  let mut basic = FILE_BASIC_INFO::default();
  unsafe {
    GetFileInformationByHandleEx(
      HANDLE(file.as_raw_handle()),
      FileBasicInfo,
      (&mut basic as *mut FILE_BASIC_INFO).cast(),
      std::mem::size_of::<FILE_BASIC_INFO>() as u32,
    )
  }
  .map_err(|_| error("FILE_NOT_FOUND"))?;
  let join = |hi: u32, lo: u32| ((hi as u64) << 32) | lo as u64;
  Ok(Fingerprint {
    size: meta.len(),
    modified_ms,
    change_time: basic.ChangeTime,
    identity: (
      info.dwVolumeSerialNumber,
      join(info.nFileIndexHigh, info.nFileIndexLow),
      join(
        info.ftCreationTime.dwHighDateTime,
        info.ftCreationTime.dwLowDateTime,
      ),
      join(
        info.ftLastWriteTime.dwHighDateTime,
        info.ftLastWriteTime.dwLowDateTime,
      ),
    ),
  })
}

struct RegisteredFile {
  path: PathBuf,
  // Keeping the original handle prevents file ID reuse after deletion.
  original: File,
  watch: Option<file_watch::FileWatch>,
  fingerprint: Fingerprint,
}

impl RegisteredFile {
  fn open_upload(&self) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // Deny writers and deletion for the duration of this upload. Registration itself
    // does not lock out edits; any edits since selection are detected here.
    let file = OpenOptions::new()
      .read(true)
      .share_mode(1)
      .open(&self.path)
      .map_err(|_| error("FILE_CHANGED"))?;
    if self.watch.as_ref().is_some_and(|watch| watch.changed())
      || fingerprint(&file)? != self.fingerprint
      || fingerprint(&self.original)? != self.fingerprint
    {
      return Err(error("FILE_CHANGED"));
    }
    Ok(file)
  }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistrationRequest {
  expected_size: u64,
  expected_last_modified: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
  file_id: String,
  name: String,
  size: u64,
  last_modified: u64,
}

#[derive(Default)]
struct PageState {
  epoch: u64,
  files: HashMap<String, Arc<RegisteredFile>>,
  requests: HashMap<String, Arc<RequestControl>>,
  cancelled: HashSet<String>,
  registration_supported: bool,
  registration_probed: bool,
}

#[derive(Default, Clone)]
pub struct NativeTransport(Arc<Mutex<PageState>>);

struct RequestControl {
  file_id: String,
  cancel: CancellationToken,
  finished: CancellationToken,
}

struct UploadLease {
  state: NativeTransport,
  epoch: u64,
  request_id: String,
  file: Option<Arc<RegisteredFile>>,
  control: Arc<RequestControl>,
}

impl Drop for UploadLease {
  fn drop(&mut self) {
    self.file.take();
    let mut page = self.state.0.lock().unwrap();
    if page.epoch == self.epoch {
      page.requests.remove(&self.request_id);
    }
    self.control.finished.cancel();
  }
}

impl NativeTransport {
  fn epoch(&self) -> u64 {
    self.0.lock().unwrap().epoch
  }

  pub fn reset(&self) {
    let mut page = self.0.lock().unwrap();
    for request in page.requests.values() {
      request.cancel.cancel();
    }
    page.requests.clear();
    page.files.clear();
    page.cancelled.clear();
    page.epoch = page.epoch.wrapping_add(1);
  }

  fn register(
    &self,
    epoch: u64,
    path: PathBuf,
    request: &RegistrationRequest,
  ) -> Result<Registration> {
    use std::os::windows::fs::OpenOptionsExt;
    if !path.is_absolute() {
      return Err(error("FILE_NOT_LOCAL"));
    }
    let mut original = OpenOptions::new()
      .read(true)
      .share_mode(1 | 2 | 4)
      .custom_flags(0x40000000)
      .open(&path)
      .map_err(|_| error("FILE_NOT_FOUND"))?;
    let watch = file_watch::FileWatch::new(&original);
    if watch.is_none() {
      // Filesystems without read oplocks must keep the selected file immutable.
      original = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&path)
        .map_err(|_| error("FILE_CHANGED"))?;
    }
    let fingerprint = fingerprint(&original)?;
    if fingerprint.size != request.expected_size
      || fingerprint.modified_ms != request.expected_last_modified
    {
      return Err(error("FILE_CHANGED"));
    }
    let result = Registration {
      file_id: Uuid::new_v4().to_string(),
      name: path
        .file_name()
        .ok_or_else(|| error("FILE_NOT_LOCAL"))?
        .to_string_lossy()
        .into_owned(),
      size: fingerprint.size,
      last_modified: fingerprint.modified_ms,
    };
    let mut page = self.0.lock().unwrap();
    if page.epoch != epoch {
      return Err(error("FILE_RELEASED"));
    }
    if page.files.len() >= MAX_REGISTRATIONS {
      return Err(error("BUSY"));
    }
    page.files.insert(
      result.file_id.clone(),
      Arc::new(RegisteredFile {
        path,
        original,
        watch,
        fingerprint,
      }),
    );
    Ok(result)
  }

  fn begin(&self, request_id: &str, file_id: &str) -> Result<UploadLease> {
    validate_request_id(request_id)?;
    let mut page = self.0.lock().unwrap();
    if page.cancelled.remove(request_id) {
      return Err(error("CANCELLED"));
    }
    if page.requests.contains_key(request_id) {
      return Err(error("INVALID_REQUEST"));
    }
    if page.requests.len() >= MAX_REGISTRATIONS {
      return Err(error("BUSY"));
    }
    let file = page
      .files
      .get(file_id)
      .cloned()
      .ok_or_else(|| error("FILE_RELEASED"))?;
    let control = Arc::new(RequestControl {
      file_id: file_id.to_owned(),
      cancel: CancellationToken::new(),
      finished: CancellationToken::new(),
    });
    page.requests.insert(request_id.to_owned(), control.clone());
    Ok(UploadLease {
      state: self.clone(),
      epoch: page.epoch,
      request_id: request_id.to_owned(),
      file: Some(file),
      control,
    })
  }

  async fn cancel(&self, request_id: &str) -> Result<()> {
    validate_request_id(request_id)?;
    let control = {
      let mut page = self.0.lock().unwrap();
      if let Some(control) = page.requests.get(request_id) {
        control.cancel.cancel();
        Some(control.clone())
      } else {
        if page.cancelled.len() >= MAX_CANCELLED_IDS && !page.cancelled.contains(request_id) {
          return Err(error("BUSY"));
        }
        // Handles cancellation arriving before the upload command is scheduled.
        page.cancelled.insert(request_id.to_owned());
        None
      }
    };
    if let Some(control) = control {
      control.finished.cancelled().await;
    }
    Ok(())
  }

  async fn release(&self, file_id: &str) {
    let controls: Vec<_> = {
      let mut page = self.0.lock().unwrap();
      page.files.remove(file_id);
      page
        .requests
        .values()
        .filter(|control| control.file_id == file_id)
        .map(|control| {
          control.cancel.cancel();
          control.clone()
        })
        .collect()
    };
    for control in controls {
      control.finished.cancelled().await;
    }
  }
}

fn validate_request_id(id: &str) -> Result<()> {
  if id.is_empty() || id.len() > 128 {
    Err(error("INVALID_REQUEST"))
  } else {
    Ok(())
  }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportInfo {
  version: u32,
  http: bool,
  browser_file_registration: bool,
  streaming_file_upload: bool,
}

#[tauri::command]
pub async fn native_transport_info(
  webview: Webview,
  state: tauri::State<'_, NativeTransport>,
) -> Result<TransportInfo> {
  require_editor(&webview)?;
  if !state.0.lock().unwrap().registration_probed {
    let epoch = state.epoch();
    webview
      .eval(PROBE_SCRIPT)
      .map_err(|_| error("UNSUPPORTED_RUNTIME"))?;
    for _ in 0..200 {
      if state.epoch() != epoch {
        return Err(error("CANCELLED"));
      }
      if state.0.lock().unwrap().registration_probed {
        break;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  }
  Ok(TransportInfo {
    version: 1,
    http: true,
    browser_file_registration: state.0.lock().unwrap().registration_supported,
    streaming_file_upload: true,
  })
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadProgress {
  request_id: String,
  transferred: u64,
  total: u64,
}

#[derive(Debug, Serialize)]
pub struct UploadResponse {
  status: u16,
  headers: Vec<(String, String)>,
  body: String,
}

#[derive(Clone)]
struct Progress {
  callback: Arc<dyn Fn(UploadProgress) + Send + Sync>,
  request_id: String,
  total: u64,
  last: Arc<Mutex<(Instant, u64, u64)>>,
}

impl Progress {
  fn emit(&self, transferred: u64) {
    let mut last = self.last.lock().unwrap();
    last.2 = last.2.max(transferred);
    if transferred > last.1 && last.0.elapsed() >= Duration::from_millis(100) {
      (self.callback)(UploadProgress {
        request_id: self.request_id.clone(),
        transferred,
        total: self.total,
      });
      last.0 = Instant::now();
      last.1 = transferred;
    }
  }
  async fn finish(&self) {
    let (wait, transferred) = {
      let last = self.last.lock().unwrap();
      if last.1 == last.2 && self.total != 0 {
        return;
      }
      (
        Duration::from_millis(100).saturating_sub(last.0.elapsed()),
        last.2,
      )
    };
    tokio::time::sleep(wait).await;
    (self.callback)(UploadProgress {
      request_id: self.request_id.clone(),
      transferred,
      total: self.total,
    });
  }
}

fn parse_url(raw: &str) -> Result<Url> {
  let url = Url::parse(raw).map_err(|_| error("INVALID_REQUEST"))?;
  if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
    return Err(error("INVALID_REQUEST"));
  }
  Ok(url)
}

fn upload_headers(input: HashMap<String, String>) -> Result<header::HeaderMap> {
  let mut headers = header::HeaderMap::new();
  for (name, value) in input {
    let name =
      header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| error("INVALID_REQUEST"))?;
    if matches!(
      name,
      header::CONTENT_LENGTH | header::TRANSFER_ENCODING | header::HOST
    ) {
      return Err(error("INVALID_REQUEST"));
    }
    let value = header::HeaderValue::from_str(&value).map_err(|_| error("INVALID_REQUEST"))?;
    headers.insert(name, value);
  }
  Ok(headers)
}

async fn send_upload(
  registered: Arc<RegisteredFile>,
  mut url: Url,
  mut method: Method,
  mut headers: header::HeaderMap,
  progress: Progress,
  cancel: CancellationToken,
) -> Result<UploadResponse> {
  // Open once with write/delete sharing disabled and retain it throughout redirects.
  let opening = registered.clone();
  let _guard = tokio::task::spawn_blocking(move || opening.open_upload())
    .await
    .map_err(|_| error("FILE_NOT_FOUND"))??;
  let client = Client::builder()
    .redirect(reqwest::redirect::Policy::none())
    .retry(reqwest::retry::never())
    .build()
    .map_err(network_error)?;
  let mut has_body = true;
  for redirects in 0..=10 {
    if cancel.is_cancelled() {
      return Err(error("CANCELLED"));
    }
    let opening = registered.clone();
    let reader = tokio::task::spawn_blocking(move || opening.open_upload())
      .await
      .map_err(|_| error("FILE_NOT_FOUND"))??;
    let stop_reader = cancel.child_token();
    let (sender, receiver) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(1);
    let producer = tokio::spawn(produce_body(
      reader,
      sender,
      progress.clone(),
      stop_reader.clone(),
      has_body,
    ));
    let body = stream::unfold(receiver, |mut receiver| async move {
      receiver.recv().await.map(|chunk| (chunk, receiver))
    });
    let mut request = client
      .request(method.clone(), url.clone())
      .headers(headers.clone());
    if has_body {
      request = request
        .header(header::CONTENT_LENGTH, progress.total)
        .body(reqwest::Body::wrap_stream(body));
    }
    let response = tokio::select! {
      biased;
      _ = cancel.cancelled() => Err(error("CANCELLED")),
      response = request.send() => response.map_err(network_error),
    };
    stop_reader.cancel();
    // Await the producer even on cancellation, so release() really closes its handles.
    producer.await.map_err(|_| error("FILE_CHANGED"))?;
    let mut response = response?;
    let status = response.status();
    if matches!(
      status,
      StatusCode::MOVED_PERMANENTLY
        | StatusCode::FOUND
        | StatusCode::SEE_OTHER
        | StatusCode::TEMPORARY_REDIRECT
        | StatusCode::PERMANENT_REDIRECT
    ) {
      if let Some(location) = response.headers().get(header::LOCATION) {
        if redirects == 10 {
          return Err(error("NETWORK_ERROR"));
        }
        let next = location
          .to_str()
          .ok()
          .and_then(|value| url.join(value).ok())
          .ok_or_else(|| error("NETWORK_ERROR"))?;
        parse_url(next.as_str()).map_err(|_| error("NETWORK_ERROR"))?;
        if url.origin() != next.origin() {
          for name in [
            header::AUTHORIZATION,
            header::COOKIE,
            header::PROXY_AUTHORIZATION,
          ] {
            headers.remove(name);
          }
        }
        if status == StatusCode::SEE_OTHER
          || (matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND)
            && method == Method::POST)
        {
          method = Method::GET;
          has_body = false;
          headers.remove(header::CONTENT_TYPE);
          headers.remove(header::CONTENT_ENCODING);
        }
        url = next;
        continue;
      }
    }
    let response_headers = response
      .headers()
      .iter()
      .map(|(name, value)| {
        (
          name.as_str().to_owned(),
          String::from_utf8_lossy(value.as_bytes()).into_owned(),
        )
      })
      .collect();
    if response
      .content_length()
      .map(|len| len > RESPONSE_LIMIT as u64)
      .unwrap_or(false)
    {
      return Err(error("RESPONSE_TOO_LARGE"));
    }
    let mut bytes = Vec::new();
    loop {
      let chunk = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(error("CANCELLED")),
        chunk = response.chunk() => chunk.map_err(network_error)?,
      };
      let Some(chunk) = chunk else {
        break;
      };
      if chunk.len() > RESPONSE_LIMIT - bytes.len() {
        return Err(error("RESPONSE_TOO_LARGE"));
      }
      bytes.extend_from_slice(&chunk);
    }
    tokio::select! {
      biased;
      _ = cancel.cancelled() => return Err(error("CANCELLED")),
      _ = progress.finish() => {},
    }
    return Ok(UploadResponse {
      status: status.as_u16(),
      headers: response_headers,
      body: String::from_utf8_lossy(&bytes).into_owned(),
    });
  }
  Err(error("NETWORK_ERROR"))
}

async fn produce_body(
  file: File,
  sender: tokio::sync::mpsc::Sender<std::io::Result<Vec<u8>>>,
  progress: Progress,
  cancel: CancellationToken,
  has_body: bool,
) {
  let mut file = tokio::fs::File::from_std(file);
  let mut transferred = 0;
  while has_body && transferred < progress.total && !cancel.is_cancelled() {
    let mut buffer = vec![0u8; BUFFER_SIZE.min((progress.total - transferred) as usize)];
    let read = tokio::select! {
      biased;
      _ = cancel.cancelled() => break,
      count = file.read(&mut buffer) => count,
    };
    let chunk = match read {
      Ok(0) => Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
      Ok(count) => {
        buffer.truncate(count);
        Ok(buffer)
      }
      Err(err) => Err(err),
    };
    let count = chunk.as_ref().map(|bytes| bytes.len()).unwrap_or(0);
    tokio::select! {
      biased;
      _ = cancel.cancelled() => break,
      result = sender.send(chunk) => if result.is_err() { break; },
    }
    if count == 0 {
      break;
    }
    transferred += count as u64;
    if !cancel.is_cancelled() {
      progress.emit(transferred);
    }
  }
  // Tokio may still be completing one bounded filesystem read after cancellation.
  // Wait for that operation before reporting that the descriptor was released.
  drop(file.into_std().await);
}

async fn run_upload(
  lease: UploadLease,
  url: String,
  method: String,
  headers: HashMap<String, String>,
  timeout_ms: u64,
  callback: Arc<dyn Fn(UploadProgress) + Send + Sync>,
) -> Result<UploadResponse> {
  let file = lease.file.as_ref().unwrap().clone();
  let progress = Progress {
    callback,
    request_id: lease.request_id.clone(),
    total: file.fingerprint.size,
    last: Arc::new(Mutex::new((
      Instant::now() - Duration::from_millis(100),
      0,
      0,
    ))),
  };
  let request = async {
    let url = parse_url(&url)?;
    let method = Method::from_bytes(method.as_bytes()).map_err(|_| error("INVALID_REQUEST"))?;
    if !matches!(method, Method::PUT | Method::POST | Method::PATCH) || timeout_ms == 0 {
      return Err(error("INVALID_REQUEST"));
    }
    if lease.control.cancel.is_cancelled() {
      return Err(error("CANCELLED"));
    }
    send_upload(
      file,
      url,
      method,
      upload_headers(headers)?,
      progress,
      lease.control.cancel.clone(),
    )
    .await
  };
  tokio::pin!(request);
  tokio::select! {
    biased;
    _ = lease.control.cancel.cancelled() => { let _ = request.await; Err(error("CANCELLED")) },
    _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
      lease.control.cancel.cancel();
      let _ = request.await;
      Err(error("TIMEOUT"))
    },
    result = &mut request => result,
  }
}

#[tauri::command]
pub async fn http_upload_file(
  webview: Webview,
  state: tauri::State<'_, NativeTransport>,
  request_id: String,
  file_id: String,
  url: String,
  method: String,
  headers: HashMap<String, String>,
  timeout_ms: Option<u64>,
  on_progress: Channel<UploadProgress>,
) -> Result<UploadResponse> {
  require_editor(&webview)?;
  let lease = state.begin(&request_id, &file_id)?;
  let epoch = lease.epoch;
  let page = state.inner().clone();
  let cancel = lease.control.cancel.clone();
  run_upload(
    lease,
    url,
    method,
    headers,
    timeout_ms.unwrap_or(600_000),
    Arc::new(move |event| {
      if page.epoch() == epoch && !cancel.is_cancelled() {
        let _ = on_progress.send(event);
      }
    }),
  )
  .await
}

#[tauri::command]
pub async fn http_cancel_request(
  webview: Webview,
  state: tauri::State<'_, NativeTransport>,
  request_id: String,
) -> Result<()> {
  require_editor(&webview)?;
  state.cancel(&request_id).await
}

#[tauri::command]
pub async fn native_file_release(
  webview: Webview,
  state: tauri::State<'_, NativeTransport>,
  file_id: String,
) -> Result<()> {
  require_editor(&webview)?;
  state.release(&file_id).await;
  Ok(())
}

pub fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
  tauri::plugin::Builder::new("native-transport")
    .js_init_script(format!(
      "{}\n{}",
      include_str!("native_transport/registration.js"),
      PROBE_SCRIPT
    ))
    .on_webview_ready(|webview| {
      if webview.label() == crate::MAIN_WINDOW_LABEL {
        let owner = webview.clone();
        webview.window().on_window_event(move |event| {
          if matches!(event, tauri::WindowEvent::Destroyed) {
            owner.state::<NativeTransport>().reset();
            close_http_resources(&owner);
          }
        });
        webview::install(webview);
      }
    })
    .on_page_load(|webview, payload| {
      if webview.label() == crate::MAIN_WINDOW_LABEL
        && matches!(payload.event(), tauri::webview::PageLoadEvent::Started)
      {
        webview.state::<NativeTransport>().reset();
        close_http_resources(webview);
      }
    })
    .on_event(|app, event| {
      if matches!(event, tauri::RunEvent::Exit) {
        app.state::<NativeTransport>().reset();
        if let Some(window) = app.get_webview_window(crate::MAIN_WINDOW_LABEL) {
          close_http_resources(window.as_ref());
        }
      }
    })
    .build()
}

pub fn http_plugin() -> impl tauri::plugin::Plugin<tauri::Wry> {
  http_plugin::init()
}

fn close_http_resources(webview: &Webview) {
  let mut resources = webview.resources_table();
  let ids: Vec<_> = resources
    .names()
    .filter(|(_, name)| name.starts_with("tauri_plugin_http::"))
    .map(|(id, _)| id)
    .collect();
  for id in ids {
    let _ = resources.close(id);
  }
}
