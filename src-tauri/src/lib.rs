use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::{
  collections::{HashMap, HashSet},
  env, fs,
  io::{Read, Write},
  mem,
  net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
  path::{Path, PathBuf},
  process::{Command, Stdio},
  slice,
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
  },
  thread,
  time::Duration,
};
use tauri::{
  image::Image,
  webview::{DownloadEvent, NewWindowResponse, PageLoadEvent},
  Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  net::{lookup_host, TcpListener, TcpStream, UdpSocket},
  sync::watch,
  time::{timeout, Instant},
};
use tokio_tungstenite::{
  accept_hdr_async,
  tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    Message,
  },
};
use url::Url;
use uuid::Uuid;
#[cfg(target_os = "windows")]
use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{HANDLE, HGLOBAL, HWND},
    System::{
      DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
      },
      Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
      Ole::CF_UNICODETEXT,
    },
    UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK},
  },
};

const MAIN_WINDOW_LABEL: &str = "main";
const SPLASH_WINDOW_LABEL: &str = "splash";
const STABLE_BASE_URL: &str = "https://bdengine.app/";
const BETA_BASE_URL: &str = "https://beta.bdengine.app/";
const TASKBAR_ICON_PNG: &[u8] = include_bytes!("../icons/32x32.png");
const SPLASH_IMAGE_PNG: &[u8] = include_bytes!("../splash/splash.png");
const APP_CONFIG_FILE_NAME: &str = "config.json";
const APP_IDENTIFIER: &str = "app.bdengine.desktop";
const APP_VERSION: u32 = 13;
const DISCORD_APPLICATION_ID: &str = "1514012998455529483";
const DISCORD_LARGE_IMAGE_KEY: &str = "bde_logo";
const DISCORD_OPEN_URL: &str = "https://bdengine.app";
const MAIN_WINDOW_REVEAL_TIMEOUT_MS: u64 = 4_000;
const UPDATE_DOWNLOAD_STARTED_EVENT: &str = "update-download-started";
const UPDATE_DOWNLOAD_PROGRESS_EVENT: &str = "update-download-progress";
const UPDATE_DOWNLOAD_FINISHED_EVENT: &str = "update-download-finished";
const UPDATE_DOWNLOAD_FAILED_EVENT: &str = "update-download-failed";
const MINECRAFT_PROXY_DEFAULT_PORT: u32 = 25565;
const MINECRAFT_PROXY_PATH_PREFIX: &str = "/minecraft/";
const MINECRAFT_PROXY_READ_BUFFER_BYTES: usize = 64 * 1024;
const MINECRAFT_PROXY_STARTED_EVENT: &str = "minecraft-proxy-started";
const MINECRAFT_PROXY_STOPPED_EVENT: &str = "minecraft-proxy-stopped";
const MINECRAFT_PROXY_CONNECTED_EVENT: &str = "minecraft-proxy-connected";
const MINECRAFT_PROXY_DISCONNECTED_EVENT: &str = "minecraft-proxy-disconnected";
const MINECRAFT_PROXY_ERROR_EVENT: &str = "minecraft-proxy-error";
const MINECRAFT_LAN_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 2, 60);
const MINECRAFT_LAN_MULTICAST_PORT: u16 = 4445;
const MINECRAFT_LAN_DISCOVERY_DEFAULT_TIMEOUT_MS: u64 = 3_000;
const MINECRAFT_LAN_PACKET_MAX_BYTES: usize = 2_048;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;
#[cfg(target_os = "windows")]
const CF_DIB_FORMAT: u32 = 8;
#[cfg(target_os = "windows")]
const WEBVIEW2_DOWNLOAD_URL: &str = "https://developer.microsoft.com/en-us/microsoft-edge/webview2";
#[cfg(target_os = "windows")]
const WEBVIEW2_CLIENT_GUID: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReleaseChannel {
  #[default]
  Stable,
  Beta,
}

