use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use bbqueue::framed::{FrameConsumer, FrameProducer};
use python_offsets_types::py313;
use python_unwind::RawFrame;
use sig_ring::RING_SIZE;

const STATE_UNINITIALIZED: u32 = 0;
const STATE_RUNNING: u32 = 1;

/// Default number of shards for concurrent signal handler access.
const DEFAULT_NUM_SHARDS: usize = 16;

static LIFECYCLE: AtomicU32 = AtomicU32::new(STATE_UNINITIALIZED);

/// Whether to log diagnostic messages to stderr. Off by default.
/// Set via `pyroscope_configure` before calling `pyroscope_start`.
static LOG_ENABLED: AtomicBool = AtomicBool::new(false);

/// Write a message to stderr when logging is enabled.
macro_rules! log_info {
    ($($arg:tt)*) => {
        if LOG_ENABLED.load(Ordering::Relaxed) {
            eprintln!("pyroscope_cpython: {}", format_args!($($arg)*));
        }
    };
}

/// Write an error message to stderr when logging is enabled.
macro_rules! log_error {
    ($($arg:tt)*) => {
        if LOG_ENABLED.load(Ordering::Relaxed) {
            eprintln!("pyroscope_cpython ERROR: {}", format_args!($($arg)*));
        }
    };
}

// ── Configuration ────────────────────────────────────────────────────────────

/// Runtime configuration for the profiler.
///
/// All fields have reasonable defaults matching the original hardcoded values.
/// Call `pyroscope_configure` before `pyroscope_start` to override.
struct Config {
    /// Number of shards for concurrent signal handler access (default: 16).
    num_shards: usize,
    /// Notify the reader thread every N successful sample writes (default: 32).
    notify_interval: u32,
    /// Enable diagnostic logging to stderr (default: false).
    log_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            num_shards: DEFAULT_NUM_SHARDS,
            notify_interval: sig_ring::DEFAULT_NOTIFY_INTERVAL,
            log_enabled: false,
        }
    }
}

/// Global config, set before start. Protected by LIFECYCLE state machine:
/// only written when STATE_UNINITIALIZED, only read during init_sequence.
static CONFIG: std::sync::Mutex<Option<Config>> = std::sync::Mutex::new(None);

// ── Per-shard state ──────────────────────────────────────────────────────────

/// Per-shard mutable state protected by a spin::Mutex.
///
/// The signal handler `try_lock()`s a shard, unwinds into `frame_buffer`,
/// then writes the result into the bbqueue `producer`.
struct Shard {
    frame_buffer: [RawFrame; python_unwind::MAX_DEPTH],
    producer: FrameProducer<'static, RING_SIZE>,
}

// ── Handler state (shared between init + signal handler) ─────────────────────

/// Global profiler state accessed by the signal handler via `AtomicPtr`.
///
/// Allocated once at init time via `Box::into_raw`. Published to the handler
/// with `Release`; the handler loads with `Acquire`. Never deallocated.
struct HandlerState {
    debug_offsets: py313::_Py_DebugOffsets,
    tls_offset: u64,
    /// Expected type-object addresses for runtime type checking.
    type_addrs: python_unwind::TypeAddrs,
    /// Dynamically-sized shard array (length = config.num_shards).
    shards: Vec<notlibc::ShardMutex<Shard>>,
    eventfd: notlibc::EventFd,
    samples_since_notify: AtomicU32,
    /// Cached config values used in the signal handler hot path.
    num_shards: usize,
    notify_interval: u32,
}

// SAFETY: HandlerState is initialized once and then only accessed via:
// - signal handler: loads AtomicPtr(Acquire), takes shard try_lock,
//   reads debug_offsets/tls_offset (immutable), writes to producer.
// - reader thread: takes shard lock, reads consumers (separate from handler).
// All accesses are properly synchronized via AtomicPtr + spin::Mutex.
unsafe impl Sync for HandlerState {}

static HANDLER_STATE: AtomicPtr<HandlerState> = AtomicPtr::new(core::ptr::null_mut());

// ── Signal handler ───────────────────────────────────────────────────────────

