//! QWERTY to Dvorak keyboard remapper daemon
//!
//! Monitors keyboard input devices via evdev, applies Dvorak remapping with
//! modifier-aware passthrough (Ctrl/Alt/Super shortcuts remain QWERTY),
//! and emits remapped events via uinput.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use evdev::{enumerate, EventType, Key};
use signal_hook::consts::signal::*;
use signal_hook::flag;
use std::os::fd::BorrowedFd;
use std::os::unix::io::AsRawFd;

// Constants for timing
// How often threads wake up to notice shutdown.
const SHUTDOWN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
// How long the uinput writer waits for events before checking the shutdown flag.
const UINPUT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

// Startup robustness
// On some desktops, uaccess ACLs for /dev/input and /dev/uinput may be applied shortly
// after the user session starts. If we enumerate devices too early, we can see zero
// devices and would otherwise exit successfully, leaving only the tray running.
const STARTUP_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const STARTUP_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

// Device filtering
// KEYBOARD_DEVICE_FILTER: Identify laptop keyboard devices (AT Translated Set 2 keyboards).
const KEYBOARD_DEVICE_FILTER: &str = "AT Translated";

// Channel configuration
// EVENT_BUFFER_SIZE: Bounded channel capacity for keyboard events.
// A larger buffer reduces blocking during short bursts without meaningfully increasing memory.
const EVENT_BUFFER_SIZE: usize = 8192;

// Error handling
// MAX_CONSECUTIVE_FAILURES: Maximum consecutive uinput write failures before giving up.
const MAX_CONSECUTIVE_FAILURES: u32 = 100;

// Backoff timing
// Initial backoff multiplier for uinput write failures (10ms per failure, capped at 100ms).
const BACKOFF_BASE_MS: u32 = 10;

/// Tracks the current state of modifier keys to determine whether to remap.
/// When any modifier is held, keys are passed through unmapped for shortcuts.
#[derive(Default)]
struct ModifierState {
    ctrl: bool,
    alt: bool,
    super_key: bool,
}

/// Maps QWERTY key codes to Dvorak layout.
/// Returns the original code if no mapping exists (non-alphabetic keys, etc.).
fn remap_key_code(key: Key, original_code: u16) -> u16 {
    match key {
        Key::KEY_MINUS => Key::KEY_LEFTBRACE.code(),
        Key::KEY_EQUAL => Key::KEY_RIGHTBRACE.code(),
        Key::KEY_Q => Key::KEY_APOSTROPHE.code(),
        Key::KEY_W => Key::KEY_COMMA.code(),
        Key::KEY_E => Key::KEY_DOT.code(),
        Key::KEY_R => Key::KEY_P.code(),
        Key::KEY_T => Key::KEY_Y.code(),
        Key::KEY_Y => Key::KEY_F.code(),
        Key::KEY_U => Key::KEY_G.code(),
        Key::KEY_I => Key::KEY_C.code(),
        Key::KEY_O => Key::KEY_R.code(),
        Key::KEY_P => Key::KEY_L.code(),
        Key::KEY_LEFTBRACE => Key::KEY_SLASH.code(),
        Key::KEY_RIGHTBRACE => Key::KEY_EQUAL.code(),
        Key::KEY_S => Key::KEY_O.code(),
        Key::KEY_D => Key::KEY_E.code(),
        Key::KEY_F => Key::KEY_U.code(),
        Key::KEY_G => Key::KEY_I.code(),
        Key::KEY_H => Key::KEY_D.code(),
        Key::KEY_J => Key::KEY_H.code(),
        Key::KEY_K => Key::KEY_T.code(),
        Key::KEY_L => Key::KEY_N.code(),
        Key::KEY_SEMICOLON => Key::KEY_S.code(),
        Key::KEY_APOSTROPHE => Key::KEY_MINUS.code(),
        Key::KEY_Z => Key::KEY_SEMICOLON.code(),
        Key::KEY_X => Key::KEY_Q.code(),
        Key::KEY_C => Key::KEY_J.code(),
        Key::KEY_V => Key::KEY_K.code(),
        Key::KEY_B => Key::KEY_X.code(),
        Key::KEY_N => Key::KEY_B.code(),
        Key::KEY_COMMA => Key::KEY_W.code(),
        Key::KEY_DOT => Key::KEY_V.code(),
        Key::KEY_SLASH => Key::KEY_Z.code(),
        _ => original_code,
    }
}