impl ReleaseChannel {
  fn as_str(self) -> &'static str {
    match self {
      Self::Stable => "stable",
      Self::Beta => "beta",
    }
  }

  fn from_str(value: &str) -> Option<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "stable" | "release" => Some(Self::Stable),
      "beta" => Some(Self::Beta),
      _ => None,
    }
  }

  fn base_url(self) -> &'static str {
    match self {
      Self::Stable => STABLE_BASE_URL,
      Self::Beta => BETA_BASE_URL,
    }
  }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppConfig {
  release_channel: ReleaseChannel,
  webview2_checked: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardItemPayload {
  mime_type: String,
  text: Option<String>,
  base64: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateDownloadStartedPayload {
  file_name: String,
  total_bytes: Option<u64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateDownloadProgressPayload {
  file_name: String,
  downloaded_bytes: u64,
  total_bytes: Option<u64>,
  progress_percent: Option<f64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateDownloadFinishedPayload {
  file_name: String,
  path: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateDownloadFailedPayload {
  file_name: String,
  error: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MinecraftProxyStartPayload {
  proxy_id: String,
  ws_url: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MinecraftProxyInfo {
  proxy_id: String,
  host: String,
  port: u16,
  ws_url: String,
  connected: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MinecraftProxyStatusPayload {
  running: bool,
  proxies: Vec<MinecraftProxyInfo>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MinecraftProxyErrorPayload {
  proxy_id: String,
  error: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MinecraftLanServerPayload {
  host: String,
  port: u16,
  motd: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscordPresencePayload {
  mode: String,
  party_id: Option<String>,
  current_size: Option<i32>,
  max_size: Option<i32>,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchContext {
  deeplink: Option<String>,
  query_pairs: Vec<LaunchQueryPair>,
  files: Vec<LaunchFile>,
}

impl LaunchContext {
  fn has_payload(&self) -> bool {
    self.deeplink.is_some() || !self.query_pairs.is_empty() || !self.files.is_empty()
  }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchQueryPair {
  key: String,
  value: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchFile {
  path: String,
  name: String,
  mime_type: String,
  base64: String,
}

struct MinecraftProxyHandle {
  proxy_id: String,
  host: String,
  port: u16,
  ws_url: String,
  connected: Arc<AtomicBool>,
  shutdown_tx: watch::Sender<bool>,
}

impl MinecraftProxyHandle {
  fn info(&self) -> MinecraftProxyInfo {
    MinecraftProxyInfo {
      proxy_id: self.proxy_id.clone(),
      host: self.host.clone(),
      port: self.port,
      ws_url: self.ws_url.clone(),
      connected: self.connected.load(Ordering::SeqCst),
    }
  }
}

#[derive(Default)]
struct AppState {
  launch_context: Mutex<LaunchContext>,
  release_channel: Mutex<ReleaseChannel>,
  pending_installer_path: Mutex<Option<PathBuf>>,
  discord_client: Mutex<Option<DiscordIpcClient>>,
  minecraft_proxies: Mutex<HashMap<String, MinecraftProxyHandle>>,
}

impl AppState {
  fn get_launch_context(&self) -> LaunchContext {
    self
      .launch_context
      .lock()
      .expect("launch state poisoned")
      .clone()
  }

  fn take_launch_context(&self) -> LaunchContext {
    mem::take(&mut *self.launch_context.lock().expect("launch state poisoned"))
  }

  fn set_launch_context(&self, context: LaunchContext) {
    *self.launch_context.lock().expect("launch state poisoned") = context;
  }

  fn get_release_channel(&self) -> ReleaseChannel {
    *self
      .release_channel
      .lock()
      .expect("release channel state poisoned")
  }

  fn set_release_channel(&self, channel: ReleaseChannel) {
    *self
      .release_channel
      .lock()
      .expect("release channel state poisoned") = channel;
  }

  fn set_pending_installer_path(&self, path: PathBuf) {
    *self
      .pending_installer_path
      .lock()
      .expect("pending installer state poisoned") = Some(path);
  }

  fn clear_pending_installer_path(&self) {
    *self
      .pending_installer_path
      .lock()
      .expect("pending installer state poisoned") = None;
  }

  fn take_pending_installer_path(&self) -> Option<PathBuf> {
    self
      .pending_installer_path
      .lock()
      .expect("pending installer state poisoned")
      .take()
  }

  fn with_discord_client<F>(&self, mut action: F) -> Result<(), String>
  where
    F: FnMut(&mut DiscordIpcClient) -> Result<(), String>,
  {
    let mut guard = self
      .discord_client
      .lock()
      .expect("discord presence state poisoned");

    if guard.is_none() {
      let mut client = DiscordIpcClient::new(DISCORD_APPLICATION_ID);
      if client.connect().is_err() {
        return Ok(());
      }
      *guard = Some(client);
    }

    let client = guard.as_mut().expect("discord client must be initialized");
    if action(client).is_err() {
      let mut client = DiscordIpcClient::new(DISCORD_APPLICATION_ID);
      if client.connect().is_err() {
        *guard = None;
        return Ok(());
      }

      if action(&mut client).is_err() {
        *guard = None;
        return Ok(());
      }

      *guard = Some(client);
    }

    Ok(())
  }

  fn clear_discord_client(&self) {
    let mut guard = self
      .discord_client
      .lock()
      .expect("discord presence state poisoned");

    if let Some(client) = guard.as_mut() {
      let _ = client.clear_activity();
      let _ = client.close();
    }

    *guard = None;
  }

  fn insert_minecraft_proxy(&self, proxy: MinecraftProxyHandle) {
    self
      .minecraft_proxies
      .lock()
      .expect("minecraft proxy state poisoned")
      .insert(proxy.proxy_id.clone(), proxy);
  }

  fn remove_minecraft_proxy(&self, proxy_id: &str) -> Option<MinecraftProxyHandle> {
    self
      .minecraft_proxies
      .lock()
      .expect("minecraft proxy state poisoned")
      .remove(proxy_id)
  }

  fn minecraft_proxy_status(&self) -> MinecraftProxyStatusPayload {
    let proxies: Vec<MinecraftProxyInfo> = self
      .minecraft_proxies
      .lock()
      .expect("minecraft proxy state poisoned")
      .values()
      .map(MinecraftProxyHandle::info)
      .collect();

    MinecraftProxyStatusPayload {
      running: !proxies.is_empty(),
      proxies,
    }
  }

  fn take_minecraft_proxies(&self) -> Vec<MinecraftProxyHandle> {
    self
      .minecraft_proxies
      .lock()
      .expect("minecraft proxy state poisoned")
      .drain()
      .map(|(_, proxy)| proxy)
      .collect()
  }
}

fn is_bdengine_file(path: &Path) -> bool {
  path.is_file()
    && path
      .extension()
      .and_then(|ext| ext.to_str())
      .map(|ext| ext.eq_ignore_ascii_case("bdengine"))
      .unwrap_or(false)
}

fn is_supported_launch_url(url: &Url) -> bool {
  match url.scheme() {
    "bdengine" => true,
    "https" => matches!(url.domain(), Some("bdengine.app" | "beta.bdengine.app")),
    _ => false,
  }
}

fn is_embedded_app_url(url: &Url) -> bool {
  matches!(url.scheme(), "https")
    && matches!(url.domain(), Some("bdengine.app" | "beta.bdengine.app"))
}

fn open_url_in_system_browser(url: &Url) -> bool {
  #[cfg(target_os = "windows")]
  {
    Command::new("rundll32")
      .args(["url.dll,FileProtocolHandler", url.as_str()])
      .spawn()
      .is_ok()
  }

  #[cfg(target_os = "macos")]
  {
    Command::new("open").arg(url.as_str()).spawn().is_ok()
  }

  #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
  {
    Command::new("xdg-open").arg(url.as_str()).spawn().is_ok()
  }
}

#[cfg(target_os = "windows")]
fn webview2_installer_candidates() -> &'static [&'static str] {
  &[
    "redist\\MicrosoftEdgeWebView2RuntimeInstallerX64.exe",
    "redist\\MicrosoftEdgeWebview2Setup.exe",
  ]
}

#[cfg(target_os = "windows")]
fn relaunch_current_executable() -> Result<(), String> {
  let current_exe =
    env::current_exe().map_err(|err| format!("Could not resolve current executable: {err}"))?;
  let mut command = Command::new(current_exe);
  command
    .args(env::args_os().skip(1))
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .creation_flags(CREATE_NO_WINDOW)
    .spawn()
    .map_err(|err| format!("Could not relaunch application: {err}"))?;
  Ok(())
}

#[cfg(target_os = "windows")]
fn show_native_error_message(title: &str, message: &str) {
  let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
  let message_wide: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
  unsafe {
    let _ = MessageBoxW(
      Some(HWND(std::ptr::null_mut())),
      PCWSTR(message_wide.as_ptr()),
      PCWSTR(title_wide.as_ptr()),
      MB_OK | MB_ICONERROR,
    );
  }
}

#[cfg(target_os = "windows")]
fn get_webview2_runtime_version() -> Option<String> {
  let query_targets = [
    format!(
      r"HKCU\Software\Microsoft\EdgeUpdate\Clients\{}",
      WEBVIEW2_CLIENT_GUID
    ),
    format!(
      r"HKLM\Software\Microsoft\EdgeUpdate\Clients\{}",
      WEBVIEW2_CLIENT_GUID
    ),
    format!(
      r"HKLM\Software\WOW6432Node\Microsoft\EdgeUpdate\Clients\{}",
      WEBVIEW2_CLIENT_GUID
    ),
  ];

  for key in query_targets {
    let output = Command::new("reg")
      .args(["query", &key, "/v", "pv"])
      .creation_flags(CREATE_NO_WINDOW)
      .output()
      .ok()?;

    if !output.status.success() {
      continue;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
      if line.contains("REG_SZ") {
        let value = line
          .split_whitespace()
          .last()
          .unwrap_or_default()
          .trim()
          .to_string();
        if !value.is_empty() {
          return Some(value);
        }
      }
    }
  }

  None
}

#[cfg(target_os = "windows")]
fn resolve_webview2_installer_path() -> Option<PathBuf> {
  let exe_dir = env::current_exe().ok()?.parent()?.to_path_buf();
  webview2_installer_candidates()
    .iter()
    .map(|relative_path| exe_dir.join(relative_path))
    .find(|path| path.is_file())
}

#[cfg(target_os = "windows")]
fn ensure_webview2_runtime() -> Result<bool, String> {
  if get_webview2_runtime_version().is_some() {
    return Ok(false);
  }

  let installer_path = resolve_webview2_installer_path()
    .ok_or_else(|| "WebView2 Runtime is missing and no bundled installer was found.".to_string())?;

  let status = Command::new(&installer_path)
    .args(["/silent", "/install"])
    .creation_flags(CREATE_NO_WINDOW)
    .status()
    .map_err(|err| format!("Could not launch bundled WebView2 installer: {err}"))?;

  if !status.success() {
    return Err(format!(
      "Bundled WebView2 installer exited with code {:?}.",
      status.code()
    ));
  }

  if get_webview2_runtime_version().is_none() {
    return Err("WebView2 installer finished, but the runtime still could not be detected.".into());
  }

  relaunch_current_executable()?;
  Ok(true)
}

#[cfg(target_os = "windows")]
struct ClipboardGuard;

#[cfg(target_os = "windows")]
impl Drop for ClipboardGuard {
  fn drop(&mut self) {
    unsafe {
      let _ = CloseClipboard();
    }
  }
}

#[cfg(target_os = "windows")]
fn open_clipboard_guard() -> Result<ClipboardGuard, String> {
  unsafe {
    OpenClipboard(None).map_err(|err| format!("Could not open clipboard: {err}"))?;
  }
  Ok(ClipboardGuard)
}

#[cfg(target_os = "windows")]
fn html_clipboard_format() -> u32 {
  unsafe { RegisterClipboardFormatW(w!("HTML Format")) }
}

#[cfg(target_os = "windows")]
fn png_clipboard_format() -> u32 {
  unsafe { RegisterClipboardFormatW(w!("PNG")) }
}

#[cfg(target_os = "windows")]
fn clipboard_format_available(format: u32) -> bool {
  unsafe { IsClipboardFormatAvailable(format).is_ok() }
}

#[cfg(target_os = "windows")]
fn read_global_bytes(handle: HANDLE) -> Option<Vec<u8>> {
  unsafe {
    let hglobal = HGLOBAL(handle.0);
    let size = GlobalSize(hglobal);
    if size == 0 {
      return None;
    }

    let ptr = GlobalLock(hglobal);
    if ptr.is_null() {
      return None;
    }

    let bytes = slice::from_raw_parts(ptr as *const u8, size).to_vec();
    let _ = GlobalUnlock(hglobal);
    Some(bytes)
  }
}

#[cfg(target_os = "windows")]
fn read_clipboard_text() -> Option<String> {
  let _guard = open_clipboard_guard().ok()?;
  if !clipboard_format_available(CF_UNICODETEXT.0.into()) {
    return None;
  }

  let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0.into()).ok()? };
  let bytes = read_global_bytes(handle)?;
  if bytes.len() < 2 {
    return None;
  }

  let mut utf16 = Vec::with_capacity(bytes.len() / 2);
  for chunk in bytes.chunks_exact(2) {
    utf16.push(u16::from_le_bytes([chunk[0], chunk[1]]));
  }

  if let Some(null_pos) = utf16.iter().position(|&value| value == 0) {
    utf16.truncate(null_pos);
  }

  Some(String::from_utf16_lossy(&utf16))
}

#[cfg(target_os = "windows")]
fn read_clipboard_registered_text(format: u32) -> Option<String> {
  let _guard = open_clipboard_guard().ok()?;
  if !clipboard_format_available(format) {
    return None;
  }

  let handle = unsafe { GetClipboardData(format).ok()? };
  let mut bytes = read_global_bytes(handle)?;
  while matches!(bytes.last(), Some(0)) {
    bytes.pop();
  }
  String::from_utf8(bytes).ok()
}

#[cfg(target_os = "windows")]
fn read_clipboard_png_base64() -> Option<String> {
  let _guard = open_clipboard_guard().ok()?;
  let format = png_clipboard_format();
  if !clipboard_format_available(format) {
    return None;
  }

  let handle = unsafe { GetClipboardData(format).ok()? };
  let bytes = read_global_bytes(handle)?;
  Some(BASE64.encode(bytes))
}

#[cfg(target_os = "windows")]
fn alloc_global_handle(bytes: &[u8]) -> Result<HANDLE, String> {
  unsafe {
    let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes.len())
      .map_err(|err| format!("GlobalAlloc failed: {err}"))?;
    let ptr = GlobalLock(hglobal);
    if ptr.is_null() {
      return Err("GlobalLock failed.".into());
    }

    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
    let _ = GlobalUnlock(hglobal);
    Ok(HANDLE(hglobal.0))
  }
}

#[cfg(target_os = "windows")]
fn set_clipboard_text(text: &str) -> Result<(), String> {
  let mut utf16: Vec<u16> = text.encode_utf16().collect();
  utf16.push(0);
  let bytes = unsafe {
    slice::from_raw_parts(
      utf16.as_ptr() as *const u8,
      mem::size_of_val(utf16.as_slice()),
    )
    .to_vec()
  };
  let handle = alloc_global_handle(&bytes)?;
  unsafe {
    SetClipboardData(CF_UNICODETEXT.0.into(), Some(handle))
      .map_err(|err| format!("SetClipboardData text failed: {err}"))?;
  }
  Ok(())
}

#[cfg(target_os = "windows")]
fn set_clipboard_registered_bytes(format: u32, bytes: &[u8]) -> Result<(), String> {
  let handle = alloc_global_handle(bytes)?;
  unsafe {
    SetClipboardData(format, Some(handle))
      .map_err(|err| format!("SetClipboardData failed: {err}"))?;
  }
  Ok(())
}

#[cfg(target_os = "windows")]
fn png_bytes_to_cf_dib(bytes: &[u8]) -> Result<Vec<u8>, String> {
  let image = image::load_from_memory(bytes)
    .map_err(|err| format!("Could not decode image/png for CF_DIB: {err}"))?
    .to_rgba8();

  let width = image.width();
  let height = image.height();

  if width == 0 || height == 0 {
    return Err("Image is empty.".into());
  }

  let row_stride = width as usize * 4;
  let pixel_bytes_len = row_stride * height as usize;

  let mut dib = Vec::with_capacity(40 + pixel_bytes_len);

  dib.extend_from_slice(&40u32.to_le_bytes());
  dib.extend_from_slice(&(width as i32).to_le_bytes());
  dib.extend_from_slice(&(-(height as i32)).to_le_bytes());
  dib.extend_from_slice(&1u16.to_le_bytes());
  dib.extend_from_slice(&32u16.to_le_bytes());
  dib.extend_from_slice(&0u32.to_le_bytes());
  dib.extend_from_slice(&(pixel_bytes_len as u32).to_le_bytes());
  dib.extend_from_slice(&0i32.to_le_bytes());
  dib.extend_from_slice(&0i32.to_le_bytes());
  dib.extend_from_slice(&0u32.to_le_bytes());
  dib.extend_from_slice(&0u32.to_le_bytes());

  for pixel in image.pixels() {
    let [r, g, b, a] = pixel.0;
    dib.push(b);
    dib.push(g);
    dib.push(r);
    dib.push(a);
  }

  Ok(dib)
}

#[cfg(target_os = "windows")]
fn read_clipboard_items_windows() -> Vec<ClipboardItemPayload> {
  let mut items = Vec::new();

  if let Some(text) = read_clipboard_text() {
    items.push(ClipboardItemPayload {
      mime_type: "text/plain".into(),
      text: Some(text),
      base64: None,
    });
  }

  if let Some(html) = read_clipboard_registered_text(html_clipboard_format()) {
    items.push(ClipboardItemPayload {
      mime_type: "text/html".into(),
      text: Some(html),
      base64: None,
    });
  }

  if let Some(base64) = read_clipboard_png_base64() {
    items.push(ClipboardItemPayload {
      mime_type: "image/png".into(),
      text: None,
      base64: Some(base64),
    });
  }

  items
}

#[cfg(target_os = "windows")]
fn write_clipboard_items_windows(items: &[ClipboardItemPayload]) -> Result<(), String> {
  let _guard = open_clipboard_guard()?;
  unsafe {
    EmptyClipboard().map_err(|err| format!("Could not clear clipboard: {err}"))?;
  }

  for item in items {
    match item.mime_type.as_str() {
      "text/plain" => {
        let text = item.text.clone().unwrap_or_default();
        set_clipboard_text(&text)?;
      }
      "text/html" => {
        let text = item.text.clone().unwrap_or_default();
        set_clipboard_registered_bytes(html_clipboard_format(), text.as_bytes())?;
      }
      "image/png" => {
        let base64 = item
          .base64
          .as_deref()
          .ok_or_else(|| "image/png item is missing base64.".to_string())?;
        let bytes = BASE64
          .decode(base64)
          .map_err(|err| format!("Could not decode image/png clipboard payload: {err}"))?;

        set_clipboard_registered_bytes(png_clipboard_format(), &bytes)?;

        let dib = png_bytes_to_cf_dib(&bytes)?;
        set_clipboard_registered_bytes(CF_DIB_FORMAT, &dib)?;
      }
      _ => {}
    }
  }

  Ok(())
}

#[cfg(target_os = "windows")]
fn escape_powershell_single_quoted(value: &str) -> String {
  value.replace('\'', "''")
}

#[cfg(target_os = "windows")]
fn prompt_download_destination(suggested_path: &Path) -> Option<PathBuf> {
  let file_name = suggested_path
    .file_name()
    .and_then(|name| name.to_str())
    .filter(|name| !name.trim().is_empty())
    .unwrap_or("download");

  let initial_directory = suggested_path
    .parent()
    .and_then(|path| path.to_str())
    .filter(|path| !path.trim().is_empty())
    .unwrap_or("");

  let script = format!(
    r#"
Add-Type -AssemblyName System.Windows.Forms
$dialog = New-Object System.Windows.Forms.SaveFileDialog
$dialog.Title = 'Save File'
$dialog.OverwritePrompt = $true
$dialog.CheckPathExists = $true
$dialog.FileName = '{file_name}'
if ('{initial_directory}' -ne '') {{
  $dialog.InitialDirectory = '{initial_directory}'
}}
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {{
  $bytes = [System.Text.Encoding]::UTF8.GetBytes($dialog.FileName)
  [Console]::OpenStandardOutput().Write($bytes, 0, $bytes.Length)
}}
"#,
    file_name = escape_powershell_single_quoted(file_name),
    initial_directory = escape_powershell_single_quoted(initial_directory)
  );

  let output = Command::new("powershell")
    .args([
      "-NoProfile",
      "-STA",
      "-WindowStyle",
      "Hidden",
      "-Command",
      &script,
    ])
    .creation_flags(CREATE_NO_WINDOW)
    .output()
    .ok()?;

  if !output.status.success() {
    return None;
  }

  let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
  if value.is_empty() {
    None
  } else {
    Some(PathBuf::from(value))
  }
}

#[cfg(not(target_os = "windows"))]
fn prompt_download_destination(suggested_path: &Path) -> Option<PathBuf> {
  Some(suggested_path.to_path_buf())
}

fn load_launch_file(path: &Path) -> Option<LaunchFile> {
  let bytes = fs::read(path).ok()?;
  let name = path.file_name()?.to_string_lossy().into_owned();

  Some(LaunchFile {
    path: path.to_string_lossy().into_owned(),
    name,
    mime_type: "application/x-bdengine".into(),
    base64: BASE64.encode(bytes),
  })
}

#[cfg(target_os = "windows")]
fn early_app_config_path() -> Option<PathBuf> {
  env::var_os("APPDATA")
    .map(PathBuf::from)
    .map(|dir| dir.join(APP_IDENTIFIER).join(APP_CONFIG_FILE_NAME))
}

fn app_config_path(app: &tauri::AppHandle) -> Option<PathBuf> {
  app
    .path()
    .app_config_dir()
    .ok()
    .map(|dir| dir.join(APP_CONFIG_FILE_NAME))
}

fn load_app_config_from_path(path: &Path) -> AppConfig {
  fs::read_to_string(path)
    .ok()
    .and_then(|contents| serde_json::from_str::<AppConfig>(&contents).ok())
    .unwrap_or_default()
}

fn save_app_config_to_path(path: &Path, config: &AppConfig) -> Result<(), String> {
  let Some(parent) = path.parent() else {
    return Err("Could not resolve app config directory.".into());
  };

  fs::create_dir_all(parent)
    .map_err(|err| format!("Could not create app config directory: {err}"))?;
  let contents = serde_json::to_vec_pretty(config)
    .map_err(|err| format!("Could not serialize app config: {err}"))?;
  fs::write(path, contents).map_err(|err| format!("Could not save app config: {err}"))
}

fn load_app_config(app: &tauri::AppHandle) -> AppConfig {
  let Some(path) = app_config_path(app) else {
    return AppConfig::default();
  };

  load_app_config_from_path(&path)
}

fn load_release_channel(app: &tauri::AppHandle) -> ReleaseChannel {
  load_app_config(app).release_channel
}

fn persist_release_channel(app: &tauri::AppHandle, channel: ReleaseChannel) -> Result<(), String> {
  let Some(path) = app_config_path(app) else {
    return Err("Could not resolve app config directory.".into());
  };

  let mut config = load_app_config_from_path(&path);
  config.release_channel = channel;
  save_app_config_to_path(&path, &config)
}

fn taskbar_icon() -> Option<Image<'static>> {
  Image::from_bytes(TASKBAR_ICON_PNG)
    .ok()
    .map(|icon| icon.to_owned())
}

fn update_downloads_dir() -> PathBuf {
  env::temp_dir().join(APP_IDENTIFIER).join("updates")
}

fn splash_runtime_dir() -> PathBuf {
  env::temp_dir().join(APP_IDENTIFIER).join("splash")
}

fn sanitize_download_file_name(file_name: &str) -> Result<String, String> {
  let trimmed_name = file_name.trim();
  if trimmed_name.is_empty() {
    return Err("File name is empty.".into());
  }

  Path::new(trimmed_name)
    .file_name()
    .and_then(|name| name.to_str())
    .filter(|name| !name.trim().is_empty())
    .map(|name| name.to_string())
    .ok_or_else(|| "Could not resolve download file name.".into())
}

fn emit_update_download_failed(app: &tauri::AppHandle, file_name: String, error: String) {
  let _ = app.emit(
    UPDATE_DOWNLOAD_FAILED_EVENT,
    UpdateDownloadFailedPayload { file_name, error },
  );
}

#[cfg(target_os = "windows")]
fn launch_installer(installer_path: &Path) -> Result<(), String> {
  Command::new(installer_path)
    .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"])
    .spawn()
    .map_err(|err| format!("Could not launch installer: {err}"))?;
  Ok(())
}

#[cfg(not(target_os = "windows"))]
fn launch_installer(installer_path: &Path) -> Result<(), String> {
  Command::new(installer_path)
    .spawn()
    .map_err(|err| format!("Could not launch installer: {err}"))?;
  Ok(())
}

fn launch_pending_installer_if_any(app: &tauri::AppHandle) {
  let state = app.state::<AppState>();
  let Some(installer_path) = state.take_pending_installer_path() else {
    return;
  };

  if let Err(err) = launch_installer(&installer_path) {
    emit_update_download_failed(
      app,
      installer_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("update-installer")
        .to_string(),
      err,
    );
  }
}

fn close_discord_presence_if_any(app: &tauri::AppHandle) {
  app.state::<AppState>().clear_discord_client();
}

fn emit_minecraft_proxy_info(app: &tauri::AppHandle, event: &str, info: MinecraftProxyInfo) {
  let _ = app.emit(event, info);
}

fn emit_minecraft_proxy_error(app: &tauri::AppHandle, proxy_id: &str, error: impl Into<String>) {
  let _ = app.emit(
    MINECRAFT_PROXY_ERROR_EVENT,
    MinecraftProxyErrorPayload {
      proxy_id: proxy_id.to_string(),
      error: error.into(),
    },
  );
}

fn validate_minecraft_proxy_ip(ip: IpAddr) -> Result<(), String> {
  match ip {
    IpAddr::V4(ip) => {
      if ip == Ipv4Addr::new(169, 254, 169, 254) {
        return Err("Minecraft proxy target cannot be the metadata service address.".into());
      }

      if ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || ip.is_link_local() {
        return Err(format!(
          "Minecraft proxy target address is not allowed: {ip}"
        ));
      }
    }
    IpAddr::V6(ip) => {
      if ip.is_unspecified() || ip.is_multicast() || ip.is_unicast_link_local() {
        return Err(format!(
          "Minecraft proxy target address is not allowed: {ip}"
        ));
      }
    }
  }

  Ok(())
}

fn is_valid_minecraft_proxy_domain(host: &str) -> bool {
  if host.len() > 253 || host.starts_with('.') || host.ends_with('.') {
    return false;
  }

  host.split('.').all(|part| {
    !part.is_empty()
      && part
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
  })
}

async fn validate_minecraft_proxy_target(
  host: &str,
  port: u32,
) -> Result<(String, Vec<SocketAddr>), String> {
  let host = host.trim();
  if host.is_empty() {
    return Err("Minecraft proxy host is empty.".into());
  }

  if !(1..=65535).contains(&port) {
    return Err("Minecraft proxy port must be in range 1-65535.".into());
  }
  let port = port as u16;

  if host.chars().any(char::is_control) || host.contains('/') || host.contains('\\') {
    return Err("Minecraft proxy host contains unsupported characters.".into());
  }

  if host.eq_ignore_ascii_case("localhost") {
    return Ok((
      "localhost".into(),
      vec![SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), port))],
    ));
  }

  if let Ok(ip) = host.parse::<IpAddr>() {
    validate_minecraft_proxy_ip(ip)?;
    return Ok((host.to_string(), vec![SocketAddr::new(ip, port)]));
  }

  if !is_valid_minecraft_proxy_domain(host) {
    return Err("Minecraft proxy host must be a valid domain or IP address.".into());
  }

  let addresses: Vec<SocketAddr> = lookup_host((host, port))
    .await
    .map_err(|err| format!("Could not resolve Minecraft proxy host: {err}"))?
    .collect();

  if addresses.is_empty() {
    return Err("Minecraft proxy host did not resolve to any address.".into());
  }

  for address in &addresses {
    validate_minecraft_proxy_ip(address.ip())?;
  }

  Ok((host.to_string(), addresses))
}

fn create_minecraft_lan_discovery_socket() -> Result<UdpSocket, String> {
  let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
    .map_err(|err| format!("Could not create Minecraft LAN discovery socket: {err}"))?;

  socket
    .set_reuse_address(true)
    .map_err(|err| format!("Could not configure Minecraft LAN discovery socket: {err}"))?;

  let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MINECRAFT_LAN_MULTICAST_PORT);
  socket
    .bind(&SocketAddr::from(bind_addr).into())
    .map_err(|err| format!("Could not bind Minecraft LAN discovery socket: {err}"))?;

  socket
    .join_multicast_v4(&MINECRAFT_LAN_MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED)
    .map_err(|err| format!("Could not join Minecraft LAN multicast group: {err}"))?;

  socket
    .set_nonblocking(true)
    .map_err(|err| format!("Could not configure Minecraft LAN discovery socket mode: {err}"))?;

  let std_socket: std::net::UdpSocket = socket.into();
  UdpSocket::from_std(std_socket)
    .map_err(|err| format!("Could not create async Minecraft LAN discovery socket: {err}"))
}

fn extract_minecraft_lan_tag<'a>(
  message: &'a str,
  open_tag: &str,
  close_tag: &str,
) -> Option<&'a str> {
  let start = message.find(open_tag)? + open_tag.len();
  let end = message[start..].find(close_tag)? + start;
  Some(&message[start..end])
}

fn parse_minecraft_lan_announcement(
  message: &str,
  sender: SocketAddr,
) -> Option<MinecraftLanServerPayload> {
  let motd = extract_minecraft_lan_tag(message, "[MOTD]", "[/MOTD]")?;
  let port = extract_minecraft_lan_tag(message, "[AD]", "[/AD]")?
    .trim()
    .parse::<u16>()
    .ok()?;

  if port == 0 {
    return None;
  }

  Some(MinecraftLanServerPayload {
    host: sender.ip().to_string(),
    port,
    motd: motd.to_string(),
  })
}

fn is_allowed_minecraft_proxy_origin(origin: &str) -> bool {
  let Ok(url) = Url::parse(origin) else {
    return false;
  };

  match (url.scheme(), url.host_str()) {
    ("https", Some("bdengine.app" | "beta.bdengine.app")) => true,
    ("http" | "https", Some("localhost" | "127.0.0.1")) => true,
    _ => false,
  }
}

fn minecraft_proxy_handshake_error(message: &str) -> ErrorResponse {
  let mut response = ErrorResponse::new(Some(message.to_string()));
  *response.status_mut() = tauri::http::StatusCode::FORBIDDEN;
  response
}

fn validate_minecraft_proxy_handshake(
  request: &Request,
  expected_path: &str,
) -> Result<(), String> {
  if request.uri().path() != expected_path {
    return Err("Invalid Minecraft proxy path.".into());
  }

  let origin = request
    .headers()
    .get("origin")
    .and_then(|value| value.to_str().ok())
    .ok_or_else(|| "Minecraft proxy origin is missing.".to_string())?;

  if !is_allowed_minecraft_proxy_origin(origin) {
    return Err("Minecraft proxy origin is not allowed.".into());
  }

  Ok(())
}

async fn run_minecraft_proxy_connection(
  app: tauri::AppHandle,
  proxy_id: String,
  host: String,
  port: u16,
  ws_url: String,
  target_addresses: Vec<SocketAddr>,
  local_stream: TcpStream,
  connected: Arc<AtomicBool>,
  mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
  let expected_path = format!("{MINECRAFT_PROXY_PATH_PREFIX}{proxy_id}");
  let websocket = tokio::select! {
    _ = shutdown_rx.changed() => return Ok(()),
    result = accept_hdr_async(local_stream, |request: &Request, response: Response| {
      validate_minecraft_proxy_handshake(request, &expected_path)
        .map(|_| response)
        .map_err(|err| minecraft_proxy_handshake_error(&err))
    }) => result.map_err(|err| format!("Could not accept Minecraft proxy WebSocket: {err}"))?,
  };

  let tcp_stream = tokio::select! {
    _ = shutdown_rx.changed() => return Ok(()),
    result = TcpStream::connect(target_addresses.as_slice()) => {
      result.map_err(|err| format!("Could not connect to Minecraft server {host}:{port}: {err}"))?
    }
  };

  connected.store(true, Ordering::SeqCst);
  emit_minecraft_proxy_info(
    &app,
    MINECRAFT_PROXY_CONNECTED_EVENT,
    MinecraftProxyInfo {
      proxy_id: proxy_id.clone(),
      host: host.clone(),
      port,
      ws_url: ws_url.clone(),
      connected: true,
    },
  );

  let result = async {
    let (mut ws_write, mut ws_read) = websocket.split();
    let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();
    let mut buffer = vec![0u8; MINECRAFT_PROXY_READ_BUFFER_BYTES];

    loop {
      tokio::select! {
        _ = shutdown_rx.changed() => break,
        message = ws_read.next() => {
          match message {
            Some(Ok(Message::Binary(bytes))) => {
              tcp_write
                .write_all(&bytes)
                .await
                .map_err(|err| format!("Could not write Minecraft proxy TCP data: {err}"))?;
            }
            Some(Ok(Message::Ping(bytes))) => {
              ws_write
                .send(Message::Pong(bytes))
                .await
                .map_err(|err| format!("Could not write Minecraft proxy WebSocket pong: {err}"))?;
            }
            Some(Ok(Message::Text(_))) => {
              return Err("Minecraft proxy accepts only binary WebSocket frames.".into());
            }
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(_)) => {}
            Some(Err(err)) => {
              return Err(format!("Minecraft proxy WebSocket failed: {err}"));
            }
          }
        }
        read = tcp_read.read(&mut buffer) => {
          let read = read.map_err(|err| format!("Could not read Minecraft proxy TCP data: {err}"))?;
          if read == 0 {
            break;
          }

          ws_write
            .send(Message::Binary(buffer[..read].to_vec().into()))
            .await
            .map_err(|err| format!("Could not write Minecraft proxy WebSocket data: {err}"))?;
        }
      }
    }

    let _ = ws_write.close().await;
    Ok(())
  }
  .await;

  if connected.swap(false, Ordering::SeqCst) {
    emit_minecraft_proxy_info(
      &app,
      MINECRAFT_PROXY_DISCONNECTED_EVENT,
      MinecraftProxyInfo {
        proxy_id,
        host,
        port,
        ws_url,
        connected: false,
      },
    );
  }

  result
}

async fn run_minecraft_proxy_listener(
  app: tauri::AppHandle,
  proxy_id: String,
  host: String,
  port: u16,
  ws_url: String,
  target_addresses: Vec<SocketAddr>,
  listener: TcpListener,
  connected: Arc<AtomicBool>,
  mut shutdown_rx: watch::Receiver<bool>,
) {
  loop {
    let accepted = tokio::select! {
      _ = shutdown_rx.changed() => break,
      accepted = listener.accept() => accepted,
    };

    let (local_stream, peer_addr) = match accepted {
      Ok(value) => value,
      Err(err) => {
        emit_minecraft_proxy_error(
          &app,
          &proxy_id,
          format!("Minecraft proxy listener failed: {err}"),
        );
        break;
      }
    };

    if !peer_addr.ip().is_loopback() {
      emit_minecraft_proxy_error(
        &app,
        &proxy_id,
        "Minecraft proxy rejected non-local WebSocket client.",
      );
      continue;
    }

    if let Err(err) = run_minecraft_proxy_connection(
      app.clone(),
      proxy_id.clone(),
      host.clone(),
      port,
      ws_url.clone(),
      target_addresses.clone(),
      local_stream,
      Arc::clone(&connected),
      shutdown_rx.clone(),
    )
    .await
    {
      emit_minecraft_proxy_error(&app, &proxy_id, err);
    }
  }

  connected.store(false, Ordering::SeqCst);
  if let Some(proxy) = app.state::<AppState>().remove_minecraft_proxy(&proxy_id) {
    emit_minecraft_proxy_info(&app, MINECRAFT_PROXY_STOPPED_EVENT, proxy.info());
  }
}

fn shutdown_minecraft_proxies(app: &tauri::AppHandle) {
  for proxy in app.state::<AppState>().take_minecraft_proxies() {
    proxy.connected.store(false, Ordering::SeqCst);
    let info = proxy.info();
    let _ = proxy.shutdown_tx.send(true);
    emit_minecraft_proxy_info(app, MINECRAFT_PROXY_STOPPED_EVENT, info);
  }
}

fn discord_presence_details(mode: &str) -> Option<&'static str> {
  match mode {
    "editing" => Some("Editing a project"),
    "animating" => Some("Animating a model"),
    "sound" => Some("Creating sound effects"),
    "painting" => Some("Painting textures"),
    "share_party" => Some("In a Share Party"),
    _ => None,
  }
}

fn build_discord_activity(
  payload: &DiscordPresencePayload,
) -> Result<activity::Activity<'static>, String> {
  let details = discord_presence_details(payload.mode.trim())
    .ok_or_else(|| format!("Unsupported Discord presence mode: {}", payload.mode))?;

  let mut activity = activity::Activity::new()
    .details(details)
    .assets(
      activity::Assets::new()
        .large_image(DISCORD_LARGE_IMAGE_KEY)
        .large_text("BDEngine"),
    )
    .buttons(vec![activity::Button::new(
      "Open BDEngine",
      DISCORD_OPEN_URL,
    )]);

  if payload.mode.trim() == "share_party" {
    let current_size = payload.current_size.unwrap_or(0).max(0);
    let max_size = payload.max_size.unwrap_or(0).max(current_size);
    activity = activity.state(format!("{current_size}/{max_size} users connected"));

    if let Some(party_id) = payload
      .party_id
      .as_deref()
      .filter(|value| !value.trim().is_empty())
    {
      activity = activity.party(
        activity::Party::new()
          .id(party_id.to_string())
          .size([current_size, max_size]),
      );
    }
  }

  Ok(activity)
}

