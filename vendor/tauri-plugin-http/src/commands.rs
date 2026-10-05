// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{future::Future, pin::Pin, str::FromStr, sync::Arc, time::Duration};

use http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use reqwest::{redirect::Policy, NoProxy};
use serde::{Deserialize, Serialize};
use tauri::{
    async_runtime::Mutex,
    command,
    ipc::{CommandScope, GlobalScope},
    Manager, ResourceId, ResourceTable, Runtime, State, Webview,
};
use tokio::sync::oneshot::{channel, Receiver, Sender};

use crate::{
    scope::{Entry, Scope},
    Error, Http, Result,
};

const HTTP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

struct ReqwestResponse {
    response: Mutex<reqwest::Response>,
    cancel: tokio_util::sync::CancellationToken,
}
impl tauri::Resource for ReqwestResponse {
    fn close(self: Arc<Self>) { self.cancel.cancel(); }
}

type CancelableResponseResult = Result<reqwest::Response>;
type CancelableResponseFuture =
    Pin<Box<dyn Future<Output = CancelableResponseResult> + Send + Sync>>;

struct FetchRequest {
    fut: Mutex<CancelableResponseFuture>,
    abort_tx_rid: ResourceId,
    abort_rx_rid: ResourceId,
}
impl tauri::Resource for FetchRequest {}

struct AbortSender(Sender<()>);
impl tauri::Resource for AbortRecveiver {}

impl AbortSender {
    fn abort(self) {
        let _ = self.0.send(());
    }
}

struct AbortRecveiver(Receiver<()>);
impl tauri::Resource for AbortSender {}

trait AddRequest {
    fn add_request(&mut self, fut: CancelableResponseFuture) -> ResourceId;
}

