use super::*;
use std::{fs, io::Write};
use tokio::{io::AsyncWriteExt, net::TcpListener};

pub(super) struct TestFile(pub(super) PathBuf);
impl TestFile {
  pub(super) fn new(size: u64) -> Self {
    let path = std::env::temp_dir().join(format!("bde transport Кириллица {}.bin", Uuid::new_v4()));
    File::create(&path).unwrap().set_len(size).unwrap();
    Self(path)
  }
  fn register(&self, state: &NativeTransport) -> Registration {
    let fp = fingerprint(&File::open(&self.0).unwrap()).unwrap();
    state
      .register(
        state.epoch(),
        self.0.clone(),
        &RegistrationRequest {
          expected_size: fp.size,
          expected_last_modified: fp.modified_ms,
        },
      )
      .unwrap()
  }
}
impl Drop for TestFile {
  fn drop(&mut self) {
    let _ = fs::remove_file(&self.0);
  }
}

pub(super) async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, u64) {
  let mut header = Vec::new();
  while !header.ends_with(b"\r\n\r\n") {
    let mut byte = [0];
    if stream.read_exact(&mut byte).await.is_err() {
      return (String::new(), 0);
    }
    header.push(byte[0]);
    assert!(header.len() < 16384);
  }
  let header = String::from_utf8(header).unwrap();
  let size: u64 = header
    .lines()
    .find_map(|line| {
      line
        .to_ascii_lowercase()
        .strip_prefix("content-length: ")
        .map(|n| n.parse().unwrap())
    })
    .unwrap_or(0);
  (header, size)
}

pub(super) async fn server(
  response: Vec<u8>,
  delay: Duration,
) -> (String, tokio::task::JoinHandle<(String, u64)>) {
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let url = format!(
    "http://{}/upload?signature=must-not-leak",
    listener.local_addr().unwrap()
  );
  let task = tokio::spawn(async move {
    let (mut stream, _) = listener.accept().await.unwrap();
    let (header, size) = read_request(&mut stream).await;
    let mut buffer = [0u8; BUFFER_SIZE];
    let mut received = 0;
    while received < size {
      let max = buffer.len().min((size - received) as usize);
      match stream.read(&mut buffer[..max]).await {
        Ok(0) | Err(_) => return (header, received),
        Ok(n) => {
          assert!(buffer[..n].iter().all(|byte| *byte == 0));
          received += n as u64;
        }
      }
    }
    tokio::time::sleep(delay).await;
    let _ = stream.write_all(&response).await;
    (header, received)
  });
  (url, task)
}

pub(super) fn reply(status: u16, body: &str) -> Vec<u8> {
  format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nX-Test: one\r\nX-Test: two\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}

async fn upload(
  state: &NativeTransport,
  registration: &Registration,
  url: String,
  method: &str,
  timeout: u64,
) -> Result<UploadResponse> {
  run_upload(
    state.begin(&Uuid::new_v4().to_string(), &registration.file_id)?,
    url,
    method.into(),
    HashMap::new(),
    timeout,
    Arc::new(|_| {}),
  )
  .await
}

#[test]
fn registration_checks_metadata_and_page_ownership() {
  let file = TestFile::new(4096);
  let state = NativeTransport::default();
  let registration = file.register(&state);
  assert_eq!(registration.size, 4096);
  assert!(registration.name.contains("Кириллица"));
  let value = serde_json::to_string(&registration).unwrap();
  assert!(!value.contains("path"));
  let stale_epoch = state.epoch();
  state.reset();
  assert!(matches!(
    state.begin("test", &registration.file_id),
    Err(TransportError {
      code: "FILE_RELEASED",
      ..
    })
  ));
  assert_eq!(
    state
      .register(
        stale_epoch,
        file.0.clone(),
        &RegistrationRequest {
          expected_size: 4096,
          expected_last_modified: registration.last_modified
        }
      )
      .err()
      .unwrap()
      .code,
    "FILE_RELEASED"
  );
  assert_eq!(
    state
      .register(
        state.epoch(),
        file.0.clone(),
        &RegistrationRequest {
          expected_size: 0,
          expected_last_modified: 0
        }
      )
      .err()
      .unwrap()
      .code,
    "FILE_CHANGED"
  );
}

