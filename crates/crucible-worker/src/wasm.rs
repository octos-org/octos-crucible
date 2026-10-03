//! Workers runtime glue: converts requests/responses and implements
//! [`Backend`] over Workers KV and `fetch`. Everything else is in
//! platform-independent modules.

use worker::{
    Context, Date, Env, Fetch, Headers, KvStore, Method, Request, RequestInit, Response,
    ResponseBuilder, console_error, console_log, event, js_sys,
};

use crate::app;
use crate::config::Config;
use crate::http::{Backend, HttpRequest, HttpResponse, KvKey, PutOptions, Req, Resp};
use crate::model::MAX_UPLOAD;

const KV_BINDING: &str = "CRUCIBLE_KV";
/// Largest body read at all; per-route limits are enforced by the handlers.
const MAX_BODY: usize = MAX_UPLOAD + 64 * 1024;

struct WorkerBackend {
    kv: KvStore,
}

impl Backend for WorkerBackend {
    async fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.kv.get(key).bytes().await.map_err(|e| e.to_string())
    }

    async fn kv_put(&self, key: &str, value: &[u8], opts: PutOptions) -> Result<(), String> {
        let mut put = self.kv.put_bytes(key, value).map_err(|e| e.to_string())?;
        if let Some(ttl) = opts.ttl {
            put = put.expiration_ttl(ttl);
        }
        if let Some(meta) = opts.metadata {
            put = put.metadata(meta).map_err(|e| e.to_string())?;
        }
        put.execute().await.map_err(|e| e.to_string())
    }

    async fn kv_delete(&self, key: &str) -> Result<(), String> {
        self.kv.delete(key).await.map_err(|e| e.to_string())
    }

    async fn kv_list(&self, prefix: &str, limit: usize) -> Result<Vec<KvKey>, String> {
        let resp = self
            .kv
            .list()
            .prefix(prefix.to_owned())
            .limit(limit as u64)
            .execute()
            .await
            .map_err(|e| e.to_string())?;
        Ok(resp
            .keys
            .into_iter()
            .map(|k| KvKey {
                name: k.name,
                metadata: k.metadata,
            })
            .collect())
    }

    async fn fetch(&self, r: HttpRequest) -> Result<HttpResponse, String> {
        let headers = Headers::new();
        for (k, v) in &r.headers {
            headers.set(k, v).map_err(|e| e.to_string())?;
        }
        let mut init = RequestInit::new();
        init.with_method(Method::from(r.method.to_string()))
            .with_headers(headers);
        if let Some(body) = &r.body {
            init.with_body(Some(js_sys::Uint8Array::from(body.as_slice()).into()));
        }
        let req = Request::new_with_init(&r.url, &init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        Ok(HttpResponse {
            status: resp.status_code(),
            body: resp.bytes().await.map_err(|e| e.to_string())?,
        })
    }

    fn now_s(&self) -> u64 {
        Date::now().as_millis() / 1000
    }

    fn random_bytes(&self, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        getrandom::getrandom(&mut v).expect("crypto.getRandomValues");
        v
    }

    fn log(&self, line: &str) {
        console_log!("{}", line);
    }
}

fn to_worker(r: Resp) -> worker::Result<Response> {
    let headers = Headers::new();
    for (k, v) in &r.headers {
        headers.append(k, v)?;
    }
    let builder = ResponseBuilder::new()
        .with_status(r.status)
        .with_headers(headers);
    Ok(if r.body.is_empty() {
        builder.empty()
    } else {
        builder.fixed(r.body)
    })
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    let cfg = Config::from_lookup(|name| env.var(name).ok().map(|v| v.to_string()));
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => {
            // Names the setting, never a value.
            console_error!("configuration: {}", e);
            return to_worker(app::misconfigured());
        }
    };
    let backend = WorkerBackend {
        kv: env.kv(KV_BINDING)?,
    };

    let declared = req
        .headers()
        .get("content-length")?
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|l| l > MAX_BODY) {
        return to_worker(crate::http::ApiError::too_large(MAX_UPLOAD).into_resp());
    }
    let method = req.method().to_string().to_ascii_uppercase();
    let body = if matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
        req.bytes().await?
    } else {
        Vec::new()
    };
    if body.len() > MAX_BODY {
        return to_worker(crate::http::ApiError::too_large(MAX_UPLOAD).into_resp());
    }

    let url = req.url()?;
    let r = Req {
        method,
        origin: url.origin().ascii_serialization(),
        host: url.host_str().unwrap_or_default().to_owned(),
        path: url.path().to_owned(),
        query: url.query().unwrap_or_default().to_owned(),
        headers: req
            .headers()
            .entries()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect(),
        body,
    };
    to_worker(app::handle(&backend, &cfg, &r).await)
}