fn parse_launch_context<I, S>(args: I) -> LaunchContext
where
  I: IntoIterator<Item = S>,
  S: Into<String>,
{
  let mut context = LaunchContext::default();

  for arg in args.into_iter().map(Into::into) {
    if arg.trim().is_empty() {
      continue;
    }

    if let Ok(url) = Url::parse(&arg) {
      if is_supported_launch_url(&url) {
        if context.deeplink.is_none() {
          context.deeplink = Some(arg.clone());
        }

        for (key, value) in url.query_pairs() {
          if key != "appReal" {
            context.query_pairs.push(LaunchQueryPair {
              key: key.into_owned(),
              value: value.into_owned(),
            });
          }
        }

        continue;
      }
    }

    let path = PathBuf::from(&arg);
    if is_bdengine_file(&path) {
      if let Some(file) = load_launch_file(&path) {
        context.files.push(file);
      }
    }
  }

  context
}

fn build_remote_url(context: &LaunchContext, channel: ReleaseChannel) -> Url {
  let mut url = Url::parse(channel.base_url()).expect("remote base URL must be valid");

  {
    let mut query = url.query_pairs_mut();
    query.append_pair("appReal", "true");
    query.append_pair("appVersion", &APP_VERSION.to_string());

    for pair in &context.query_pairs {
      query.append_pair(&pair.key, &pair.value);
    }

    if !context.files.is_empty() {
      query.append_pair("openFile", "true");
    }

    if context.deeplink.is_some() {
      query.append_pair("fromAppLink", "true");
    }
  }

  url
}