fn main() {
    env_logger::init();

    // systemd manages lifecycle; exit cleanly on SIGTERM/SIGINT.
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    if let Err(e) = flag::register(SIGTERM, Arc::clone(&shutdown_flag)) {
        eprintln!("Warning: Failed to register SIGTERM handler: {e}");
    }
    if let Err(e) = flag::register(SIGINT, Arc::clone(&shutdown_flag)) {
        eprintln!("Warning: Failed to register SIGINT handler: {e}");
    }

    println!("Key mapping loaded with 33 entries");

    // Wait for keyboard devices + uinput to become available.
    let mut last_startup_log = Instant::now() - STARTUP_LOG_INTERVAL;
    let (keyboards, mut uinput_device) = loop {
        if shutdown_flag.load(Ordering::Relaxed) {
            println!("Shutdown requested before devices were ready");
            return;
        }

        let devices: Vec<_> = enumerate().collect();
        let mut keyboards = Vec::new();
        // Filter for physical keyboard devices by checking for A-Z key support.
        // Only grab devices matching KEYBOARD_DEVICE_FILTER to avoid mice, touchpads, etc.
        for (_path, device) in devices {
            if let Some(keys) = device.supported_keys()
                && keys.contains(Key::KEY_A)
                && keys.contains(Key::KEY_Z)
                && device.name().map(|n| n.contains(KEYBOARD_DEVICE_FILTER)).unwrap_or(false)
            {
                keyboards.push(device);
            }
        }

        if keyboards.is_empty() {
            if last_startup_log.elapsed() >= STARTUP_LOG_INTERVAL {
                eprintln!(
                    "No compatible keyboard devices available yet; retrying every {:?}…",
                    STARTUP_RETRY_INTERVAL
                );
                eprintln!(
                    "If this persists, check udev uaccess rules for /dev/input/event* (ID_INPUT_KEYBOARD==1)."
                );
                last_startup_log = Instant::now();
            }
            std::thread::sleep(STARTUP_RETRY_INTERVAL);
            continue;
        }

        let uinput_builder = match uinput::default() {
            Ok(builder) => builder,
            Err(e) => {
                if last_startup_log.elapsed() >= STARTUP_LOG_INTERVAL {
                    eprintln!("Failed to create uinput builder: {e}");
                    eprintln!("If this persists, check that the uinput kernel module is available.");
                    last_startup_log = Instant::now();
                }
                std::thread::sleep(STARTUP_RETRY_INTERVAL);
                continue;
            }
        };
        let uinput_builder = match uinput_builder.name("QwertDvert") {
            Ok(b) => b,
            Err(e) => {
                if last_startup_log.elapsed() >= STARTUP_LOG_INTERVAL {
                    eprintln!("Failed to set uinput device name: {e}");
                    eprintln!("This may indicate a permissions issue with /dev/uinput.");
                    last_startup_log = Instant::now();
                }
                std::thread::sleep(STARTUP_RETRY_INTERVAL);
                continue;
            }
        };
        let uinput_builder = match uinput_builder.event(uinput::event::Keyboard::All) {
            Ok(b) => b,
            Err(e) => {
                if last_startup_log.elapsed() >= STARTUP_LOG_INTERVAL {
                    eprintln!("Failed to configure uinput keyboard events: {e}");
                    last_startup_log = Instant::now();
                }
                std::thread::sleep(STARTUP_RETRY_INTERVAL);
                continue;
            }
        };
        let uinput_device = match uinput_builder.create() {
            Ok(device) => device,
            Err(e) => {
                if last_startup_log.elapsed() >= STARTUP_LOG_INTERVAL {
                    eprintln!("Failed to create uinput device: {e}");
                    eprintln!("If this persists, check udev uaccess rules for /dev/uinput.");
                    last_startup_log = Instant::now();
                }
                std::thread::sleep(STARTUP_RETRY_INTERVAL);
                continue;
            }
        };

        break (keyboards, uinput_device);
    };

    println!("Found {} keyboard devices", keyboards.len());
    println!("Created uinput device");

    // Channel for events (bounded to prevent memory issues)
    let (tx, rx) = mpsc::sync_channel::<(i32, i32, i32)>(EVENT_BUFFER_SIZE);

    let shutdown_flag_writer = shutdown_flag.clone();
    let writer_handle = std::thread::spawn(move || {
        let mut consecutive_failures = 0;

        loop {
            match rx.recv_timeout(UINPUT_TIMEOUT) {
                Ok((kind, code, value)) => {
                    if let Err(e) = uinput_device.write(kind, code, value) {
                        consecutive_failures += 1;
                        eprintln!("Failed to write to uinput device (failure {}/{}): {}", 
                                consecutive_failures, MAX_CONSECUTIVE_FAILURES, e);
                        
                        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                            eprintln!("Too many consecutive uinput write failures, exiting writer thread");
                            shutdown_flag_writer.store(true, Ordering::Relaxed);
                            break;
                        }
                        
                        // Continue trying with backoff - don't let temporary failures stop the writer
                        let backoff_ms = BACKOFF_BASE_MS * consecutive_failures.min(10);
                        std::thread::sleep(std::time::Duration::from_millis(backoff_ms as u64));
                    } else {
                        consecutive_failures = 0;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if shutdown_flag_writer.load(Ordering::Relaxed) {
                        println!("Uinput writer thread exiting due to shutdown signal");
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // All senders are gone; nothing else to do.
                    shutdown_flag_writer.store(true, Ordering::Relaxed);
                    println!("Uinput writer thread exiting (event channel disconnected)");
                    break;
                }
            }
        }
    });

    // Channel for device thread status reporting
    let (status_tx, status_rx) = mpsc::channel();

    let mut handles = vec![];
    for mut device in keyboards {
        let tx_clone = tx.clone();
        let shutdown_flag_clone = shutdown_flag.clone();

        let status_tx_clone = status_tx.clone();

        let handle = std::thread::spawn(move || {
            let device_name = device.name().map(|s| s.to_string()).unwrap_or_else(|| "Unknown".to_string());

            match device.grab() {
                Ok(_) => {
                    println!("Grabbed keyboard device: {}", device_name);
                }
                Err(e) => {
                    eprintln!("Failed to grab keyboard device {}: {}", device_name, e);
                    let _ = status_tx_clone.send(format!("Device {}: grab failed", device_name));
                    return;
                }
            }

            // Make the underlying evdev FD non-blocking and use epoll to wait for readability.
            // This allows quick shutdown when systemd sends SIGTERM.
            let raw_fd = device.as_raw_fd();
            if let Err(e) = (|| -> Result<(), nix::Error> {
                use nix::fcntl::{fcntl, FcntlArg, OFlag};
                let current = OFlag::from_bits_truncate(fcntl(raw_fd, FcntlArg::F_GETFL)?);
                let new_flags = current | OFlag::O_NONBLOCK;
                fcntl(raw_fd, FcntlArg::F_SETFL(new_flags))?;
                Ok(())
            })() {
                eprintln!("Warning: Failed to set O_NONBLOCK for {}: {}", device_name, e);
            }

            let epoll = match nix::sys::epoll::Epoll::new(nix::sys::epoll::EpollCreateFlags::EPOLL_CLOEXEC) {
                Ok(epoll) => epoll,
                Err(e) => {
                    eprintln!("Failed to create epoll instance for {}: {}", device_name, e);
                    let _ = status_tx_clone.send(format!("Device {}: epoll create failed", device_name));
                    return;
                }
            };

            let event = nix::sys::epoll::EpollEvent::new(nix::sys::epoll::EpollFlags::EPOLLIN, 0);
            let borrowed_fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
            if let Err(e) = epoll.add(borrowed_fd, event) {
                eprintln!("Failed to add fd to epoll for {}: {}", device_name, e);
                let _ = status_tx_clone.send(format!("Device {}: epoll ctl failed", device_name));
                return;
            }

            let mut epoll_events = [nix::sys::epoll::EpollEvent::empty(); 2];

            let mut modifier_state = ModifierState::default();

            loop {
                if shutdown_flag_clone.load(Ordering::Relaxed) {
                    println!("Keyboard thread exiting due to shutdown signal");
                    break;
                }

                match device.fetch_events() {
                    Ok(events) => {
                        for event in events {
                            if event.event_type() == EventType::KEY {
                                let key_code = event.code();
                                let value = event.value();
                                let key = Key::new(key_code);

                                match key {
                                    Key::KEY_LEFTCTRL | Key::KEY_RIGHTCTRL => {
                                        modifier_state.ctrl = value != 0;
                                    }
                                    Key::KEY_LEFTALT | Key::KEY_RIGHTALT => {
                                        modifier_state.alt = value != 0;
                                    }
                                    Key::KEY_LEFTMETA | Key::KEY_RIGHTMETA => {
                                        modifier_state.super_key = value != 0;
                                    }
                                    _ => {}
                                }

                                let output_code = if modifier_state.ctrl || modifier_state.alt || modifier_state.super_key {
                                    key_code
                                } else {
                                    remap_key_code(key, key_code)
                                };

                                // Event prioritization: Key press/release must never be dropped (causes stuck keys).
                                // Autorepeat (value=2) can be dropped under load. SYN events frame the input stream.
                                if value == 2 {
                                    match tx_clone.try_send((event.event_type().0 as i32, output_code as i32, value)) {
                                        Ok(_) => {}
                                        Err(mpsc::TrySendError::Full(_)) => {
                                            // Drop repeats under pressure
                                        }
                                        Err(mpsc::TrySendError::Disconnected(_)) => {
                                            eprintln!(
                                                "Failed to send key event to uinput writer: channel disconnected"
                                            );
                                            return;
                                        }
                                    }
                                } else if let Err(e) = tx_clone.send((
                                    event.event_type().0 as i32,
                                    output_code as i32,
                                    value,
                                )) {
                                    eprintln!("Failed to send key event to uinput writer: {e}");
                                    return;
                                }
                            } else {
                                // Pass through other events.
                                // SYN events are critical framing for the input stream; do not drop them.
                                if event.event_type() == EventType::SYNCHRONIZATION {
                                    if let Err(e) = tx_clone.send((
                                        event.event_type().0 as i32,
                                        event.code() as i32,
                                        event.value(),
                                    )) {
                                        eprintln!("Failed to send syn event to uinput writer: {e}");
                                        return;
                                    }
                                } else {
                                    match tx_clone.try_send((
                                        event.event_type().0 as i32,
                                        event.code() as i32,
                                        event.value(),
                                    )) {
                                        Ok(_) => {}
                                        Err(mpsc::TrySendError::Full(_)) => {
                                            // Non-critical events can be dropped under sustained load.
                                        }
                                        Err(mpsc::TrySendError::Disconnected(_)) => {
                                            eprintln!(
                                                "Failed to send event to uinput writer: channel disconnected"
                                            );
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // When non-blocking, "no events" is a normal condition.
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            // Wait briefly for bytes available, but wake periodically to check shutdown.
                            let _ = epoll.wait(
                                &mut epoll_events,
                                SHUTDOWN_POLL_INTERVAL
                                    .as_millis()
                                    .min(u16::MAX as u128) as u16,
                            );
                            continue;
                        }

                        eprintln!("Failed to fetch events from device {}: {}", device_name, e);
                        let _ = status_tx_clone.send(format!("Device {}: runtime error - {}", device_name, e));
                        break;
                    }
                }
            }
        });

        handles.push(handle);
    }

    // Thread to monitor device status
    let shutdown_flag_status = shutdown_flag.clone();
    let status_handle = std::thread::spawn(move || {
        while let Ok(status) = status_rx.recv() {
            println!("Device status: {}", status);
            // Log device status changes. Device restart is not implemented;
            // systemd will restart the entire daemon on total failure.
        }
        if !shutdown_flag_status.load(Ordering::Relaxed) {
            println!("All device threads have exited unexpectedly");
        }
    });

    // Wait for all threads to exit (successful ones run until shutdown, failed ones exit immediately)
    for handle in handles {
        let _ = handle.join();
    }

    // Allow background threads to terminate cleanly.
    drop(tx);
    drop(status_tx);
    let _ = writer_handle.join();
    let _ = status_handle.join();

    // If we weren't asked to shut down but we got here, it means all device threads exited.
    // Exit with failure so systemd can restart the daemon.
    if !shutdown_flag.load(Ordering::Relaxed) {
        eprintln!("ERROR: all device threads exited; exiting so systemd can restart");
        std::process::exit(1);
    }
}