extern "C" fn on_sigprof(_sig: c_int, _info: *mut libc::siginfo_t, _ctx: *mut c_void) {
    // Step 1: Load global profiler state.
    let state_ptr = HANDLER_STATE.load(Ordering::Acquire);
    if state_ptr.is_null() {
        return;
    }
    let state = unsafe { &*state_ptr };

    // Step 2: Read FS base.
    let fs_base = match kindasafe::fs_0x0() {
        Ok(v) => v,
        Err(_) => return,
    };

    // Step 3: Read tstate from TLS.
    let tstate_addr = fs_base.wrapping_add(state.tls_offset);
    let tstate = match kindasafe::u64(tstate_addr) {
        Ok(v) => v,
        Err(_) => return,
    };
    if tstate == 0 {
        return;
    }

    // Step 4: Select shard via gettid, try-lock with 3 fallback attempts.
    let tid = notlibc::gettid();
    let num_shards = state.num_shards;
    let base = tid as usize % num_shards;

    let mut guard = None;
    for attempt in 0..3 {
        let idx = (base + attempt) % num_shards;
        if let Some(g) = state.shards[idx].try_lock() {
            guard = Some(g);
            break;
        }
    }
    let mut guard = match guard {
        Some(g) => g,
        None => return, // all 3 shards contended — drop sample
    };

    // Step 5: Unwind Python stack into the shard's pre-allocated frame buffer.
    let depth = python_unwind::unwind(
        tstate,
        &state.debug_offsets,
        &state.type_addrs,
        &mut guard.frame_buffer,
    );
    if depth == 0 {
        return;
    }

    // Step 6: Write stack trace record into the shard's bbqueue producer.
    // Split the borrow: take a shared ref to the frame_buffer data, then
    // pass the producer as &mut. This is safe because write() only reads
    // from frames[..depth] and only writes to the producer.
    let shard = &mut *guard;
    sig_ring::write(&mut shard.producer, tid, &shard.frame_buffer, depth);

    // Step 7: Notify reader thread periodically.
    let total = state.samples_since_notify.fetch_add(1, Ordering::Relaxed);
    if total % state.notify_interval == 0 {
        state.eventfd.notify();
    }
}

// ── Reader thread ────────────────────────────────────────────────────────────

/// Reader thread entry point. Wakes on eventfd or 15s timeout, drains all
/// shard consumers, and debug-prints the received stacks.
fn reader_thread(
    state: &'static HandlerState,
    mut consumers: Vec<FrameConsumer<'static, RING_SIZE>>,
) {
    // Set up epoll to wait on the eventfd.
    let mut event_set = match notlibc::EventSet::new() {
        Ok(es) => es,
        Err(_) => {
            log_error!("reader: failed to create EventSet");
            return;
        }
    };
    if event_set.add(&state.eventfd).is_err() {
        log_error!("reader: failed to add eventfd to EventSet");
        return;
    }

    log_info!("reader thread started, {} shards", consumers.len());

    loop {
        // Wait for eventfd notification or 15s timeout.
        let _ = event_set.wait(15_000);

        // Drain all shards.
        for (shard_idx, consumer) in consumers.iter_mut().enumerate() {
            // Lock the shard to ensure no signal handler is mid-write.
            let _guard = state.shards[shard_idx].lock();

            // Drain all available frames from this shard's consumer.
            while let Some(grant) = consumer.read() {
                if let Some(record) = sig_ring::parse_record(&grant) {
                    notlibc::debug::writes("reader: tid=");
                    notlibc::debug::write_hex(record.tid as usize);
                    notlibc::debug::writes(" depth=");
                    notlibc::debug::write_hex(record.depth as usize);
                    notlibc::debug::puts("");

                    for i in 0..record.depth as usize {
                        let frame = record.frame(i);
                        notlibc::debug::writes("  reader: [");
                        notlibc::debug::write_hex(i);
                        notlibc::debug::writes("] code=0x");
                        notlibc::debug::write_hex(frame.code_object as usize);
                        notlibc::debug::writes(" instr=0x");
                        notlibc::debug::write_hex(frame.instr_offset as usize);
                        notlibc::debug::puts("");
                    }
                }

                grant.release();
            }
        }
    }
}

// ── Public C API ─────────────────────────────────────────────────────────────