fn launch_context_script(context: &LaunchContext) -> String {
  let payload = serde_json::to_string(context).expect("launch context must serialize");

  format!(
    r#"(() => {{
  const payload = {payload};
  const install = () => {{
    if (!window.__BDENGINE_DESKTOP__) {{
      const pendingFileBatches = [];
      let consumer = null;
      const existingLaunchQueue = window.launchQueue;
      const decodeBase64 = (value) => Uint8Array.from(atob(value), (char) => char.charCodeAt(0));
      const makeHandle = (file) => ({{
        kind: 'file',
        name: file.name,
        async getFile() {{
          return new File([decodeBase64(file.base64)], file.name, {{
            type: file.mimeType || 'application/octet-stream'
          }});
        }},
        async queryPermission() {{
          return 'granted';
        }},
        async requestPermission() {{
          return 'granted';
        }}
      }});
      const flush = () => {{
        if (!consumer) {{
          return;
        }}
        while (pendingFileBatches.length) {{
          consumer({{ files: pendingFileBatches.shift().map(makeHandle) }});
        }}
      }};
      Object.defineProperty(window, 'launchQueue', {{
        configurable: true,
        enumerable: false,
        writable: true,
        value: {{
          setConsumer(fn) {{
            consumer = fn;
            if (existingLaunchQueue && typeof existingLaunchQueue.setConsumer === 'function') {{
              existingLaunchQueue.setConsumer(fn);
            }}
            flush();
          }}
        }}
      }});
      window.__BDENGINE_DESKTOP__ = {{
        lastLaunchContext: null,
        pushLaunchContext(context) {{
          this.lastLaunchContext = context;
          window.dispatchEvent(new CustomEvent('bdengine-launch-context', {{ detail: context }}));
          const files = Array.isArray(context.files) ? context.files : [];
          if (files.length) {{
            pendingFileBatches.push(files);
            flush();
          }}
        }}
      }};
    }}
    window.__BDENGINE_DESKTOP__.pushLaunchContext(payload);
  }};
  if (document.readyState === 'loading') {{
    document.addEventListener('DOMContentLoaded', install, {{ once: true }});
  }} else {{
    install();
  }}
}})();"#,
    payload = payload
  )
}

