//! Run with `cargo test --lib --features transport-webview-tests -- --ignored`.
//! Opt-in integration test using the installed WebView2. The test page is served
//! entirely in-process; no editor project or production service is accessed.
use super::*;
use serde_json::{json, Value};
use tauri::{WebviewUrl, WebviewWindowBuilder};
use webview2_com::{
  CallDevToolsProtocolMethodCompletedHandler, Microsoft::Web::WebView2::Win32::*,
  WebResourceRequestedEventHandler,
};
use windows::{core::HSTRING, Win32::UI::Shell::SHCreateMemStream};

async fn cdp(webview: &Webview, method: &str, params: Value) -> Value {
  let (tx, rx) = tokio::sync::oneshot::channel();
  let method = method.to_owned();
  webview
    .with_webview(move |platform| unsafe {
      let core = platform.controller().CoreWebView2().unwrap();
      core
        .CallDevToolsProtocolMethod(
          &HSTRING::from(method),
          &HSTRING::from(params.to_string()),
          &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |status, value| {
            let result = status
              .ok()
              .map(|_| serde_json::from_str::<Value>(&value).unwrap());
            let _ = tx.send(result);
            Ok(())
          })),
        )
        .unwrap();
    })
    .unwrap();
  tokio::time::timeout(Duration::from_secs(15), rx)
    .await
    .unwrap()
    .unwrap()
    .unwrap()
}

async fn evaluate(webview: &Webview, expression: String) -> Value {
  let result = cdp(
    webview,
    "Runtime.evaluate",
    json!({"expression":expression,"awaitPromise":true,"returnByValue":true}),
  )
  .await;
  assert!(
    result.get("exceptionDetails").is_none(),
    "JavaScript failed: {result}"
  );
  result["result"]["value"].clone()
}

const PAGE: &str = r#"<!doctype html><input id="file" type="file"><div id="drop"></div><script>
window.pageId = crypto.randomUUID();
window.invoke = (command, args) => window.__TAURI_INTERNALS__.invoke(command, args);
window.register = file => new Promise((resolve, reject) => {
  const requestId = crypto.randomUUID();
  const listener = event => {
    if (event.data?.type !== 'bde:file-register-result' || event.data.requestId !== requestId) return;
    chrome.webview.removeEventListener('message', listener);
    resolve(event.data);
  };
  chrome.webview.addEventListener('message', listener);
  chrome.webview.postMessageWithAdditionalObjects({type:'bde:file-register',requestId,
    expectedSize:file.size,expectedLastModified:file.lastModified}, [file]);
});
document.querySelector('#drop').addEventListener('drop', event => {
  event.preventDefault(); window.dropped = register(event.dataTransfer.files[0]);
});
</script>"#;

async fn stalled_response() -> (String, tokio::task::JoinHandle<()>) {
  use tokio::{io::AsyncWriteExt, net::TcpListener};
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  let task = tokio::spawn(async move {
    let (mut stream, _) = listener.accept().await.unwrap();
    super::tests::read_request(&mut stream).await;
    stream
      .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n")
      .await
      .unwrap();
    let mut byte = [0];
    let closed = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
      .await
      .unwrap();
    assert!(
      matches!(closed, Ok(0) | Err(_)),
      "cancelled body connection remained open"
    );
  });
  (url, task)
}

