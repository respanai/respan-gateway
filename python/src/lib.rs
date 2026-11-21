//! The native half of the `respan-gateway` Python package.
//!
//! Requests go through the gateway's router in-process (no socket); servers
//! and the command line run on one shared tokio runtime. Every call that
//! waits releases the GIL.

use parking_lot::Mutex;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Request};
use http_body_util::BodyExt;
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};
use respan_gateway::{App, Config, LogFormat, Trusted};
use tokio::runtime::Runtime;
use tokio::sync::{Notify, oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle};
use tower::ServiceExt;

create_exception!(
    _native,
    ConfigError,
    PyValueError,
    "The configuration is invalid."
);
create_exception!(
    _native,
    GatewayError,
    PyException,
    "The gateway could not carry out a request or start a server."
);

/// The runtime shared by every gateway, server and the command line.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("respan-gateway")
            .build()
            .expect("the tokio runtime starts")
    })
}

create_exception!(
    _native,
    GatewayTimeout,
    GatewayError,
    "A request or a read of its body outlasted its timeout."
);

fn gateway_error(e: impl std::fmt::Display) -> PyErr {
    GatewayError::new_err(e.to_string())
}

/// A header value as the string of its bytes, one char per byte (latin-1),
/// which Python turns back into bytes.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

fn unlatin1(text: &str) -> Result<Vec<u8>, PyErr> {
    text.chars()
        .map(|c| u8::try_from(c).map_err(|_| gateway_error("header text is not latin-1")))
        .collect()
}

/// Why a wait ended without a value.
#[derive(Debug)]
enum Failure {
    Timeout,
    Error(String),
}

type Outcome<T> = Result<T, Failure>;

impl From<Failure> for PyErr {
    fn from(failure: Failure) -> PyErr {
        match failure {
            Failure::Timeout => GatewayTimeout::new_err("timed out"),
            Failure::Error(message) => GatewayError::new_err(message),
        }
    }
}

/// `work` within `limit` seconds (forever if `None`).
async fn within<T>(
    limit: Option<f64>,
    work: impl Future<Output = Result<T, String>>,
) -> Outcome<T> {
    let result = match limit {
        Some(seconds) => {
            let limit = Duration::try_from_secs_f64(seconds.max(0.0))
                .unwrap_or(Duration::from_secs(365 * 24 * 3600));
            tokio::time::timeout(limit, work)
                .await
                .map_err(|_| Failure::Timeout)?
        }
        None => work.await,
    };
    result.map_err(Failure::Error)
}

/// Runs `work` on the runtime and waits for it with the GIL released,
/// waking every 100 ms to deliver signals (Ctrl-C); an interrupted wait
/// drops `work`, which cancels whatever it was doing.
fn wait<T: Send + 'static>(
    py: Python<'_>,
    work: impl Future<Output = Outcome<T>> + Send + 'static,
) -> PyResult<T> {
    let mut task = runtime().spawn(work);
    loop {
        let slice = py.detach(|| {
            runtime().block_on(async {
                tokio::time::timeout(Duration::from_millis(100), &mut task).await
            })
        });
        match slice {
            Ok(done) => return done.map_err(gateway_error)?.map_err(PyErr::from),
            Err(_) => {
                if let Err(e) = py.check_signals() {
                    task.abort();
                    return Err(e);
                }
            }
        }
    }
}

/// Runs `work` on the runtime and, when it ends, calls `done(value, error)`
/// from the runtime's thread. The returned handle cancels it.
fn start<T: Send + 'static>(
    work: impl Future<Output = Outcome<T>> + Send + 'static,
    done: Py<PyAny>,
    into_python: impl FnOnce(Python<'_>, T) -> PyResult<Py<PyAny>> + Send + 'static,
) -> Pending {
    let task = runtime().spawn(async move {
        let outcome = work.await;
        Python::attach(|py| {
            let args = match outcome {
                Ok(value) => into_python(py, value).map(|v| (v, py.None())),
                Err(failure) => Ok((py.None(), PyErr::from(failure).into_value(py).into_any())),
            };
            let called = args.and_then(|args| done.call1(py, args));
            if let Err(e) = called {
                e.print(py);
            }
        });
    });
    Pending {
        task: task.abort_handle(),
    }
}

/// A request or read in flight on the runtime.
#[pyclass(frozen, module = "respan_gateway._native")]
#[derive(Debug)]
struct Pending {
    task: AbortHandle,
}

#[pymethods]
impl Pending {
    /// Drops the work in flight (the provider call with it); `done` is then
    /// never called.
    fn cancel(&self) {
        self.task.abort();
    }
}

