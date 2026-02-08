//! System tray UI for QwertDvert using KDE StatusNotifierItem protocol.
//!
//! Provides a simple "Quit" menu that stops the daemon via systemd.

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Status, ToolTip, Tray, TrayService};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use signal_hook::consts::signal::*;
use signal_hook::flag;

// UI configuration
const KEYBOARD_ICON_NAME: &str = "input-keyboard";
const APP_TITLE: &str = "QwertDvert";
const TRAY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

fn stop_qwertdvert_via_systemd() {
    // Preferred integration: systemd manages singleton, startup, and shutdown.
    // If systemd isn't available (or the user isn't running the services via systemd),
    // we still exit cleanly and avoid any custom process-control hacks.
    let _ = std::process::Command::new("systemctl")
        .arg("--user")
        .arg("stop")
        .arg("qwertdvert.target")
        .status();
}

fn stop_and_exit() -> ! {
    stop_qwertdvert_via_systemd();
    std::process::exit(0);
}

/// Minimal tray implementation. All state is managed by systemd services.
struct MyTray;

impl Tray for MyTray {
    fn icon_name(&self) -> String {
        KEYBOARD_ICON_NAME.to_string()
    }

    fn title(&self) -> String {
        APP_TITLE.to_string()
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn tool_tip(&self) -> ToolTip {
        let pid = std::process::id();
        let icon = KEYBOARD_ICON_NAME.to_string();
        ToolTip {
            icon_name: icon,
            icon_pixmap: Vec::new(),
            title: APP_TITLE.to_string(),
            description: format!("QWERTY to Dvorak remapper running (PID {})", pid),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // Left-click handler (currently just logs the click).
        println!("Tray icon clicked");
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![StandardItem {
            label: "Quit".to_string(),
            activate: Box::new(|_tray: &mut MyTray| {
                stop_and_exit();
            }),
            ..Default::default()
        }
        .into()]
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Register signal handlers for clean shutdown.
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, Arc::clone(&shutdown_flag))?;
    flag::register(SIGINT, Arc::clone(&shutdown_flag))?;

    let tray = MyTray;
    let service = TrayService::new(tray);
    service.spawn();

    // Keep the tray process running in foreground for KDE integration.
    loop {
        std::thread::sleep(TRAY_POLL_INTERVAL);

        // Check if we received a shutdown signal.
        if shutdown_flag.load(Ordering::Relaxed) {
            println!("Shutting down due to signal...");
            stop_and_exit();
        }
    }
}