use super::*;
use webview2_com::{
  CoTaskMemPWSTR, Microsoft::Web::WebView2::Win32::*, WebMessageReceivedEventHandler,
};
use windows::core::{Interface, HSTRING, PWSTR};

unsafe fn string_value(
  get: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>,
) -> windows::core::Result<String> {
  let mut value = PWSTR::null();
  get(&mut value)?;
  Ok(CoTaskMemPWSTR::from(value).to_string())
}

pub(super) fn install(webview: Webview) {
  let owner = webview.clone();
  let _ = webview.with_webview(move |platform| unsafe {
    let Ok(core) = platform.controller().CoreWebView2() else {
      return;
    };
    let state = owner.state::<NativeTransport>().inner().clone();
    let mut token = 0;
    let _ = core.add_WebMessageReceived(
      &WebMessageReceivedEventHandler::create(Box::new(move |sender, args| {
        let (Some(core), Some(args)) = (sender, args) else {
          return Ok(());
        };
        let source = string_value(|out| args.Source(out))?;
        if !Url::parse(&source)
          .map(|url| trusted_url(&url))
          .unwrap_or(false)
        {
          return Ok(());
        }
        if string_value(|out| core.Source(out))? != source {
          return Ok(());
        }
        let message = string_value(|out| args.WebMessageAsJson(out))?;
        if message.len() > 4096 {
          return Ok(());
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&message) else {
          return Ok(());
        };
        let extended = args.cast::<ICoreWebView2WebMessageReceivedEventArgs2>();
        if value["type"] == "bde:transport-probe" {
          let mut page = state.0.lock().unwrap();
          page.registration_supported = extended.is_ok() && value["supported"] == true;
          page.registration_probed = true;
          return Ok(());
        }
        if value["type"] != "bde:file-register" {
          return Ok(());
        }
        let Some(request_id) = value["requestId"]
          .as_str()
          .filter(|id| !id.is_empty() && id.len() <= 128)
        else {
          return Ok(());
        };
        let request_id = request_id.to_owned();
        let registration = serde_json::from_value::<RegistrationRequest>(value)
          .map_err(|_| error("INVALID_REQUEST"));
        let path = (|| -> Result<PathBuf> {
          let extended = extended.map_err(|_| error("UNSUPPORTED_RUNTIME"))?;
          let objects = extended
            .AdditionalObjects()
            .map_err(|_| error("FILE_NOT_LOCAL"))?;
          let mut count = 0;
          objects
            .Count(&mut count)
            .map_err(|_| error("FILE_NOT_LOCAL"))?;
          if count != 1 {
            return Err(error("FILE_NOT_LOCAL"));
          }
          let file = objects
            .GetValueAtIndex(0)
            .and_then(|object| object.cast::<ICoreWebView2File>())
            .map_err(|_| error("FILE_NOT_LOCAL"))?;
          let path = string_value(|out| file.Path(out)).map_err(|_| error("FILE_NOT_LOCAL"))?;
          if path.is_empty() {
            return Err(error("FILE_NOT_LOCAL"));
          }
          Ok(PathBuf::from(path))
        })();
        let epoch = state.epoch();
        let state = state.clone();
        let owner = owner.clone();
        tauri::async_runtime::spawn(async move {
          let worker_state = state.clone();
          let result = tokio::task::spawn_blocking(move || {
            let request = registration?;
            worker_state.register(epoch, path?, &request)
          })
          .await
          .unwrap_or_else(|_| Err(error("FILE_NOT_FOUND")));
          let mut response = match result {
            Ok(registration) => {
              let mut value = serde_json::to_value(registration).unwrap();
              value["ok"] = true.into();
              value
            }
            Err(err) => serde_json::json!({"ok": false, "error": err}),
          };
          response["type"] = "bde:file-register-result".into();
          response["requestId"] = request_id.into();
          let _ = owner.with_webview(move |platform| {
            if state.epoch() != epoch {
              return;
            }
            let Ok(core) = platform.controller().CoreWebView2() else {
              return;
            };
            if string_value(|out| core.Source(out)).ok().as_deref() != Some(&source) {
              return;
            }
            let _ = core.PostWebMessageAsJson(&HSTRING::from(response.to_string()));
          });
        });
        Ok(())
      })),
      &mut token,
    );
  });
}