/// Configure the profiler before starting.
///
/// Must be called before `pyroscope_start`. Parameters:
/// - `num_shards`: number of shards (0 = use default 16). Must be >= 1.
/// - `queue_size_kb`: per-shard queue size in KiB (ignored — set at compile
///   time via `ring-512k` / `ring-1m` features; included for future use).
/// - `log_enabled`: if nonzero, print diagnostic messages to stderr.
///
/// Returns 0 on success, 9 if the profiler is already running.
///
/// # Safety
///
/// Must be called from a single thread before `pyroscope_start`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pyroscope_configure(
    num_shards: c_int,
    _queue_size_kb: c_int,
    log_enabled: c_int,
) -> c_int {
    if LIFECYCLE.load(Ordering::Acquire) != STATE_UNINITIALIZED {
        return 9;
    }

    let num_shards = if num_shards <= 0 {
        DEFAULT_NUM_SHARDS
    } else {
        num_shards as usize
    };

    let config = Config {
        num_shards,
        notify_interval: sig_ring::DEFAULT_NOTIFY_INTERVAL,
        log_enabled: log_enabled != 0,
    };

    if config.log_enabled {
        LOG_ENABLED.store(true, Ordering::Release);
        eprintln!(
            "pyroscope_cpython: configured num_shards={}, ring_size={}KiB, logging=on",
            config.num_shards,
            RING_SIZE / 1024,
        );
    }

    if let Ok(mut guard) = CONFIG.lock() {
        *guard = Some(config);
    }

    0
}

/// Start the CPython profiler.
///
/// Runs the full init sequence: kindasafe crash recovery, Python binary
/// discovery, ELF symbol resolution, version detection, debug offsets,
/// TLS offset discovery, ring buffer allocation, reader thread spawn,
/// then installs a SIGPROF handler + 10 ms timer.
///
/// Returns 0 on success, nonzero error code on failure.
///
/// # Safety
///
/// `app_name` and `server_url` must be valid pointers to NUL-terminated
/// C strings, or null (which returns error code 1).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pyroscope_start(
    _app_name: *const c_char,
    _server_url: *const c_char,
) -> c_int {
    if LIFECYCLE
        .compare_exchange(
            STATE_UNINITIALIZED,
            STATE_RUNNING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return 9;
    }

    match init_sequence() {
        Ok(()) => 0,
        Err(code) => {
            log_error!("init failed with code {}", code);
            LIFECYCLE.store(STATE_UNINITIALIZED, Ordering::Release);
            code
        }
    }
}

