use crate::common::{Profile, RunCommon, RunTarget};
use anyhow::{anyhow, bail, Result};
use clap::Parser;
use std::net::SocketAddr;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use wasmtime::component::Linker;
use wasmtime::{Config, Engine, Memory, MemoryType, Store, StoreLimits};
use wasmtime_wasi::{StreamError, StreamResult, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::proxy::ProxyPre;
use wasmtime_wasi_http::{
    bindings::http::types as http_types, body::HyperOutgoingBody, hyper_response_error,
    WasiHttpCtx, WasiHttpView,
};

#[cfg(feature = "wasi-nn")]
use wasmtime_wasi_nn::WasiNnCtx;

struct Host {
    table: wasmtime::component::ResourceTable,
    ctx: WasiCtx,
    http: WasiHttpCtx,

    limits: StoreLimits,

    #[cfg(feature = "wasi-nn")]
    nn: Option<WasiNnCtx>,
}

impl WasiView for Host {
    fn table(&mut self) -> &mut wasmtime::component::ResourceTable {
        &mut self.table
    }

    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.ctx
    }
}

impl WasiHttpView for Host {
    fn table(&mut self) -> &mut wasmtime::component::ResourceTable {
        &mut self.table
    }

    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.http
    }
}

const DEFAULT_ADDR: std::net::SocketAddr = std::net::SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
    8080,
);

/// Runs a WebAssembly module
#[derive(Parser, PartialEq)]
pub struct ServeCommand {
    #[command(flatten)]
    run: RunCommon,

    /// Socket address for the web server to bind to.
    #[arg(long = "addr", value_name = "SOCKADDR", default_value_t = DEFAULT_ADDR )]
    addr: SocketAddr,

    /// The WebAssembly component to run.
    #[arg(value_name = "WASM", required = true)]
    component: PathBuf,
}

