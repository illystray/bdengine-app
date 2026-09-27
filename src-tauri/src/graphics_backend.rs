use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphicsBackend {
  D3d11on12,
  #[default]
  #[serde(other)]
  Auto,
}

impl GraphicsBackend {
  pub fn parse(value: &str) -> Result<Self, String> {
    match value {
      "auto" => Ok(Self::Auto),
      "d3d11on12" => Ok(Self::D3d11on12),
      _ => Err("Unsupported graphics backend. Expected 'auto' or 'd3d11on12'.".into()),
    }
  }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphicsSettings {
  pub backend: GraphicsBackend,
  pub restart_required: bool,
}

// Immutable for the lifetime of the process, including when settings change.
pub struct LaunchGraphics {
  backend: GraphicsBackend,
  external_override: bool,
}

impl LaunchGraphics {
  pub fn new(backend: GraphicsBackend, external_arguments: &str) -> Self {
    let external_override = external_arguments.split_ascii_whitespace().any(|argument| {
      let argument = argument.trim_matches('"');
      argument == "--use-angle" || argument.starts_with("--use-angle=")
    });
    Self {
      backend,
      external_override,
    }
  }

  pub fn browser_arguments(&self) -> Option<&'static str> {
    if self.backend == GraphicsBackend::Auto || self.external_override {
      return None;
    }
    // additional_browser_args replaces Wry 0.54's defaults. Keep them and use
    // these same options for both WebViews; never inject flags into process env.
    Some("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --use-angle=d3d11on12")
  }

  pub fn settings(&self, saved: GraphicsBackend) -> GraphicsSettings {
    GraphicsSettings {
      backend: saved,
      restart_required: saved != self.backend,
    }
  }

  pub fn ensure_configurable(&self) -> Result<(), String> {
    if self.external_override {
      Err("Graphics backend is overridden by launch arguments. Close the app and launch the executable normally before changing graphics settings.".into())
    } else {
      Ok(())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn automatic_mode_preserves_webview_defaults() {
    assert_eq!(
      LaunchGraphics::new(GraphicsBackend::Auto, "").browser_arguments(),
      None
    );
  }

  #[test]
  fn diagnostic_overrides_are_respected_and_do_not_claim_settings_can_apply() {
    for arguments in ["--use-angle=gl", "--use-angle d3d11", "\"--use-angle=gl\""] {
      let launch = LaunchGraphics::new(GraphicsBackend::D3d11on12, arguments);
      assert_eq!(launch.browser_arguments(), None);
      assert!(launch.ensure_configurable().is_err());
    }
  }

  #[test]
  fn changing_selection_does_not_change_running_webview_options() {
    let launch = LaunchGraphics::new(GraphicsBackend::Auto, "");
    assert!(!launch.settings(GraphicsBackend::Auto).restart_required);
    assert!(launch.settings(GraphicsBackend::D3d11on12).restart_required);
    assert_eq!(launch.browser_arguments(), None);
    assert!(!launch.settings(GraphicsBackend::Auto).restart_required);
  }

  #[test]
  fn switching_to_compatibility_and_back_survives_relaunch() {
    let external = "--enable-logging";
    let compatibility = LaunchGraphics::new(GraphicsBackend::D3d11on12, external);
    assert!(
      !compatibility
        .settings(GraphicsBackend::D3d11on12)
        .restart_required
    );
    assert!(
      compatibility
        .settings(GraphicsBackend::Auto)
        .restart_required
    );
    let arguments = compatibility.browser_arguments().unwrap();
    assert!(arguments.contains("--use-angle=d3d11on12"));
    assert!(arguments.contains("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection"));
    assert!(!arguments.contains("--enable-logging"));
    let automatic = LaunchGraphics::new(GraphicsBackend::Auto, external);
    assert_eq!(automatic.browser_arguments(), None);
    assert!(!automatic.settings(GraphicsBackend::Auto).restart_required);
  }
}
