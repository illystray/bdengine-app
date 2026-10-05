//! Keep the official plugin API, with an additional native caller-origin check.
//! Tauri considers the remote frontendDist URL a local app origin, so its ACL
//! alone cannot distinguish that URL from other local pages in the main webview.
use super::require_editor;
use tauri::{
  ipc::Invoke,
  plugin::{Plugin, TauriPlugin},
  AppHandle, RunEvent, Wry,
};

struct EditorHttp(TauriPlugin<Wry>);

impl Plugin<Wry> for EditorHttp {
  fn name(&self) -> &'static str {
    self.0.name()
  }

  fn initialize(
    &mut self,
    app: &AppHandle,
    config: serde_json::Value,
  ) -> std::result::Result<(), Box<dyn std::error::Error>> {
    self.0.initialize(app, config)
  }

  fn on_event(&mut self, app: &AppHandle, event: &RunEvent) {
    self.0.on_event(app, event);
  }

  fn extend_api(&mut self, invoke: Invoke) -> bool {
    if let Err(error) = require_editor(invoke.message.webview_ref()) {
      invoke.resolver.reject(error);
      return true;
    }
    if invoke.message.command() == "fetch" {
      let allowed = match invoke.message.payload() {
        tauri::ipc::InvokeBody::Json(value) => value["clientConfig"]["url"]
          .as_str()
          .is_some_and(|url| super::parse_url(url).is_ok()),
        _ => false,
      };
      if !allowed {
        invoke.resolver.reject(super::error("INVALID_REQUEST"));
        return true;
      }
    }
    self.0.extend_api(invoke)
  }
}

pub(super) fn init() -> impl Plugin<Wry> {
  EditorHttp(tauri_plugin_http::init())
}