impl ServeCommand {
    /// Start a server to run the given wasi-http proxy component
    pub fn execute(mut self) -> Result<()> {
        self.run.common.init_logging()?;

        // We force cli errors before starting to listen for connections so then we don't
        // accidentally delay them to the first request.
        if self.run.common.wasi.nn == Some(true) {
            #[cfg(not(feature = "wasi-nn"))]
            {
                bail!("Cannot enable wasi-nn when the binary is not compiled with this feature.");
            }
        }

        if let Some(Profile::Guest { .. }) = &self.run.profile {
            bail!("Cannot use the guest profiler with components");
        }

        if self.run.common.wasi.nn == Some(true) {
            #[cfg(not(feature = "wasi-nn"))]
            {
                bail!("Cannot enable wasi-nn when the binary is not compiled with this feature.");
            }
        }

        if self.run.common.wasi.threads == Some(true) {
            bail!("wasi-threads does not support components yet")
        }

        // The serve command requires both wasi-http and the component model, so we enable those by
        // default here.
        if self.run.common.wasi.http.replace(true) == Some(false) {
            bail!("wasi-http is required for the serve command, and must not be disabled");
        }
        if self.run.common.wasm.component_model.replace(true) == Some(false) {
            bail!("components are required for the serve command, and must not be disabled");
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .enable_io()
            .build()?;

        runtime.block_on(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    Ok::<_, anyhow::Error>(())
                }

                res = self.serve() => {
                    res
                }
            }
        })?;

        Ok(())
    }

    fn new_store(&self, engine: &Engine, req_id: u64) -> Result<Store<Host>> {
        let mut builder = WasiCtxBuilder::new();
        self.run.configure_wasip2(&mut builder)?;

        builder.env("REQUEST_ID", req_id.to_string());

        builder.stdout(LogStream {
            prefix: format!("stdout [{req_id}] :: "),
            output: Output::Stdout,
        });

        builder.stderr(LogStream {
            prefix: format!("stderr [{req_id}] :: "),
            output: Output::Stderr,
        });

        let mut host = Host {
            table: wasmtime::component::ResourceTable::new(),
            ctx: builder.build(),
            http: WasiHttpCtx::new(),

            limits: StoreLimits::default(),

            #[cfg(feature = "wasi-nn")]
            nn: None,
        };

        if self.run.common.wasi.nn == Some(true) {
            #[cfg(feature = "wasi-nn")]
            {
                let graphs = self
                    .run
                    .common
                    .wasi
                    .nn_graph
                    .iter()
                    .map(|g| (g.format.clone(), g.dir.clone()))
                    .collect::<Vec<_>>();
                let (backends, registry) = wasmtime_wasi_nn::preload(&graphs)?;
                host.nn.replace(WasiNnCtx::new(backends, registry));
            }
        }

        let mut store = Store::new(engine, host);

        if self.run.common.wasm.timeout.is_some() {
            store.set_epoch_deadline(u64::from(EPOCH_PRECISION) + 1);
        }

        store.data_mut().limits = self.run.store_limits();
        store.limiter(|t| &mut t.limits);

        // If fuel has been configured, we want to add the configured
        // fuel amount to this store.
        if let Some(fuel) = self.run.common.wasm.fuel {
            store.set_fuel(fuel)?;
        }

        Ok(store)
    }

    fn add_to_linker(&self, linker: &mut Linker<Host>) -> Result<()> {
        let mut cli = self.run.common.wasi.cli;

        // Accept -Scommon as a deprecated alias for -Scli.
        if let Some(common) = self.run.common.wasi.common {
            if cli.is_some() {
                bail!(
                    "The -Scommon option should not be use with -Scli as it is a deprecated alias"
                );
            } else {
                // In the future, we may add a warning here to tell users to use
                // `-S cli` instead of `-S common`.
                cli = Some(common);
            }
        }

        // Repurpose the `-Scli` flag of `wasmtime run` for `wasmtime serve`
        // to serve as a signal to enable all WASI interfaces instead of just
        // those in the `proxy` world. If `-Scli` is present then add all
        // `command` APIs and then additionally add in the required HTTP APIs.
        //
        // If `-Scli` isn't passed then use the `proxy::add_to_linker`
        // bindings which adds just those interfaces that the proxy interface
        // uses.
        if cli == Some(true) {
            wasmtime_wasi::add_to_linker_async(linker)?;
            wasmtime_wasi_http::proxy::add_only_http_to_linker(linker)?;
        } else {
            wasmtime_wasi_http::proxy::add_to_linker(linker)?;
        }

        if self.run.common.wasi.nn == Some(true) {
            #[cfg(not(feature = "wasi-nn"))]
            {
                bail!("support for wasi-nn was disabled at compile time");
            }
            #[cfg(feature = "wasi-nn")]
            {
                wasmtime_wasi_nn::wit::ML::add_to_linker(linker, |host| host.nn.as_mut().unwrap())?;
            }
        }

        if self.run.common.wasi.threads == Some(true) {
            bail!("support for wasi-threads is not available with components");
        }

        if self.run.common.wasi.http == Some(false) {
            bail!("support for wasi-http must be enabled for `serve` subcommand");
        }

        Ok(())
    }

    async fn serve(mut self) -> Result<()> {
        use hyper::server::conn::http1;

        let mut config = self
            .run
            .common
            .config(None, use_pooling_allocator_by_default().unwrap_or(None))?;
        config.wasm_component_model(true);
        config.async_support(true);

        if self.run.common.wasm.timeout.is_some() {
            config.epoch_interruption(true);
        }

        match self.run.profile {
            Some(Profile::Native(s)) => {
                config.profiler(s);
            }

            // We bail early in `execute` if the guest profiler is configured.
            Some(Profile::Guest { .. }) => unreachable!(),

            None => {}
        }

        let engine = Engine::new(&config)?;
        let mut linker = Linker::new(&engine);

        self.add_to_linker(&mut linker)?;

        let component = match self.run.load_module(&engine, &self.component)? {
            RunTarget::Core(_) => bail!("The serve command currently requires a component"),
            RunTarget::Component(c) => c,
        };

        let instance = linker.instantiate_pre(&component)?;
        let instance = ProxyPre::new(instance)?;

        let socket = match &self.addr {
            SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
            SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
        };
        // Conditionally enable `SO_REUSEADDR` depending on the current
        // platform. On Unix we want this to be able to rebind an address in
        // the `TIME_WAIT` state which can happen then a server is killed with
        // active TCP connections and then restarted. On Windows though if
        // `SO_REUSEADDR` is specified then it enables multiple applications to
        // bind the port at the same time which is not something we want. Hence
        // this is conditionally set based on the platform (and deviates from
        // Tokio's default from always-on).
        socket.set_reuseaddr(!cfg!(windows))?;
        socket.bind(self.addr)?;
        let listener = socket.listen(100)?;

        eprintln!("Serving HTTP on http://{}/", listener.local_addr()?);

        let _epoch_thread = if let Some(timeout) = self.run.common.wasm.timeout {
            Some(EpochThread::spawn(
                timeout / EPOCH_PRECISION,
                engine.clone(),
            ))
        } else {
            None
        };

        log::info!("Listening on {}", self.addr);

        let handler = ProxyHandler::new(self, engine, instance);

        loop {
            let (stream, _) = listener.accept().await?;
            let stream = TokioIo::new(stream);
            let h = handler.clone();
            tokio::task::spawn(async {
                if let Err(e) = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(
                        stream,
                        hyper::service::service_fn(move |req| handle_request(h.clone(), req)),
                    )
                    .await
                {
                    eprintln!("error: {e:?}");
                }
            });
        }
    }
}