impl AddRequest for ResourceTable {
    fn add_request(&mut self, fut: CancelableResponseFuture) -> ResourceId {
        let (tx, rx) = channel::<()>();
        let (tx, rx) = (AbortSender(tx), AbortRecveiver(rx));
        let req = FetchRequest {
            fut: Mutex::new(fut),
            abort_tx_rid: self.add(tx),
            abort_rx_rid: self.add(rx),
        };
        self.add(req)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchResponse {
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    url: String,
    rid: ResourceId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] //feature flags shoudln't affect api
pub struct DangerousSettings {
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientConfig {
    method: String,
    url: url::Url,
    headers: Vec<(String, String)>,
    data: Option<Vec<u8>>,
    connect_timeout: Option<u64>,
    max_redirections: Option<usize>,
    proxy: Option<Proxy>,
    danger: Option<DangerousSettings>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Proxy {
    all: Option<UrlOrConfig>,
    http: Option<UrlOrConfig>,
    https: Option<UrlOrConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(untagged)]
pub enum UrlOrConfig {
    Url(String),
    Config(ProxyConfig),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    url: String,
    basic_auth: Option<BasicAuth>,
    no_proxy: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BasicAuth {
    username: String,
    password: String,
}

#[inline]
fn proxy_creator(
    url_or_config: UrlOrConfig,
    proxy_fn: fn(String) -> reqwest::Result<reqwest::Proxy>,
) -> reqwest::Result<reqwest::Proxy> {
    match url_or_config {
        UrlOrConfig::Url(url) => Ok(proxy_fn(url)?),
        UrlOrConfig::Config(ProxyConfig {
            url,
            basic_auth,
            no_proxy,
        }) => {
            let mut proxy = proxy_fn(url)?;
            if let Some(basic_auth) = basic_auth {
                proxy = proxy.basic_auth(&basic_auth.username, &basic_auth.password);
            }
            if let Some(no_proxy) = no_proxy {
                proxy = proxy.no_proxy(NoProxy::from_string(&no_proxy));
            }
            Ok(proxy)
        }
    }
}

fn attach_proxy(
    proxy: Proxy,
    mut builder: reqwest::ClientBuilder,
) -> crate::Result<reqwest::ClientBuilder> {
    let Proxy { all, http, https } = proxy;

    if let Some(all) = all {
        let proxy = proxy_creator(all, reqwest::Proxy::all)?;
        builder = builder.proxy(proxy);
    }

    if let Some(http) = http {
        let proxy = proxy_creator(http, reqwest::Proxy::http)?;
        builder = builder.proxy(proxy);
    }

    if let Some(https) = https {
        let proxy = proxy_creator(https, reqwest::Proxy::https)?;
        builder = builder.proxy(proxy);
    }

    Ok(builder)
}

#[command]
pub async fn fetch<R: Runtime>(
    webview: Webview<R>,
    _state: State<'_, Http>,
    client_config: ClientConfig,
    command_scope: CommandScope<Entry>,
    global_scope: GlobalScope<Entry>,
) -> crate::Result<ResourceId> {
    let ClientConfig {
        method,
        url,
        headers: headers_raw,
        data,
        connect_timeout,
        max_redirections,
        proxy,
        danger,
    } = client_config;

    let scheme = url.scheme();
    let method = Method::from_bytes(method.as_bytes())?;

    let mut headers = HeaderMap::new();
    for (h, v) in headers_raw {
        let name = HeaderName::from_str(&h)?;
        #[cfg(not(feature = "unsafe-headers"))]
        if is_unsafe_header(&name) {
            #[cfg(debug_assertions)]
            {
                eprintln!("[\x1b[33mWARNING\x1b[0m] Skipping {name} header as it is a forbidden header per fetch spec https://fetch.spec.whatwg.org/#terminology-headers");
                eprintln!("[\x1b[33mWARNING\x1b[0m] if keeping the header is a desired behavior, you can enable `unsafe-headers` feature flag in your Cargo.toml");
            }
            continue;
        }

        headers.append(name, HeaderValue::from_str(&v)?);
    }

    match scheme {
        "http" | "https" => {
            if Scope::new(
                command_scope
                    .allows()
                    .iter()
                    .chain(global_scope.allows())
                    .collect(),
                command_scope
                    .denies()
                    .iter()
                    .chain(global_scope.denies())
                    .collect(),
            )
            .is_allowed(&url)
            {
                let mut builder = reqwest::ClientBuilder::new().retry(reqwest::retry::never());

                if let Some(danger_config) = danger {
                    #[cfg(not(feature = "dangerous-settings"))]
                    {
                        #[cfg(debug_assertions)]
                        {
                            eprintln!("[\x1b[33mWARNING\x1b[0m] using dangerous settings requires `dangerous-settings` feature flag in your Cargo.toml");
                        }
                        let _ = danger_config;
                        return Err(Error::DangerousSettings);
                    }
                    #[cfg(feature = "dangerous-settings")]
                    {
                        builder = builder
                            .danger_accept_invalid_certs(danger_config.accept_invalid_certs)
                            .danger_accept_invalid_hostnames(danger_config.accept_invalid_hostnames)
                    }
                }

                if let Some(timeout) = connect_timeout {
                    builder = builder.connect_timeout(Duration::from_millis(timeout));
                }

                if let Some(max_redirections) = max_redirections {
                    builder = builder.redirect(if max_redirections == 0 {
                        Policy::none()
                    } else {
                        Policy::limited(max_redirections)
                    });
                }

                if let Some(proxy_config) = proxy {
                    builder = attach_proxy(proxy_config, builder)?;
                }

                #[cfg(feature = "cookies")]
                {
                    builder = builder.cookie_provider(_state.cookies_jar.clone());
                }

                let mut request = builder.build()?.request(method.clone(), url);

                // POST and PUT requests should always have a 0 length content-length,
                // if there is no body. https://fetch.spec.whatwg.org/#http-network-or-cache-fetch
                if data.is_none() && matches!(method, Method::POST | Method::PUT) {
                    headers.append(header::CONTENT_LENGTH, HeaderValue::from_str("0")?);
                }

                if headers.contains_key(header::RANGE) {
                    // https://fetch.spec.whatwg.org/#http-network-or-cache-fetch step 18
                    // If httpRequest's header list contains `Range`, then append (`Accept-Encoding`, `identity`)
                    headers.append(header::ACCEPT_ENCODING, HeaderValue::from_str("identity")?);
                }

                if !headers.contains_key(header::USER_AGENT) {
                    headers.append(header::USER_AGENT, HeaderValue::from_str(HTTP_USER_AGENT)?);
                }

                // ensure we have an Origin header set
                if cfg!(not(feature = "unsafe-headers")) || !headers.contains_key(header::ORIGIN) {
                    if let Ok(url) = webview.url() {
                        // The url crate returns OpaqueOrigin for tauri://localhost which serializes to "null"
                        let origin = if url.scheme() == "tauri" {
                            "tauri://localhost".to_string()
                        } else {
                            url.origin().ascii_serialization()
                        };
                        headers.append(header::ORIGIN, HeaderValue::from_str(&origin)?);
                    }
                }

                // In case empty origin is passed, remove it. Some services do not like Origin header
                // so this way we can remove it in explicit way. The default behaviour is still to set it
                if cfg!(feature = "unsafe-headers")
                    && headers.get(header::ORIGIN) == Some(&HeaderValue::from_static(""))
                {
                    headers.remove(header::ORIGIN);
                };

                if let Some(data) = data {
                    request = request.body(data);
                }

                request = request.headers(headers);

                #[cfg(feature = "tracing")]
                tracing::trace!("{:?}", request);

                let fut = async move { request.send().await.map_err(Into::into) };

                let mut resources_table = webview.resources_table();
                let rid = resources_table.add_request(Box::pin(fut));

                Ok(rid)
            } else {
                Err(Error::UrlNotAllowed(url))
            }
        }
        "data" => {
            let data_url =
                data_url::DataUrl::process(url.as_str()).map_err(|_| Error::DataUrlError)?;
            let (body, _) = data_url
                .decode_to_vec()
                .map_err(|_| Error::DataUrlDecodeError)?;

            let response = http::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, data_url.mime_type().to_string())
                .body(reqwest::Body::from(body))?;

            #[cfg(feature = "tracing")]
            tracing::trace!("{:?}", response);

            let fut = async move { Ok(reqwest::Response::from(response)) };
            let mut resources_table = webview.resources_table();
            let rid = resources_table.add_request(Box::pin(fut));
            Ok(rid)
        }
        _ => Err(Error::SchemeNotSupport(scheme.to_string())),
    }
}

#[command]
pub fn fetch_cancel<R: Runtime>(webview: Webview<R>, rid: ResourceId) -> crate::Result<()> {
    let mut resources_table = webview.resources_table();
    let Ok(req) = resources_table.get::<FetchRequest>(rid) else { return Ok(()); };
    let Ok(abort_tx) = resources_table.take::<AbortSender>(req.abort_tx_rid) else { return Ok(()); };
    if let Some(abort_tx) = Arc::into_inner(abort_tx) {
        abort_tx.abort();
    }
    Ok(())
}

#[command]
pub async fn fetch_send<R: Runtime>(
    webview: Webview<R>,
    rid: ResourceId,
) -> crate::Result<FetchResponse> {
    let (req, abort_rx) = {
        let mut resources_table = webview.resources_table();
        let req = resources_table.get::<FetchRequest>(rid)?;
        let abort_rx = resources_table.take::<AbortRecveiver>(req.abort_rx_rid)?;
        (req, abort_rx)
    };

    let Some(abort_rx) = Arc::into_inner(abort_rx) else {
        return Err(Error::RequestCanceled);
    };

    let mut fut = req.fut.lock().await;

    let result = tokio::select! {
        biased;
        _ = abort_rx.0 => Err(Error::RequestCanceled),
        res = fut.as_mut() => res,
    };

    // Keep this guard until the response is inserted: navigation cleanup must not
    // race a completed request into publishing a response owned by the old page.
    let mut resources_table = webview.resources_table();
    let active = resources_table.has(rid);
    let _ = resources_table.close(rid);
    let _ = resources_table.close(req.abort_tx_rid);
    let _ = resources_table.close(req.abort_rx_rid);
    if !active { return Err(Error::RequestCanceled); }
    let res = result?;

    #[cfg(feature = "tracing")]
    tracing::trace!("{:?}", res);

    let status = res.status();
    let url = res.url().to_string();
    let mut headers = Vec::new();
    for (key, val) in res.headers().iter() {
        headers.push((
            key.as_str().into(),
            String::from_utf8(val.as_bytes().to_vec())?,
        ));
    }

    // The official JS API does not create a body reader for these statuses.
    let rid = if matches!(status.as_u16(), 101 | 103 | 204 | 205 | 304) { 0 } else {
        resources_table.add(ReqwestResponse {
            response: Mutex::new(res), cancel: tokio_util::sync::CancellationToken::new(),
        })
    };

    Ok(FetchResponse {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or_default().to_string(),
        headers,
        url,
        rid,
    })
}

#[command]
pub async fn fetch_read_body<R: Runtime>(
    webview: Webview<R>,
    rid: ResourceId,
) -> crate::Result<tauri::ipc::Response> {
    let res = {
        let resources_table = webview.resources_table();
        resources_table.get::<ReqwestResponse>(rid)?
    };

    // Serialize readers without casting an Arc to a mutable reference. Closing
    // the resource also cancels an in-flight read of a stalled response body.
    let read = async {
        let mut response = res.response.lock().await;
        response.chunk().await.map_err(Error::from)
    };
    let chunk = tokio::select! {
        biased;
        _ = res.cancel.cancelled() => return Err(Error::RequestCanceled),
        chunk = read => chunk,
    };
    let chunk = match chunk {
        Ok(chunk) => chunk,
        Err(error) => {
            let _ = webview.resources_table().close(rid);
            return Err(error);
        }
    };
    let Some(chunk) = chunk else {
        let mut resources_table = webview.resources_table();
        let _ = resources_table.close(rid);

        // return a response with a single byte to indicate that the body is empty
        return Ok(tauri::ipc::Response::new(vec![1]));
    };

    let mut chunk = chunk.to_vec();
    // append a 0 byte to indicate that the body is not empty
    chunk.push(0);

    Ok(tauri::ipc::Response::new(chunk))
}

#[command]
pub async fn fetch_cancel_body<R: Runtime>(
    webview: Webview<R>,
    rid: ResourceId,
) -> crate::Result<()> {
    let mut resources_table = webview.resources_table();
    if resources_table.get::<ReqwestResponse>(rid).is_ok() {
        let _ = resources_table.close(rid);
    }
    Ok(())
}

// forbidden headers per fetch spec https://fetch.spec.whatwg.org/#terminology-headers
#[cfg(not(feature = "unsafe-headers"))]
fn is_unsafe_header(header: &HeaderName) -> bool {
    matches!(
        *header,
        header::ACCEPT_CHARSET
            | header::ACCEPT_ENCODING
            | header::ACCESS_CONTROL_REQUEST_HEADERS
            | header::ACCESS_CONTROL_REQUEST_METHOD
            | header::CONNECTION
            | header::CONTENT_LENGTH
            | header::COOKIE
            | header::DATE
            | header::DNT
            | header::EXPECT
            | header::HOST
            | header::ORIGIN
            | header::REFERER
            | header::SET_COOKIE
            | header::TE
            | header::TRAILER
            | header::TRANSFER_ENCODING
            | header::UPGRADE
            | header::VIA
    ) || {
        let lower = header.as_str().to_lowercase();
        lower.starts_with("proxy-") || lower.starts_with("sec-")
    }
}