fn init_sequence() -> Result<(), c_int> {
    // Take config (or use defaults).
    let config = CONFIG
        .lock()
        .ok()
        .and_then(|mut guard| guard.take())
        .unwrap_or_default();

    let num_shards = config.num_shards;
    let notify_interval = config.notify_interval;

    log_info!(
        "starting init: num_shards={}, ring_size={}KiB, notify_interval={}",
        num_shards,
        RING_SIZE / 1024,
        notify_interval,
    );

    // Step 1: Install kindasafe SIGSEGV/SIGBUS recovery.
    kindasafe_init::init().map_err(|_| {
        log_error!("kindasafe_init failed");
        1
    })?;
    log_info!("kindasafe_init ok");

    // Step 2: Find Python binary in /proc/self/maps.
    let binary = python_offsets::find_python_in_maps().map_err(|e| {
        log_error!("find_python_in_maps: {:?}", e);
        map_init_error(&e)
    })?;
    log_info!("found Python binary");

    // Step 3: Resolve _PyRuntime and Py_Version ELF symbols.
    let symbols = python_offsets::resolve_elf_symbols(&binary).map_err(|e| {
        log_error!("resolve_elf_symbols: {:?}", e);
        map_init_error(&e)
    })?;
    log_info!("resolved ELF symbols");

    // Step 4: Detect and validate Python version.
    let version = python_offsets::detect_version(symbols.py_version_addr).map_err(|e| {
        log_error!("detect_version: {:?}", e);
        map_init_error(&e)
    })?;
    log_info!("detected Python version: {:?}", version);

    // Read raw version hex (needed by read_debug_offsets).
    let version_hex = python_offsets::read_version_hex(symbols.py_version_addr).map_err(|e| {
        log_error!("read_version_hex: {:?}", e);
        map_init_error(&e)
    })?;

    // Step 5: Read _Py_DebugOffsets from _PyRuntime.
    let debug_offsets =
        python_offsets::read_debug_offsets(symbols.py_runtime_addr, &version, version_hex)
            .map_err(|e| {
                log_error!("read_debug_offsets: {:?}", e);
                map_init_error(&e)
            })?;
    log_info!("read debug offsets");

    // Step 6: Discover TLS offset for _PyThreadState_GetCurrent.
    let tls_offset = python_offsets::find_tls_offset(&binary).map_err(|e| {
        log_error!("find_tls_offset: {:?}", e);
        map_init_error(&e)
    })?;
    log_info!("TLS offset: 0x{:x}", tls_offset);

    // Step 7: Allocate bbqueue buffers and split into producer/consumer pairs.
    let mut producers: Vec<Option<FrameProducer<'static, RING_SIZE>>> =
        (0..num_shards).map(|_| None).collect();
    let mut consumers: Vec<Option<FrameConsumer<'static, RING_SIZE>>> =
        (0..num_shards).map(|_| None).collect();

    for i in 0..num_shards {
        let bb = Box::new(bbqueue::BBBuffer::<RING_SIZE>::new());
        let bb: &'static bbqueue::BBBuffer<RING_SIZE> = Box::leak(bb);
        let (prod, cons) = bb.try_split_framed().map_err(|_| {
            log_error!("bbqueue split failed for shard {}", i);
            7
        })?;
        producers[i] = Some(prod);
        consumers[i] = Some(cons);
    }
    log_info!("allocated {} ring buffers", num_shards);

    // Step 8: Create eventfd for reader thread notification.
    let eventfd = notlibc::EventFd::new().map_err(|_| {
        log_error!("eventfd creation failed");
        7
    })?;

    // Step 9: Build shard vec.
    let empty_frame = RawFrame {
        code_object: 0,
        instr_offset: 0,
    };
    let shards: Vec<notlibc::ShardMutex<Shard>> = (0..num_shards)
        .map(|i| {
            notlibc::ShardMutex::new(Shard {
                frame_buffer: [empty_frame; python_unwind::MAX_DEPTH],
                producer: producers[i].take().unwrap(),
            })
        })
        .collect();

    // Unwrap consumers into a vec.
    let consumers: Vec<FrameConsumer<'static, RING_SIZE>> =
        consumers.into_iter().map(|c| c.unwrap()).collect();

    // Step 10: Publish handler state.
    let type_addrs = python_unwind::TypeAddrs {
        code_type: symbols.py_code_type_addr,
    };
    let state = Box::new(HandlerState {
        debug_offsets,
        tls_offset,
        type_addrs,
        shards,
        eventfd,
        samples_since_notify: AtomicU32::new(0),
        num_shards,
        notify_interval,
    });
    let state: &'static HandlerState = unsafe { &*Box::into_raw(state) };
    HANDLER_STATE.store(
        state as *const HandlerState as *mut HandlerState,
        Ordering::Release,
    );

    // Step 11: Spawn reader thread.
    std::thread::Builder::new()
        .name("pyroscope-reader".into())
        .spawn(move || reader_thread(state, consumers))
        .map_err(|_| {
            log_error!("failed to spawn reader thread");
            7
        })?;

    // Steps 12+13: Install SIGPROF handler and start 10 ms ITIMER_PROF timer.
    sighandler::start(on_sigprof).map_err(|_| {
        log_error!("signal handler installation failed");
        8
    })?;

    log_info!("init complete");
    notlibc::debug::puts("pyroscope_cpython: init complete");
    Ok(())
}

/// Map `python_offsets::InitError` variants to integer error codes.
fn map_init_error(err: &python_offsets::InitError) -> c_int {
    match err {
        python_offsets::InitError::KindasafeInitFailed => 1,
        python_offsets::InitError::PythonNotFound => 2,
        python_offsets::InitError::Io => 2,
        python_offsets::InitError::SymbolNotFound(_) => 3,
        python_offsets::InitError::ElfParse => 3,
        python_offsets::InitError::DebugOffsetsMismatch => 4,
        python_offsets::InitError::UnsupportedVersion => 5,
        python_offsets::InitError::TlsDiscoveryFailed => 6,
    }
}
