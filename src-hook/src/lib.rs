use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use futures::sink::SinkExt;
use interprocess::os::windows::named_pipe::tokio::PipeListenerOptionsExt;
use interprocess::os::windows::named_pipe::{PipeListenerOptions, PipeMode};
use log::{info, warn};
use tokio::sync::broadcast;

// Dev-only hook control channel (self-teardown for hot-reload). Kept out of
// the toolbox RPC on purpose — a lifecycle command is not a Toolbox tool.
#[cfg(feature = "eject")]
mod control;
mod event;
mod hooks;
mod process;
// Proton only: the dinput8 proxy export. Absent from the Windows DLL, which
// is injected and needs no export at all — see proxy.rs.
#[cfg(feature = "proton")]
mod proxy;
#[cfg(any(feature = "eject", test))]
mod teardown;
mod toolbox;
mod transport;

use protocol::Message;
use tokio_util::codec::{FramedWrite, LengthDelimitedCodec};

async fn handle_client<S>(
    mut stream: FramedWrite<S, LengthDelimitedCodec>,
    mut rx: event::Rx,
) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let msg = match rx.recv().await {
            Ok(msg) => msg,
            // The receiver fell behind the ring, so the oldest messages are
            // gone. Losing those is survivable; treating it as end-of-stream is
            // not — it closed the pipe, and the app reads that as "the game is
            // gone", force-saves the half-finished encounter and alerts the
            // user mid-fight. Every event AFTER the burst is still worth having,
            // so skip the gap and keep reading.
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!("event stream lagged, dropped {skipped} events");
                continue;
            }
            // Every sender is gone: the hook is shutting down.
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let bytes = protocol::bincode::serialize(&msg)?;
        stream.send(bytes.into()).await?;
    }

    Ok(())
}

#[derive(Debug)]
struct Server {
    tx: event::Tx,
}

impl Server {
    fn new() -> Self {
        let (tx, _) = broadcast::channel::<Message>(1024);
        Server { tx }
    }

    async fn run(&self) {
        // The toolbox RPC channel is independent of the event stream and
        // must not die with a client, so it gets its own task.
        let (toolbox_ready, wait_for_toolbox) = tokio::sync::oneshot::channel();
        tokio::spawn(toolbox::run(toolbox_ready));
        // Dev eject: a SEPARATE control endpoint carries the self-teardown
        // command (kept out of the toolbox tool RPC). Own task, same reason.
        #[cfg(feature = "eject")]
        tokio::spawn(control::run(self.tx.clone()));

        // Order matters: the app fires its `Hello` on the toolbox channel the
        // instant the EVENT stream below accepts it, so publishing the event
        // pipe first leaves a window where a healthy hook cannot answer.
        // Bounded, and a dropped sender resolves immediately: under Wine
        // `run_tcp` retries a taken port every 5s, and event delivery must
        // never wait on that.
        if tokio::time::timeout(std::time::Duration::from_secs(2), wait_for_toolbox)
            .await
            .is_err()
        {
            warn!("toolbox channel not ready within 2s; starting the event server anyway");
        }

        match transport::select_transport() {
            transport::Transport::NamedPipe => self.run_pipe().await,
            #[cfg(feature = "proton")]
            transport::Transport::Tcp => self.run_tcp().await,
        }
    }

    async fn run_pipe(&self) {
        if let Ok(listener) = PipeListenerOptions::new()
            .path(protocol::PIPE_NAME)
            .mode(PipeMode::Bytes)
            .accept_remote(false)
            .create_tokio_send_only()
        {
            loop {
                let read_pipe = listener.accept().await;
                match read_pipe {
                    Ok(stream) => {
                        let rx = self.tx.subscribe();
                        tokio::spawn(async move {
                            let encoder = LengthDelimitedCodec::new();
                            let writer = FramedWrite::new(stream, encoder);

                            let _ = handle_client(writer, rx).await;
                        });
                    }
                    Err(e) => {
                        warn!("Error accepting client: {:?}", e);
                    }
                }
            }
        }
    }