/// A response head with the body still to pull.
struct Head {
    status: u16,
    headers: Vec<(String, String)>,
    body: Body,
}

impl Head {
    fn into_reply(self) -> Reply {
        Reply {
            status: self.status,
            headers: self.headers,
            inner: Arc::new(Inner {
                body: tokio::sync::Mutex::new(Some(self.body)),
                closed: AtomicBool::new(false),
                wake: Notify::new(),
            }),
        }
    }
}

async fn open(router: Router, request: Request<Body>, limit: Option<f64>) -> Outcome<Head> {
    within(limit, async move {
        let response = router
            .oneshot(request)
            .await
            .unwrap_or_else(|never| match never {});
        let (head, body) = response.into_parts();
        Ok(Head {
            status: head.status.as_u16(),
            headers: head
                .headers
                .iter()
                .map(|(n, v)| (n.as_str().to_owned(), latin1(v.as_bytes())))
                .collect(),
            body,
        })
    })
    .await
}

fn chunk_to_python(py: Python<'_>, chunk: Option<Bytes>) -> PyResult<Py<PyAny>> {
    Ok(match chunk {
        Some(data) => PyBytes::new(py, &data).into_any().unbind(),
        None => py.None(),
    })
}

/// A configured gateway.
#[pyclass(frozen, module = "respan_gateway._native")]
#[derive(Debug)]
struct Native {
    app: Arc<App>,
    router: Router,
}

/// Whether the TOML in `text` sets `log` itself.
fn sets_log(text: &str) -> bool {
    toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|v| v.get("log").map(|_| true))
        .unwrap_or(false)
}

impl Native {
    /// A library prints nothing per request unless its configuration asks:
    /// `log` defaults to `off` here (the binary's default is `text`).
    /// `log_explicit` records whether the caller supplied the log setting.
    fn new(config: Result<Config, respan_gateway::Error>, log_explicit: bool) -> PyResult<Self> {
        let config_error = |e: respan_gateway::Error| ConfigError::new_err(e.to_string());
        let mut config = config.map_err(config_error)?;
        if !log_explicit {
            config.log = LogFormat::Off;
        }
        // Providers may need the runtime to set up their clients.
        let _entered = runtime().enter();
        let app = App::new(config).map_err(config_error)?;
        Ok(Self {
            router: app.router(),
            app: Arc::new(app),
        })
    }

    fn request(
        method: &str,
        target: &str,
        headers: &[(String, String)],
        body: &[u8],
        trusted: bool,
    ) -> PyResult<Request<Body>> {
        let mut request = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).map_err(gateway_error)?)
            .uri(target);
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(gateway_error)?;
            let value = HeaderValue::from_bytes(&unlatin1(value)?).map_err(gateway_error)?;
            request = request.header(name, value);
        }
        let mut request = request
            .body(Body::from(Bytes::copy_from_slice(body)))
            .map_err(gateway_error)?;
        if trusted {
            request.extensions_mut().insert(Trusted);
        }
        Ok(request)
    }
}

#[pymethods]
impl Native {
    /// A gateway from TOML.
    #[staticmethod]
    fn from_toml(text: &str) -> PyResult<Self> {
        Self::new(Config::parse(text), sets_log(text))
    }