#[test]
fn file_change_and_replacement_are_detected() {
  let file = TestFile::new(4096);
  let state = NativeTransport::default();
  let registration = file.register(&state);
  File::options()
    .write(true)
    .open(&file.0)
    .unwrap()
    .write_all(b"changed")
    .unwrap();
  assert_eq!(
    state.0.lock().unwrap().files[&registration.file_id]
      .open_upload()
      .unwrap_err()
      .code,
    "FILE_CHANGED"
  );
  let fresh = file.register(&state);
  let original_times = fs::metadata(&file.0).unwrap();
  fs::remove_file(&file.0).unwrap();
  let replacement = File::create(&file.0).unwrap();
  replacement.set_len(4096).unwrap();
  replacement
    .set_times(fs::FileTimes::new().set_modified(original_times.modified().unwrap()))
    .unwrap();
  drop(replacement);
  assert_eq!(
    state.0.lock().unwrap().files[&fresh.file_id]
      .open_upload()
      .unwrap_err()
      .code,
    "FILE_CHANGED"
  );
}

#[tokio::test]
async fn immediate_and_repeated_cancellation_and_release() {
  let file = TestFile::new(4096);
  let state = NativeTransport::default();
  let registration = file.register(&state);
  state.cancel("early").await.unwrap();
  state.cancel("early").await.unwrap();
  assert!(matches!(
    state.begin("early", &registration.file_id),
    Err(TransportError {
      code: "CANCELLED",
      ..
    })
  ));
  state.release(&registration.file_id).await;
  state.release(&registration.file_id).await;
  assert!(state.0.lock().unwrap().files.is_empty());
  fs::remove_file(&file.0).unwrap();
}

#[tokio::test]
async fn put_post_patch_and_http_errors_preserve_status_and_body() {
  let file = TestFile::new(200_000);
  let state = NativeTransport::default();
  let registration = file.register(&state);
  for (method, status) in [("PUT", 200), ("POST", 403), ("PATCH", 500)] {
    let (url, task) = server(reply(status, "result"), Duration::ZERO).await;
    let result = upload(&state, &registration, url, method, 5000)
      .await
      .unwrap();
    assert_eq!(result.status, status);
    assert_eq!(result.body, "result");
    assert_eq!(
      result
        .headers
        .iter()
        .filter(|(name, _)| name == "x-test")
        .count(),
      2
    );
    let (headers, bytes) = task.await.unwrap();
    assert!(headers.starts_with(method));
    assert_eq!(bytes, registration.size);
    assert!(headers.to_lowercase().contains("content-length: 200000"));
  }
}

#[tokio::test]
async fn cancellation_stops_upload_and_closes_handles() {
  for release in [false, true] {
    let file = TestFile::new(64 * 1024 * 1024);
    let state = NativeTransport::default();
    let registration = file.register(&state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let lease = state
      .begin("cancel-running", &registration.file_id)
      .unwrap();
    let uploading = tokio::spawn(run_upload(
      lease,
      url,
      "PUT".into(),
      HashMap::new(),
      5000,
      Arc::new(|_| {}),
    ));
    let (mut socket, _) = listener.accept().await.unwrap();
    let (_, size) = read_request(&mut socket).await;
    assert_eq!(size, registration.size);
    if release {
      state.release(&registration.file_id).await;
    } else {
      state.cancel("cancel-running").await.unwrap();
    }
    assert_eq!(uploading.await.unwrap().unwrap_err().code, "CANCELLED");
    assert!(state.0.lock().unwrap().requests.is_empty());
    // No lingering producer may hold a write/delete-denying handle after cancellation.
    File::options().write(true).open(&file.0).unwrap();
  }
}

#[tokio::test]
async fn reload_cancels_active_transfer_and_invalidates_file_ids() {
  let file = TestFile::new(64 * 1024 * 1024);
  let state = NativeTransport::default();
  let registration = file.register(&state);
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap());
  let task = tokio::spawn(run_upload(
    state.begin("reload", &registration.file_id).unwrap(),
    url,
    "PUT".into(),
    HashMap::new(),
    5000,
    Arc::new(|_| {}),
  ));
  let (_socket, _) = listener.accept().await.unwrap();
  state.reset();
  assert_eq!(task.await.unwrap().unwrap_err().code, "CANCELLED");
  assert!(state.0.lock().unwrap().files.is_empty());
  assert!(state.0.lock().unwrap().requests.is_empty());
  fs::remove_file(&file.0).unwrap();
}