fn dispatch_launch_context(window: &WebviewWindow, context: &LaunchContext) -> tauri::Result<()> {
  if context.has_payload() {
    window.eval(launch_context_script(context))?;
  }

  Ok(())
}

fn build_splash_html() -> String {
  let image_base64 = BASE64.encode(SPLASH_IMAGE_PNG);
  format!(
    r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>BDEngine</title>
    <style>
      html, body {{
        margin: 0;
        width: 100%;
        height: 100%;
        overflow: hidden;
        background: transparent;
      }}
      body {{
        display: flex;
        align-items: center;
        justify-content: center;
      }}
      img {{
        display: block;
        width: 100%;
        height: 100%;
        user-select: none;
        -webkit-user-drag: none;
        pointer-events: none;
      }}
    </style>
  </head>
  <body>
    <img src="data:image/png;base64,{image_base64}" alt="BDEngine splash">
  </body>
</html>"#
  )
}

fn prepare_splash_html_file() -> Result<PathBuf, String> {
  let splash_dir = splash_runtime_dir();
  fs::create_dir_all(&splash_dir)
    .map_err(|err| format!("Could not create splash directory: {err}"))?;
  let splash_path = splash_dir.join("splash.html");
  fs::write(&splash_path, build_splash_html())
    .map_err(|err| format!("Could not write splash html: {err}"))?;
  Ok(splash_path)
}

