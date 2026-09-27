use super::*;

struct TestConfig(PathBuf);

impl TestConfig {
  fn new() -> Self {
    Self(env::temp_dir().join(format!("bdengine-config-test-{}.json", Uuid::new_v4())))
  }
}

impl Drop for TestConfig {
  fn drop(&mut self) {
    let _ = fs::remove_file(&self.0);
  }
}

#[test]
fn existing_config_keeps_channel_and_runtime_check_when_graphics_changes() {
  let file = TestConfig::new();
  fs::write(
    &file.0,
    r#"{"releaseChannel":"beta","webview2Checked":true}"#,
  )
  .unwrap();
  let old = read_app_config_from_path(&file.0).unwrap();
  assert_eq!(old.graphics_backend, GraphicsBackend::Auto);
  let launch = LaunchGraphics::new(old.graphics_backend, "");
  let saved = persist_graphics_backend_to_path(&file.0, "d3d11on12").unwrap();
  assert_eq!(
    serde_json::to_value(launch.settings(saved)).unwrap(),
    serde_json::json!({
      "backend": "d3d11on12", "restartRequired": true
    })
  );
  let updated = read_app_config_from_path(&file.0).unwrap();
  assert_eq!(updated.release_channel.as_str(), "beta");
  assert!(updated.webview2_checked);
  let restarted = LaunchGraphics::new(updated.graphics_backend, "");
  assert!(
    !restarted
      .settings(updated.graphics_backend)
      .restart_required
  );
  let saved = persist_graphics_backend_to_path(&file.0, "auto").unwrap();
  assert!(restarted.settings(saved).restart_required);
  let restored = read_app_config_from_path(&file.0).unwrap();
  assert_eq!(restored.graphics_backend, GraphicsBackend::Auto);
  assert_eq!(restored.release_channel.as_str(), "beta");
}

#[test]
fn invalid_mode_or_corrupt_config_is_not_silently_overwritten() {
  let file = TestConfig::new();
  let original = r#"{"releaseChannel":"beta","webview2Checked":true}"#;
  fs::write(&file.0, original).unwrap();
  assert!(persist_graphics_backend_to_path(&file.0, "opengl").is_err());
  assert_eq!(fs::read_to_string(&file.0).unwrap(), original);
  fs::write(&file.0, "broken config").unwrap();
  assert!(persist_graphics_backend_to_path(&file.0, "d3d11on12").is_err());
  assert_eq!(fs::read_to_string(&file.0).unwrap(), "broken config");
}

#[test]
fn absent_config_uses_auto_and_unknown_saved_backend_keeps_other_settings() {
  let file = TestConfig::new();
  assert_eq!(
    read_app_config_from_path(&file.0).unwrap().graphics_backend,
    GraphicsBackend::Auto
  );
  fs::write(
    &file.0,
    r#"{"releaseChannel":"beta","webview2Checked":true,"graphicsBackend":"future-mode"}"#,
  )
  .unwrap();
  let config = read_app_config_from_path(&file.0).unwrap();
  assert_eq!(config.graphics_backend, GraphicsBackend::Auto);
  assert_eq!(config.release_channel.as_str(), "beta");
  assert!(config.webview2_checked);
}

#[test]
fn restart_does_not_replay_initial_files_or_deep_links() {
  let mut environment = tauri::Env::default();
  environment.args_os = vec![
    "bdengine_app.exe".into(),
    "old-project.bdengine".into(),
    "bdengine://open?project=old".into(),
  ];
  let cleaned = clean_restart_environment(environment);
  assert_eq!(
    cleaned.args_os,
    vec![std::ffi::OsString::from("bdengine_app.exe")]
  );
}