#[tokio::test]
async fn timeout_network_failure_and_response_limits() {
  let file = TestFile::new(200_000);
  let state = NativeTransport::default();
  let registration = file.register(&state);
  let (url, task) = server(reply(200, "ok"), Duration::from_secs(2)).await;
  assert_eq!(
    upload(&state, &registration, url, "PUT", 100)
      .await
      .unwrap_err()
      .code,
    "TIMEOUT"
  );
  task.abort();
  let (url, task) = server(Vec::new(), Duration::ZERO).await;
  let error = upload(&state, &registration, url, "PUT", 5000)
    .await
    .unwrap_err();
  assert_eq!(error.code, "NETWORK_ERROR");
  assert!(!serde_json::to_string(&error).unwrap().contains("signature"));
  task.await.unwrap();
  for response in [
    format!(
      "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
      RESPONSE_LIMIT + 1
    )
    .into_bytes(),
    format!(
      "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
      RESPONSE_LIMIT + 1,
      "a".repeat(RESPONSE_LIMIT + 1)
    )
    .into_bytes(),
  ] {
    let (url, task) = server(response, Duration::ZERO).await;
    assert_eq!(
      upload(&state, &registration, url, "PUT", 5000)
        .await
        .unwrap_err()
        .code,
      "RESPONSE_TOO_LARGE"
    );
    task.await.unwrap();
  }
}

#[tokio::test]
async fn redirects_replay_file_and_strip_cross_origin_credentials() {
  let file = TestFile::new(200_000);
  let state = NativeTransport::default();
  let registration = file.register(&state);
  let (destination, target) = server(reply(200, "done"), Duration::ZERO).await;
  let redirect = format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
  let (url, source) = server(redirect.into_bytes(), Duration::ZERO).await;
  let headers = HashMap::from([
    ("Authorization".into(), "private-key".into()),
    ("Cookie".into(), "session=secret".into()),
  ]);
  let result = run_upload(
    state.begin("redirect", &registration.file_id).unwrap(),
    url,
    "PUT".into(),
    headers,
    5000,
    Arc::new(|_| {}),
  )
  .await
  .unwrap();
  assert_eq!(result.body, "done");
  let (headers, bytes) = source.await.unwrap();
  assert!(headers.contains("private-key"));
  assert_eq!(bytes, registration.size);
  let (headers, bytes) = target.await.unwrap();
  assert!(!headers.contains("private-key") && !headers.contains("secret"));
  assert_eq!(bytes, registration.size);
}

#[tokio::test]
async fn large_file_streaming_and_progress_wait_for_response() {
  let file = TestFile::new(64 * 1024 * 1024);
  let state = NativeTransport::default();
  let registration = file.register(&state);
  let (url, task) = server(reply(200, "complete"), Duration::from_millis(250)).await;
  let events = Arc::new(Mutex::new(Vec::new()));
  let captured = events.clone();
  let start = Instant::now();
  let result = run_upload(
    state.begin("large", &registration.file_id).unwrap(),
    url,
    "PUT".into(),
    HashMap::new(),
    15_000,
    Arc::new(move |event| captured.lock().unwrap().push((Instant::now(), event))),
  )
  .await
  .unwrap();
  assert!(start.elapsed() >= Duration::from_millis(250));
  assert_eq!(result.body, "complete");
  assert_eq!(task.await.unwrap().1, registration.size);
  let events = events.lock().unwrap();
  assert_eq!(events.last().unwrap().1.transferred, registration.size);
  for pair in events.windows(2) {
    assert!(pair[1].1.transferred > pair[0].1.transferred);
    assert!(pair[1].0.duration_since(pair[0].0) >= Duration::from_millis(95));
  }
}

#[test]
fn network_policy_accepts_arbitrary_http_hosts_only() {
  for url in [
    "https://example.com:9443/a?signature=x",
    "http://localhost:8080",
    "http://127.0.0.1:1234",
    "http://[::1]:3000",
    "http://192.168.1.1",
  ] {
    assert!(parse_url(url).is_ok());
  }
  for url in [
    "file:///C:/private",
    "data:text/plain,a",
    "ftp://example.com",
    "nonsense",
  ] {
    assert!(parse_url(url).is_err());
  }
  for url in ["https://bdengine.app/", "https://beta.bdengine.app/a"] {
    assert!(trusted_url(&Url::parse(url).unwrap()));
  }
  for url in [
    "http://bdengine.app",
    "https://bdengine.app:444",
    "https://bdengine.app.example.com",
    "https://evil@bdengine.app",
  ] {
    assert!(!trusted_url(&Url::parse(url).unwrap()));
  }
}