    /// Structured Python mappings cross the FFI boundary as JSON.
    #[staticmethod]
    fn from_json(text: &str) -> PyResult<Self> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| ConfigError::new_err(e.to_string()))?;
        let log_explicit = value.get("log").is_some();
        Self::new(Config::from_value(value), log_explicit)
    }

    /// A gateway from the TOML file at `path`.
    #[staticmethod]
    fn from_file(path: &str) -> PyResult<Self> {
        Self::new(
            Config::load(path),
            std::fs::read_to_string(path)
                .ok()
                .is_some_and(|text| sets_log(&text)),
        )
    }

    /// A gateway as the binary starts with no arguments: `./gateway.toml`
    /// if there is one, else the vendors the environment has keys for.
    #[staticmethod]
    fn discover() -> PyResult<Self> {
        let file = std::fs::read_to_string(respan_gateway::DEFAULT_FILE).ok();
        Self::new(
            Config::discover(None),
            file.is_some_and(|text| sets_log(&text)),
        )
    }

    /// Sends one request through the router and returns once its head is
    /// ready (within `timeout` seconds); the body is then pulled from the
    /// returned [`Reply`].
    #[pyo3(signature = (method, target, headers, body, timeout=None))]
    fn send(
        &self,
        py: Python<'_>,
        method: &str,
        target: &str,
        headers: Vec<(String, String)>,
        body: &[u8],
        timeout: Option<f64>,
    ) -> PyResult<Reply> {
        let request = Self::request(method, target, &headers, body, false)?;
        wait(py, open(self.router.clone(), request, timeout)).map(Head::into_reply)
    }

    /// `send` for the package's own reads of the admin API: the request is
    /// marked [`Trusted`], which stands in for a client key there. Nothing
    /// else uses it; client traffic goes through `send` and presents keys.
    #[pyo3(signature = (method, target, headers, body, timeout=None))]
    fn send_admin(
        &self,
        py: Python<'_>,
        method: &str,
        target: &str,
        headers: Vec<(String, String)>,
        body: &[u8],
        timeout: Option<f64>,
    ) -> PyResult<Reply> {
        let request = Self::request(method, target, &headers, body, true)?;
        wait(py, open(self.router.clone(), request, timeout)).map(Head::into_reply)
    }

    /// `send` without blocking: calls `done(reply, error)` from another
    /// thread when the head is ready. The returned handle cancels the request.
    #[pyo3(signature = (method, target, headers, body, done, timeout=None))]
    fn send_async(
        &self,
        method: &str,
        target: &str,
        headers: Vec<(String, String)>,
        body: &[u8],
        done: Py<PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Pending> {
        let request = Self::request(method, target, &headers, body, false)?;
        Ok(start(
            open(self.router.clone(), request, timeout),
            done,
            |py, head| Ok(Py::new(py, head.into_reply())?.into_any()),
        ))
    }

    /// `send_admin` without blocking, as `send_async` is to `send`.
    #[pyo3(signature = (method, target, headers, body, done, timeout=None))]
    fn send_admin_async(
        &self,
        method: &str,
        target: &str,
        headers: Vec<(String, String)>,
        body: &[u8],
        done: Py<PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Pending> {
        let request = Self::request(method, target, &headers, body, true)?;
        Ok(start(
            open(self.router.clone(), request, timeout),
            done,
            |py, head| Ok(Py::new(py, head.into_reply())?.into_any()),
        ))
    }

    /// Starts serving on `host:port` (port 0 picks a free one). The server
    /// runs until `stop()` or the end of the process, even if the returned
    /// handle is dropped.
    fn serve(&self, py: Python<'_>, host: &str, port: u16) -> PyResult<Server> {
        let app = self.app.clone();
        let host = host.to_owned();
        py.detach(|| {
            runtime().block_on(async move {
                let listener = tokio::net::TcpListener::bind((host.as_str(), port))
                    .await
                    .map_err(|e| gateway_error(format!("listen on {host}:{port}: {e}")))?;
                let addr = listener.local_addr().map_err(gateway_error)?;
                let banner = app.banner(addr);
                let (stop, stopped) = oneshot::channel::<()>();
                let (ended_tx, ended) = watch::channel(false);
                let task = tokio::spawn(async move {
                    let served = app
                        .serve(listener, async {
                            // A dropped sender is not a stop.
                            if stopped.await.is_err() {
                                std::future::pending::<()>().await;
                            }
                        })
                        .await;
                    ended_tx.send_replace(true);
                    served
                });
                Ok(Server {
                    url: format!("http://{addr}"),
                    banner,
                    ended,
                    stop: Mutex::new(Some(stop)),
                    task: Mutex::new(Some(task)),
                })
            })
        })
    }
}

/// The body of a response and whether it was abandoned.
#[derive(Debug)]
struct Inner {
    body: tokio::sync::Mutex<Option<Body>>,
    closed: AtomicBool,
    wake: Notify,
}

impl Inner {
    /// Completes once closed.
    async fn closed(&self) {
        loop {
            let woken = self.wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            woken.await;
        }
    }

    /// Drops the body (and with it the provider call), unless a read holds
    /// it; that read drops it as it returns.
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake.notify_waiters();
        if let Ok(mut body) = self.body.try_lock() {
            *body = None;
        }
    }

    /// The next chunk of the body, or `None` at its end or once closed.
    async fn pull(&self) -> Result<Option<Bytes>, String> {
        let read = async {
            let mut guard = self.body.lock().await;
            let Some(body) = guard.as_mut() else {
                return Ok(None);
            };
            loop {
                match body.frame().await {
                    None => return Ok(None),
                    Some(Err(e)) => return Err(e.to_string()),
                    // Trailers carry nothing a client reads.
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data()
                            && !data.is_empty()
                        {
                            return Ok(Some(data));
                        }
                    }
                }
            }
        };
        tokio::select! {
            biased;
            () = self.closed() => {
                self.close();
                Ok(None)
            }
            chunk = read => chunk,
        }
    }
}

