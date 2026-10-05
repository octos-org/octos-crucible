//! Workers runtime glue: converts requests/responses and implements
//! [`Backend`] over D1 and `fetch`. Everything else is in
//! platform-independent modules.

use worker::wasm_bindgen::JsValue;
use worker::{
    Context, D1Database, D1PreparedStatement, Date, Env, Fetch, Headers, Method, Request,
    RequestInit, Response, ResponseBuilder, ScheduleContext, ScheduledEvent, console_error,
    console_log, event, js_sys,
};

use crate::app;
use crate::config::Config;
use crate::http::{Backend, HttpRequest, HttpResponse, Req, Resp, Row, SqlArg, Stmt};
use crate::model::MAX_UPLOAD;

const DB_BINDING: &str = "CRUCIBLE_DB";
/// Largest body read at all; per-route limits are enforced by the handlers.
const MAX_BODY: usize = MAX_UPLOAD + 64 * 1024;

struct WorkerBackend {
    db: D1Database,
}

fn js_arg(a: &SqlArg) -> JsValue {
    match a {
        SqlArg::Null => JsValue::NULL,
        // Ids, sizes and unix seconds: all well below 2^53.
        SqlArg::Int(i) => JsValue::from_f64(*i as f64),
        SqlArg::Real(f) => JsValue::from_f64(*f),
        SqlArg::Text(s) => JsValue::from_str(s),
    }
}

impl WorkerBackend {
    fn stmt(&self, sql: &str, args: &[SqlArg]) -> Result<D1PreparedStatement, String> {
        let args: Vec<JsValue> = args.iter().map(js_arg).collect();
        self.db.prepare(sql).bind(&args).map_err(|e| e.to_string())
    }
}

impl Backend for WorkerBackend {
    async fn db_query(&self, sql: &str, args: &[SqlArg]) -> Result<Vec<Row>, String> {
        let res = self
            .stmt(sql, args)?
            .all()
            .await
            .map_err(|e| e.to_string())?;
        res.results::<Row>().map_err(|e| e.to_string())
    }

    async fn db_exec(&self, sql: &str, args: &[SqlArg]) -> Result<u64, String> {
        let res = self
            .stmt(sql, args)?
            .run()
            .await
            .map_err(|e| e.to_string())?;
        let changes = res
            .meta()
            .map_err(|e| e.to_string())?
            .and_then(|m| m.changes)
            .unwrap_or(0);
        Ok(changes as u64)
    }

    async fn db_batch(&self, stmts: &[Stmt]) -> Result<(), String> {
        let stmts = stmts
            .iter()
            .map(|s| self.stmt(s.sql, &s.args))
            .collect::<Result<Vec<_>, _>>()?;
        self.db.batch(stmts).await.map_err(|e| e.to_string())?;
        Ok(())
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
        db: env.d1(DB_BINDING)?,
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

/// Cron Trigger (wrangler.toml `[triggers]`): purge expired rows, fail
/// stuck registrations and evals.
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    let cfg = Config::from_lookup(|name| env.var(name).ok().map(|v| v.to_string()));
    if let Err(e) = &cfg {
        console_error!("cron: configuration: {}", e);
    }
    match env.d1(DB_BINDING) {
        Ok(db) => app::scheduled(&WorkerBackend { db }, cfg.ok().as_ref()).await,
        Err(e) => console_error!("cron: no D1 binding: {}", e),
    }
}