fn create_splash_window(app: &tauri::AppHandle) -> tauri::Result<WebviewWindow> {
  if let Some(window) = app.get_webview_window(SPLASH_WINDOW_LABEL) {
    return Ok(window);
  }

  let splash_path = prepare_splash_html_file().map_err(std::io::Error::other)?;
  let splash_url = Url::from_file_path(&splash_path)
    .map_err(|_| std::io::Error::other("Could not convert splash path to file URL."))?;

  WebviewWindowBuilder::new(app, SPLASH_WINDOW_LABEL, WebviewUrl::External(splash_url))
    .title("BDEngine")
    .inner_size(600.0, 338.0)
    .resizable(false)
    .minimizable(false)
    .maximizable(false)
    .closable(false)
    .fullscreen(false)
    .visible(true)
    .center()
    .decorations(false)
    .shadow(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .build()
}

fn reveal_main_window(app: &tauri::AppHandle, window: &WebviewWindow, is_revealed: &AtomicBool) {
  if is_revealed.swap(true, Ordering::SeqCst) {
    return;
  }

  let _ = window.show();
  let _ = window.set_focus();

  if let Some(splash) = app.get_webview_window(SPLASH_WINDOW_LABEL) {
    let _ = splash.close();
  }
}

fn create_main_window(
  app: &tauri::AppHandle,
  context: &LaunchContext,
) -> tauri::Result<WebviewWindow> {
  let mut config = app
    .config()
    .app
    .windows
    .first()
    .cloned()
    .expect("main window config must exist");
  let channel = app.state::<AppState>().get_release_channel();
  config.url = WebviewUrl::External(build_remote_url(context, channel));

  let app_handle = app.clone();
  let is_revealed = Arc::new(AtomicBool::new(false));
  let page_load_revealed = Arc::clone(&is_revealed);

  let mut builder = WebviewWindowBuilder::from_config(app, &config)?
    .visible(false)
    .on_navigation(|url| {
      if is_embedded_app_url(url) {
        true
      } else {
        let _ = open_url_in_system_browser(url);
        false
      }
    })
    .on_new_window(|url, _features| {
      let _ = open_url_in_system_browser(&url);
      NewWindowResponse::Deny
    })
    .on_download(|_, event| match event {
      DownloadEvent::Requested { destination, .. } => {
        if let Some(selected_path) = prompt_download_destination(destination) {
          *destination = selected_path;
          true
        } else {
          false
        }
      }
      DownloadEvent::Finished { .. } => true,
      _ => true,
    })
    .on_page_load(move |window, payload| {
      if matches!(payload.event(), PageLoadEvent::Finished) {
        reveal_main_window(&app_handle, &window, &page_load_revealed);
      }
    });

  if let Some(icon) = taskbar_icon() {
    builder = builder.icon(icon)?;
  }

  let window = builder.build()?;

  if let Some(icon) = taskbar_icon() {
    let _ = window.set_icon(icon);
  }

  let timeout_app = app.clone();
  let timeout_window = window.clone();
  let timeout_revealed = Arc::clone(&is_revealed);
  thread::spawn(move || {
    thread::sleep(Duration::from_millis(MAIN_WINDOW_REVEAL_TIMEOUT_MS));
    reveal_main_window(&timeout_app, &timeout_window, &timeout_revealed);
  });

  Ok(window)
}

fn apply_launch_context(app: &tauri::AppHandle, context: LaunchContext) -> tauri::Result<()> {
  let state = app.state::<AppState>();

  let window = if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
    window
  } else {
    state.set_launch_context(context.clone());
    return create_main_window(app, &context).map(|_| ());
  };

  if !window.is_visible()? && app.get_webview_window(SPLASH_WINDOW_LABEL).is_none() {
    let _ = window.show();
  }
  let _ = window.set_focus();

  match dispatch_launch_context(&window, &context) {
    Ok(()) => state.set_launch_context(LaunchContext::default()),
    Err(err) => {
      state.set_launch_context(context);
      return Err(err);
    }
  }

  Ok(())
}