/// This is the number of epochs that we will observe before expiring a request handler. As
/// instances may be started at any point within an epoch, and epochs are counted globally per
/// engine, we expire after `EPOCH_PRECISION + 1` epochs have been observed. This gives a maximum
/// overshoot of `timeout / EPOCH_PRECISION`, which is more desirable than expiring early.
const EPOCH_PRECISION: u32 = 10;

struct EpochThread {
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochThread {
    fn spawn(timeout: std::time::Duration, engine: Engine) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = {
            let shutdown = Arc::clone(&shutdown);
            let handle = std::thread::spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    std::thread::sleep(timeout);
                    engine.increment_epoch();
                }
            });
            Some(handle)
        };

        EpochThread { shutdown, handle }
    }
}

impl Drop for EpochThread {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.shutdown.store(true, Ordering::Relaxed);
            handle.join().unwrap();
        }
    }
}

struct ProxyHandlerInner {
    cmd: ServeCommand,
    engine: Engine,
    instance_pre: ProxyPre<Host>,
    next_id: AtomicU64,
}

impl ProxyHandlerInner {
    fn next_req_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Clone)]
struct ProxyHandler(Arc<ProxyHandlerInner>);

impl ProxyHandler {
    fn new(cmd: ServeCommand, engine: Engine, instance_pre: ProxyPre<Host>) -> Self {
        Self(Arc::new(ProxyHandlerInner {
            cmd,
            engine,
            instance_pre,
            next_id: AtomicU64::from(0),
        }))
    }
}

type Request = hyper::Request<hyper::body::Incoming>;

async fn handle_request(
    ProxyHandler(inner): ProxyHandler,
    req: Request,
) -> Result<hyper::Response<HyperOutgoingBody>> {
    use http_body_util::BodyExt;

    let (sender, receiver) = tokio::sync::oneshot::channel();

    let task = tokio::task::spawn(async move {
        let req_id = inner.next_req_id();
        let (mut parts, body) = req.into_parts();

        parts.uri = {
            let uri_parts = parts.uri.into_parts();

            let scheme = uri_parts.scheme.unwrap_or(http::uri::Scheme::HTTP);

            let host = if let Some(val) = parts.headers.get(hyper::header::HOST) {
                std::str::from_utf8(val.as_bytes())
                    .map_err(|_| http_types::ErrorCode::HttpRequestUriInvalid)?
            } else {
                uri_parts
                    .authority
                    .as_ref()
                    .ok_or(http_types::ErrorCode::HttpRequestUriInvalid)?
                    .host()
            };

            let path_with_query = uri_parts
                .path_and_query
                .ok_or(http_types::ErrorCode::HttpRequestUriInvalid)?;

            hyper::Uri::builder()
                .scheme(scheme)
                .authority(host)
                .path_and_query(path_with_query)
                .build()
                .map_err(|_| http_types::ErrorCode::HttpRequestUriInvalid)?
        };

        let req = hyper::Request::from_parts(parts, body.map_err(hyper_response_error).boxed());

        log::info!(
            "Request {req_id} handling {} to {}",
            req.method(),
            req.uri()
        );

        let mut store = inner.cmd.new_store(&inner.engine, req_id)?;

        let req = store.data_mut().new_incoming_request(req)?;
        let out = store.data_mut().new_response_outparam(sender)?;

        let proxy = inner.instance_pre.instantiate_async(&mut store).await?;

        if let Err(e) = proxy
            .wasi_http_incoming_handler()
            .call_handle(store, req, out)
            .await
        {
            log::error!("[{req_id}] :: {:#?}", e);
            return Err(e);
        }

        Ok(())
    });

    match receiver.await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => {
            // An error in the receiver (`RecvError`) only indicates that the
            // task exited before a response was sent (i.e., the sender was
            // dropped); it does not describe the underlying cause of failure.
            // Instead we retrieve and propagate the error from inside the task
            // which should more clearly tell the user what went wrong. Note
            // that we assume the task has already exited at this point so the
            // `await` should resolve immediately.
            let e = match task.await {
                Ok(r) => r.expect_err("if the receiver has an error, the task must have failed"),
                Err(e) => e.into(),
            };
            bail!("guest never invoked `response-outparam::set` method: {e:?}")
        }
    }
}

