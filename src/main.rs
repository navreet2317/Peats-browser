mod browser;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "io.github.peats.Browser";

fn main() -> glib::ExitCode {
    glib::set_application_name("Peats");

    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("style.css"));

        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });

    let browser_holder: std::rc::Rc<std::cell::RefCell<Option<std::rc::Rc<browser::Browser>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));

    app.connect_activate(move |app| {
        if let Some(browser) = browser_holder.borrow().as_ref() {
            browser.present();
            return;
        }

        match browser::Browser::open(app) {
            Ok(browser) => *browser_holder.borrow_mut() = Some(browser),
            Err(message) => show_startup_error(app, &message),
        }
    });

    app.run()
}

/// Errore di avvio (cartelle XDG o database): si informa l'utente invece di
/// terminare con un panic.
fn show_startup_error(app: &adw::Application, message: &str) {
    eprintln!("avvio non riuscito: {message}");

    let dialog = adw::AlertDialog::new(Some("Peats non può avviarsi"), Some(message));
    dialog.add_response("close", "Chiudi");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");

    let app = app.clone();
    dialog.connect_response(None, move |dialog, _| {
        dialog.close();
        app.quit();
    });

    dialog.present(None::<&gtk::Widget>);
}
