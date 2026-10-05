fn main() {
  tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
    tauri_build::AppManifest::new().commands(&[
      "get_release_channel",
      "get_graphics_settings",
      "set_graphics_backend",
      "restart_app",
      "get_launch_file_path",
      "set_release_channel",
      "app_ready_for_launch_context",
      "write_project_file",
      "save_binary_file",
      "set_discord_presence",
      "clear_discord_presence",
      "download_update",
      "clipboard_read_items",
      "clipboard_write_items",
      "minecraft_proxy_start",
      "minecraft_proxy_stop",
      "minecraft_proxy_status",
      "minecraft_lan_discover",
      "native_transport_info",
      "http_upload_file",
      "http_cancel_request",
      "native_file_release",
    ]),
  ))
  .expect("failed to build application permissions");
  // Tauri normally attaches the Windows manifest only to application binaries.
  // The opt-in WebView2 test also needs Common Controls v6 during process loading.
  if cfg!(windows) && std::env::var_os("CARGO_FEATURE_TRANSPORT_WEBVIEW_TESTS").is_some() {
    let out = std::env::var("OUT_DIR").expect("OUT_DIR must be set");
    println!("cargo:rustc-link-arg={out}/resource.lib");
  }
}
