use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::{
  env,
  fs,
  mem,
  path::{Path, PathBuf},
  process::{Command, Stdio},
  slice,
  sync::Mutex,
};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use tauri::{
  image::Image,
  webview::{DownloadEvent, NewWindowResponse, PageLoadEvent},
  Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};
use url::Url;
#[cfg(target_os = "windows")]
use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{HANDLE, HGLOBAL, HWND},
    System::{
      DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW, SetClipboardData,
      },
      Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
      Ole::CF_UNICODETEXT,
    },
    UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK},
  },
};

const MAIN_WINDOW_LABEL: &str = "main";
const STABLE_BASE_URL: &str = "https://bdengine.app/";
const BETA_BASE_URL: &str = "https://beta.bdengine.app/";
const TASKBAR_ICON_PNG: &[u8] = include_bytes!("../icons/32x32.png");
const APP_CONFIG_FILE_NAME: &str = "config.json";
const APP_IDENTIFIER: &str = "app.bdengine.desktop";
const APP_VERSION: u32 = 1;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;
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

#[derive(Default)]
struct AppState {
  launch_context: Mutex<LaunchContext>,
  release_channel: Mutex<ReleaseChannel>,
}

impl AppState {
  fn get_launch_context(&self) -> LaunchContext {
    self.launch_context.lock().expect("launch state poisoned").clone()
  }

  fn set_launch_context(&self, context: LaunchContext) {
    *self.launch_context.lock().expect("launch state poisoned") = context;
  }

  fn get_release_channel(&self) -> ReleaseChannel {
    *self.release_channel.lock().expect("release channel state poisoned")
  }

  fn set_release_channel(&self, channel: ReleaseChannel) {
    *self.release_channel.lock().expect("release channel state poisoned") = channel;
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
  matches!(url.scheme(), "https") && matches!(url.domain(), Some("bdengine.app" | "beta.bdengine.app"))
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
  let current_exe = env::current_exe().map_err(|err| format!("Could not resolve current executable: {err}"))?;
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
    format!(r"HKCU\Software\Microsoft\EdgeUpdate\Clients\{}", WEBVIEW2_CLIENT_GUID),
    format!(r"HKLM\Software\Microsoft\EdgeUpdate\Clients\{}", WEBVIEW2_CLIENT_GUID),
    format!(r"HKLM\Software\WOW6432Node\Microsoft\EdgeUpdate\Clients\{}", WEBVIEW2_CLIENT_GUID),
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
        let value = line.split_whitespace().last().unwrap_or_default().trim().to_string();
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

  let installer_path =
    resolve_webview2_installer_path().ok_or_else(|| "WebView2 Runtime is missing and no bundled installer was found.".to_string())?;

  let status = Command::new(&installer_path)
    .args(["/silent", "/install"])
    .creation_flags(CREATE_NO_WINDOW)
    .status()
    .map_err(|err| format!("Could not launch bundled WebView2 installer: {err}"))?;

  if !status.success() {
    return Err(format!("Bundled WebView2 installer exited with code {:?}.", status.code()));
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
    let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|err| format!("GlobalAlloc failed: {err}"))?;
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
    slice::from_raw_parts(utf16.as_ptr() as *const u8, mem::size_of_val(utf16.as_slice())).to_vec()
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
    SetClipboardData(format, Some(handle)).map_err(|err| format!("SetClipboardData failed: {err}"))?;
  }
  Ok(())
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
  [Console]::Out.Write($dialog.FileName)
}}
"#,
    file_name = escape_powershell_single_quoted(file_name),
    initial_directory = escape_powershell_single_quoted(initial_directory)
  );

  let output = Command::new("powershell")
    .args(["-NoProfile", "-STA", "-WindowStyle", "Hidden", "-Command", &script])
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
  env::var_os("APPDATA").map(PathBuf::from).map(|dir| dir.join(APP_IDENTIFIER).join(APP_CONFIG_FILE_NAME))
}

fn app_config_path(app: &tauri::AppHandle) -> Option<PathBuf> {
  app.path().app_config_dir().ok().map(|dir| dir.join(APP_CONFIG_FILE_NAME))
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

  fs::create_dir_all(parent).map_err(|err| format!("Could not create app config directory: {err}"))?;
  let contents = serde_json::to_vec_pretty(config).map_err(|err| format!("Could not serialize app config: {err}"))?;
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
  Image::from_bytes(TASKBAR_ICON_PNG).ok().map(|icon| icon.to_owned())
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

fn create_main_window(app: &tauri::AppHandle, context: &LaunchContext) -> tauri::Result<WebviewWindow> {
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
        let state = app_handle.state::<AppState>();
        let context = state.get_launch_context();
        let _ = dispatch_launch_context(&window, &context);
        let _ = window.show();
        let _ = window.set_focus();
      }
    });

  if let Some(icon) = taskbar_icon() {
    builder = builder.icon(icon)?;
  }

  let window = builder.build()?;

  if let Some(icon) = taskbar_icon() {
    let _ = window.set_icon(icon);
  }

  Ok(window)
}

fn apply_launch_context(app: &tauri::AppHandle, context: LaunchContext) -> tauri::Result<()> {
  let state = app.state::<AppState>();
  state.set_launch_context(context.clone());
  let target_url = build_remote_url(&context, state.get_release_channel());

  let window = if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
    window
  } else {
    return create_main_window(app, &context).map(|_| ());
  };

  if !window.is_visible()? {
    let _ = window.show();
  }
  let _ = window.set_focus();

  let should_navigate = match window.url() {
    Ok(current_url) => current_url != target_url,
    Err(_) => true,
  };

  if should_navigate {
    window.navigate(target_url)?;
  } else {
    dispatch_launch_context(&window, &context)?;
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
  let channel = ReleaseChannel::from_str(&channel).ok_or_else(|| "Unsupported release channel.".to_string())?;
  persist_release_channel(&app, channel)?;
  state.set_release_channel(channel);
  Ok(channel.as_str().to_string())
}

#[tauri::command]
fn write_project_file(path: String, content: String) -> Result<(), String> {
  let trimmed_path = path.trim();
  if trimmed_path.is_empty() {
    return Err("Project file path is empty.".into());
  }

  fs::write(trimmed_path, content).map_err(|err| format!("Could not write project file: {err}"))
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
            let _ = Url::parse(WEBVIEW2_DOWNLOAD_URL).ok().map(|url| open_url_in_system_browser(&url));
            return;
          }
        }
      }
    }
  }

  tauri::Builder::default()
    .manage(AppState::default())
    .invoke_handler(tauri::generate_handler![
      get_release_channel,
      get_launch_file_path,
      set_release_channel,
      write_project_file,
      clipboard_read_items,
      clipboard_write_items
    ])
    .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
      let context = parse_launch_context(argv.into_iter().skip(1));
      let _ = apply_launch_context(app, context);
    }))
    .setup(|app| {
      let channel = load_release_channel(app.handle());
      app.state::<AppState>().set_release_channel(channel);
      let context = parse_launch_context(env::args_os().skip(1).map(|arg| arg.to_string_lossy().into_owned()));
      Ok(apply_launch_context(app.handle(), context)?)
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}