#[tauri::command]
fn get_release_channel(state: tauri::State<'_, AppState>) -> String {
  state.get_release_channel().as_str().to_string()
}

#[tauri::command]
fn get_launch_file_path(state: tauri::State<'_, AppState>) -> Option<String> {
  state
    .get_launch_context()
    .files
    .first()
    .map(|file| file.path.clone())
}

#[tauri::command]
fn set_release_channel(
  app: tauri::AppHandle,
  state: tauri::State<'_, AppState>,
  channel: String,
) -> Result<String, String> {
  let channel =
    ReleaseChannel::from_str(&channel).ok_or_else(|| "Unsupported release channel.".to_string())?;
  persist_release_channel(&app, channel)?;
  state.set_release_channel(channel);
  Ok(channel.as_str().to_string())
}

#[tauri::command]
fn app_ready_for_launch_context(
  app: tauri::AppHandle,
  state: tauri::State<'_, AppState>,
) -> Result<(), String> {
  let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
    return Err("Main window is not available.".into());
  };

  let context = state.take_launch_context();
  if let Err(err) = dispatch_launch_context(&window, &context) {
    state.set_launch_context(context);
    return Err(format!("Could not dispatch launch context: {err}"));
  }

  let _ = window.show();
  let _ = window.set_focus();

  if let Some(splash) = app.get_webview_window(SPLASH_WINDOW_LABEL) {
    let _ = splash.close();
  }

  Ok(())
}

#[tauri::command]
fn write_project_file(path: String, content: Vec<u8>) -> Result<(), String> {
  let trimmed_path = path.trim();
  if trimmed_path.is_empty() {
    return Err("Project file path is empty.".into());
  }

  fs::write(trimmed_path, content).map_err(|err| format!("Could not write project file: {err}"))
}

#[tauri::command]
fn save_binary_file(file_name: String, content: Vec<u8>) -> Result<Option<String>, String> {
  let trimmed_name = file_name.trim();
  if trimmed_name.is_empty() {
    return Err("File name is empty.".into());
  }

  let suggested_path = PathBuf::from(trimmed_name);
  let Some(path) = prompt_download_destination(&suggested_path) else {
    return Ok(None);
  };

  fs::write(&path, content).map_err(|err| format!("Could not save file: {err}"))?;
  Ok(Some(path.to_string_lossy().into_owned()))
}

#[tauri::command]
fn set_discord_presence(
  state: tauri::State<'_, AppState>,
  mode: String,
  party_id: Option<String>,
  current_size: Option<i32>,
  max_size: Option<i32>,
) -> Result<(), String> {
  let payload = DiscordPresencePayload {
    mode,
    party_id,
    current_size,
    max_size,
  };
  let activity = build_discord_activity(&payload)?;
  state.with_discord_client(|client| {
    client
      .set_activity(activity.clone())
      .map_err(|err| format!("Could not set Discord activity: {err}"))
  })
}

#[tauri::command]
fn clear_discord_presence(state: tauri::State<'_, AppState>) -> Result<(), String> {
  state.clear_discord_client();
  Ok(())
}

#[tauri::command]
fn download_update(app: tauri::AppHandle, url: String, file_name: String) -> Result<(), String> {
  let download_url = url.trim().to_string();
  if download_url.is_empty() {
    return Err("Update URL is empty.".into());
  }

  let file_name = sanitize_download_file_name(&file_name)?;
  let download_dir = update_downloads_dir();
  let download_path = download_dir.join(&file_name);

  app.state::<AppState>().clear_pending_installer_path();

  std::thread::spawn(move || {
    let result = (|| -> Result<(), String> {
      fs::create_dir_all(&download_dir)
        .map_err(|err| format!("Could not create update directory: {err}"))?;

      if download_path.exists() {
        let _ = fs::remove_file(&download_path);
      }

      let response = reqwest::blocking::get(&download_url)
        .map_err(|err| format!("Could not start update download: {err}"))?;

      if !response.status().is_success() {
        return Err(format!(
          "Update download failed with status {}.",
          response.status()
        ));
      }

      let total_bytes = response.content_length();
      let _ = app.emit(
        UPDATE_DOWNLOAD_STARTED_EVENT,
        UpdateDownloadStartedPayload {
          file_name: file_name.clone(),
          total_bytes,
        },
      );

      let mut response = response;
      let mut file = fs::File::create(&download_path)
        .map_err(|err| format!("Could not create update file: {err}"))?;
      let mut buffer = [0u8; 64 * 1024];
      let mut downloaded_bytes = 0u64;

      loop {
        let read = response
          .read(&mut buffer)
          .map_err(|err| format!("Could not read update stream: {err}"))?;

        if read == 0 {
          break;
        }

        file
          .write_all(&buffer[..read])
          .map_err(|err| format!("Could not write update file: {err}"))?;

        downloaded_bytes += read as u64;

        let progress_percent = total_bytes.map(|total| {
          if total == 0 {
            0.0
          } else {
            (downloaded_bytes as f64 / total as f64) * 100.0
          }
        });

        let _ = app.emit(
          UPDATE_DOWNLOAD_PROGRESS_EVENT,
          UpdateDownloadProgressPayload {
            file_name: file_name.clone(),
            downloaded_bytes,
            total_bytes,
            progress_percent,
          },
        );
      }

      file
        .flush()
        .map_err(|err| format!("Could not finalize update file: {err}"))?;

      app
        .state::<AppState>()
        .set_pending_installer_path(download_path.clone());

      let _ = app.emit(
        UPDATE_DOWNLOAD_FINISHED_EVENT,
        UpdateDownloadFinishedPayload {
          file_name: file_name.clone(),
          path: download_path.to_string_lossy().into_owned(),
        },
      );

      Ok(())
    })();

    if let Err(err) = result {
      let _ = fs::remove_file(&download_path);
      app.state::<AppState>().clear_pending_installer_path();
      emit_update_download_failed(&app, file_name, err);
    }
  });

  Ok(())
}