/// A response whose head has arrived and whose body is pulled chunk by chunk.
#[pyclass(frozen, module = "respan_gateway._native")]
#[derive(Debug)]
struct Reply {
    #[pyo3(get)]
    status: u16,
    /// Header names and latin-1 values, in order.
    #[pyo3(get)]
    headers: Vec<(String, String)>,
    inner: Arc<Inner>,
}

#[pymethods]
impl Reply {
    /// The next chunk of the body, or `None` at its end, blocking (GIL
    /// released) until one arrives or `timeout` seconds pass. Returns `None`
    /// once closed, even from another thread.
    #[pyo3(signature = (timeout=None))]
    fn next_chunk<'py>(
        &self,
        py: Python<'py>,
        timeout: Option<f64>,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let inner = self.inner.clone();
        let chunk = wait(py, within(timeout, async move { inner.pull().await }))?;
        Ok(chunk.map(|data| PyBytes::new(py, &data)))
    }

    /// `next_chunk` without blocking: calls `done(chunk, error)` from
    /// another thread. The returned handle cancels the read.
    #[pyo3(signature = (done, timeout=None))]
    fn next_chunk_async(&self, done: Py<PyAny>, timeout: Option<f64>) -> Pending {
        let inner = self.inner.clone();
        start(
            within(timeout, async move { inner.pull().await }),
            done,
            chunk_to_python,
        )
    }

    /// Abandons the body and the provider call behind it; a blocked
    /// `next_chunk` returns `None`.
    fn close(&self) {
        self.inner.close();
    }
}

/// A gateway listening on a real socket in the background. Dropping the
/// handle leaves it running; `stop()` ends it.
#[pyclass(frozen, module = "respan_gateway._native")]
#[derive(Debug)]
struct Server {
    #[pyo3(get)]
    url: String,
    /// What the binary prints on start: providers, endpoints, the UI.
    #[pyo3(get)]
    banner: String,
    ended: watch::Receiver<bool>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    task: Mutex<Option<JoinHandle<std::io::Result<()>>>>,
}

impl Server {
    fn signal(&self) {
        if let Some(stop) = self.stop.lock().take() {
            let _ = stop.send(());
        }
    }
}

#[pymethods]
impl Server {
    /// Stops accepting, lets open responses finish (up to 30 s) and returns;
    /// Ctrl-C meanwhile cuts the wait short and drops the open responses.
    fn stop(&self, py: Python<'_>) -> PyResult<()> {
        self.signal();
        let task = self.task.lock().take();
        let Some(task) = task else { return Ok(()) };
        let server = task.abort_handle();
        wait(py, async move {
            let served = task.await.map_err(|e| Failure::Error(e.to_string()))?;
            served.map_err(|e| Failure::Error(e.to_string()))
        })
        .inspect_err(|_| server.abort())
    }

    /// `stop` without blocking: calls `done(None, error)` from another
    /// thread once the server has ended.
    fn stop_async(&self, done: Py<PyAny>) -> Pending {
        self.signal();
        let task = self.task.lock().take();
        start(
            async move {
                let Some(task) = task else { return Ok(()) };
                let served = task.await.map_err(|e| Failure::Error(e.to_string()))?;
                served.map_err(|e| Failure::Error(e.to_string()))
            },
            done,
            |py, ()| Ok(py.None()),
        )
    }

    /// Blocks (GIL released, Ctrl-C delivered) until the server has ended.
    fn wait(&self, py: Python<'_>) -> PyResult<()> {
        let mut ended = self.ended.clone();
        wait(py, async move {
            // The sender lives in the serving task, so a closed channel is an end too.
            let _ = ended.wait_for(|ended| *ended).await;
            Ok(())
        })
    }

    fn __repr__(&self) -> String {
        format!("Server(url={:?})", self.url)
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_exc))]
    fn __exit__(&self, py: Python<'_>, _exc: &Bound<'_, PyTuple>) -> PyResult<()> {
        self.stop(py)
    }
}

/// Runs the `respan-gateway` command line on `args`, the program name
/// first, and returns its exit status.
#[pyfunction]
fn run_cli(py: Python<'_>, args: Vec<String>) -> i32 {
    py.detach(|| i32::from(runtime().block_on(respan_gateway::run(args))))
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Native>()?;
    m.add_class::<Reply>()?;
    m.add_class::<Pending>()?;
    m.add_class::<Server>()?;
    m.add_function(wrap_pyfunction!(run_cli, m)?)?;
    m.add("ConfigError", m.py().get_type::<ConfigError>())?;
    m.add("GatewayError", m.py().get_type::<GatewayError>())?;
    m.add("GatewayTimeout", m.py().get_type::<GatewayTimeout>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