async fn scenario(webview: Webview, file: PathBuf) {
  for _ in 0..100 {
    if evaluate(&webview, "Boolean(window.register)".into()).await == true {
      break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  let info = evaluate(&webview, "invoke('native_transport_info')".into()).await;
  assert_eq!(
    info,
    json!({"version":1,"http":true,"browserFileRegistration":true,"streamingFileUpload":true})
  );
  assert_eq!(
    evaluate(&webview, "invoke('get_release_channel')".into()).await,
    "stable"
  );
  let node = cdp(
    &webview,
    "Runtime.evaluate",
    json!({"expression":"document.querySelector('#file')"}),
  )
  .await;
  cdp(
    &webview,
    "DOM.setFileInputFiles",
    json!({"objectId":node["result"]["objectId"],"files":[file]}),
  )
  .await;
  let registration = evaluate(
    &webview,
    "register(document.querySelector('#file').files[0])".into(),
  )
  .await;
  assert_eq!(registration["ok"], true, "{registration}");
  assert_eq!(registration["size"], 200_000);
  assert!(registration["name"].as_str().unwrap().contains("Кириллица"));
  let dropped = evaluate(&webview, "(async()=>{const dt=new DataTransfer();dt.items.add(document.querySelector('#file').files[0]);document.querySelector('#drop').dispatchEvent(new DragEvent('drop',{dataTransfer:dt}));return await window.dropped})()".into()).await;
  assert_eq!(dropped["ok"], true, "{dropped}");
  let virtual_file = evaluate(
    &webview,
    "register(new File(['test'],'virtual.txt'))".into(),
  )
  .await;
  assert_eq!(virtual_file["error"]["code"], "FILE_NOT_LOCAL");

  let (url, server) =
    super::tests::server(super::tests::reply(403, "denied"), Duration::ZERO).await;
  let upload = evaluate(&webview, format!(r#"(async()=>{{
    const id=window.__TAURI_INTERNALS__.transformCallback(()=>{{}});
    return await invoke('http_upload_file',{{requestId:crypto.randomUUID(), fileId:{},url:{},method:'PUT',headers:{{}},timeoutMs:5000,onProgress:'__CHANNEL__:'+id}});
  }})()"#, registration["fileId"], json!(url))).await;
  assert_eq!(upload["status"], 403);
  assert_eq!(upload["body"], "denied");
  assert_eq!(server.await.unwrap().1, 200_000);

  // Exercise the official HTTP plugin's actual IPC and URL scope on a random local port.
  let (url, server) =
    super::tests::server(super::tests::reply(200, "native-http"), Duration::ZERO).await;
  let http = evaluate(&webview, format!(r#"(async()=>{{
    const rid=await invoke('plugin:http|fetch',{{clientConfig:{{method:'GET',url:{},headers:[],data:null}}}});
    const response=await invoke('plugin:http|fetch_send',{{rid}});
    const bytes=await invoke('plugin:http|fetch_read_body',{{rid:response.rid}});
    await invoke('plugin:http|fetch_cancel_body',{{rid:response.rid}});
    return {{status:response.status,body:new TextDecoder().decode(new Uint8Array(bytes).slice(0,-1))}};
  }})()"#, json!(url))).await;
  assert_eq!(http, json!({"status":200,"body":"native-http"}));
  server.await.unwrap();

  let (url, server) = super::tests::server(Vec::new(), Duration::ZERO).await;
  let network_error = evaluate(&webview, format!(r#"(async()=>{{
    const rid=await invoke('plugin:http|fetch',{{clientConfig:{{method:'GET',url:{},headers:[],data:null}}}});
    try {{ await invoke('plugin:http|fetch_send',{{rid}}); }} catch (error) {{ return error; }}
  }})()"#, json!(url))).await;
  assert_eq!(
    network_error,
    json!({"code":"NETWORK_ERROR","message":"The network request failed."})
  );
  server.await.unwrap();

  let (url, server) = stalled_response().await;
  let cancelled = evaluate(&webview, format!(r#"(async()=>{{
    const rid=await invoke('plugin:http|fetch',{{clientConfig:{{method:'GET',url:{},headers:[],data:null}}}});
    const response=await invoke('plugin:http|fetch_send',{{rid}});
    const pending=invoke('plugin:http|fetch_read_body',{{rid:response.rid}}).catch(error=>error);
    await invoke('native_transport_info');
    await invoke('plugin:http|fetch_cancel_body',{{rid:response.rid}});
    await invoke('plugin:http|fetch_cancel_body',{{rid:response.rid}});
    return await pending;
  }})()"#, json!(url))).await;
  assert_eq!(
    cancelled,
    json!({"code":"CANCELLED","message":"The request was cancelled."})
  );
  server.await.unwrap();
  assert_eq!(
    webview
      .resources_table()
      .names()
      .filter(|(_, name)| name.starts_with("tauri_plugin_http::"))
      .count(),
    0
  );

  // Reload must also cancel a body read that is already awaiting network data.
  let (url, stalled) = stalled_response().await;
  evaluate(&webview, format!(r#"(async()=>{{
    const rid=await invoke('plugin:http|fetch',{{clientConfig:{{method:'GET',url:{},headers:[],data:null}}}});
    const response=await invoke('plugin:http|fetch_send',{{rid}});
    window.pendingBody=invoke('plugin:http|fetch_read_body',{{rid:response.rid}}).catch(()=>null);
    await invoke('native_transport_info');
    return true;
  }})()"#, json!(url))).await;

  let state = webview.state::<NativeTransport>().inner().clone();
  let epoch = state.epoch();
  webview.reload().unwrap();
  for _ in 0..100 {
    if state.epoch() != epoch {
      break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  assert_ne!(state.epoch(), epoch);
  stalled.await.unwrap();
  assert_eq!(
    webview
      .resources_table()
      .names()
      .filter(|(_, name)| name.starts_with("tauri_plugin_http::"))
      .count(),
    0
  );
  assert!(state.0.lock().unwrap().files.is_empty());
  webview
    .navigate(Url::parse("https://untrusted.invalid/__native_transport_test__").unwrap())
    .unwrap();
  for _ in 0..100 {
    if evaluate(&webview, "location.hostname".into()).await == "untrusted.invalid" {
      break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  let denied = evaluate(&webview, r#"(async()=>{
    let nativeDenied=false,httpDenied=false;
    try { await invoke('native_transport_info'); } catch (_) { nativeDenied=true; }
    try { await invoke('plugin:http|fetch',{clientConfig:{method:'GET',url:'http://127.0.0.1:1',headers:[],data:null}}); } catch (_) { httpDenied=true; }
    return {nativeDenied,httpDenied};
  })()"#.into()).await;
  assert_eq!(denied, json!({"nativeDenied":true,"httpDenied":true}));

  webview
    .navigate(Url::parse("https://bdengine.app/__native_transport_test__").unwrap())
    .unwrap();
  for _ in 0..100 {
    if evaluate(
      &webview,
      "location.hostname === 'bdengine.app' && Boolean(window.invoke)".into(),
    )
    .await
      == true
    {
      break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  let (url, closed) = stalled_response().await;
  evaluate(&webview, format!(r#"(async()=>{{
    const rid=await invoke('plugin:http|fetch',{{clientConfig:{{method:'GET',url:{},headers:[],data:null}}}});
    const response=await invoke('plugin:http|fetch_send',{{rid}});
    window.pendingBody=invoke('plugin:http|fetch_read_body',{{rid:response.rid}}).catch(()=>null);
    await invoke('native_transport_info');
    return true;
  }})()"#, json!(url))).await;
  webview.window().destroy().unwrap();
  closed.await.unwrap();
  assert_eq!(
    webview
      .resources_table()
      .names()
      .filter(|(_, name)| name.starts_with("tauri_plugin_http::"))
      .count(),
    0
  );
}

#[test]
#[ignore = "requires installed WebView2 and a Windows desktop session"]
fn real_webview_file_input_dom_drop_http_and_reload() {
  let file = super::tests::TestFile::new(200_000);
  let data_dir = std::env::temp_dir().join(format!("bde-webview-test-{}", Uuid::new_v4()));
  let result = Arc::new(Mutex::new(None));
  let saved_result = result.clone();
  let file_path = file.0.clone();
  let app = tauri::Builder::default()
    .any_thread()
    .manage(NativeTransport::default())
    .manage(crate::AppState::default())
    .plugin(http_plugin())
    .plugin(plugin())
    .invoke_handler(tauri::generate_handler![
      crate::get_release_channel,
      native_transport_info,
      http_upload_file,
      http_cancel_request,
      native_file_release
    ])
    .setup(move |app| {
      let window = WebviewWindowBuilder::new(
        app,
        "main",
        WebviewUrl::External(Url::parse("about:blank").unwrap()),
      )
      .visible(false)
      .data_directory(data_dir)
      .drag_and_drop(false)
      .build()?;
      let webview = window.as_ref().clone();
      window.with_webview(move |platform| unsafe {
        let core = platform.controller().CoreWebView2().unwrap();
        let environment = platform.environment();
        core
          .AddWebResourceRequestedFilter(
            &HSTRING::from("https://bdengine.app/__native_transport_test__*"),
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
          )
          .unwrap();
        core
          .AddWebResourceRequestedFilter(
            &HSTRING::from("https://untrusted.invalid/__native_transport_test__*"),
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
          )
          .unwrap();
        let mut token = 0;
        core
          .add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
              let stream = SHCreateMemStream(Some(PAGE.as_bytes())).unwrap();
              let response = environment.CreateWebResourceResponse(
                &stream,
                200,
                &HSTRING::from("OK"),
                &HSTRING::from("Content-Type: text/html; charset=utf-8"),
              )?;
              args.unwrap().SetResponse(&response)?;
              Ok(())
            })),
            &mut token,
          )
          .unwrap();
        core
          .Navigate(&HSTRING::from(
            "https://bdengine.app/__native_transport_test__",
          ))
          .unwrap();
      })?;
      let app_handle = app.handle().clone();
      tauri::async_runtime::spawn(async move {
        use futures_util::FutureExt;
        let outcome = std::panic::AssertUnwindSafe(scenario(webview, file_path))
          .catch_unwind()
          .await;
        *saved_result.lock().unwrap() = Some(outcome.is_ok());
        app_handle.exit(0);
      });
      let watchdog = app.handle().clone();
      tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(45)).await;
        watchdog.exit(1);
      });
      Ok(())
    })
    .build(tauri::generate_context!())
    .unwrap();
  assert_eq!(
    app.run_return(|_, event| {
      if let tauri::RunEvent::ExitRequested {
        code: None, api, ..
      } = event
      {
        api.prevent_exit();
      }
    }),
    0
  );
  assert_eq!(*result.lock().unwrap(), Some(true));
}
