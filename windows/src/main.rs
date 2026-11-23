//! Peats per Windows.
//!
//! Il motore è WebView2 tramite `wry`: la barra dell'indirizzo è una piccola
//! WebView con l'HTML di `ui/chrome.html`, la pagina è una seconda WebView
//! sotto di essa. Tutto ciò che non è interfaccia (impostazioni, cronologia,
//! risoluzione degli indirizzi, segnalibri) arriva da `peats-core`.

// Un browser è un'applicazione grafica: senza questo, Windows apre anche una
// console vuota accanto alla finestra.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::cell::RefCell;
use std::rc::Rc;

use peats_core::model::Settings;
use peats_core::storage::{Paths, Storage, load_json};
use peats_core::url::resolve_input;
use serde::Deserialize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::{Window, WindowBuilder};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebView, WebViewBuilder};

const CHROME: &str = include_str!("../ui/chrome.html");
const HOME: &str = "https://duckduckgo.com";
/// Altezza della barra in pixel logici.
const BAR_HEIGHT: f64 = 56.0;

type Slot = Rc<RefCell<Option<Rc<WebView>>>>;

struct App {
    storage: Storage,
    settings: Settings,
}

/// Messaggi che la barra invia a Rust (`window.ipc.postMessage`).
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
enum Command {
    Navigate { url: String },
    Back,
    Forward,
    Reload,
}

fn main() -> anyhow::Result<()> {
    let paths = Paths::discover()?;
    let storage = Storage::open(&paths.database)?;
    let settings = load_json::<Settings>(&paths.settings)
        .unwrap_or_default()
        .sanitize();
    let app = Rc::new(App { storage, settings });

    let event_loop = EventLoop::new();
    let window: Window = WindowBuilder::new()
        .with_title("Peats")
        .with_inner_size(LogicalSize::new(1200.0, 800.0))
        .build(&event_loop)?;

    let size = logical_size(&window);
    let chrome_slot: Slot = Rc::new(RefCell::new(None));
    let content_slot: Slot = Rc::new(RefCell::new(None));

    // Pagina: registra la visita conclusa e allinea la barra all'indirizzo.
    let content = {
        let app = app.clone();
        let chrome_slot = chrome_slot.clone();
        WebViewBuilder::new()
            .with_url(HOME)
            .with_bounds(rect(0.0, BAR_HEIGHT, size.width, below_bar(size.height)))
            .with_navigation_handler(move |url| {
                let _ = app
                    .storage
                    .add_history_many(&[(url.clone(), String::new())]);
                if let Some(chrome) = chrome_slot.borrow().as_ref() {
                    let _ = chrome.evaluate_script(&format!(
                        "window.peats.setAddress({});",
                        json_string(&url)
                    ));
                }
                true
            })
            .build_as_child(&window)?
    };

    // Barra: riceve i comandi e li applica alla pagina.
    let chrome = {
        let app = app.clone();
        let content_slot = content_slot.clone();
        WebViewBuilder::new()
            .with_html(CHROME)
            .with_bounds(rect(0.0, 0.0, size.width, BAR_HEIGHT))
            .with_ipc_handler(move |request| handle(&app, &content_slot, request.body()))
            .build_as_child(&window)?
    };

    let chrome: Rc<WebView> = Rc::new(chrome);
    let content: Rc<WebView> = Rc::new(content);
    *chrome_slot.borrow_mut() = Some(chrome.clone());
    *content_slot.borrow_mut() = Some(content.clone());

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::Resized(_),
                ..
            } => {
                let size = logical_size(&window);
                let _ = chrome.set_bounds(rect(0.0, 0.0, size.width, BAR_HEIGHT));
                let _ = content.set_bounds(rect(
                    0.0,
                    BAR_HEIGHT,
                    size.width,
                    below_bar(size.height),
                ));
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
}

/// Esegue un comando arrivato dalla barra.
fn handle(app: &Rc<App>, content_slot: &Slot, body: &str) {
    let Ok(command) = serde_json::from_str::<Command>(body) else {
        return;
    };
    let content = content_slot.borrow().clone();
    let Some(content) = content else {
        return;
    };

    match command {
        Command::Navigate { url } => {
            let template = app.settings.default_engine().template.clone();
            let target = resolve_input(&url, &template).unwrap_or(url);
            let _ = content.load_url(&target);
        }
        Command::Back => {
            let _ = content.evaluate_script("history.back()");
        }
        Command::Forward => {
            let _ = content.evaluate_script("history.forward()");
        }
        Command::Reload => {
            let _ = content.reload();
        }
    }
}

fn logical_size(window: &Window) -> LogicalSize<f64> {
    window.inner_size().to_logical(window.scale_factor())
}

fn below_bar(height: f64) -> f64 {
    (height - BAR_HEIGHT).max(1.0)
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
    Rect {
        position: LogicalPosition::new(x, y).into(),
        size: LogicalSize::new(width, height).into(),
    }
}

/// Stringa JSON valida per `evaluate_script`.
fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}
