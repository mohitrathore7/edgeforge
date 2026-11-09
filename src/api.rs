//! HTTP control API, Prometheus endpoint and the built-in dashboard.
//!
//! | Method | Path                          | Action                         |
//! |--------|-------------------------------|--------------------------------|
//! | GET    | `/`                           | web dashboard                  |
//! | GET    | `/healthz`                    | daemon liveness                |
//! | GET    | `/metrics`                    | Prometheus text exposition     |
//! | GET    | `/api/apps`                   | list apps                      |
//! | POST   | `/api/apps`                   | install `{"source","start"}`   |
//! | GET    | `/api/apps/{name}`            | app status                     |
//! | POST   | `/api/apps/{name}/start`      | start                          |
//! | POST   | `/api/apps/{name}/stop`       | stop                           |
//! | POST   | `/api/apps/{name}/restart`    | restart                        |
//! | POST   | `/api/apps/{name}/update`     | OTA update `{"source"}`        |
//! | POST   | `/api/apps/{name}/rollback`   | roll back to previous slot     |
//! | DELETE | `/api/apps/{name}`            | stop and remove                |

use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;

use log::{debug, error};
use serde::Deserialize;
use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::error::{EdgeError, Result};
use crate::manager::Manager;

const DASHBOARD: &str = include_str!("../web/dashboard.html");
const MAX_BODY: u64 = 64 * 1024;

#[derive(Deserialize)]
struct InstallRequest {
    source: String,
    #[serde(default)]
    start: bool,
}

#[derive(Deserialize)]
struct UpdateRequest {
    source: String,
}

pub struct ApiServer {
    server: Arc<Server>,
    workers: Vec<thread::JoinHandle<()>>,
    pub addr: SocketAddr,
}

impl ApiServer {
    /// Bind `addr` and serve requests on `workers` threads.
    pub fn start(mgr: Arc<Manager>, addr: &str, workers: usize) -> Result<ApiServer> {
        let server = Server::http(addr)
            .map_err(|e| EdgeError::Runtime(format!("cannot listen on {addr}: {e}")))?;
        let server = Arc::new(server);
        let local = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| EdgeError::Runtime("listener has no ip address".into()))?;
        let workers = (0..workers.max(1))
            .map(|i| {
                let server = Arc::clone(&server);
                let mgr = Arc::clone(&mgr);
                thread::Builder::new()
                    .name(format!("api-{i}"))
                    .spawn(move || {
                        while let Ok(rq) = server.recv() {
                            handle(&mgr, rq);
                        }
                    })
                    .expect("spawn api worker")
            })
            .collect();
        Ok(ApiServer {
            server,
            workers,
            addr: local,
        })
    }

    pub fn shutdown(self) {
        self.server.unblock();
        for _ in 1..self.workers.len() {
            self.server.unblock();
        }
        for w in self.workers {
            let _ = w.join();
        }
    }
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Reply {
    fn json(status: u16, v: serde_json::Value) -> Reply {
        Reply {
            status,
            content_type: "application/json",
            body: serde_json::to_vec_pretty(&v).unwrap_or_default(),
        }
    }
    fn error(e: &EdgeError) -> Reply {
        Reply::json(
            e.http_status(),
            json!({ "error": e.kind(), "message": e.to_string() }),
        )
    }
}

fn handle(mgr: &Manager, mut rq: Request) {
    let method = rq.method().clone();
    let url = rq.url().split('?').next().unwrap_or("/").to_string();
    let mut body = String::new();
    let _ = rq.as_reader().take(MAX_BODY).read_to_string(&mut body);

    let reply = route(mgr, &method, &url, &body);
    debug!("{method} {url} -> {}", reply.status);
    let header = Header::from_bytes(&b"Content-Type"[..], reply.content_type.as_bytes())
        .expect("static header");
    let resp = Response::from_data(reply.body)
        .with_status_code(StatusCode(reply.status))
        .with_header(header);
    if let Err(e) = rq.respond(resp) {
        error!("failed to send response: {e}");
    }
}

fn to_reply<T: serde::Serialize>(r: Result<T>, ok: u16) -> Reply {
    match r {
        Ok(v) => Reply::json(ok, serde_json::to_value(v).unwrap_or_default()),
        Err(e) => Reply::error(&e),
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|e| EdgeError::Invalid(format!("bad request body: {e}")))
}

fn route(mgr: &Manager, method: &Method, url: &str, body: &str) -> Reply {
    let segs: Vec<&str> = url.split('/').filter(|s| !s.is_empty()).collect();
    match (method, segs.as_slice()) {
        (Method::Get, []) => Reply {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: DASHBOARD.as_bytes().to_vec(),
        },
        (Method::Get, ["healthz"]) => Reply::json(200, json!({ "status": "ok" })),
        (Method::Get, ["metrics"]) => Reply {
            status: 200,
            content_type: "text/plain; version=0.0.4",
            body: mgr.metrics().render().into_bytes(),
        },
        (Method::Get, ["api", "apps"]) => Reply::json(200, json!(mgr.list())),
        (Method::Post, ["api", "apps"]) => to_reply(
            parse::<InstallRequest>(body).and_then(|r| mgr.install(&r.source, r.start)),
            201,
        ),
        (Method::Get, ["api", "apps", name]) => to_reply(mgr.status(name), 200),
        (Method::Delete, ["api", "apps", name]) => {
            to_reply(mgr.remove(name).map(|_| json!({ "removed": name })), 200)
        }
        (Method::Post, ["api", "apps", name, action]) => {
            let r = match *action {
                "start" => mgr.start(name),
                "stop" => mgr.stop(name),
                "restart" => mgr.restart(name),
                "rollback" => mgr.rollback(name),
                "update" => parse::<UpdateRequest>(body).and_then(|r| mgr.update(name, &r.source)),
                other => Err(EdgeError::NotFound(format!("action '{other}'"))),
            };
            to_reply(r, 200)
        }
        _ => Reply::json(
            404,
            json!({ "error": "not_found", "message": format!("no route for {method} {url}") }),
        ),
    }
}
