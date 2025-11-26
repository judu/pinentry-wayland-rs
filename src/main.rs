mod wayland_window;

use wayland_window::PinEntryWindow;
use pinentry::{Buttons, ConfirmChoice, PinentryCmds, PinentryServer};
use std::io::{stdin, stdout};
use std::sync::{Arc, Mutex};
use std::thread;
use std::path::PathBuf;

struct WaylandPinentry {
    _tty: Option<PathBuf>,
}

impl WaylandPinentry {
    fn new() -> Self {
        Self { _tty: None }
    }

    fn show_pin_dialog(
        &self,
        error: Option<&str>,
        window_title: &str,
        desc: Option<&str>,
        prompt: &str,
    ) -> Result<Option<String>, PinentryError> {
        log::debug!("Creating Wayland window for PIN entry");

        let description = if let Some(error_msg) = error {
            format!("{}\n\n{}", error_msg, desc.unwrap_or(""))
        } else {
            desc.unwrap_or("Please enter your PIN").to_string()
        };

        let result = Arc::new(Mutex::new(None));
        let result_clone = Arc::clone(&result);

        let title = window_title.to_string();
        let prompt = prompt.to_string();

        let wayland_thread = thread::spawn(move || {
            let (mut app, _conn, mut event_queue) = PinEntryWindow::new(description, prompt, title);

            app.create_window(&event_queue.handle());

            let app_result = app.get_result();

            loop {
                event_queue.blocking_dispatch(&mut app).unwrap();
                log::debug!("An event has been handled");

                if let Some(res) = app_result.lock().unwrap().take() {
                    *result_clone.lock().unwrap() = Some(res);
                    break;
                }
            }
        });

        wayland_thread
            .join()
            .map_err(|_| PinentryError::ThreadPanic)?;

        match result.lock().unwrap().take() {
            Some(Ok(pin)) => Ok(Some(pin)),
            Some(Err(_)) => Ok(None),
            None => Ok(None),
        }
    }
}

#[derive(Debug)]
enum PinentryError {
    ThreadPanic,
}

impl std::fmt::Display for PinentryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ThreadPanic => write!(f, "Wayland thread panicked"),
        }
    }
}

impl pinentry::HasErrorCode for PinentryError {
    fn code(&self) -> assuan::ErrorCode {
        assuan::ErrorCode::INTERNAL
    }
}

impl PinentryCmds for WaylandPinentry {
    type Error = PinentryError;

    fn set_tty(&mut self, path: PathBuf) -> Result<(), Self::Error> {
        self._tty = Some(path);
        Ok(())
    }

    fn get_pin(
        &mut self,
        error: Option<&str>,
        window_title: &str,
        desc: Option<&str>,
        prompt: &str,
    ) -> Result<Option<pinentry::SecretData>, Self::Error> {
        let pin = self.show_pin_dialog(error, window_title, desc, prompt)?;
        Ok(pin.map(|p| {
            let mut secret_data = pinentry::SecretData::default();
            secret_data.append(&p).expect("PIN should fit in response");
            secret_data
        }))
    }

    fn confirm(
        &mut self,
        error: Option<&str>,
        window_title: &str,
        desc: Option<&str>,
        _buttons: Buttons,
    ) -> Result<ConfirmChoice, Self::Error> {
        let result = self.show_pin_dialog(
            error,
            window_title,
            desc,
            "Press Enter to confirm, Escape to cancel",
        )?;

        if result.is_some() {
            Ok(ConfirmChoice::Ok)
        } else {
            Ok(ConfirmChoice::Canceled)
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    log::debug!("Pinentry Wayland starting");

    let pinentry = WaylandPinentry::new();
    let server = PinentryServer::new(pinentry).build_assuan_server();

    let mut server = server;

    if let Err(e) = server.serve_client(stdin(), stdout()) {
        log::error!("Error serving client: {}", e);
        std::process::exit(1);
    }

    log::debug!("Pinentry Wayland exiting");
}