#[tauri::command]
fn clipboard_read_items() -> Result<Vec<ClipboardItemPayload>, String> {
  #[cfg(target_os = "windows")]
  {
    return Ok(read_clipboard_items_windows());
  }

  #[cfg(not(target_os = "windows"))]
  {
    Err("Clipboard bridge is only implemented on Windows.".into())
  }
}

#[tauri::command]
fn clipboard_write_items(items: Vec<ClipboardItemPayload>) -> Result<(), String> {
  #[cfg(target_os = "windows")]
  {
    return write_clipboard_items_windows(&items);
  }

  #[cfg(not(target_os = "windows"))]
  {
    let _ = items;
    Err("Clipboard bridge is only implemented on Windows.".into())
  }
}

#[tauri::command]
async fn minecraft_proxy_start(
  app: tauri::AppHandle,
  state: tauri::State<'_, AppState>,
  host: String,
  port: Option<u32>,
) -> Result<MinecraftProxyStartPayload, String> {
  let (host, target_addresses) =
    validate_minecraft_proxy_target(&host, port.unwrap_or(MINECRAFT_PROXY_DEFAULT_PORT)).await?;
  let port = target_addresses
    .first()
    .map(SocketAddr::port)
    .ok_or_else(|| "Minecraft proxy target did not resolve to any address.".to_string())?;

  let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 0)))
    .await
    .map_err(|err| format!("Could not start local Minecraft proxy: {err}"))?;
  let local_port = listener
    .local_addr()
    .map_err(|err| format!("Could not read local Minecraft proxy address: {err}"))?
    .port();

  let proxy_id = Uuid::new_v4().to_string();
  let ws_url = format!("ws://127.0.0.1:{local_port}{MINECRAFT_PROXY_PATH_PREFIX}{proxy_id}");
  let connected = Arc::new(AtomicBool::new(false));
  let (shutdown_tx, shutdown_rx) = watch::channel(false);

  let proxy = MinecraftProxyHandle {
    proxy_id: proxy_id.clone(),
    host: host.clone(),
    port,
    ws_url: ws_url.clone(),
    connected: Arc::clone(&connected),
    shutdown_tx,
  };
  let proxy_info = proxy.info();
  state.insert_minecraft_proxy(proxy);

  emit_minecraft_proxy_info(&app, MINECRAFT_PROXY_STARTED_EVENT, proxy_info);

  tauri::async_runtime::spawn(run_minecraft_proxy_listener(
    app,
    proxy_id.clone(),
    host,
    port,
    ws_url.clone(),
    target_addresses,
    listener,
    connected,
    shutdown_rx,
  ));

  Ok(MinecraftProxyStartPayload { proxy_id, ws_url })
}

#[tauri::command]
fn minecraft_proxy_stop(
  app: tauri::AppHandle,
  state: tauri::State<'_, AppState>,
  proxy_id: String,
) -> Result<(), String> {
  let proxy_id = proxy_id.trim();
  if proxy_id.is_empty() {
    return Err("Minecraft proxy id is empty.".into());
  }

  let Some(proxy) = state.remove_minecraft_proxy(proxy_id) else {
    return Ok(());
  };

  proxy.connected.store(false, Ordering::SeqCst);
  let info = proxy.info();
  let _ = proxy.shutdown_tx.send(true);
  emit_minecraft_proxy_info(&app, MINECRAFT_PROXY_STOPPED_EVENT, info);
  Ok(())
}

#[tauri::command]
fn minecraft_proxy_status(state: tauri::State<'_, AppState>) -> MinecraftProxyStatusPayload {
  state.minecraft_proxy_status()
}

#[tauri::command]
async fn minecraft_lan_discover(
  timeout_ms: Option<u64>,
) -> Result<Vec<MinecraftLanServerPayload>, String> {
  let timeout_ms = timeout_ms.unwrap_or(MINECRAFT_LAN_DISCOVERY_DEFAULT_TIMEOUT_MS);
  let socket = create_minecraft_lan_discovery_socket()?;
  let deadline = Instant::now() + Duration::from_millis(timeout_ms);
  let mut buffer = vec![0u8; MINECRAFT_LAN_PACKET_MAX_BYTES];
  let mut servers = Vec::new();
  let mut seen = HashSet::new();

  loop {
    let now = Instant::now();
    if now >= deadline {
      break;
    }

    let remaining = deadline - now;
    let received = match timeout(remaining, socket.recv_from(&mut buffer)).await {
      Ok(Ok(value)) => value,
      Ok(Err(err)) => {
        return Err(format!(
          "Could not receive Minecraft LAN announcement: {err}"
        ))
      }
      Err(_) => break,
    };

    let (read, sender) = received;
    let message = String::from_utf8_lossy(&buffer[..read]);
    let Some(server) = parse_minecraft_lan_announcement(&message, sender) else {
      continue;
    };

    if seen.insert(format!("{}:{}", server.host, server.port)) {
      servers.push(server);
    }
  }

  Ok(servers)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  #[cfg(target_os = "windows")]
  {
    if let Some(path) = early_app_config_path() {
      let mut config = load_app_config_from_path(&path);
      if !config.webview2_checked {
        config.webview2_checked = true;
        let _ = save_app_config_to_path(&path, &config);

        match ensure_webview2_runtime() {
          Ok(true) => return,
          Ok(false) => {}
          Err(err) => {
            show_native_error_message(
              "BDEngine",
              &format!(
                "{}\n\nOpen this page to install Microsoft Edge WebView2 Runtime:\n{}",
                err, WEBVIEW2_DOWNLOAD_URL
              ),
            );
            let _ = Url::parse(WEBVIEW2_DOWNLOAD_URL)
              .ok()
              .map(|url| open_url_in_system_browser(&url));
            return;
          }
        }
      }
    }
  }

  let app = tauri::Builder::default()
    .manage(AppState::default())
    .invoke_handler(tauri::generate_handler![
      get_release_channel,
      get_launch_file_path,
      set_release_channel,
      app_ready_for_launch_context,
      write_project_file,
      save_binary_file,
      set_discord_presence,
      clear_discord_presence,
      download_update,
      clipboard_read_items,
      clipboard_write_items,
      minecraft_proxy_start,
      minecraft_proxy_stop,
      minecraft_proxy_status,
      minecraft_lan_discover
    ])
    .plugin(tauri_plugin_notification::init())
    .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
      let context = parse_launch_context(argv.into_iter().skip(1));
      let _ = apply_launch_context(app, context);
    }))
    .setup(|app| {
      let channel = load_release_channel(app.handle());
      app.state::<AppState>().set_release_channel(channel);
      let context = parse_launch_context(
        env::args_os()
          .skip(1)
          .map(|arg| arg.to_string_lossy().into_owned()),
      );
      create_splash_window(app.handle())?;
      Ok(apply_launch_context(app.handle(), context)?)
    })
    .build(tauri::generate_context!())
    .expect("error while building tauri application");

  app.run(|app_handle, event| {
    if let tauri::RunEvent::Exit = event {
      shutdown_minecraft_proxies(app_handle);
      close_discord_presence_if_any(app_handle);
      launch_pending_installer_if_any(app_handle);
    }
  });
}