    // Under Wine/Proton: a native Linux app connects to this directly (Wine
    // sockets are real Linux sockets). Bind failures (port taken) retry
    // rather than killing event delivery for the whole session.
    #[cfg(feature = "proton")]
    async fn run_tcp(&self) {
        let listener = loop {
            match tokio::net::TcpListener::bind(protocol::TCP_ADDR).await {
                Ok(listener) => break listener,
                Err(e) => {
                    warn!(
                        "Could not bind {}: {e:?}; retrying in 5s",
                        protocol::TCP_ADDR
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        };
        info!("Listening on {}", protocol::TCP_ADDR);
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let rx = self.tx.subscribe();
                    tokio::spawn(async move {
                        let writer = FramedWrite::new(stream, LengthDelimitedCodec::new());
                        let _ = handle_client(writer, rx).await;
                    });
                }
                Err(e) => {
                    warn!("Error accepting client: {:?}", e);
                    // A persistent accept error (fd exhaustion, Wine socket
                    // quirks) must not busy-spin inside the game process.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
}

#[tokio::main]
async fn setup() {
    info!("Setting up event listener");

    let server = Server::new();
    let tx = server.tx.clone();

    info!("Setting up hooks...");

    match hooks::setup_hooks(tx) {
        Ok(_) => info!("Hooks initialized"),
        Err(e) => warn!("Error initializing hooks: {:?}", e),
    }

    #[cfg(feature = "console")]
    println!("Hook library initialized");

    let _ = std::io::stdout().flush();

    // Dev eject: exit the runtime when teardown signals, which closes every
    // listener — the app's cue that this module is safe to FreeLibrary.
    #[cfg(feature = "eject")]
    tokio::select! {
        _ = server.run() => {}
        _ = teardown::shutdown_notified() => {
            log::info!("eject: hook runtime shutting down");
        }
    }
    #[cfg(not(feature = "eject"))]
    server.run().await;
}

/// Get the directory containing this DLL using the Windows linker's
/// __ImageBase symbol + GetModuleFileNameW. Bypasses the Known Folder
/// API so the log stays portable alongside the DLL instead of polluting
/// the real %APPDATA%/Roaming.
fn get_dll_dir() -> Option<PathBuf> {
    extern "C" {
        #[link_name = "\x01__ImageBase"]
        static __IMAGE_BASE: std::ffi::c_void;
    }
    extern "system" {
        fn GetModuleFileNameW(hModule: isize, lpFilename: *mut u16, nSize: u32) -> u32;
    }

    let hmod = unsafe { &__IMAGE_BASE as *const _ as isize };
    let mut buf: Vec<u16> = vec![0; 260];
    let len = unsafe { GetModuleFileNameW(hmod, buf.as_mut_ptr(), buf.len() as u32) } as usize;

    if len > 0 && len < buf.len() {
        let path_str = String::from_utf16_lossy(&buf[..len]);
        PathBuf::from(path_str).parent().map(|p| p.to_path_buf())
    } else {
        None
    }
}

fn initialize_logger() -> anyhow::Result<()> {
    // Write the log next to the DLL (portable), NOT into the real
    // %APPDATA%/Roaming which uses Known Folder API and ignores env vars.
    let log_dir = get_dll_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("portable_data")
        .join("hook-logs");

    std::fs::create_dir_all(&log_dir)?;

    let log_file = log_dir.join("gbfr-logs.txt");

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {}] {}",
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Info)
        .chain(fern::log_file(log_file)?)
        .apply()?;

    Ok(())
}

/// Log any panic (location + message) to the fern log before it unwinds. A panic inside a
/// detour would otherwise unwind across the FFI boundary into game code (UB) and typically
/// manifests as a silent game freeze with NO record — the log just stops mid-stream. With
/// this hook a future fault that IS a Rust panic leaves a `[ERROR] hook panic: ...` line
/// pointing at the exact file:line, turning a silent freeze into a diagnosable event.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());

        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());

        log::error!("hook panic at {location}: {message}");
    }));
}

// Not in test builds: the ctor would run inside the test process — sigscanning
// the test binary and creating the app's named pipe are both unwanted there.
#[cfg(not(test))]
#[ctor::ctor]
fn entry() {
    #[cfg(feature = "console")]
    unsafe {
        let _ = windows::Win32::System::Console::AllocConsole();
    }

    let _ = initialize_logger();
    install_panic_hook();
    std::thread::spawn(setup);
}