#[derive(Clone)]
enum Output {
    Stdout,
    Stderr,
}

impl Output {
    fn write_all(&self, buf: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;

        match self {
            Output::Stdout => std::io::stdout().write_all(buf),
            Output::Stderr => std::io::stderr().write_all(buf),
        }
        .map_err(|e| anyhow!(e))
    }
}

#[derive(Clone)]
struct LogStream {
    prefix: String,
    output: Output,
}

impl wasmtime_wasi::StdoutStream for LogStream {
    fn stream(&self) -> Box<dyn wasmtime_wasi::HostOutputStream> {
        Box::new(self.clone())
    }

    fn isatty(&self) -> bool {
        use std::io::IsTerminal;

        match &self.output {
            Output::Stdout => std::io::stdout().is_terminal(),
            Output::Stderr => std::io::stderr().is_terminal(),
        }
    }
}

impl wasmtime_wasi::HostOutputStream for LogStream {
    fn write(&mut self, bytes: bytes::Bytes) -> StreamResult<()> {
        let mut msg = Vec::new();

        for line in bytes.split(|c| *c == b'\n') {
            if !line.is_empty() {
                msg.extend_from_slice(&self.prefix.as_bytes());
                msg.extend_from_slice(line);
                msg.push(b'\n');
            }
        }

        self.output
            .write_all(&msg)
            .map_err(StreamError::LastOperationFailed)
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(1024 * 1024)
    }
}

#[async_trait::async_trait]
impl wasmtime_wasi::Subscribe for LogStream {
    async fn ready(&mut self) {}
}

/// The pooling allocator is tailor made for the `wasmtime serve` use case, so
/// try to use it when we can. The main cost of the pooling allocator, however,
/// is the virtual memory required to run it. Not all systems support the same
/// amount of virtual memory, for example some aarch64 and riscv64 configuration
/// only support 39 bits of virtual address space.
///
/// The pooling allocator, by default, will request 1000 linear memories each
/// sized at 6G per linear memory. This is 6T of virtual memory which ends up
/// being about 42 bits of the address space. This exceeds the 39 bit limit of
/// some systems, so there the pooling allocator will fail by default.
///
/// This function attempts to dynamically determine the hint for the pooling
/// allocator. This returns `Some(true)` if the pooling allocator should be used
/// by default, or `None` or an error otherwise.
///
/// The method for testing this is to allocate a 0-sized 64-bit linear memory
/// with a maximum size that's N bits large where we force all memories to be
/// static. This should attempt to acquire N bits of the virtual address space.
/// If successful that should mean that the pooling allocator is OK to use, but
/// if it fails then the pooling allocator is not used and the normal mmap-based
/// implementation is used instead.
fn use_pooling_allocator_by_default() -> Result<Option<bool>> {
    const BITS_TO_TEST: u32 = 42;
    let mut config = Config::new();
    config.wasm_memory64(true);
    config.static_memory_maximum_size(1 << BITS_TO_TEST);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    // NB: the maximum size is in wasm pages to take out the 16-bits of wasm
    // page size here from the maximum size.
    let ty = MemoryType::new64(0, Some(1 << (BITS_TO_TEST - 16)));
    if Memory::new(&mut store, ty).is_ok() {
        Ok(Some(true))
    } else {
        Ok(None)
    }
}
