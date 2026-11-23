use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use gtk::glib::variant::ToVariant;
use gtk::{gdk, gio, glib, pango};
use webkit::prelude::*;
use webkit::{LoadEvent, NetworkSession, UserContentManager, WebView};

use crate::blocklist;
use crate::model::{
    Bookmark, DownloadEntry, GROUP_COLORS, GroupId, HistoryEntry, SearchEngine, Session, Settings,
    TabGroup, TabId, TabSession, ThemePreference, WorkspaceId, WorkspaceSession,
};
use crate::reader::READER_JS;
use crate::storage::{HistoryOrder, Paths, Storage, StoredCredential, load_json, save_json};
use crate::suggest::{self, Candidate, EngineSpec, Suggestion, SuggestionKind};
use crate::url::{
    KeywordEngine, is_history_url, origin_of, resolve_input, resolve_input_with_keywords, search_url,
};

const APP_NAME: &str = "Peats";
const START_PAGE_TITLE: &str = "Nuova scheda";
/// Profilo dati storico: usa le cartelle WebKit `<data>/webkit` che l'app
/// usava prima dell'isolamento per spazio.
const DEFAULT_PROFILE: &str = "default";

thread_local! {
    /// Finestre private aperte: GTK non possiede oggetti Rust, quindi senza
    /// questo elenco l'`Rc` della finestra verrebbe liberato alla fine
    /// dell'azione che l'ha creata.
    static EXTRA_WINDOWS: RefCell<Vec<Rc<Browser>>> = const { RefCell::new(Vec::new()) };
}

pub struct Browser {
    app: adw::Application,
    window: adw::ApplicationWindow,
    paths: Paths,
    storage: Storage,
    network_session: NetworkSession,
    /// Sessioni di rete aggiuntive, una per profilo di spazio (cookie e
    /// archiviazione separati). La sessione `default` è `network_session`.
    workspace_sessions: RefCell<HashMap<String, NetworkSession>>,
    state: RefCell<State>,
    tab_widgets: RefCell<HashMap<TabId, TabWidgets>>,
    stack: gtk::Stack,
    tab_list: gtk::ListBox,
    pinned_strip: gtk::FlowBox,
    privacy_button: gtk::Button,
    split_view: adw::OverlaySplitView,
    workspace_button: gtk::MenuButton,
    address: gtk::Entry,
    back: gtk::Button,
    forward: gtk::Button,
    reload: gtk::Button,
    bookmark: gtk::Button,
    progress: gtk::ProgressBar,
    toast_overlay: adw::ToastOverlay,
    suppress_selection: Cell<bool>,
    user_content_manager: UserContentManager,
    /// Schede chiuse di recente (Ctrl+Shift+T).
    closed_tabs: RefCell<Vec<ClosedTab>>,
    next_closed_id: Cell<u64>,
    /// Navigazioni che stanno diventando un download: WebKit interrompe il
    /// caricamento del frame, ma non è un errore da segnalare.
    pending_downloads: RefCell<HashSet<String>>,
    /// Vero mentre la finestra si chiude: i webview vengono distrutti e i loro
    /// callback di chiusura non devono più toccare schede né sessione.
    shutting_down: Cell<bool>,
    /// Script dei segnali di privacy, per poterlo sostituire o rimuovere.
    signals_script: RefCell<Option<webkit::UserScript>>,
    /// Script di assistenza ai banner di consenso dei CMP riconosciuti.
    consent_script: RefCell<Option<webkit::UserScript>>,
    /// Menu a tendina dei suggerimenti sotto la barra indirizzi.
    suggestion_popover: gtk::Popover,
    suggestion_list: gtk::ListBox,
    /// Contatore delle richieste di suggerimento: scarta le risposte arrivate
    /// dopo che il testo è cambiato.
    suggestion_generation: Cell<u64>,
    /// Suggerimenti mostrati adesso, nell'ordine delle righe.
    suggestion_items: RefCell<Vec<suggest::Suggestion>>,
    /// Vero mentre il popover dei suggerimenti è aperto.
    suggestion_open: Cell<bool>,
    /// Indice della riga evidenziata nell'elenco suggerimenti (-1 = nessuna).
    suggestion_highlight: Cell<i32>,
    /// Vero mentre la barra viene aggiornata dal browser: quel cambio di testo
    /// non è una digitazione e non deve aprire i suggerimenti.
    suppress_suggestions: Cell<bool>,
    /// Riquadro di destra della vista affiancata e scheda che mostra.
    ///
    /// Invarianti:
    /// - `split_tab` è `Some(id)` solo se `split_stack` è visibile e mostra la
    ///   pagina `tab-<id>`;
    /// - la WebView di quella scheda sta **solo** in `split_stack`, tutte le
    ///   altre **solo** in `stack`;
    /// - `secondary_focused` è vero solo con lo split attivo.
    split_stack: gtk::Stack,
    content_panes: gtk::Paned,
    split_tab: RefCell<Option<TabId>>,
    /// Vero quando l'ultimo riquadro ad avere il fuoco è quello di destra:
    /// barra indirizzi e comandi seguono il riquadro attivo.
    secondary_focused: Cell<bool>,
    /// Finestra privata: sessioni effimere, niente cronologia, niente
    /// salvataggio della sessione su disco.
    private: bool,
    /// Visite in attesa di essere scritte in un'unica transazione.
    pending_history: RefCell<Vec<(String, String)>>,
    history_flush_scheduled: Cell<bool>,
}

struct State {
    workspaces: Vec<Workspace>,
    tabs: HashMap<TabId, Tab>,
    active_workspace: WorkspaceId,
    next_tab_id: TabId,
    next_workspace_id: WorkspaceId,
    next_group_id: GroupId,
    settings: Settings,
}

struct Workspace {
    id: WorkspaceId,
    name: String,
    tabs: Vec<TabId>,
    active_tab: Option<TabId>,
    groups: Vec<TabGroup>,
    /// Profilo dati (cartella WebKit) di questo spazio.
    profile: String,
}

struct Tab {
    id: TabId,
    workspace_id: WorkspaceId,
    webview: Option<WebView>,
    title: String,
    uri: String,
    pinned: bool,
    loading: bool,
    progress: f64,
    pending_uri: Option<String>,
    is_start_page: bool,
    load_failed: bool,
    favicon: Option<gdk::Texture>,
    group: Option<GroupId>,
    reader: bool,
    /// Ultimo momento in cui la scheda è stata visibile: serve a liberare le
    /// WebView inattive quando l'utente lo chiede.
    last_used: Instant,
}

#[derive(Clone, Debug)]
struct ClosedTab {
    id: u64,
    uri: String,
    title: String,
    workspace_id: WorkspaceId,
}

/// Dati necessari per ridisegnare una riga della lista schede.
struct TabRowData {
    id: TabId,
    title: String,
    uri: String,
    pinned: bool,
    favicon: Option<gdk::Texture>,
    group: Option<GroupId>,
}

struct TabWidgets {
    row: gtk::ListBoxRow,
    title: gtk::Label,
    favicon: gtk::Image,
    pin: gtk::Image,
}

impl State {
    fn workspace(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.workspaces.iter().find(|workspace| workspace.id == id)
    }

    fn workspace_mut(&mut self, id: WorkspaceId) -> Option<&mut Workspace> {
        self.workspaces
            .iter_mut()
            .find(|workspace| workspace.id == id)
    }

    fn active_workspace(&self) -> &Workspace {
        self.workspace(self.active_workspace)
            .expect("the active workspace must exist")
    }

    fn active_tab_id(&self) -> Option<TabId> {
        self.active_workspace().active_tab
    }

    fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.get(&id)
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.get_mut(&id)
    }

    fn search_template(&self) -> String {
        self.settings.default_engine().template.clone()
    }
}

impl Browser {
    pub fn open(app: &adw::Application) -> Result<Rc<Self>, String> {
        Self::open_mode(app, false)
    }

    /// Finestra privata: nessun dato persistente. Le sessioni sono effimere e
    /// il salvataggio della sessione è disattivato.
    pub fn open_private(app: &adw::Application) -> Result<Rc<Self>, String> {
        Self::open_mode(app, true)
    }

    fn open_mode(app: &adw::Application, private: bool) -> Result<Rc<Self>, String> {
        let paths = Paths::discover()
            .map_err(|error| format!("Impossibile preparare le cartelle di dati: {error}"))?;
        let settings = load_json::<Settings>(&paths.settings)
            .unwrap_or_default()
            .sanitize();
        let storage = Storage::open(&paths.database)
            .map_err(|error| format!("Impossibile aprire il database: {error}"))?;
        let network_session = if private {
            create_private_session()
        } else {
            create_network_session(&paths, DEFAULT_PROFILE)
        };
        // La cronologia non cresce all'infinito: potatura all'avvio secondo le
        // preferenze (0 = nessun limite).
        if let Err(error) = storage.prune_history(settings.history_retention_days) {
            eprintln!("cannot prune history: {error}");
        }
        let user_content_manager = UserContentManager::new();

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(if private { "Peats — privata" } else { APP_NAME })
            .default_width(1280)
            .default_height(820)
            .build();
        window.set_size_request(880, 560);

        let toolbar_view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();

        let sidebar_toggle = gtk::Button::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Mostra o nascondi la barra laterale (F9)")
            .build();
        sidebar_toggle.set_action_name(Some("win.toggle-sidebar"));

        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Indietro (Alt+←)")
            .sensitive(false)
            .build();
        back.set_action_name(Some("win.back"));
        back.add_css_class("nav-button");
        let forward = gtk::Button::builder()
            .icon_name("go-next-symbolic")
            .tooltip_text("Avanti (Alt+→)")
            .sensitive(false)
            .build();
        forward.set_action_name(Some("win.forward"));
        forward.add_css_class("nav-button");
        let reload = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Ricarica (Ctrl+R)")
            .build();
        reload.set_action_name(Some("win.reload"));
        reload.add_css_class("nav-button");

        let home = gtk::Button::builder()
            .icon_name("go-home-symbolic")
            .tooltip_text("Pagina iniziale (Alt+Home)")
            .build();
        home.set_action_name(Some("win.home"));
        home.add_css_class("nav-button");

        let nav = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        nav.append(&back);
        nav.append(&forward);
        nav.append(&reload);
        nav.append(&home);
        header.pack_start(&sidebar_toggle);
        header.pack_start(&nav);

        let address = gtk::Entry::builder()
            .placeholder_text("Cerca con DuckDuckGo o inserisci un indirizzo")
            .hexpand(true)
            .build();
        address.add_css_class("address-entry");
        address.set_width_chars(48);
        address.set_icon_from_icon_name(
            gtk::EntryIconPosition::Primary,
            Some("system-search-symbolic"),
        );
        address.set_icon_tooltip_text(
            gtk::EntryIconPosition::Primary,
            Some("Cerca sul web o apri un indirizzo"),
        );
        header.set_title_widget(Some(&address));

        // Suggerimenti: schede aperte, cronologia, segnalibri e ricerca rapida.
        // Il popover è agganciato alla barra indirizzi e si apre verso il basso.
        let suggestion_list = gtk::ListBox::new();
        // Nessuna selezione propria: l'evidenza la gestisce il browser con una
        // classe CSS, così il fuoco non esce mai dalla barra indirizzi.
        suggestion_list.set_selection_mode(gtk::SelectionMode::None);
        suggestion_list.set_can_focus(false);
        suggestion_list.add_css_class("suggestion-list");

        let suggestion_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .max_content_height(380)
            .propagate_natural_height(true)
            .child(&suggestion_list)
            .build();

        let suggestion_popover = gtk::Popover::new();
        suggestion_popover.set_parent(&address);
        // Un popover che prende il fuoco ruba i tasti alla barra indirizzi.
        suggestion_popover.set_can_focus(false);
        suggestion_popover.set_has_arrow(false);
        suggestion_popover.set_autohide(true);
        suggestion_popover.set_position(gtk::PositionType::Bottom);
        suggestion_popover.add_css_class("suggestion-popover");
        suggestion_popover.set_child(Some(&suggestion_scroll));

        let bookmark = gtk::Button::builder()
            .icon_name("non-starred-symbolic")
            .tooltip_text("Aggiungi ai segnalibri (Ctrl+D)")
            .build();
        bookmark.set_action_name(Some("win.toggle-bookmark"));
        header.pack_end(&bookmark);

        let primary_menu = build_primary_menu();
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&primary_menu)
            .tooltip_text("Menu principale")
            .build();
        header.pack_end(&menu_button);

        let privacy_button = gtk::Button::builder()
            .icon_name("security-high-symbolic")
            .tooltip_text("Privacy: blocco contenuti e pulizia dati")
            .build();
        privacy_button.add_css_class("flat");
        header.pack_end(&privacy_button);

        toolbar_view.add_top_bar(&header);

        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.add_css_class("sidebar");
        sidebar.set_width_request(200);

        let sidebar_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sidebar_header.add_css_class("sidebar-header");
        sidebar_header.set_margin_top(10);
        sidebar_header.set_margin_bottom(8);
        sidebar_header.set_margin_start(10);
        sidebar_header.set_margin_end(8);

        let workspace_button = gtk::MenuButton::new();
        workspace_button.set_label("Principale");
        workspace_button.set_always_show_arrow(true);
        workspace_button.set_hexpand(true);
        workspace_button.add_css_class("flat");
        workspace_button.add_css_class("workspace-button");
        workspace_button.set_tooltip_text(Some("Workspace"));

        let new_tab_button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Nuova scheda (Ctrl+T)")
            .build();
        new_tab_button.set_action_name(Some("win.new-tab"));
        new_tab_button.add_css_class("flat");

        let space_label = gtk::Label::new(Some("SPAZIO"));
        space_label.set_xalign(0.0);
        space_label.add_css_class("sidebar-section-label");
        space_label.set_margin_start(14);
        space_label.set_margin_top(10);
        space_label.set_margin_bottom(2);
        sidebar.append(&space_label);

        sidebar_header.set_margin_top(0);
        sidebar_header.append(&workspace_button);
        sidebar_header.append(&new_tab_button);
        sidebar.append(&sidebar_header);

        let pinned_strip = gtk::FlowBox::new();
        pinned_strip.set_selection_mode(gtk::SelectionMode::None);
        pinned_strip.set_max_children_per_line(6);
        pinned_strip.set_min_children_per_line(1);
        pinned_strip.set_homogeneous(true);
        pinned_strip.set_row_spacing(4);
        pinned_strip.set_column_spacing(4);
        pinned_strip.set_margin_top(4);
        pinned_strip.set_margin_bottom(8);
        pinned_strip.set_margin_start(10);
        pinned_strip.set_margin_end(10);
        pinned_strip.add_css_class("pinned-strip");
        pinned_strip.set_visible(false);
        sidebar.append(&pinned_strip);

        let tab_list = gtk::ListBox::new();
        tab_list.set_selection_mode(gtk::SelectionMode::Single);
        tab_list.add_css_class("tab-list");

        let tab_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&tab_list)
            .build();
        tab_scroll.add_css_class("tab-scroll");
        sidebar.append(&tab_scroll);

        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.add_css_class("sidebar-separator");
        sidebar.append(&separator);

        let sidebar_actions = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        sidebar_actions.add_css_class("sidebar-actions");
        sidebar_actions.set_margin_top(4);
        sidebar_actions.set_margin_bottom(6);
        sidebar_actions.set_margin_start(6);
        sidebar_actions.set_margin_end(6);

        let bookmarks_button = sidebar_action_button(
            "starred-symbolic",
            "Segnalibri (Ctrl+Shift+O)",
            "win.bookmarks",
        );
        let history_button = sidebar_action_button(
            "document-open-recent-symbolic",
            "Cronologia (Ctrl+H)",
            "win.history",
        );
        let downloads_button = sidebar_action_button(
            "folder-download-symbolic",
            "Download (Ctrl+J)",
            "win.downloads",
        );
        let preferences_button = sidebar_action_button(
            "emblem-system-symbolic",
            "Impostazioni (Ctrl+,)",
            "win.preferences",
        );

        sidebar_actions.append(&bookmarks_button);
        sidebar_actions.append(&history_button);
        sidebar_actions.append(&downloads_button);
        sidebar_actions.append(&preferences_button);
        sidebar.append(&sidebar_actions);

        let split_view = adw::OverlaySplitView::new();
        split_view.set_sidebar(Some(&sidebar));
        split_view.set_show_sidebar(true);
        split_view.set_collapsed(false);
        split_view.set_sidebar_width_unit(adw::LengthUnit::Px);
        split_view.set_min_sidebar_width(200.0);
        split_view.set_max_sidebar_width(260.0);
        split_view.set_sidebar_width_fraction(0.18);

        let progress = gtk::ProgressBar::new();
        progress.set_show_text(false);
        progress.add_css_class("loading-progress");
        progress.set_visible(false);

        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(150);
        stack.set_hexpand(true);
        stack.set_vexpand(true);

        // Vista affiancata: due riquadri reali, ognuno con la propria WebView.
        // Il riquadro di destra resta vuoto (e il manico nascosto) finché lo
        // split non viene attivato.
        let split_stack = gtk::Stack::new();
        split_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        split_stack.set_transition_duration(150);
        split_stack.set_hexpand(true);
        split_stack.set_vexpand(true);
        split_stack.set_visible(false);

        let content_panes = gtk::Paned::new(gtk::Orientation::Horizontal);
        content_panes.set_resize_start_child(true);
        content_panes.set_resize_end_child(true);
        content_panes.set_shrink_start_child(false);
        content_panes.set_shrink_end_child(false);
        content_panes.set_start_child(Some(&stack));
        content_panes.set_end_child(Some(&split_stack));
        content_panes.set_position(640);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_hexpand(true);
        content.append(&progress);
        content.append(&content_panes);

        split_view.set_content(Some(&content));
        toolbar_view.set_content(Some(&split_view));

        let toast_overlay = adw::ToastOverlay::new();
        toast_overlay.set_child(Some(&toolbar_view));
        window.set_content(Some(&toast_overlay));

        let browser = Rc::new(Self {
            app: app.clone(),
            window: window.clone(),
            paths,
            storage,
            network_session,
            workspace_sessions: RefCell::new(HashMap::new()),
            state: RefCell::new(State {
                workspaces: Vec::new(),
                tabs: HashMap::new(),
                active_workspace: 0,
                next_tab_id: 1,
                next_workspace_id: 1,
                next_group_id: 1,
                settings,
            }),
            tab_widgets: RefCell::new(HashMap::new()),
            stack,
            tab_list,
            pinned_strip,
            privacy_button,
            split_view,
            workspace_button,
            address,
            back,
            forward,
            reload,
            bookmark,
            progress,
            toast_overlay,
            suppress_selection: Cell::new(false),
            user_content_manager,
            closed_tabs: RefCell::new(Vec::new()),
            next_closed_id: Cell::new(1),
            pending_downloads: RefCell::new(HashSet::new()),
            shutting_down: Cell::new(false),
            signals_script: RefCell::new(None),
            consent_script: RefCell::new(None),
            suggestion_popover,
            suggestion_list,
            suggestion_generation: Cell::new(0),
            suggestion_items: RefCell::new(Vec::new()),
            suggestion_open: Cell::new(false),
            suggestion_highlight: Cell::new(-1),
            suppress_suggestions: Cell::new(false),
            split_stack,
            content_panes,
            split_tab: RefCell::new(None),
            secondary_focused: Cell::new(false),
            private,
            pending_history: RefCell::new(Vec::new()),
            history_flush_scheduled: Cell::new(false),
        });

        browser.install_actions();
        // Prima della gestione standard dell'Invio: con una riga scelta il
        // tasto deve aprire il suggerimento, non l'indirizzo digitato.
        browser.connect_suggestions();
        browser.connect_ui_signals();
        browser.connect_window_events();
        browser.connect_downloads();
        if private {
            browser.start_private_session();
        } else {
            browser.restore_session();
        }
        browser.apply_theme();
        browser.install_extensions();
        browser.apply_network_privacy();
        browser.refresh_privacy_scripts();
        browser.refresh_content_filter();
        browser.schedule_list_update();
        browser.start_idle_timer();
        browser.start_update_timer();

        window.present();
        Ok(browser)
    }

    pub fn present(&self) {
        self.window.present();
    }

    fn install_actions(self: &Rc<Self>) {
        self.add_action("new-tab", |browser| {
            browser.new_tab(None, true, false);
        });
        self.add_action("tab-search", |browser| {
            browser.open_tab_search();
        });
        self.add_action("close-tab", |browser| {
            if let Some(id) = browser.active_tab_id() {
                browser.close_tab(id);
            }
        });
        self.add_action("duplicate-tab", |browser| {
            if let Some(id) = browser.active_tab_id() {
                browser.duplicate_tab(id);
            }
        });
        self.add_action("toggle-pin", |browser| {
            if let Some(id) = browser.active_tab_id() {
                browser.toggle_pin(id);
            }
        });
        self.add_action("next-tab", |browser| browser.cycle_tab(1));
        self.add_action("prev-tab", |browser| browser.cycle_tab(-1));
        self.add_action("back", |browser| browser.go_back());
        self.add_action("forward", |browser| browser.go_forward());
        self.add_action("reload", |browser| browser.reload_or_stop());
        self.add_action("home", |browser| browser.go_home());
        self.add_action("focus-address", |browser| {
            browser.address.grab_focus();
            browser.address.select_region(0, -1);
        });
        self.add_action("toggle-bookmark", |browser| browser.toggle_bookmark());
        self.add_action("new-workspace", |browser| browser.new_workspace());
        self.add_action("rename-workspace", |browser| browser.rename_workspace());
        self.add_action("delete-workspace", |browser| browser.delete_workspace());
        self.add_action("bookmarks", |browser| browser.open_bookmarks());
        self.add_action("history", |browser| browser.open_history());
        self.add_action("downloads", |browser| browser.open_downloads());
        self.add_action("preferences", |browser| browser.open_preferences());
        self.add_action("clear-data", |browser| {
            browser.clear_browsing_data_prompt()
        });
        self.add_action("permissions", |browser| browser.open_permissions());
        self.add_action("toggle-site-blocking", |browser| {
            match browser.current_origin() {
                Some(origin) => browser.toggle_site_blocking(&origin),
                None => browser.toast("Nessun sito valido in questa scheda"),
            }
        });
        self.add_action("about", |browser| browser.open_about());
        self.add_action("toggle-sidebar", |browser| {
            let show = !browser.split_view.shows_sidebar();
            browser.split_view.set_show_sidebar(show);
        });
        self.add_action("close", |browser| browser.window.close());
        self.add_action("quit", |browser| browser.app.quit());
        self.add_action("reopen-closed-tab", |browser| {
            browser.reopen_closed_tab(None)
        });
        self.add_action("closed-tabs", |browser| browser.open_closed_tabs());
        self.add_action("print", |browser| browser.print_active());
        self.add_action("reader-mode", |browser| browser.toggle_reader());
        self.add_action("zoom-in", |browser| browser.change_zoom(0.1));
        self.add_action("zoom-out", |browser| browser.change_zoom(-0.1));
        self.add_action("zoom-reset", |browser| browser.reset_zoom());
        self.add_action("save-session", |browser| browser.save_named_session_prompt());
        self.add_action("open-session", |browser| browser.open_sessions());
        self.add_action("extensions", |browser| browser.open_extensions());
        self.add_action("private-window", |browser| browser.spawn_private_window());
        self.add_action("import-data", |browser| browser.import_data());
        self.add_action("credentials", |browser| browser.open_credentials());
        self.add_action("toggle-split", |browser| browser.toggle_split());
        self.add_action("swap-split", |browser| browser.swap_split());

        let switch_action =
            gio::SimpleAction::new("switch-workspace", Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(self);
        switch_action.connect_activate(move |_, parameter| {
            let Some(parameter) = parameter else {
                return;
            };
            let Some(workspace_id) = parameter
                .get::<String>()
                .and_then(|value| value.parse::<WorkspaceId>().ok())
            else {
                return;
            };
            if let Some(browser) = weak.upgrade() {
                browser.switch_workspace(workspace_id);
            }
        });
        self.window.add_action(&switch_action);

        let accelerators: &[(&str, &[&str])] = &[
            ("win.new-tab", &["<Control>t"]),
            ("win.close-tab", &["<Control>w"]),
            ("win.toggle-pin", &["<Control><Shift>p"]),
            ("win.next-tab", &["<Control>Tab"]),
            ("win.prev-tab", &["<Control><Shift>Tab"]),
            ("win.back", &["<Alt>Left"]),
            ("win.forward", &["<Alt>Right"]),
            ("win.reload", &["<Control>r", "F5"]),
            ("win.home", &["<Alt>Home"]),
            ("win.focus-address", &["<Control>l"]),
            ("win.toggle-bookmark", &["<Control>d"]),
            ("win.new-workspace", &["<Control><Shift>n"]),
            ("win.bookmarks", &["<Control><Shift>o"]),
            ("win.history", &["<Control>h"]),
            ("win.downloads", &["<Control>j"]),
            ("win.preferences", &["<Control>comma"]),
            ("win.toggle-sidebar", &["F9"]),
            ("win.close", &["<Control><Shift>w"]),
            ("win.quit", &["<Control>q"]),
            ("win.reopen-closed-tab", &["<Control><Shift>t"]),
            ("win.print", &["<Control>p"]),
            ("win.reader-mode", &["<Control><Alt>r"]),
            ("win.zoom-in", &["<Control>plus", "<Control>equal"]),
            ("win.zoom-out", &["<Control>minus"]),
            ("win.zoom-reset", &["<Control>0"]),
            ("win.tab-search", &["<Control><Shift>a"]),
            ("win.private-window", &["<Control><Alt>p"]),
            ("win.toggle-split", &["<Control><Shift>s"]),
            ("win.swap-split", &["<Control><Shift>x"]),
        ];
        for (action, accels) in accelerators {
            self.app.set_accels_for_action(action, accels);
        }
    }

    fn add_action<F>(self: &Rc<Self>, name: &str, callback: F)
    where
        F: Fn(&Rc<Browser>) + 'static,
    {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, _| {
            if let Some(browser) = weak.upgrade() {
                callback(&browser);
            }
        });
        self.window.add_action(&action);
    }

    fn connect_ui_signals(self: &Rc<Self>) {
        {
            let weak = Rc::downgrade(self);
            let button = self.privacy_button.clone();
            self.privacy_button.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    browser.show_privacy_popover(&button);
                }
            });
        }

        let weak = Rc::downgrade(self);
        self.address.connect_activate(move |entry| {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            let input = entry.text().to_string();
            if let Some(url) = browser.resolve_address_input(&input) {
                browser.navigate_active(&url);
            }
        });

        // Tasto destro sullo spazio libero della barra schede: nuova scheda.
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        let weak = Rc::downgrade(self);
        let tab_list = self.tab_list.clone();
        gesture.connect_pressed(move |_, _, x, y| {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            if tab_list.row_at_y(y as i32).is_some() {
                return;
            }
            browser.show_new_tab_menu(&tab_list, x, y);
        });
        self.tab_list.add_controller(gesture);

        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_enter(move |_| {
            if let Some(browser) = weak.upgrade() {
                browser.address.select_region(0, -1);
            }
        });
        self.address.add_controller(focus);

        let weak = Rc::downgrade(self);
        self.tab_list.connect_row_selected(move |_, row| {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            if browser.suppress_selection.get() {
                return;
            }
            let Some(row) = row else {
                return;
            };
            if let Ok(tab_id) = row.widget_name().parse::<TabId>() {
                browser.activate_tab(tab_id);
            }
        });
    }

    fn connect_window_events(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            if let Some(browser) = weak.upgrade() {
                // Prima il salvataggio, poi la distruzione dei webview: senza
                // questo flag i callback di chiusura risalverebbero una
                // sessione sempre più vuota.
                browser.shutting_down.set(true);
                // Le visite in sospeso non devono andare perse.
                browser.flush_history();
                browser.save_session();
                // Una finestra privata chiusa non deve restare viva nell'elenco
                // delle finestre extra: si libera dopo la chiusura.
                if browser.private {
                    let keep_alive = browser.clone();
                    glib::idle_add_local_once(move || {
                        EXTRA_WINDOWS.with(|windows| {
                            windows
                                .borrow_mut()
                                .retain(|other| !Rc::ptr_eq(other, &keep_alive));
                        });
                    });
                }
            }
            glib::Propagation::Proceed
        });
    }

    fn connect_downloads(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.network_session
            .connect_download_started(move |_, download| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };

                let uri = download
                    .request()
                    .and_then(|request| request.uri())
                    .map(|uri| uri.to_string())
                    .unwrap_or_default();

                // Il nome suggerito (Content-Disposition) esiste solo qui: in
                // `download-started` la risposta non è ancora pronta e il file
                // finirebbe chiamato "download".
                browser
                    .pending_downloads
                    .borrow_mut()
                    .insert(uri.clone());

                let weak_decide = weak.clone();
                let decide_uri = uri.clone();
                download.connect_decide_destination(move |download, suggested| {
                    let Some(browser) = weak_decide.upgrade() else {
                        return false;
                    };

                    let filename = download_filename(suggested, &decide_uri);
                    let directory = browser.paths.download_dir();
                    let destination = unique_download_path(&directory, &filename);
                    let destination_string = destination.to_string_lossy().to_string();
                    // Il nome definitivo è quello del file su disco (può differire
                    // dal nome suggerito quando esisteva già un file omonimo).
                    let stored_name = destination
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| name.to_owned())
                        .unwrap_or_else(|| filename.clone());

                    download.set_allow_overwrite(false);
                    download.set_destination(&destination_string);

                    // Una finestra privata scarica il file ma non lascia traccia
                    // nel database condiviso.
                    if !browser.private {
                        if let Err(error) = browser.storage.add_download(
                            &decide_uri,
                            &stored_name,
                            &destination_string,
                            "in_progress",
                        ) {
                            eprintln!("cannot record download: {error}");
                        }
                    }

                    let weak_finished = Rc::downgrade(&browser);
                    let finished_name = stored_name.clone();
                    let finished_destination = destination_string.clone();
                    let finished_uri = decide_uri.clone();
                    download.connect_finished(move |_| {
                        if let Some(browser) = weak_finished.upgrade() {
                            browser.pending_downloads.borrow_mut().remove(&finished_uri);
                            if browser.private {
                                browser.toast(&format!("Download completato: {finished_name}"));
                            } else {
                                match browser
                                    .storage
                                    .set_download_status(&finished_destination, "completed")
                                {
                                    Ok(()) => browser
                                        .toast(&format!("Download completato: {finished_name}")),
                                    Err(error) => browser.toast(&format!(
                                        "Download salvato, ma non registrato: {error}"
                                    )),
                                }
                            }
                        }
                    });

                    let weak_failed = Rc::downgrade(&browser);
                    let failed_name = stored_name.clone();
                    let failed_destination = destination_string.clone();
                    let failed_uri = decide_uri.clone();
                    download.connect_failed(move |_, error| {
                        if let Some(browser) = weak_failed.upgrade() {
                            browser.pending_downloads.borrow_mut().remove(&failed_uri);
                            if !browser.private {
                                let _ = browser
                                    .storage
                                    .set_download_status(&failed_destination, "failed");
                            }
                            browser.toast(&format!(
                                "Download non riuscito ({failed_name}): {error}"
                            ));
                        }
                    });

                    browser.toast(&format!("Download avviato: {stored_name}"));
                    true
                });
            });
    }

    fn restore_session(self: &Rc<Self>) {
        let saved = load_json::<Session>(&self.paths.session)
            .filter(|session| !session.workspaces.is_empty())
            .unwrap_or_else(|| Session::new("Principale"));

        self.apply_session(saved);
    }

    /// Rimuove tutte le schede correnti e le loro web view.
    fn clear_tabs(&self) {
        // Prima lo split, mentre le schede esistono ancora: così la scheda di
        // destra viene sganciata e non resta attaccata a un contenitore dopo
        // che la sua WebView è stata distrutta.
        self.take_split(false);
        let webviews = {
            let mut state = self.state.borrow_mut();
            state
                .tabs
                .drain()
                .filter_map(|(_, tab)| tab.webview)
                .collect::<Vec<_>>()
        };
        for webview in webviews {
            self.stack.remove(&webview);
            self.split_stack.remove(&webview);
        }
        self.tab_widgets.borrow_mut().clear();
    }

    /// Sostituisce la sessione corrente con quella indicata.
    fn apply_session(self: &Rc<Self>, saved: Session) {
        self.clear_tabs();

        let mut next_tab_id: TabId = 1;
        let mut next_workspace_id: WorkspaceId = 1;
        let mut next_group_id: GroupId = 1;
        let mut workspaces = Vec::new();
        let mut tabs = HashMap::new();
        let mut active_workspace_index = saved.active_workspace;
        let mut workspace_index = 0usize;

        for saved_workspace in saved.workspaces {
            let workspace_id = next_workspace_id;
            next_workspace_id += 1;
            // Profilo dati dello spazio. Le sessioni salvate prima
            // dell'isolamento non lo hanno: il primo spazio continua a usare il
            // profilo storico (nessun cookie perso), gli altri ne ricevono uno
            // nuovo, stabile fra i riavvii perché viene salvato in session.json.
            let profile = if saved_workspace.profile.trim().is_empty() {
                if workspace_index == 0 {
                    DEFAULT_PROFILE.to_owned()
                } else {
                    new_profile_id()
                }
            } else {
                saved_workspace.profile.clone()
            };
            workspace_index += 1;
            let mut tab_ids = Vec::new();

            let mut groups = Vec::new();
            for mut group in saved_workspace.groups {
                if !GROUP_COLORS.contains(&group.color.as_str()) {
                    group.color = GROUP_COLORS[0].to_owned();
                }
                next_group_id = next_group_id.max(group.id + 1);
                groups.push(group);
            }

            for saved_tab in saved_workspace.tabs {
                let tab_id = next_tab_id;
                next_tab_id += 1;
                let is_start_page = saved_tab.uri.is_empty();
                let group = saved_tab
                    .group
                    .filter(|id| groups.iter().any(|group| group.id == *id));
                let tab = Tab {
                    id: tab_id,
                    workspace_id,
                    webview: None,
                    title: if saved_tab.title.is_empty() {
                        START_PAGE_TITLE.to_owned()
                    } else {
                        saved_tab.title
                    },
                    uri: if is_start_page {
                        String::new()
                    } else {
                        saved_tab.uri.clone()
                    },
                    pinned: saved_tab.pinned,
                    loading: false,
                    progress: 0.0,
                    pending_uri: if is_start_page {
                        None
                    } else {
                        Some(saved_tab.uri)
                    },
                    is_start_page,
                    load_failed: false,
                    favicon: None,
                    group,
                    reader: false,
                    last_used: Instant::now(),
                };
                tabs.insert(tab_id, tab);
                tab_ids.push(tab_id);
            }

            if tab_ids.is_empty() {
                let tab_id = next_tab_id;
                next_tab_id += 1;
                tabs.insert(
                    tab_id,
                    Tab {
                        id: tab_id,
                        workspace_id,
                        webview: None,
                        title: START_PAGE_TITLE.to_owned(),
                        uri: String::new(),
                        pinned: false,
                        loading: false,
                        progress: 0.0,
                        pending_uri: None,
                        is_start_page: true,
                        load_failed: false,
                        favicon: None,
                        group: None,
                        reader: false,
                        last_used: Instant::now(),
                    },
                );
                tab_ids.push(tab_id);
            }

            let active_index = saved_workspace.active_tab.min(tab_ids.len() - 1);
            workspaces.push(Workspace {
                id: workspace_id,
                name: saved_workspace.name,
                active_tab: Some(tab_ids[active_index]),
                tabs: tab_ids,
                groups,
                profile,
            });
        }

        if workspaces.is_empty() {
            workspaces.push(Workspace {
                id: next_workspace_id,
                name: "Principale".to_owned(),
                tabs: Vec::new(),
                active_tab: None,
                groups: Vec::new(),
                profile: DEFAULT_PROFILE.to_owned(),
            });
            next_workspace_id += 1;
        }

        if active_workspace_index >= workspaces.len() {
            active_workspace_index = 0;
        }
        let active_workspace = workspaces[active_workspace_index].id;

        {
            let mut state = self.state.borrow_mut();
            state.workspaces = workspaces;
            state.tabs = tabs;
            state.active_workspace = active_workspace;
            state.next_tab_id = next_tab_id;
            state.next_workspace_id = next_workspace_id;
            state.next_group_id = next_group_id;
        }

        self.rebuild_workspace_menu();
        self.rebuild_tab_list();

        let active_tab = self.state.borrow().active_workspace().active_tab;
        if let Some(tab_id) = active_tab {
            self.activate_tab(tab_id);
        } else {
            self.new_tab(None, true, false);
        }
        self.save_session();
    }

    /// Scheda su cui agiscono barra indirizzi e comandi: quella del riquadro
    /// che ha il fuoco (destra nella vista affiancata, altrimenti la scheda
    /// attiva dello spazio).
    fn active_tab_id(&self) -> Option<TabId> {
        if let Some(split) = *self.split_tab.borrow() {
            if self.secondary_focused.get() {
                return Some(split);
            }
        }
        self.state.borrow().active_tab_id()
    }

    fn search_template(&self) -> String {
        self.state.borrow().search_template()
    }

    /// Sessione di rete di uno spazio: il profilo `default` usa la sessione
    /// storica, ogni altro profilo ha cartelle WebKit dedicate (cookie,
    /// localStorage, credenziali e cache separati).
    fn session_for(&self, profile: &str) -> NetworkSession {
        if profile.trim().is_empty() || profile == DEFAULT_PROFILE {
            return self.network_session.clone();
        }
        if let Some(session) = self.workspace_sessions.borrow().get(profile) {
            return session.clone();
        }
        let session = if self.private {
            // Isolamento senza persistenza: ogni spazio di una finestra privata
            // ha comunque una propria sessione, ma in memoria.
            create_private_session()
        } else {
            create_network_session(&self.paths, profile)
        };
        let block_third_party = self.state.borrow().settings.block_third_party_cookies;
        apply_session_privacy(&session, block_third_party);
        self.workspace_sessions
            .borrow_mut()
            .insert(profile.to_owned(), session.clone());
        session
    }

    /// Titolo della finestra, con la marca «privata» quando serve.
    fn window_title(&self, title: &str) -> String {
        if self.private {
            format!("{title} — {APP_NAME} (privata)")
        } else {
            format!("{title} — {APP_NAME}")
        }
    }

    // --------------------------------------------------------- Finestra privata

    /// Prima sessione di una finestra privata: nessun dato letto dal disco.
    fn start_private_session(self: &Rc<Self>) {
        self.apply_session(Session::new("Privata"));
    }

    /// Apre una finestra privata e la tiene viva finché il processo esiste:
    /// GTK non possiede l'`Rc`, che altrimenti verrebbe liberato subito.
    fn spawn_private_window(self: &Rc<Self>) {
        match Self::open_private(&self.app) {
            Ok(window) => EXTRA_WINDOWS.with(|windows| windows.borrow_mut().push(window)),
            Err(message) => self.toast(&format!("Finestra privata non aperta: {message}")),
        }
    }

    // ------------------------------------------------------------- Cronologia

    /// Accoda una visita: viene scritta in blocco dopo due secondi, così una
    /// raffica di caricamenti costa una sola transazione. Le finestre private
    /// non registrano nulla.
    fn record_visit(self: &Rc<Self>, url: &str, title: &str) {
        if self.private || !is_history_url(url) {
            return;
        }
        self.pending_history
            .borrow_mut()
            .push((url.to_owned(), title.to_owned()));
        if self.history_flush_scheduled.get() {
            return;
        }
        self.history_flush_scheduled.set(true);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(2), move || {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            browser.history_flush_scheduled.set(false);
            browser.flush_history();
        });
    }

    /// Scrive le visite in sospeso in un'unica transazione.
    fn flush_history(&self) {
        let visits = self
            .pending_history
            .borrow_mut()
            .drain(..)
            .collect::<Vec<_>>();
        if visits.is_empty() {
            return;
        }
        if let Err(error) = self.storage.add_history_many(&visits) {
            eprintln!("cannot record history: {error}");
        }
    }

    // ------------------------------------------------------ Schede inattive

    /// Controlla ogni minuto se ci sono schede da alleggerire.
    fn start_idle_timer(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(60, move || {
            let Some(browser) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            browser.unload_idle_tabs();
            glib::ControlFlow::Continue
        });
    }

    /// Libera le WebView delle schede non visibili da più di dieci minuti.
    /// Titolo e URL restano; la scheda si ricarica alla prima apertura (il
    /// testo non inviato nei moduli va perso: per questo l'opzione è spenta di
    /// default).
    fn unload_idle_tabs(self: &Rc<Self>) {
        if !self.state.borrow().settings.unload_inactive_tabs {
            return;
        }
        let active = self.state.borrow().active_tab_id();
        let split = *self.split_tab.borrow();
        let victims = {
            let state = self.state.borrow();
            state
                .tabs
                .iter()
                .filter(|(id, tab)| {
                    tab.webview.is_some()
                        && Some(**id) != active
                        && Some(**id) != split
                        && !tab.loading
                        && tab.last_used.elapsed() >= Duration::from_secs(600)
                })
                .filter_map(|(id, tab)| tab.webview.clone().map(|webview| (*id, webview)))
                .collect::<Vec<_>>()
        };
        for (tab_id, webview) in victims {
            self.stack.remove(&webview);
            self.split_stack.remove(&webview);
            let mut state = self.state.borrow_mut();
            if let Some(tab) = state.tab_mut(tab_id) {
                tab.webview = None;
                tab.pending_uri = Some(tab.uri.clone());
                tab.loading = false;
                tab.progress = 0.0;
            }
        }
    }

    // ---------------------------------------------------------- Aggiornamenti

    /// Primo controllo poco dopo l'avvio; la verifica giornaliera avviene poi
    /// all'apertura successiva.
    fn start_update_timer(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(20, move || {
            if let Some(browser) = weak.upgrade() {
                browser.maybe_check_updates(false);
            }
            glib::ControlFlow::Break
        });
    }

    fn maybe_check_updates(self: &Rc<Self>, manual: bool) {
        let (enabled, url, last) = {
            let state = self.state.borrow();
            (
                state.settings.check_updates,
                state.settings.update_url.trim().to_owned(),
                state.settings.last_update_check,
            )
        };
        if !manual {
            if !enabled || self.private {
                return;
            }
            if unix_seconds() - last < 86_400 {
                return;
            }
        }
        if url.is_empty() {
            if manual {
                self.toast("Imposta l'indirizzo del manifest nelle Preferenze");
            }
            return;
        }
        if !url.starts_with("https://") {
            self.toast("Il manifest degli aggiornamenti deve usare HTTPS");
            return;
        }
        if manual {
            self.toast("Controllo degli aggiornamenti…");
        }

        let Ok(message) = soup::Message::new("GET", &url) else {
            self.toast("Indirizzo del manifest non valido");
            return;
        };
        let session = soup::Session::new();
        let weak = Rc::downgrade(self);
        session.send_and_read_async(
            &message,
            glib::Priority::DEFAULT,
            None::<&gio::Cancellable>,
            move |result| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let bytes = match result {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        browser.toast(&format!("Controllo aggiornamenti non riuscito: {error}"));
                        return;
                    }
                };
                match crate::update::parse_manifest(&String::from_utf8_lossy(bytes.as_ref())) {
                    Ok(manifest) => browser.offer_update(manifest),
                    Err(error) => browser.toast(&format!("{error:#}")),
                }
            },
        );
    }

    fn offer_update(self: &Rc<Self>, manifest: crate::update::Manifest) {
        {
            let mut state = self.state.borrow_mut();
            state.settings.last_update_check = unix_seconds();
        }
        self.save_settings();

        if !crate::update::is_newer(env!("CARGO_PKG_VERSION"), &manifest.version) {
            self.toast(&format!(
                "Peats è aggiornato ({})",
                env!("CARGO_PKG_VERSION")
            ));
            return;
        }
        if !manifest.url.starts_with("https://") {
            self.toast("Il manifest non indica un indirizzo HTTPS per il pacchetto");
            return;
        }

        let mut body = String::from(
            "Scaricare e installare l'aggiornamento adesso? Il pacchetto viene controllato \
             con SHA-256 prima dell'installazione.",
        );
        if !manifest.notes.trim().is_empty() {
            body.push_str("\n\n");
            body.push_str(manifest.notes.trim());
        }

        let dialog = adw::AlertDialog::new(
            Some(&format!("Peats {} è disponibile", manifest.version)),
            Some(&body),
        );
        dialog.add_response("later", "Più tardi");
        dialog.add_response("install", "Installa");
        dialog.set_default_response(Some("install"));
        dialog.set_close_response("later");

        let manifest = manifest.clone();
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |dialog, response| {
            dialog.close();
            if response != "install" {
                return;
            }
            let Some(browser) = weak.upgrade() else {
                return;
            };
            browser.download_update(manifest.clone());
        });
        dialog.present(Some(&self.window));
    }

    fn download_update(self: &Rc<Self>, manifest: crate::update::Manifest) {
        let Ok(message) = soup::Message::new("GET", &manifest.url) else {
            self.toast("Indirizzo del pacchetto non valido");
            return;
        };
        let session = soup::Session::new();
        let weak = Rc::downgrade(self);
        session.send_and_read_async(
            &message,
            glib::Priority::DEFAULT,
            None::<&gio::Cancellable>,
            move |result| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let bytes = match result {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        browser.toast(&format!(
                            "Download dell'aggiornamento non riuscito: {error}"
                        ));
                        return;
                    }
                };
                browser.install_update(&manifest, bytes.as_ref());
            },
        );
    }

    fn install_update(self: &Rc<Self>, manifest: &crate::update::Manifest, bytes: &[u8]) {
        let expected = manifest.sha256.trim().to_ascii_lowercase();
        if expected.is_empty() {
            self.toast("Il manifest non contiene lo SHA-256: aggiornamento annullato");
            return;
        }
        if crate::update::sha256_hex(bytes) != expected {
            self.toast("Il pacchetto non corrisponde allo SHA-256 del manifest");
            return;
        }
        if !crate::update::managed_install() {
            self.toast("Build di sviluppo: nessuna installazione automatica");
            return;
        }

        let directory = self.paths.cache_dir.join("updates");
        if let Err(error) = fs::create_dir_all(&directory) {
            self.toast(&format!("Cartella degli aggiornamenti non creata: {error}"));
            return;
        }
        let file_name = manifest
            .url
            .rsplit('/')
            .next()
            .unwrap_or("peats-update.rpm")
            .split('?')
            .next()
            .unwrap_or("peats-update.rpm");
        let file_name = if file_name.is_empty() {
            "peats-update.rpm"
        } else {
            file_name
        };
        let destination = directory.join(file_name);
        if let Err(error) = fs::write(&destination, bytes) {
            self.toast(&format!("Pacchetto non salvato: {error}"));
            return;
        }
        match crate::update::install_command(&destination).spawn() {
            Ok(_) => self.toast("Autorizza l'installazione nella finestra di sistema"),
            Err(error) => self.toast(&format!("Installazione non avviata: {error}")),
        }
    }

    // ------------------------------------------------------------ Suggerimenti

    /// Schede aperte nello spazio corrente, come candidati per i suggerimenti.
    fn tab_candidates(&self) -> Vec<Candidate> {
        let state = self.state.borrow();
        state
            .active_workspace()
            .tabs
            .iter()
            .filter_map(|tab_id| state.tab(*tab_id))
            .filter(|tab| !tab.is_start_page)
            .map(|tab| Candidate::new(tab.title.clone(), tab.uri.clone()))
            .collect()
    }

    /// Ricalcola i suggerimenti per il testo digitato.
    ///
    /// I risultati locali (schede, segnalibri, cronologia) compaiono subito;
    /// quelli di DuckDuckGo arrivano dopo 250 ms di pausa e solo se il testo
    /// non è cambiato nel frattempo.
    fn update_suggestions(self: &Rc<Self>, text: &str) {
        let generation = self.suggestion_generation.get() + 1;
        self.suggestion_generation.set(generation);

        let query = text.trim().to_owned();
        if query.is_empty() {
            self.close_suggestions();
            return;
        }

        let (engine_name, engine_template, remote_enabled) = {
            let state = self.state.borrow();
            let engine = state.settings.default_engine();
            (
                engine.name.clone(),
                engine.template.clone(),
                state.settings.search_suggestions,
            )
        };

        let local = self.local_suggestions(&query, &[], &engine_name, &engine_template);
        self.show_suggestions(local);

        // Un indirizzo non merita una richiesta al servizio di ricerca.
        if !remote_enabled || suggest::is_probably_url(&query) {
            return;
        }

        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            if browser.suggestion_generation.get() != generation {
                return;
            }
            browser.fetch_remote_suggestions(&query, generation, &engine_name, &engine_template);
        });
    }

    /// Elenco costruito senza rete: schede, segnalibri, cronologia e la voce
    /// «Cerca …» che porta al motore predefinito.
    fn local_suggestions(
        &self,
        query: &str,
        remote: &[String],
        engine_name: &str,
        engine_template: &str,
    ) -> Vec<Suggestion> {
        let tabs = self.tab_candidates();
        let bookmarks = self
            .storage
            .search_bookmarks(query, 5)
            .unwrap_or_default()
            .into_iter()
            .map(|bookmark| Candidate::new(bookmark.title, bookmark.url))
            .collect::<Vec<_>>();
        let history = self
            .storage
            .search_history(query, 5)
            .unwrap_or_default()
            .into_iter()
            .map(|entry| Candidate::new(entry.title, entry.url))
            .collect::<Vec<_>>();

        suggest::build(
            query,
            &tabs,
            &bookmarks,
            &history,
            remote,
            EngineSpec {
                name: engine_name,
                template: engine_template,
            },
        )
    }

    /// Interroga l'autocompletamento di DuckDuckGo con una sessione HTTP
    /// dedicata: nessun cookie del browser viene inviato e non c'è referrer.
    fn fetch_remote_suggestions(
        self: &Rc<Self>,
        query: &str,
        generation: u64,
        engine_name: &str,
        engine_template: &str,
    ) {
        let Ok(message) = soup::Message::new("GET", &suggest::ddg_url(query)) else {
            return;
        };
        let session = soup::Session::new();
        let weak = Rc::downgrade(self);
        let query = query.to_owned();
        let engine_name = engine_name.to_owned();
        let engine_template = engine_template.to_owned();
        session.send_and_read_async(
            &message,
            glib::Priority::DEFAULT,
            None::<&gio::Cancellable>,
            move |result| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                // Risposta troppo vecchia: il testo è cambiato.
                if browser.suggestion_generation.get() != generation {
                    return;
                }
                let Ok(bytes) = result else {
                    return;
                };
                let phrases = suggest::parse_ddg(&String::from_utf8_lossy(bytes.as_ref()));
                if phrases.is_empty() {
                    return;
                }
                let list = browser.local_suggestions(
                    &query,
                    &phrases,
                    &engine_name,
                    &engine_template,
                );
                browser.show_suggestions(list);
            },
        );
    }

    /// Disegna le righe del menu e lo apre se serve.
    fn show_suggestions(&self, items: Vec<Suggestion>) {
        // Suggerimenti solo mentre si scrive nella barra: un popover aperto
        // quando il fuoco è altrove disturba la pagina e ruba i tasti.
        if !self.address.has_focus() {
            self.close_suggestions();
            return;
        }
        while let Some(child) = self.suggestion_list.first_child() {
            self.suggestion_list.remove(&child);
        }
        self.suggestion_highlight.set(-1);
        if items.is_empty() {
            self.close_suggestions();
            return;
        }

        for item in &items {
            let row = gtk::ListBoxRow::new();
            row.set_activatable(true);

            let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            content.set_margin_top(6);
            content.set_margin_bottom(6);
            content.set_margin_start(8);
            content.set_margin_end(8);

            let icon = gtk::Image::from_icon_name(item.kind.icon());
            icon.add_css_class("suggestion-icon");
            content.append(&icon);

            let labels = gtk::Box::new(gtk::Orientation::Vertical, 1);
            labels.set_hexpand(true);
            let title = gtk::Label::new(Some(&item.title));
            title.set_xalign(0.0);
            title.set_ellipsize(pango::EllipsizeMode::End);
            title.add_css_class("suggestion-title");
            let subtitle = gtk::Label::new(Some(&item.subtitle));
            subtitle.set_xalign(0.0);
            subtitle.set_ellipsize(pango::EllipsizeMode::Middle);
            subtitle.add_css_class("suggestion-subtitle");
            labels.append(&title);
            labels.append(&subtitle);
            content.append(&labels);

            let kind = gtk::Label::new(Some(item.kind.label()));
            kind.add_css_class("suggestion-kind");
            content.append(&kind);

            row.set_child(Some(&content));
            row.set_can_focus(false);
            self.suggestion_list.append(&row);
        }

        *self.suggestion_items.borrow_mut() = items;
        if !self.suggestion_open.get() {
            self.suggestion_popover.popup();
            self.suggestion_open.set(true);
        }
    }

    fn close_suggestions(&self) {
        // Invalida le risposte remote già in viaggio: senza questo una risposta
        // partita prima della scelta riaprirebbe il popover da sola.
        self.suggestion_generation
            .set(self.suggestion_generation.get() + 1);
        *self.suggestion_items.borrow_mut() = Vec::new();
        self.suggestion_highlight.set(-1);
        if self.suggestion_open.get() {
            self.suggestion_popover.popdown();
            self.suggestion_open.set(false);
        }
    }

    /// Sposta l'evidenza fra i suggerimenti senza spostare il fuoco: il testo
    /// continua a essere scritto nella barra indirizzi.
    fn move_highlight(&self, delta: i32) {
        let count = self.suggestion_items.borrow().len() as i32;
        if count == 0 {
            return;
        }
        let current = self.suggestion_highlight.get();
        let target = if current < 0 {
            if delta > 0 {
                0
            } else {
                count - 1
            }
        } else {
            (current + delta).clamp(0, count - 1)
        };
        self.set_highlight(target);
    }

    /// Evidenzia la riga `index` con la classe CSS usata dal tema; `-1` toglie
    /// ogni evidenza.
    fn set_highlight(&self, index: i32) {
        let mut position = 0;
        while let Some(row) = self.suggestion_list.row_at_index(position) {
            if position == index {
                row.add_css_class("suggestion-active");
            } else {
                row.remove_css_class("suggestion-active");
            }
            position += 1;
        }
        self.suggestion_highlight.set(index);
    }

    /// Apre la riga evidenziata, se ce n'è una (scelta da tastiera).
    fn apply_highlight(self: &Rc<Self>) {
        let index = self.suggestion_highlight.get();
        if index >= 0 {
            self.apply_suggestion(index, true);
        }
    }

    /// Apre il suggerimento scelto: una scheda già aperta passa in primo piano,
    /// un indirizzo viene caricato nel riquadro attivo. `keyboard` distingue
    /// Invio dal clic del mouse e decide dove torna il fuoco.
    fn apply_suggestion(self: &Rc<Self>, index: i32, keyboard: bool) {
        let suggestion = self
            .suggestion_items
            .borrow()
            .get(usize::try_from(index).unwrap_or(usize::MAX))
            .cloned();
        let Some(suggestion) = suggestion else {
            return;
        };
        self.close_suggestions();

        if suggestion.kind == SuggestionKind::Tab {
            if let Some(tab_id) = self.tab_for_uri(&suggestion.uri) {
                self.activate_tab(tab_id);
                self.focus_after_suggestion(keyboard);
                return;
            }
        }
        self.navigate_active(&suggestion.uri);
        self.focus_after_suggestion(keyboard);
    }

    /// Dopo la scelta il popover è chiuso e il fuoco non deve restarne prigione:
    /// la tastiera lo riporta nella barra indirizzi (già aggiornata), il mouse
    /// lo passa alla pagina, così si può subito scorrere o digitare.
    fn focus_after_suggestion(&self, keyboard: bool) {
        if keyboard {
            let text = {
                let state = self.state.borrow();
                state
                    .active_tab_id()
                    .and_then(|tab_id| state.tab(tab_id))
                    .map(|tab| {
                        if tab.is_start_page {
                            String::new()
                        } else {
                            tab.uri.clone()
                        }
                    })
                    .unwrap_or_default()
            };
            // Testo impostato dal browser: non è una digitazione, quindi non
            // deve far riapparire l'elenco appena chiuso.
            self.suppress_suggestions.set(true);
            self.address.set_text(&text);
            self.address.set_position(-1);
            self.suppress_suggestions.set(false);
            self.address.grab_focus();
        } else if let Some(webview) = self.active_webview() {
            webview.grab_focus();
        } else {
            // Pagine interne senza WebView: meglio la barra che un fuoco perso.
            self.address.grab_focus();
        }
    }

    fn tab_for_uri(&self, uri: &str) -> Option<TabId> {
        let state = self.state.borrow();
        state
            .active_workspace()
            .tabs
            .iter()
            .copied()
            .find(|tab_id| {
                state
                    .tab(*tab_id)
                    .is_some_and(|tab| tab.uri == uri && !tab.is_start_page)
            })
    }

    /// Collega barra indirizzi, elenco e tastiera.
    fn connect_suggestions(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.address.connect_changed(move |entry| {
            if let Some(browser) = weak.upgrade() {
                if browser.suppress_suggestions.get() {
                    return;
                }
                browser.update_suggestions(&entry.text());
            }
        });

        // Invio: apre la riga evidenziata, altrimenti lascia fare alla
        // navigazione normale chiudendo solo l'elenco. Il fuoco resta nella
        // barra, quindi le lettere non finiscono mai nella lista.
        let weak = Rc::downgrade(self);
        self.address.connect_activate(move |entry| {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            if !browser.suggestion_open.get() {
                return;
            }
            if browser.suggestion_highlight.get() >= 0 {
                glib::signal::signal_stop_emission_by_name(entry, "activate");
                browser.apply_highlight();
            } else {
                browser.close_suggestions();
            }
        });

        let weak = Rc::downgrade(self);
        self.suggestion_list.connect_row_activated(move |_, row| {
            if let Some(browser) = weak.upgrade() {
                // Clic del mouse: il fuoco va alla pagina, non alla barra.
                browser.apply_suggestion(row.index(), false);
            }
        });

        // Frecce e Esc nella barra indirizzi.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(browser) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Down | gdk::Key::Up => {
                    // L'elenco va interrogato senza tenere il prestito aperto,
                    // altrimenti `move_highlight` lo riprende e va in panico.
                    let empty = browser.suggestion_items.borrow().is_empty();
                    if empty {
                        return glib::Propagation::Proceed;
                    }
                    browser.move_highlight(if key == gdk::Key::Down { 1 } else { -1 });
                    glib::Propagation::Stop
                }
                gdk::Key::Escape => {
                    if !browser.suggestion_open.get() {
                        return glib::Propagation::Proceed;
                    }
                    browser.close_suggestions();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.address.add_controller(keys);
    }

    // --------------------------------------------------------- Vista affiancata

    /// Attiva la vista affiancata con un'altra scheda dello spazio corrente,
    /// oppure la disattiva se è già attiva.
    fn toggle_split(self: &Rc<Self>) {
        if self.split_tab.borrow().is_some() {
            self.close_split();
            self.toast("Vista affiancata disattivata");
            return;
        }

        let active = self.state.borrow().active_tab_id();
        let candidate = {
            let state = self.state.borrow();
            state
                .active_workspace()
                .tabs
                .iter()
                .copied()
                .find(|tab_id| Some(*tab_id) != active)
        };
        let Some(candidate) = candidate else {
            self.toast("Serve un'altra scheda in questo spazio");
            return;
        };
        self.show_split(candidate);
        self.toast("Vista affiancata attiva");
    }

    /// Mostra una scheda nel riquadro di destra.
    fn show_split(self: &Rc<Self>, tab_id: TabId) {
        let Some(webview) = self.ensure_webview(tab_id) else {
            return;
        };
        let name = format!("tab-{tab_id}");
        // Una WebView vive in un solo contenitore: la sposta nel riquadro
        // destro (GTK la rimuove dal precedente).
        if self.stack.child_by_name(&name).is_some() {
            self.stack.remove(&webview);
        }
        self.split_stack.add_named(&webview, Some(&name));
        self.split_stack.set_visible_child_name(&name);
        self.split_stack.set_visible(true);
        // Diviso a metà della larghezza disponibile, con un limite minimo.
        let width = self.content_panes.width();
        if width > 400 {
            self.content_panes.set_position(width / 2);
        }
        *self.split_tab.borrow_mut() = Some(tab_id);
        self.secondary_focused.set(false);
        // Lo split non fa parte della sessione salvata: nessuna scrittura su
        // disco qui, `save_session` resta alle modifiche alle schede.
        self.sync_toolbar();
    }

    /// Chiude il riquadro di destra e restituisce la scheda che mostrava.
    ///
    /// Con `restore` la WebView torna nello stack principale perché la scheda
    /// continua a vivere; senza `restore` viene solo sganciata: la scheda è in
    /// fase di chiusura e la sua WebView deve essere distrutta, non rimessa in
    /// un contenitore (altrimenti resterebbe una pagina fantasma).
    fn take_split(&self, restore: bool) -> Option<TabId> {
        let tab_id = self.split_tab.borrow_mut().take()?;
        let webview = {
            let state = self.state.borrow();
            state.tab(tab_id).and_then(|tab| tab.webview.clone())
        };
        if let Some(webview) = webview {
            self.split_stack.remove(&webview);
            if restore {
                self.stack
                    .add_named(&webview, Some(&format!("tab-{tab_id}")));
            }
        }
        self.split_stack.set_visible(false);
        self.secondary_focused.set(false);
        Some(tab_id)
    }

    /// Chiude il riquadro di destra tenendo viva la sua scheda.
    fn close_split(self: &Rc<Self>) {
        if self.take_split(true).is_some() {
            self.sync_toolbar();
        }
    }

    /// Sgancia la scheda dal riquadro di destra prima che venga distrutta.
    fn detach_split_tab(&self, tab_id: TabId) {
        if *self.split_tab.borrow() == Some(tab_id) {
            self.take_split(false);
        }
    }

    /// Inverte i due riquadri: le WebView cambiano contenitore e la scheda
    /// attiva diventa quella che era a destra.
    fn swap_split(self: &Rc<Self>) {
        let Some(secondary) = *self.split_tab.borrow() else {
            self.toast("Vista affiancata non attiva");
            return;
        };
        let Some(primary) = self.state.borrow().active_tab_id() else {
            return;
        };
        if primary == secondary {
            // Non dovrebbe accadere: la scheda attiva è sempre quella di
            // sinistra. Se succede, lo split è incoerente e va chiuso.
            self.close_split();
            self.toast("Vista affiancata chiusa: schede coincidenti");
            return;
        }

        // Entrambe le WebView devono esistere prima di scambiare i contenitori,
        // altrimenti lo stato cambierebbe senza che le viste seguano.
        let (Some(primary_view), Some(secondary_view)) =
            (self.ensure_webview(primary), self.ensure_webview(secondary))
        else {
            self.toast("Impossibile invertire i riquadri");
            return;
        };

        let primary_name = format!("tab-{primary}");
        let secondary_name = format!("tab-{secondary}");
        // Rimuove solo le pagine che stanno davvero nel contenitore sbagliato:
        // le due viste si scambiano di posto, ognuna in un solo contenitore.
        if self.stack.child_by_name(&secondary_name).is_some() {
            self.stack.remove(&secondary_view);
        }
        if self.split_stack.child_by_name(&primary_name).is_some() {
            self.split_stack.remove(&primary_view);
        }
        self.stack.add_named(&primary_view, Some(&primary_name));
        self.split_stack
            .add_named(&secondary_view, Some(&secondary_name));
        self.stack.set_visible_child_name(&secondary_name);
        self.split_stack.set_visible_child_name(&primary_name);

        {
            let mut state = self.state.borrow_mut();
            let workspace_id = state.active_workspace;
            if let Some(workspace) = state.workspace_mut(workspace_id) {
                workspace.active_tab = Some(secondary);
            }
        }
        *self.split_tab.borrow_mut() = Some(primary);
        self.secondary_focused.set(false);

        // La selezione nella lista segue la scheda che ora è a sinistra.
        self.suppress_selection.set(true);
        if let Some(widgets) = self.tab_widgets.borrow().get(&secondary) {
            self.tab_list.select_row(Some(&widgets.row));
        }
        self.suppress_selection.set(false);

        self.sync_toolbar();
        self.save_session();
        self.toast("Riquadri invertiti");
    }

        fn new_tab(self: &Rc<Self>, uri: Option<String>, activate: bool, pinned: bool) -> TabId {
        let uri = uri.or_else(|| self.home_uri());
        self.new_tab_with_title(uri, None, activate, pinned)
    }

    /// Pagina iniziale personalizzata, quando configurata.
    fn block_ads_enabled(&self) -> bool {
        self.state.borrow().settings.block_ads
    }

    fn home_uri(&self) -> Option<String> {
        let start_page = self.state.borrow().settings.start_page.trim().to_owned();
        (!start_page.is_empty()).then_some(start_page)
    }

    fn new_tab_with_title(
        self: &Rc<Self>,
        uri: Option<String>,
        title: Option<String>,
        activate: bool,
        pinned: bool,
    ) -> TabId {
        let workspace_id = self.state.borrow().active_workspace;
        let tab_id = {
            let mut state = self.state.borrow_mut();
            let id = state.next_tab_id;
            state.next_tab_id += 1;
            id
        };

        let is_start_page = uri.is_none();
        let tab = Tab {
            id: tab_id,
            workspace_id,
            webview: None,
            title: title.unwrap_or_else(|| {
                uri.as_deref()
                    .map(title_from_uri)
                    .unwrap_or_else(|| START_PAGE_TITLE.to_owned())
            }),
            uri: uri.clone().unwrap_or_default(),
            pinned,
            loading: false,
            progress: 0.0,
            pending_uri: uri,
            is_start_page,
            load_failed: false,
            favicon: None,
            group: None,
            reader: false,
            last_used: Instant::now(),
        };

        {
            let mut state = self.state.borrow_mut();
            state.tabs.insert(tab_id, tab);
            if let Some(workspace) = state.workspace_mut(workspace_id) {
                workspace.tabs.push(tab_id);
                if activate || workspace.active_tab.is_none() {
                    workspace.active_tab = Some(tab_id);
                }
            }
        }

        self.rebuild_tab_list();
        if activate {
            self.activate_tab(tab_id);
        }
        self.save_session();
        tab_id
    }

    fn activate_tab(self: &Rc<Self>, tab_id: TabId) {
        // Lo split resta coerente con la scheda attivata: la scheda di destra
        // torna nel riquadro principale se è lei a essere attivata (uno split
        // non mostra mai due volte la stessa scheda) e lo split si chiude se la
        // scheda appartiene a un altro spazio, per non mostrare due spazi
        // diversi affiancati.
        if let Some(split_id) = *self.split_tab.borrow() {
            let (split_workspace, target_workspace) = {
                let state = self.state.borrow();
                (
                    state.tab(split_id).map(|tab| tab.workspace_id),
                    state.tab(tab_id).map(|tab| tab.workspace_id),
                )
            };
            if split_id == tab_id || split_workspace != target_workspace {
                self.close_split();
            }
        }
        self.secondary_focused.set(false);
        let (workspace_id, workspace_changed) = {
            let state = self.state.borrow();
            let Some(tab) = state.tab(tab_id) else {
                return;
            };
            (tab.workspace_id, state.active_workspace != tab.workspace_id)
        };

        self.ensure_webview(tab_id);

        {
            let mut state = self.state.borrow_mut();
            if workspace_changed {
                state.active_workspace = workspace_id;
            }
            if let Some(workspace) = state.workspace_mut(workspace_id) {
                workspace.active_tab = Some(tab_id);
            }
            if let Some(tab) = state.tab_mut(tab_id) {
                tab.last_used = Instant::now();
            }
        }

        self.stack
            .set_visible_child_name(&format!("tab-{tab_id}"));

        if workspace_changed {
            self.rebuild_workspace_menu();
            self.rebuild_tab_list();
        } else {
            self.suppress_selection.set(true);
            if let Some(widgets) = self.tab_widgets.borrow().get(&tab_id) {
                self.tab_list.select_row(Some(&widgets.row));
            }
            self.suppress_selection.set(false);
        }

        self.sync_toolbar();
        self.sync_pinned_tiles();
        self.save_session();
    }

    fn ensure_webview(self: &Rc<Self>, tab_id: TabId) -> Option<WebView> {
        if let Some(webview) = self
            .state
            .borrow()
            .tab(tab_id)
            .and_then(|tab| tab.webview.clone())
        {
            return Some(webview);
        }

        // Ogni spazio ha la propria sessione di rete, così «Lavoro» e
        // «Personale» non condividono cookie né sessioni di navigazione.
        let profile = {
            let state = self.state.borrow();
            state
                .tab(tab_id)
                .and_then(|tab| state.workspace(tab.workspace_id))
                .map(|workspace| workspace.profile.clone())
                .unwrap_or_default()
        };
        let session = self.session_for(&profile);

        let settings = webkit::Settings::new();
        settings.set_enable_developer_extras(cfg!(debug_assertions));
        settings.set_enable_javascript(true);
        settings.set_enable_media(true);
        settings.set_enable_media_stream(true);
        settings.set_enable_mediasource(true);
        settings.set_enable_webgl(true);
        settings.set_enable_page_cache(true);

        let webview = webkit::WebView::builder()
            .network_session(&session)
            .settings(&settings)
            // Senza questo il filtro contenuti e gli script (estensioni,
            // segnali di privacy, CMP) non verrebbero mai applicati.
            .user_content_manager(&self.user_content_manager)
            .build();
        webview.set_hexpand(true);
        webview.set_vexpand(true);
        webview.set_size_request(200, 120);

        // Il riquadro che riceve il fuoco comanda barra indirizzi e comandi.
        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_enter(move |_| {
            if let Some(browser) = weak.upgrade() {
                let secondary = *browser.split_tab.borrow() == Some(tab_id);
                browser.secondary_focused.set(secondary);
                browser.sync_toolbar();
            }
        });
        webview.add_controller(focus);

        self.stack
            .add_named(&webview, Some(&format!("tab-{tab_id}")));

        let weak = Rc::downgrade(self);
        webview.connect_load_changed(move |webview, event| {
            if let Some(browser) = weak.upgrade() {
                browser.on_load_changed(tab_id, webview, event);
            }
        });

        let weak = Rc::downgrade(self);
        webview.connect_title_notify(move |webview| {
            if let Some(browser) = weak.upgrade() {
                browser.on_title_changed(tab_id, webview);
            }
        });

        let weak = Rc::downgrade(self);
        webview.connect_estimated_load_progress_notify(move |webview| {
            if let Some(browser) = weak.upgrade() {
                browser.on_progress_changed(tab_id, webview);
            }
        });

        let weak = Rc::downgrade(self);
        webview.connect_favicon_notify(move |webview| {
            if let Some(browser) = weak.upgrade() {
                browser.on_favicon_changed(tab_id, webview);
            }
        });

        let weak = Rc::downgrade(self);
        webview.connect_permission_request(move |webview, request| {
            let Some(browser) = weak.upgrade() else {
                return false;
            };
            browser.handle_permission_request(webview, request);
            true
        });

        let weak = Rc::downgrade(self);
        webview.connect_load_failed(move |_, _, uri, error| {
            if let Some(browser) = weak.upgrade() {
                browser.on_load_failed(tab_id, uri, error);
            }
            false
        });

        let weak = Rc::downgrade(self);
        webview.connect_create(move |_, action| {
            if let Some(browser) = weak.upgrade() {
                let uri = action
                    .request()
                    .and_then(|request| request.uri())
                    .map(|uri| uri.to_string());

                // Niente finestre pop-up verso le reti pubblicitarie note.
                if let Some(uri) = uri.as_deref() {
                    if browser.block_ads_enabled() && blocklist::is_blocked(uri) {
                        return None;
                    }
                }

                browser.new_tab(uri, true, false);
            }
            None
        });

        let weak = Rc::downgrade(self);
        webview.connect_close(move |_| {
            if let Some(browser) = weak.upgrade() {
                if browser.shutting_down.get() {
                    return;
                }
                browser.close_tab(tab_id);
            }
        });

        let load_target = {
            let mut state = self.state.borrow_mut();
            let Some(tab) = state.tab_mut(tab_id) else {
                return None;
            };
            tab.webview = Some(webview.clone());
            if let Some(uri) = tab.pending_uri.take() {
                Some(uri)
            } else if tab.is_start_page {
                None
            } else if !tab.uri.is_empty() {
                Some(tab.uri.clone())
            } else {
                None
            }
        };

        if let Some(uri) = load_target {
            webview.load_uri(&uri);
        } else {
            self.load_start_page(&webview);
        }

        Some(webview)
    }

    fn load_start_page(&self, webview: &WebView) {
        let template = self.state.borrow().search_template();
        webview.load_html(&start_page_html(&template), Some("about:blank"));
    }

    fn navigate_active(self: &Rc<Self>, url: &str) {
        if let Some(tab_id) = self.active_tab_id() {
            self.navigate_tab(tab_id, url);
        } else {
            self.new_tab(Some(url.to_owned()), true, false);
        }
    }

    fn navigate_tab(self: &Rc<Self>, tab_id: TabId, url: &str) {
        if self.open_internal_page(url) {
            self.sync_toolbar();
            return;
        }

        let existing = self
            .state
            .borrow()
            .tab(tab_id)
            .and_then(|tab| tab.webview.clone());

        {
            let mut state = self.state.borrow_mut();
            let Some(tab) = state.tab_mut(tab_id) else {
                return;
            };
            tab.uri = url.to_owned();
            tab.is_start_page = false;
            tab.load_failed = false;
            tab.title = title_from_uri(url);
            tab.pending_uri = if existing.is_none() {
                Some(url.to_owned())
            } else {
                None
            };
        }

        if let Some(webview) = existing {
            webview.load_uri(url);
        } else {
            self.ensure_webview(tab_id);
        }

        self.update_tab_label(tab_id);
        self.sync_toolbar();
        self.save_session();
    }

    /// Torna alla pagina iniziale: quella personalizzata se impostata,
    /// altrimenti la pagina interna di Peats.
    fn go_home(self: &Rc<Self>) {
        let start_page = self.state.borrow().settings.start_page.trim().to_owned();
        let Some(tab_id) = self.active_tab_id() else {
            let uri = if start_page.is_empty() {
                None
            } else {
                Some(start_page)
            };
            self.new_tab(uri, true, false);
            return;
        };

        if !start_page.is_empty() {
            let template = self.search_template();
            let url = resolve_input(&start_page, &template).unwrap_or(start_page);
            self.navigate_tab(tab_id, &url);
            return;
        }

        {
            let mut state = self.state.borrow_mut();
            let Some(tab) = state.tab_mut(tab_id) else {
                return;
            };
            tab.is_start_page = true;
            tab.uri.clear();
            tab.title = START_PAGE_TITLE.to_owned();
            tab.load_failed = false;
        }

        if let Some(webview) = self.ensure_webview(tab_id) {
            self.load_start_page(&webview);
        }
        self.update_tab_label(tab_id);
        self.sync_toolbar();
        self.save_session();
    }

    fn go_back(&self) {
        if let Some(webview) = self.active_webview() {
            if webview.can_go_back() {
                webview.go_back();
            }
        }
    }

    fn go_forward(&self) {
        if let Some(webview) = self.active_webview() {
            if webview.can_go_forward() {
                webview.go_forward();
            }
        }
    }

    fn active_webview(&self) -> Option<WebView> {
        let id = self.active_tab_id()?;
        self.state
            .borrow()
            .tab(id)
            .and_then(|tab| tab.webview.clone())
    }

    fn reload_or_stop(self: &Rc<Self>) {
        let snapshot = {
            let state = self.state.borrow();
            let Some(tab_id) = state.active_tab_id() else {
                return;
            };
            let Some(tab) = state.tab(tab_id) else {
                return;
            };
            (
                tab_id,
                tab.loading,
                tab.is_start_page,
                tab.webview.clone(),
            )
        };

        let (tab_id, loading, is_start_page, webview) = snapshot;
        if loading {
            if let Some(webview) = webview {
                webview.stop_loading();
            }
            return;
        }

        if is_start_page {
            let webview = webview.or_else(|| self.ensure_webview(tab_id));
            if let Some(webview) = webview {
                self.load_start_page(&webview);
            }
        } else if let Some(webview) = webview {
            webview.reload();
        } else {
            self.ensure_webview(tab_id);
        }
    }

    fn on_load_changed(self: &Rc<Self>, tab_id: TabId, webview: &WebView, event: LoadEvent) {
        let uri = webview.uri().map(|uri| uri.to_string()).unwrap_or_default();
        let title = webview
            .title()
            .map(|title| title.to_string())
            .unwrap_or_default();
        let internal = uri.is_empty() || uri == "about:blank";
        let mut history_candidate = None;
        let active;

        {
            let mut state = self.state.borrow_mut();
            let Some(tab) = state.tab_mut(tab_id) else {
                return;
            };

            match event {
                LoadEvent::Started => {
                    tab.loading = true;
                    tab.progress = 0.0;
                    tab.load_failed = false;
                    if !internal {
                        tab.is_start_page = false;
                        tab.uri = uri.clone();
                    }
                }
                LoadEvent::Redirected | LoadEvent::Committed => {
                    if !internal {
                        tab.is_start_page = false;
                        tab.uri = uri.clone();
                    }
                    if !title.is_empty() && !internal {
                        tab.title = title.clone();
                    }
                }
                LoadEvent::Finished => {
                    tab.loading = false;
                    tab.progress = 1.0;
                    if internal {
                        if tab.is_start_page {
                            tab.uri.clear();
                            tab.title = START_PAGE_TITLE.to_owned();
                        }
                    } else {
                        tab.is_start_page = false;
                        tab.uri = uri.clone();
                        if !title.is_empty() {
                            tab.title = title.clone();
                        }
                        if !tab.load_failed {
                            history_candidate = Some((tab.uri.clone(), tab.title.clone()));
                        }
                    }
                }
                _ => {}
            }

            active = state.active_workspace().active_tab == Some(tab_id);
        }

        // Zoom memorizzato per origine.
        if matches!(event, LoadEvent::Committed) && !internal {
            if let Some(origin) = origin_of(&uri) {
                if let Ok(Some(zoom)) = self.storage.site_zoom(&origin) {
                    webview.set_zoom_level(zoom);
                }
            }
        }

        self.update_tab_label(tab_id);
        if active {
            self.sync_toolbar();
            let title = self
                .state
                .borrow()
                .tab(tab_id)
                .map(|tab| tab.title.clone())
                .unwrap_or_else(|| APP_NAME.to_owned());
            self.window.set_title(Some(&self.window_title(&title)));
        }

        if let Some((url, title)) = history_candidate {
            self.record_visit(&url, &title);
        }
    }

    fn on_title_changed(self: &Rc<Self>, tab_id: TabId, webview: &WebView) {
        let title = webview
            .title()
            .map(|title| title.to_string())
            .unwrap_or_default();
        let internal = webview
            .uri()
            .map(|uri| uri.is_empty() || uri == "about:blank")
            .unwrap_or(true);

        if title.is_empty() || internal {
            return;
        }

        let active = {
            let mut state = self.state.borrow_mut();
            let active = state.active_workspace().active_tab == Some(tab_id);
            if let Some(tab) = state.tab_mut(tab_id) {
                tab.title = title.clone();
            }
            active
        };

        let pinned = self
            .state
            .borrow()
            .tab(tab_id)
            .map(|tab| tab.pinned)
            .unwrap_or(false);

        self.update_tab_label(tab_id);
        if pinned {
            self.refresh_pinned_tiles();
        }
        if active {
            self.window.set_title(Some(&self.window_title(&title)));
        }
    }

    fn on_progress_changed(&self, tab_id: TabId, webview: &WebView) {
        let progress = webview.estimated_load_progress();
        let active = {
            let mut state = self.state.borrow_mut();
            let active = state.active_workspace().active_tab == Some(tab_id);
            if let Some(tab) = state.tab_mut(tab_id) {
                tab.progress = progress;
            }
            active
        };
        if active {
            self.progress.set_fraction(progress);
        }
    }

    fn on_favicon_changed(self: &Rc<Self>, tab_id: TabId, webview: &WebView) {
        let texture = webview.favicon();

        {
            let mut state = self.state.borrow_mut();
            if let Some(tab) = state.tab_mut(tab_id) {
                tab.favicon = texture.clone();
            }
        }

        let favicon = self
            .tab_widgets
            .borrow()
            .get(&tab_id)
            .map(|widgets| widgets.favicon.clone());

        if let Some(favicon) = favicon {
            if let Some(texture) = texture {
                favicon.set_icon_name(None);
                favicon.set_paintable(Some(&texture));
            } else {
                favicon.set_paintable(None::<&gdk::Texture>);
                favicon.set_icon_name(Some("text-html-symbolic"));
            }
            favicon.set_pixel_size(16);
        }

        let pinned = self
            .state
            .borrow()
            .tab(tab_id)
            .map(|tab| tab.pinned)
            .unwrap_or(false);
        if pinned {
            self.refresh_pinned_tiles();
        }
    }

    fn on_load_failed(self: &Rc<Self>, tab_id: TabId, uri: &str, error: &glib::Error) {
        // Una navigazione che diventa un download interrompe il frame: non è
        // un errore, quindi niente errore nella scheda. Il download può essere
        // iniziato dopo il fallimento, quindi si controlla anche il codice
        // WebKit (PolicyError / frame load interrupted).
        let interrupted = error.domain() == glib::Quark::from_str("WebKitPolicyError")
            || error.message().contains("Frame load interrupted");
        // Un caricamento interrotto dall'utente (o sostituito da un'altra
        // navigazione) non è un errore: nessuna pagina di errore né toast.
        let cancelled = (!interrupted && error.message().to_lowercase().contains("cancelled"))
            || (error.domain() == glib::Quark::from_str("WebKitNetworkError")
                && error.code() == 302);
        if interrupted || cancelled || self.pending_downloads.borrow_mut().remove(uri) {
            let mut state = self.state.borrow_mut();
            if let Some(tab) = state.tab_mut(tab_id) {
                tab.loading = false;
                tab.progress = 0.0;
                if cancelled {
                    tab.load_failed = false;
                }
            }
            let active = state.active_workspace().active_tab == Some(tab_id);
            drop(state);
            if active && cancelled {
                self.sync_toolbar();
            }
            return;
        }

        let active = {
            let mut state = self.state.borrow_mut();
            let active = state.active_workspace().active_tab == Some(tab_id);
            if let Some(tab) = state.tab_mut(tab_id) {
                tab.loading = false;
                tab.load_failed = true;
                tab.progress = 0.0;
                if !uri.is_empty() {
                    tab.uri = uri.to_owned();
                }
            }
            active
        };

        self.update_tab_label(tab_id);
        if active {
            self.sync_toolbar();
        }

        // WebKit mostra già una pagina di errore nel frame; qui si aggiunge il
        // motivo leggibile, con la possibilità di riprovare.
        let short_uri = if uri.is_empty() { "la pagina" } else { uri };
        self.toast_with_action(
            &format!("Impossibile caricare {short_uri}: {error}"),
            "Riprova",
            |browser| browser.retry_failed_tab(),
        );
    }

    /// Ricarica l'indirizzo dell'ultima navigazione fallita della scheda attiva.
    fn retry_failed_tab(self: &Rc<Self>) {
        let Some(tab_id) = self.active_tab_id() else {
            self.go_home();
            return;
        };
        let uri = {
            let state = self.state.borrow();
            state.tab(tab_id).map(|tab| tab.uri.clone()).unwrap_or_default()
        };
        if uri.is_empty() {
            self.go_home();
        } else {
            self.navigate_tab(tab_id, &uri);
        }
    }

    fn sync_toolbar(&self) {
        let snapshot = {
            let state = self.state.borrow();
            let Some(tab_id) = state.active_tab_id() else {
                return;
            };
            let Some(tab) = state.tab(tab_id) else {
                return;
            };
            (
                tab.title.clone(),
                tab.uri.clone(),
                tab.is_start_page,
                tab.loading,
                tab.progress,
                tab.webview.clone(),
            )
        };

        let (title, uri, is_start_page, loading, progress, webview) = snapshot;
        // Non sovrascrivere quello che l'utente sta digitando nella barra.
        if !self.address.has_focus() {
            // Aggiornamento programmatico della barra: non deve riaprire
            // l'elenco dei suggerimenti appena chiuso.
            self.suppress_suggestions.set(true);
            self.address
                .set_text(if is_start_page { "" } else { &uri });
            self.suppress_suggestions.set(false);
        }

        let window_title = if title.is_empty() {
            APP_NAME.to_owned()
        } else {
            format!("{title} — {APP_NAME}")
        };
        self.window.set_title(Some(&window_title));

        if let Some(webview) = webview {
            self.back.set_sensitive(webview.can_go_back());
            self.forward.set_sensitive(webview.can_go_forward());
        } else {
            self.back.set_sensitive(false);
            self.forward.set_sensitive(false);
        }

        self.progress.set_visible(loading);
        self.progress
            .set_fraction(if loading { progress } else { 0.0 });
        if loading {
            self.reload.set_icon_name("process-stop-symbolic");
            self.reload.set_tooltip_text(Some("Interrompi caricamento"));
        } else {
            self.reload.set_icon_name("view-refresh-symbolic");
            self.reload.set_tooltip_text(Some("Ricarica (Ctrl+R)"));
        }

        let can_bookmark = !is_start_page && !uri.is_empty() && is_history_url(&uri);
        self.bookmark.set_sensitive(can_bookmark);
        let bookmarked = can_bookmark && self.storage.is_bookmarked(&uri).unwrap_or(false);
        if bookmarked {
            self.bookmark.set_icon_name("starred-symbolic");
            self.bookmark
                .set_tooltip_text(Some("Rimuovi dai segnalibri (Ctrl+D)"));
        } else {
            self.bookmark.set_icon_name("non-starred-symbolic");
            self.bookmark
                .set_tooltip_text(Some("Aggiungi ai segnalibri (Ctrl+D)"));
        }
    }

    fn update_tab_label(&self, tab_id: TabId) {
        let (title, uri, pinned) = {
            let state = self.state.borrow();
            let Some(tab) = state.tab(tab_id) else {
                return;
            };
            (tab.title.clone(), tab.uri.clone(), tab.pinned)
        };

        if let Some(widgets) = self.tab_widgets.borrow().get(&tab_id) {
            widgets.title.set_text(&title);
            widgets.row.set_tooltip_text(Some(&uri));
            widgets.pin.set_visible(pinned);
        }
    }

    fn rebuild_tab_list(self: &Rc<Self>) {
        self.suppress_selection.set(true);
        self.tab_list.remove_all();
        self.tab_widgets.borrow_mut().clear();

        let (pinned_rows, normal_rows, active_tab, groups) = {
            let state = self.state.borrow();
            let workspace = state.active_workspace();
            let mut pinned = Vec::new();
            let mut normal = Vec::new();
            for tab_id in &workspace.tabs {
                let Some(tab) = state.tab(*tab_id) else {
                    continue;
                };
                let snapshot = TabRowData {
                    id: tab.id,
                    title: tab.title.clone(),
                    uri: tab.uri.clone(),
                    pinned: tab.pinned,
                    favicon: tab.favicon.clone(),
                    group: tab.group,
                };
                if tab.pinned {
                    pinned.push(snapshot);
                } else {
                    normal.push(snapshot);
                }
            }
            (pinned, normal, workspace.active_tab, workspace.groups.clone())
        };

        self.refresh_pinned_tiles_with(&pinned_rows);

        for group in &groups {
            let members = normal_rows
                .iter()
                .filter(|row| row.group == Some(group.id))
                .collect::<Vec<_>>();
            if members.is_empty() {
                continue;
            }
            let header = self.build_group_header(group, members.len());
            self.tab_list.append(&header);
            if group.collapsed {
                continue;
            }
            for data in members {
                self.append_tab_row(data, &groups, true);
            }
        }

        for data in normal_rows.iter().filter(|row| {
            row.group
                .map(|id| !groups.iter().any(|group| group.id == id))
                .unwrap_or(true)
        }) {
            self.append_tab_row(data, &groups, false);
        }

        if let Some(active_tab) = active_tab {
            if let Some(widgets) = self.tab_widgets.borrow().get(&active_tab) {
                self.tab_list.select_row(Some(&widgets.row));
            }
        }

        self.suppress_selection.set(false);
        self.sync_pinned_tiles();
    }

    fn build_pinned_tile(self: &Rc<Self>, data: &TabRowData) -> gtk::Button {
        let button = gtk::Button::new();
        button.add_css_class("pinned-tile");
        button.set_widget_name(&data.id.to_string());
        let tooltip = if data.title.trim().is_empty() {
            data.uri.clone()
        } else {
            data.title.clone()
        };
        button.set_tooltip_text(Some(&tooltip));

        let image = gtk::Image::new();
        image.set_pixel_size(16);
        match data.favicon.as_ref() {
            Some(texture) => image.set_paintable(Some(texture)),
            None => image.set_icon_name(Some("text-html-symbolic")),
        }
        button.set_child(Some(&image));

        let weak = Rc::downgrade(self);
        let tab_id = data.id;
        button.connect_clicked(move |_| {
            if let Some(browser) = weak.upgrade() {
                browser.activate_tab(tab_id);
            }
        });

        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        let weak = Rc::downgrade(self);
        let button_for_menu = button.clone();
        gesture.connect_pressed(move |_, _, x, y| {
            if let Some(browser) = weak.upgrade() {
                browser.show_tab_menu(&button_for_menu, tab_id, x, y);
            }
        });
        button.add_controller(gesture);

        button
    }

    /// Ricostruisce le tile dei fissati leggendo lo stato corrente.
    fn refresh_pinned_tiles(self: &Rc<Self>) {
        let rows = {
            let state = self.state.borrow();
            let workspace = state.active_workspace();
            workspace
                .tabs
                .iter()
                .filter_map(|id| state.tab(*id))
                .filter(|tab| tab.pinned)
                .map(|tab| TabRowData {
                    id: tab.id,
                    title: tab.title.clone(),
                    uri: tab.uri.clone(),
                    pinned: true,
                    favicon: tab.favicon.clone(),
                    group: tab.group,
                })
                .collect::<Vec<_>>()
        };
        self.refresh_pinned_tiles_with(&rows);
    }

    fn refresh_pinned_tiles_with(self: &Rc<Self>, rows: &[TabRowData]) {
        while let Some(child) = self.pinned_strip.first_child() {
            self.pinned_strip.remove(&child);
        }
        for data in rows {
            self.pinned_strip.append(&self.build_pinned_tile(data));
        }
        self.pinned_strip.set_visible(!rows.is_empty());
        self.sync_pinned_tiles();
    }

    /// Evidenzia nella barra dei fissati la scheda attiva.
    fn sync_pinned_tiles(&self) {
        let active = self.state.borrow().active_workspace().active_tab;
        let mut child = self.pinned_strip.first_child();
        while let Some(widget) = child {
            let next = widget.next_sibling();
            if let Ok(tab_id) = widget.widget_name().parse::<TabId>() {
                if Some(tab_id) == active {
                    widget.add_css_class("pinned-tile-active");
                } else {
                    widget.remove_css_class("pinned-tile-active");
                }
            }
            child = next;
        }
    }

    fn append_tab_row(self: &Rc<Self>, data: &TabRowData, groups: &[TabGroup], indented: bool) {
        let color = data
            .group
            .and_then(|id| groups.iter().find(|group| group.id == id))
            .map(|group| group.color.as_str());
        let (row, tab_widgets) = self.build_tab_row(
            data.id,
            &data.title,
            &data.uri,
            data.pinned,
            data.favicon.as_ref(),
            color,
            indented,
        );
        self.tab_list.append(&row);
        self.tab_widgets.borrow_mut().insert(data.id, tab_widgets);
    }

    fn build_group_header(self: &Rc<Self>, group: &TabGroup, count: usize) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.set_selectable(false);
        row.set_activatable(false);
        row.set_widget_name("group");
        row.add_css_class("tab-group-row");

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 7);
        content.set_margin_top(4);
        content.set_margin_bottom(4);
        content.set_margin_start(10);
        content.set_margin_end(8);

        let dot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        dot.set_size_request(8, 8);
        dot.set_valign(gtk::Align::Center);
        dot.add_css_class("group-dot");
        dot.add_css_class(&format!("group-{}", group.color));

        let label = gtk::Label::new(Some(&group.name));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(pango::EllipsizeMode::End);
        label.add_css_class("tab-group-title");

        let count_label = gtk::Label::new(Some(&count.to_string()));
        count_label.add_css_class("tab-group-count");

        let chevron = gtk::Image::from_icon_name(if group.collapsed {
            "pan-end-symbolic"
        } else {
            "pan-down-symbolic"
        });
        chevron.set_pixel_size(12);
        chevron.add_css_class("tab-group-chevron");

        content.append(&dot);
        content.append(&label);
        content.append(&count_label);
        content.append(&chevron);
        row.set_child(Some(&content));

        let gesture = gtk::GestureClick::new();
        gesture.set_button(1);
        let weak = Rc::downgrade(self);
        let group_id = group.id;
        gesture.connect_released(move |_, _, _, _| {
            if let Some(browser) = weak.upgrade() {
                browser.toggle_group_collapsed(group_id);
            }
        });
        row.add_controller(gesture);

        let menu_gesture = gtk::GestureClick::new();
        menu_gesture.set_button(3);
        let weak = Rc::downgrade(self);
        let row_for_menu = row.clone();
        let group_id = group.id;
        menu_gesture.connect_pressed(move |_, _, x, y| {
            if let Some(browser) = weak.upgrade() {
                browser.show_group_menu(&row_for_menu, group_id, x, y);
            }
        });
        row.add_controller(menu_gesture);

        row
    }

    fn build_tab_row(
        self: &Rc<Self>,
        tab_id: TabId,
        title: &str,
        uri: &str,
        pinned: bool,
        favicon_texture: Option<&gdk::Texture>,
        group_color: Option<&str>,
        indented: bool,
    ) -> (gtk::ListBoxRow, TabWidgets) {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&tab_id.to_string());
        row.add_css_class("tab-row");
        if pinned {
            row.add_css_class("pinned-tab");
        }
        if indented {
            row.add_css_class("tab-in-group");
        }
        if let Some(color) = group_color {
            row.add_css_class(&format!("group-{color}"));
        }
        row.set_tooltip_text(Some(uri));

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(8);
        content.set_margin_end(6);

        let pin = gtk::Image::from_icon_name("view-pin-symbolic");
        pin.set_pixel_size(12);
        pin.set_visible(pinned);
        pin.add_css_class("tab-pin");

        let favicon = gtk::Image::from_icon_name("text-html-symbolic");
        favicon.set_pixel_size(16);
        favicon.add_css_class("tab-favicon");
        if let Some(texture) = favicon_texture {
            favicon.set_icon_name(None);
            favicon.set_paintable(Some(texture));
        }

        let label = gtk::Label::new(Some(title));
        label.set_ellipsize(pango::EllipsizeMode::End);
        label.set_max_width_chars(28);
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.add_css_class("tab-title");

        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Chiudi scheda")
            .has_frame(false)
            .focusable(false)
            .build();
        close.add_css_class("tab-close");

        content.append(&pin);
        content.append(&favicon);
        content.append(&label);
        content.append(&close);
        row.set_child(Some(&content));

        {
            let weak = Rc::downgrade(self);
            close.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    browser.close_tab(tab_id);
                }
            });
        }

        {
            let gesture = gtk::GestureClick::new();
            gesture.set_button(3);
            let weak = Rc::downgrade(self);
            let row_for_menu = row.clone();
            gesture.connect_pressed(move |_, _, x, y| {
                if let Some(browser) = weak.upgrade() {
                    browser.show_tab_menu(&row_for_menu, tab_id, x, y);
                }
            });
            row.add_controller(gesture);
        }

        (
            row.clone(),
            TabWidgets {
                row,
                title: label,
                favicon,
                pin,
            },
        )
    }

    fn show_tab_menu(
        self: &Rc<Self>,
        parent: &impl IsA<gtk::Widget>,
        tab_id: TabId,
        x: f64,
        y: f64,
    ) {
        let (pinned, groups, current_group) = {
            let state = self.state.borrow();
            let pinned = state.tab(tab_id).map(|tab| tab.pinned).unwrap_or(false);
            let current_group = state.tab(tab_id).and_then(|tab| tab.group);
            let workspace_id = state
                .tab(tab_id)
                .map(|tab| tab.workspace_id)
                .unwrap_or(state.active_workspace);
            let groups = state
                .workspace(workspace_id)
                .map(|workspace| workspace.groups.clone())
                .unwrap_or_default();
            (pinned, groups, current_group)
        };

        let popover = gtk::Popover::new();
        popover.set_parent(parent);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);

        let duplicate = gtk::Button::with_label("Duplica scheda");
        duplicate.add_css_class("flat");
        duplicate.set_halign(gtk::Align::Fill);
        {
            let weak = Rc::downgrade(self);
            let popover = popover.clone();
            duplicate.connect_clicked(move |_| {
                popover.popdown();
                if let Some(browser) = weak.upgrade() {
                    browser.duplicate_tab(tab_id);
                }
            });
        }

        let pin_button =
            gtk::Button::with_label(if pinned { "Rimuovi dai fissati" } else { "Fissa scheda" });
        pin_button.add_css_class("flat");
        pin_button.set_halign(gtk::Align::Fill);
        {
            let weak = Rc::downgrade(self);
            let popover = popover.clone();
            pin_button.connect_clicked(move |_| {
                popover.popdown();
                if let Some(browser) = weak.upgrade() {
                    browser.toggle_pin(tab_id);
                }
            });
        }

        let close = gtk::Button::with_label("Chiudi scheda");
        close.add_css_class("flat");
        close.set_halign(gtk::Align::Fill);
        {
            let weak = Rc::downgrade(self);
            let popover = popover.clone();
            close.connect_clicked(move |_| {
                popover.popdown();
                if let Some(browser) = weak.upgrade() {
                    browser.close_tab(tab_id);
                }
            });
        }

        content.append(&duplicate);
        content.append(&pin_button);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        if current_group.is_some() {
            let remove = gtk::Button::with_label("Rimuovi dal gruppo");
            remove.add_css_class("flat");
            remove.set_halign(gtk::Align::Fill);
            let weak = Rc::downgrade(self);
            let popover_for_remove = popover.clone();
            remove.connect_clicked(move |_| {
                popover_for_remove.popdown();
                if let Some(browser) = weak.upgrade() {
                    browser.assign_tab_to_group(tab_id, None);
                }
            });
            content.append(&remove);
        }

        for group in &groups {
            if Some(group.id) == current_group {
                continue;
            }
            let button = gtk::Button::with_label(&format!("Sposta in «{}»", group.name));
            button.add_css_class("flat");
            button.set_halign(gtk::Align::Fill);
            let weak = Rc::downgrade(self);
            let popover_for_group = popover.clone();
            let group_id = group.id;
            button.connect_clicked(move |_| {
                popover_for_group.popdown();
                if let Some(browser) = weak.upgrade() {
                    browser.assign_tab_to_group(tab_id, Some(group_id));
                }
            });
            content.append(&button);
        }

        {
            let new_group = gtk::Button::with_label("Nuovo gruppo…");
            new_group.add_css_class("flat");
            new_group.set_halign(gtk::Align::Fill);
            let weak = Rc::downgrade(self);
            let popover_for_new = popover.clone();
            new_group.connect_clicked(move |_| {
                popover_for_new.popdown();
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let weak_inner = Rc::downgrade(&browser);
                browser.prompt_text(
                    "Nuovo gruppo di schede",
                    "Nome del gruppo",
                    "",
                    move |name| {
                        if let Some(browser) = weak_inner.upgrade() {
                            browser.create_group_for_tab(tab_id, name);
                        }
                    },
                );
            });
            content.append(&new_group);
        }

        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&close);
        popover.set_child(Some(&content));
        popover.connect_closed(|popover| popover.unparent());
        popover.popup();
    }

    fn duplicate_tab(self: &Rc<Self>, tab_id: TabId) {
        let uri = {
            let state = self.state.borrow();
            let Some(tab) = state.tab(tab_id) else {
                return;
            };
            if tab.is_start_page || tab.uri.is_empty() {
                None
            } else {
                Some(tab.uri.clone())
            }
        };
        self.new_tab(uri, true, false);
    }

    fn close_tab(self: &Rc<Self>, tab_id: TabId) {
        if self.shutting_down.get() {
            return;
        }
        // Se la scheda sta a destra non va rimessa nello stack principale: la
        // sua WebView deve essere distrutta con la scheda, non lasciata come
        // pagina fantasma nel riquadro di sinistra.
        self.detach_split_tab(tab_id);
        // Memorizza la scheda per Ctrl+Shift+T.
        let closed = {
            let state = self.state.borrow();
            state.tab(tab_id).map(|tab| ClosedTab {
                id: 0,
                uri: if tab.is_start_page {
                    String::new()
                } else {
                    tab.uri.clone()
                },
                title: tab.title.clone(),
                workspace_id: tab.workspace_id,
            })
        };
        if let Some(mut closed) = closed {
            closed.id = self.next_closed_id.get();
            self.next_closed_id.set(closed.id + 1);
            let mut stack = self.closed_tabs.borrow_mut();
            stack.push(closed);
            if stack.len() > 25 {
                stack.remove(0);
            }
        }

        let (workspace_id, was_active, next_tab) = {
            let state = self.state.borrow();
            let Some(tab) = state.tab(tab_id) else {
                return;
            };
            let workspace_id = tab.workspace_id;
            let Some(workspace) = state.workspace(workspace_id) else {
                return;
            };
            let Some(index) = workspace.tabs.iter().position(|id| *id == tab_id) else {
                return;
            };
            let was_active = workspace.active_tab == Some(tab_id);
            let next_tab = if was_active {
                if index + 1 < workspace.tabs.len() {
                    Some(workspace.tabs[index + 1])
                } else if index > 0 {
                    Some(workspace.tabs[index - 1])
                } else {
                    None
                }
            } else {
                workspace.active_tab
            };
            (workspace_id, was_active, next_tab)
        };

        let webview = self
            .state
            .borrow_mut()
            .tabs
            .remove(&tab_id)
            .and_then(|tab| tab.webview);
        if let Some(webview) = webview {
            self.stack.remove(&webview);
        }
        self.tab_widgets.borrow_mut().remove(&tab_id);

        let workspace_empty = {
            let mut state = self.state.borrow_mut();
            let Some(workspace) = state.workspace_mut(workspace_id) else {
                return;
            };
            workspace.tabs.retain(|id| *id != tab_id);
            if workspace.active_tab == Some(tab_id) {
                workspace.active_tab = next_tab;
            }
            workspace.tabs.is_empty()
        };

        self.prune_groups();

        if workspace_empty {
            self.new_tab(None, true, false);
            return;
        }

        self.rebuild_tab_list();
        if was_active {
            if let Some(next_tab) = next_tab {
                self.activate_tab(next_tab);
            }
        } else {
            self.sync_toolbar();
        }
        self.save_session();
    }

    fn cycle_tab(self: &Rc<Self>, direction: i32) {
        let next = {
            let state = self.state.borrow();
            let workspace = state.active_workspace();
            if workspace.tabs.is_empty() {
                None
            } else {
                let active_index = workspace
                    .active_tab
                    .and_then(|active| workspace.tabs.iter().position(|id| *id == active))
                    .unwrap_or(0);
                let len = workspace.tabs.len() as i32;
                let next_index = (active_index as i32 + direction).rem_euclid(len) as usize;
                Some(workspace.tabs[next_index])
            }
        };
        if let Some(tab_id) = next {
            self.activate_tab(tab_id);
        }
    }

    fn toggle_pin(self: &Rc<Self>, tab_id: TabId) {
        {
            let mut state = self.state.borrow_mut();
            if let Some(tab) = state.tab_mut(tab_id) {
                tab.pinned = !tab.pinned;
            }
        }
        self.rebuild_tab_list();
        self.save_session();
    }

    // ----------------------------------------------------------------
    // Gruppi di schede
    // ----------------------------------------------------------------

    fn assign_tab_to_group(self: &Rc<Self>, tab_id: TabId, group_id: Option<GroupId>) {
        {
            let mut state = self.state.borrow_mut();
            if let Some(tab) = state.tab_mut(tab_id) {
                tab.group = group_id;
            }
        }
        self.prune_groups();
        self.rebuild_tab_list();
        self.save_session();
    }

    fn create_group_for_tab(self: &Rc<Self>, tab_id: TabId, name: String) {
        let group_id = {
            let mut state = self.state.borrow_mut();
            let id = state.next_group_id;
            state.next_group_id += 1;
            let color = GROUP_COLORS[(id as usize) % GROUP_COLORS.len()].to_owned();
            let workspace_id = state.active_workspace;
            if let Some(workspace) = state.workspace_mut(workspace_id) {
                workspace.groups.push(TabGroup {
                    id,
                    name,
                    color,
                    collapsed: false,
                });
            }
            id
        };
        self.assign_tab_to_group(tab_id, Some(group_id));
    }

    fn toggle_group_collapsed(self: &Rc<Self>, group_id: GroupId) {
        {
            let mut state = self.state.borrow_mut();
            let workspace_id = state.active_workspace;
            if let Some(workspace) = state.workspace_mut(workspace_id) {
                if let Some(group) = workspace
                    .groups
                    .iter_mut()
                    .find(|group| group.id == group_id)
                {
                    group.collapsed = !group.collapsed;
                }
            }
        }
        self.rebuild_tab_list();
        self.save_session();
    }

    fn rename_group(self: &Rc<Self>, group_id: GroupId, name: String) {
        {
            let mut state = self.state.borrow_mut();
            let workspace_id = state.active_workspace;
            if let Some(workspace) = state.workspace_mut(workspace_id) {
                if let Some(group) = workspace
                    .groups
                    .iter_mut()
                    .find(|group| group.id == group_id)
                {
                    group.name = name;
                }
            }
        }
        self.rebuild_tab_list();
        self.save_session();
    }

    fn cycle_group_color(self: &Rc<Self>, group_id: GroupId) {
        {
            let mut state = self.state.borrow_mut();
            let workspace_id = state.active_workspace;
            if let Some(workspace) = state.workspace_mut(workspace_id) {
                if let Some(group) = workspace
                    .groups
                    .iter_mut()
                    .find(|group| group.id == group_id)
                {
                    let index = group.color_index();
                    group.color = GROUP_COLORS[(index + 1) % GROUP_COLORS.len()].to_owned();
                }
            }
        }
        self.rebuild_tab_list();
        self.save_session();
    }

    fn dissolve_group(self: &Rc<Self>, group_id: GroupId) {
        {
            let mut state = self.state.borrow_mut();
            let workspace_id = state.active_workspace;
            let tab_ids = state
                .workspace(workspace_id)
                .map(|workspace| workspace.tabs.clone())
                .unwrap_or_default();
            for tab_id in tab_ids {
                if let Some(tab) = state.tabs.get_mut(&tab_id) {
                    if tab.group == Some(group_id) {
                        tab.group = None;
                    }
                }
            }
            if let Some(workspace) = state.workspace_mut(workspace_id) {
                workspace.groups.retain(|group| group.id != group_id);
            }
        }
        self.rebuild_tab_list();
        self.save_session();
    }

    fn prune_groups(&self) {
        let mut state = self.state.borrow_mut();
        let workspace_id = state.active_workspace;
        let (tab_ids, valid) = {
            let Some(workspace) = state.workspace(workspace_id) else {
                return;
            };
            let tab_ids = workspace.tabs.clone();
            let used = tab_ids
                .iter()
                .filter_map(|id| state.tabs.get(id).and_then(|tab| tab.group))
                .collect::<Vec<_>>();
            let valid = workspace
                .groups
                .iter()
                .filter(|group| used.contains(&group.id))
                .map(|group| group.id)
                .collect::<Vec<_>>();
            (tab_ids, valid)
        };

        if let Some(workspace) = state.workspace_mut(workspace_id) {
            workspace.groups.retain(|group| valid.contains(&group.id));
        }
        for tab_id in tab_ids {
            if let Some(tab) = state.tabs.get_mut(&tab_id) {
                if tab.group.is_some_and(|group| !valid.contains(&group)) {
                    tab.group = None;
                }
            }
        }
    }

    fn show_group_menu(self: &Rc<Self>, row: &gtk::ListBoxRow, group_id: GroupId, x: f64, y: f64) {
        let group = {
            let state = self.state.borrow();
            let workspace_id = state.active_workspace;
            state
                .workspace(workspace_id)
                .and_then(|workspace| {
                    workspace.groups.iter().find(|group| group.id == group_id)
                })
                .cloned()
        };
        let Some(group) = group else {
            return;
        };

        let popover = gtk::Popover::new();
        popover.set_parent(row);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);

        let rename = menu_button(&popover, "Rinomina gruppo");
        {
            let weak = Rc::downgrade(self);
            let current = group.name.clone();
            rename.connect_clicked(move |_| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let weak_inner = Rc::downgrade(&browser);
                let current = current.clone();
                browser.prompt_text(
                    "Rinomina gruppo",
                    "Nome del gruppo",
                    &current,
                    move |name| {
                        if let Some(browser) = weak_inner.upgrade() {
                            browser.rename_group(group_id, name);
                        }
                    },
                );
            });
        }

        let toggle = menu_button(
            &popover,
            if group.collapsed {
                "Espandi gruppo"
            } else {
                "Riduci gruppo"
            },
        );
        {
            let weak = Rc::downgrade(self);
            toggle.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    browser.toggle_group_collapsed(group_id);
                }
            });
        }

        let recolor = menu_button(&popover, "Cambia colore");
        {
            let weak = Rc::downgrade(self);
            recolor.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    browser.cycle_group_color(group_id);
                }
            });
        }

        let dissolve = menu_button(&popover, "Sciogli gruppo");
        {
            let weak = Rc::downgrade(self);
            dissolve.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    browser.dissolve_group(group_id);
                }
            });
        }

        content.append(&rename);
        content.append(&toggle);
        content.append(&recolor);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&dissolve);
        popover.set_child(Some(&content));
        popover.connect_closed(|popover| popover.unparent());
        popover.popup();
    }

    // ----------------------------------------------------------------
    // Nuova scheda con URL / motore / pagina iniziale
    // ----------------------------------------------------------------

    fn show_new_tab_menu(self: &Rc<Self>, widget: &impl IsA<gtk::Widget>, x: f64, y: f64) {
        let popover = gtk::Popover::new();
        popover.set_parent(widget);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);

        let engines = self.state.borrow().settings.engines.clone();

        let blank = menu_button(&popover, "Nuova scheda");
        {
            let weak = Rc::downgrade(self);
            blank.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    browser.new_tab(None, true, false);
                }
            });
        }
        content.append(&blank);

        let with_url = menu_button(&popover, "Nuova scheda con URL…");
        {
            let weak = Rc::downgrade(self);
            with_url.connect_clicked(move |_| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let weak_inner = Rc::downgrade(&browser);
                browser.prompt_text("Nuova scheda con URL", "URL", "https://", move |value| {
                    if let Some(browser) = weak_inner.upgrade() {
                        let template = browser.search_template();
                        if let Some(url) = resolve_input(&value, &template) {
                            browser.new_tab(Some(url), true, false);
                        }
                    }
                });
            });
        }
        content.append(&with_url);

        for engine in engines {
            let label = format!("Cerca con {}", engine.name);
            let button = menu_button(&popover, &label);
            let weak = Rc::downgrade(self);
            let template = engine.template;
            let engine_name = engine.name;
            button.connect_clicked(move |_| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let weak_inner = Rc::downgrade(&browser);
                let template = template.clone();
                let title = format!("Cerca con {engine_name}");
                browser.prompt_text(&title, "Cerca", "", move |query| {
                    if let Some(browser) = weak_inner.upgrade() {
                        browser.new_tab(Some(search_url(&template, &query)), true, false);
                    }
                });
            });
            content.append(&button);
        }

        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let start_page = menu_button(&popover, "Apri pagina iniziale");
        {
            let weak = Rc::downgrade(self);
            start_page.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    let uri = browser.home_uri();
                    browser.new_tab(uri, true, false);
                }
            });
        }
        content.append(&start_page);

        popover.set_child(Some(&content));
        popover.connect_closed(|popover| popover.unparent());
        popover.popup();
    }

    // ----------------------------------------------------------------
    // Schede chiuse di recente
    // ----------------------------------------------------------------

    fn reopen_closed_tab(self: &Rc<Self>, id: Option<u64>) {
        let closed = {
            let mut stack = self.closed_tabs.borrow_mut();
            let index = match id {
                Some(id) => stack.iter().position(|tab| tab.id == id),
                None => stack.len().checked_sub(1),
            };
            index.map(|index| stack.remove(index))
        };

        let Some(closed) = closed else {
            self.toast("Nessuna scheda chiusa di recente");
            return;
        };

        let (workspace_exists, is_active) = {
            let state = self.state.borrow();
            (
                state.workspace(closed.workspace_id).is_some(),
                state.active_workspace == closed.workspace_id,
            )
        };
        if workspace_exists && !is_active {
            {
                let mut state = self.state.borrow_mut();
                state.active_workspace = closed.workspace_id;
            }
            self.rebuild_workspace_menu();
            self.rebuild_tab_list();
        }

        let uri = (!closed.uri.is_empty()).then(|| closed.uri.clone());
        let title = (!closed.title.is_empty()).then(|| closed.title.clone());
        self.new_tab_with_title(uri, title, true, false);

        if !closed.title.is_empty() {
            self.toast(&format!("Riaperta: {}", closed.title));
        }
    }

    fn open_closed_tabs(self: &Rc<Self>) {
        self.open_list_window("Schede chiuse di recente", |browser, list| {
            let tabs = browser
                .closed_tabs
                .borrow()
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>();
            if tabs.is_empty() {
                add_empty_state(list, "Nessuna scheda chiusa di recente.");
                return;
            }

            for tab in tabs {
                let row = gtk::ListBoxRow::new();
                row.add_css_class("dialog-row");

                let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                content.set_margin_top(8);
                content.set_margin_bottom(8);
                content.set_margin_start(12);
                content.set_margin_end(8);

                let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
                labels.set_hexpand(true);
                let title_text = if tab.title.trim().is_empty() {
                    if tab.uri.is_empty() {
                        START_PAGE_TITLE.to_owned()
                    } else {
                        tab.uri.clone()
                    }
                } else {
                    tab.title.clone()
                };
                let title = gtk::Label::new(Some(&title_text));
                title.set_xalign(0.0);
                title.set_ellipsize(pango::EllipsizeMode::End);
                title.add_css_class("row-title");
                let url = gtk::Label::new(Some(if tab.uri.is_empty() {
                    "Pagina iniziale"
                } else {
                    &tab.uri
                }));
                url.set_xalign(0.0);
                url.set_ellipsize(pango::EllipsizeMode::Middle);
                url.add_css_class("row-subtitle");
                labels.append(&title);
                labels.append(&url);

                let open = gtk::Button::builder()
                    .icon_name("go-next-symbolic")
                    .tooltip_text("Riapri scheda")
                    .has_frame(false)
                    .build();
                {
                    let weak = Rc::downgrade(browser);
                    let id = tab.id;
                    open.connect_clicked(move |_| {
                        if let Some(browser) = weak.upgrade() {
                            browser.reopen_closed_tab(Some(id));
                        }
                    });
                }

                content.append(&labels);
                content.append(&open);
                row.set_child(Some(&content));
                list.append(&row);
            }
        });
    }

    // ----------------------------------------------------------------
    // Sessioni nominate
    // ----------------------------------------------------------------

    fn save_named_session_prompt(self: &Rc<Self>) {
        let suggestion = self.state.borrow().active_workspace().name.clone();
        let weak = Rc::downgrade(self);
        self.prompt_text(
            "Salva sessione",
            "Nome della sessione",
            &suggestion,
            move |name| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                match browser.write_named_session(&name) {
                    Ok(_) => browser.toast(&format!("Sessione salvata: {name}")),
                    Err(error) => {
                        browser.toast(&format!("Impossibile salvare la sessione: {error}"))
                    }
                }
            },
        );
    }

    fn write_named_session(&self, name: &str) -> anyhow::Result<PathBuf> {
        let slug = session_slug(name);
        let mut session = self
            .session_snapshot()
            .ok_or_else(|| anyhow::anyhow!("nessuna scheda da salvare"))?;
        session.name = name.to_owned();
        let path = self.paths.named_session(&slug);
        save_json(&path, &session)?;
        Ok(path)
    }

    fn load_named_session(self: &Rc<Self>, path: &Path) {
        match load_json::<Session>(path) {
            Some(session) => {
                self.apply_session(session);
                self.toast("Sessione caricata");
            }
            None => self.toast("Impossibile leggere la sessione"),
        }
    }

    fn open_sessions(self: &Rc<Self>) {
        self.open_list_window("Sessioni salvate", |browser, list| {
            let mut paths = fs::read_dir(&browser.paths.sessions_dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| {
                            path.extension().and_then(|value| value.to_str()) == Some("json")
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            paths.sort();

            if paths.is_empty() {
                add_empty_state(
                    list,
                    "Nessuna sessione salvata. Usa «Salva sessione» dal menu.",
                );
                return;
            }

            for path in paths {
                let Some(session) = load_json::<Session>(&path) else {
                    continue;
                };
                let label = if session.name.trim().is_empty() {
                    path.file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or("sessione")
                        .to_owned()
                } else {
                    session.name.clone()
                };

                let row = gtk::ListBoxRow::new();
                row.add_css_class("dialog-row");

                let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                content.set_margin_top(8);
                content.set_margin_bottom(8);
                content.set_margin_start(12);
                content.set_margin_end(8);

                let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
                labels.set_hexpand(true);
                let title = gtk::Label::new(Some(&label));
                title.set_xalign(0.0);
                title.set_ellipsize(pango::EllipsizeMode::End);
                title.add_css_class("row-title");
                let subtitle = gtk::Label::new(Some(&format!(
                    "{} schede · {} workspace",
                    session.tab_count(),
                    session.workspaces.len()
                )));
                subtitle.set_xalign(0.0);
                subtitle.add_css_class("row-subtitle");
                labels.append(&title);
                labels.append(&subtitle);

                let open = gtk::Button::builder()
                    .icon_name("document-open-symbolic")
                    .tooltip_text("Carica sessione")
                    .has_frame(false)
                    .build();
                {
                    let weak = Rc::downgrade(browser);
                    let path = path.clone();
                    open.connect_clicked(move |_| {
                        if let Some(browser) = weak.upgrade() {
                            browser.load_named_session(&path);
                        }
                    });
                }

                let remove = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text("Elimina sessione")
                    .has_frame(false)
                    .build();
                {
                    let list = list.clone();
                    let row = row.clone();
                    let path = path.clone();
                    remove.connect_clicked(move |_| {
                        if fs::remove_file(&path).is_ok() {
                            list.remove(&row);
                        }
                    });
                }

                content.append(&labels);
                content.append(&open);
                content.append(&remove);
                row.set_child(Some(&content));
                list.append(&row);
            }
        });
    }

    // ----------------------------------------------------------------
    // URL interni
    // ----------------------------------------------------------------

    fn open_internal_page(self: &Rc<Self>, url: &str) -> bool {
        match url.trim() {
            "about:settings" => {
                self.open_preferences();
                true
            }
            "about:history" => {
                self.open_history();
                true
            }
            "about:downloads" => {
                self.open_downloads();
                true
            }
            "about:bookmarks" => {
                self.open_bookmarks();
                true
            }
            "about:extensions" => {
                self.open_extensions();
                true
            }
            "about:closed-tabs" => {
                self.open_closed_tabs();
                true
            }
            "about:sessions" => {
                self.open_sessions();
                true
            }
            _ => false,
        }
    }

    // ----------------------------------------------------------------
    // Barra indirizzi con parole chiave
    // ----------------------------------------------------------------

    fn resolve_address_input(&self, input: &str) -> Option<String> {
        let (default_template, engines) = {
            let state = self.state.borrow();
            let template = state.search_template();
            let engines = state
                .settings
                .engines
                .iter()
                .filter_map(|engine| {
                    engine
                        .keyword_token()
                        .map(|keyword| (keyword, engine.template.clone()))
                })
                .collect::<Vec<_>>();
            (template, engines)
        };

        let keywords = engines
            .iter()
            .map(|(keyword, template)| KeywordEngine {
                keyword: keyword.as_str(),
                template: template.as_str(),
            })
            .collect::<Vec<_>>();

        resolve_input_with_keywords(input, &default_template, &keywords)
    }

    // ----------------------------------------------------------------
    // Tema, estensioni, filtro contenuti
    // ----------------------------------------------------------------

    fn apply_theme(&self) {
        let preference = self.state.borrow().settings.theme;
        let scheme = match preference {
            ThemePreference::System => adw::ColorScheme::Default,
            ThemePreference::Light => adw::ColorScheme::ForceLight,
            ThemePreference::Dark => adw::ColorScheme::ForceDark,
        };
        adw::StyleManager::default().set_color_scheme(scheme);
    }

    fn install_extensions(&self) {
        let extensions = crate::extensions::load_all(&self.paths.extensions_dir);
        for extension in &extensions {
            let patterns = extension.patterns();
            let allow_list = patterns.iter().map(String::as_str).collect::<Vec<_>>();
            for source in &extension.sources {
                let script = webkit::UserScript::new(
                    source,
                    webkit::UserContentInjectedFrames::TopFrame,
                    webkit::UserScriptInjectionTime::Start,
                    &allow_list,
                    &[],
                );
                self.user_content_manager.add_script(&script);
            }
        }
    }

    fn filter_dir(&self) -> std::path::PathBuf {
        self.paths.data_dir.join("content-filters")
    }

    /// Impostazioni di rete che WebKitGTK 2.54 applica davvero, su tutte le
    /// sessioni (spazi inclusi).
    fn apply_network_privacy(&self) {
        let block_third_party = self.state.borrow().settings.block_third_party_cookies;
        apply_session_privacy(&self.network_session, block_third_party);
        for session in self.workspace_sessions.borrow().values() {
            apply_session_privacy(session, block_third_party);
        }
    }

    /// Script iniettati (segnali GPC/DNT, assistenza ai CMP).
    fn refresh_privacy_scripts(&self) {
        let (signals, consent) = {
            let state = self.state.borrow();
            (
                state.settings.privacy_signals,
                state.settings.cookie_banner_helper,
            )
        };

        if let Some(script) = self.signals_script.borrow_mut().take() {
            self.user_content_manager.remove_script(&script);
        }
        if signals {
            let script = webkit::UserScript::new(
                &crate::privacy::signals_script(),
                webkit::UserContentInjectedFrames::TopFrame,
                webkit::UserScriptInjectionTime::Start,
                &[],
                &[],
            );
            self.user_content_manager.add_script(&script);
            *self.signals_script.borrow_mut() = Some(script);
        }

        if let Some(script) = self.consent_script.borrow_mut().take() {
            self.user_content_manager.remove_script(&script);
        }
        if consent {
            let script = webkit::UserScript::new(
                &crate::consent::script(),
                webkit::UserContentInjectedFrames::TopFrame,
                webkit::UserScriptInjectionTime::End,
                &[],
                &[],
            );
            self.user_content_manager.add_script(&script);
            *self.consent_script.borrow_mut() = Some(script);
        }

        // Gli script valgono per i documenti caricati dopo.
        self.reload_content_tabs();
    }

    fn reload_content_tabs(&self) {
        let webviews = {
            let state = self.state.borrow();
            state
                .tabs
                .values()
                .filter(|tab| !tab.is_start_page)
                .filter_map(|tab| tab.webview.clone())
                .collect::<Vec<_>>()
        };
        for webview in webviews {
            webview.reload();
        }
    }

    fn block_exceptions(&self) -> Vec<String> {
        self.storage.block_exceptions().unwrap_or_default()
    }

    /// Origine `http(s)://host[:porta]` della scheda attiva, se c'è.
    fn current_origin(&self) -> Option<String> {
        let tab_id = self.active_tab_id()?;
        let uri = {
            let state = self.state.borrow();
            state.tab(tab_id)?.uri.clone()
        };
        let origin = origin_of(&uri)?;
        if origin.starts_with("http://") || origin.starts_with("https://") {
            Some(origin)
        } else {
            None
        }
    }

    fn site_blocking_enabled(&self, origin: &str) -> bool {
        if !self.state.borrow().settings.block_ads {
            return false;
        }
        !self.block_exceptions().iter().any(|entry| entry == origin)
    }

    fn toggle_site_blocking(self: &Rc<Self>, origin: &str) {
        let excepted = self.block_exceptions().iter().any(|entry| entry == origin);
        let result = if excepted {
            self.storage.remove_block_exception(origin)
        } else {
            self.storage.add_block_exception(origin)
        };
        match result {
            Ok(()) => {
                self.refresh_content_filter();
                self.toast(if excepted {
                    "Blocco contenuti riattivato per questo sito"
                } else {
                    "Blocco contenuti disattivato per questo sito"
                });
            }
            Err(error) => self.toast(&format!("Eccezione non salvata: {error}")),
        }
    }

    /// Descrizione della lista in uso: fonte, licenza, host e data.
    fn filter_status(&self) -> String {
        match crate::filters::load_cache(&self.filter_dir()) {
            Some((meta, hosts)) => {
                let date = glib::DateTime::from_unix_local(meta.updated_at)
                    .ok()
                    .and_then(|date| date.format("%d/%m/%Y").ok())
                    .map(|text| text.to_string())
                    .unwrap_or_else(|| "data sconosciuta".to_owned());
                format!(
                    "{} ({}): {} host, aggiornata il {}",
                    meta.source,
                    meta.license,
                    hosts.len(),
                    date
                )
            }
            None => format!(
                "Lista incorporata Peats: {} host",
                blocklist::BLOCKED_HOSTS.len()
            ),
        }
    }

    /// Ricompila il filtro con lista locale ed eccezioni per sito.
    ///
    /// WebKit valida le regole alla compilazione: se la lista o le eccezioni
    /// producono regole non valide il filtro non viene installato e resta
    /// visibile l'errore, invece di un falso stato «attivo».
    /// Installa il filtro contenuti. Se le regole sono identiche all'ultima
    /// compilazione riusa il filtro già compilato da WebKit (`load`), così
    /// l'avvio non ricompila centinaia di regole; altrimenti compila e salva.
    fn refresh_content_filter(self: &Rc<Self>) {
        self.user_content_manager.remove_all_filters();

        if !self.state.borrow().settings.block_ads {
            self.reload_content_tabs();
            return;
        }

        let directory = self.filter_dir();
        if let Err(error) = fs::create_dir_all(&directory) {
            self.toast(&format!("Filtro contenuti non installato: {error}"));
            return;
        }

        let cached = crate::filters::load_cache(&directory)
            .map(|(_, hosts)| hosts)
            .unwrap_or_default();
        let hosts = crate::filters::all_hosts(&cached);
        let exceptions = crate::filters::exception_hosts(&self.block_exceptions());
        let cosmetic = self.state.borrow().settings.cosmetic_blocking;
        let json = crate::filters::build_filter_json(&hosts, &exceptions, cosmetic);
        let fingerprint = crate::filters::fingerprint(&json);
        let identifier = blocklist::FILTER_IDENTIFIER;

        let reusable = crate::filters::load_compiled(&directory)
            .map(|meta| meta.identifier == identifier && meta.fingerprint == fingerprint)
            .unwrap_or(false);

        if !reusable {
            self.compile_content_filter(&directory, &json, &fingerprint);
            return;
        }

        let store = webkit::UserContentFilterStore::new(&directory.to_string_lossy());
        let manager = self.user_content_manager.clone();
        let weak = Rc::downgrade(self);
        let json_fallback = json.clone();
        let fingerprint_fallback = fingerprint.clone();
        let keep_store = store.clone();
        store.load(
            identifier,
            None::<&gio::Cancellable>,
            move |result| {
                // Il filtro salvato può essere stato eliminato o non essere più
                // compatibile: in quel caso si ricompila.
                let _ = &keep_store;
                match result {
                    Ok(filter) => {
                        manager.add_filter(&filter);
                        if let Some(browser) = weak.upgrade() {
                            browser.reload_content_tabs();
                        }
                    }
                    Err(error) => {
                        eprintln!("cannot reuse content filter: {error}");
                        if let Some(browser) = weak.upgrade() {
                            let directory = browser.filter_dir();
                            browser.compile_content_filter(
                                &directory,
                                &json_fallback,
                                &fingerprint_fallback,
                            );
                        }
                    }
                }
            },
        );
    }

    /// Compila il filtro e memorizza l'impronta delle regole, così il prossimo
    /// avvio può riusarlo senza ricompilare.
    fn compile_content_filter(self: &Rc<Self>, directory: &Path, json: &str, fingerprint: &str) {
        let store = webkit::UserContentFilterStore::new(&directory.to_string_lossy());
        let source = glib::Bytes::from_owned(json.as_bytes().to_vec());
        let manager = self.user_content_manager.clone();
        let identifier = blocklist::FILTER_IDENTIFIER;
        let fingerprint = fingerprint.to_owned();
        let directory = directory.to_path_buf();
        let weak = Rc::downgrade(self);
        store.save(
            identifier,
            &source,
            None::<&gio::Cancellable>,
            move |result| match result {
                Ok(filter) => {
                    manager.add_filter(&filter);
                    let meta = crate::filters::CompiledMeta {
                        identifier: identifier.to_owned(),
                        fingerprint,
                    };
                    if let Err(error) = crate::filters::save_compiled(&directory, &meta) {
                        eprintln!("cannot save compiled filter metadata: {error}");
                    }
                    if let Some(browser) = weak.upgrade() {
                        browser.reload_content_tabs();
                    }
                }
                Err(error) => {
                    eprintln!("cannot install content filter: {error}");
                    if let Some(browser) = weak.upgrade() {
                        browser.toast(&format!("Filtro contenuti non compilato: {error}"));
                    }
                }
            },
        );
    }

    /// Scarica la lista pubblica e la converte; in caso di errore la copia
    /// locale precedente resta in uso.
    fn update_filter_lists(self: &Rc<Self>, quiet: bool) {
        let url = crate::filters::list_url();
        let Ok(message) = soup::Message::new("GET", &url) else {
            self.toast("URL della lista non valido");
            return;
        };
        let session = soup::Session::new();
        let directory = self.filter_dir();
        let weak = Rc::downgrade(self);
        session.send_and_read_async(
            &message,
            glib::Priority::DEFAULT,
            None::<&gio::Cancellable>,
            move |result| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(bytes) => {
                        let data: &[u8] = bytes.as_ref();
                        let text = String::from_utf8_lossy(data);
                        let hosts = crate::filters::parse_hosts(&text);
                        if hosts.len() < 100 {
                            if !quiet {
                                browser.toast(
                                    "Lista non valida: resta la versione precedente",
                                );
                            }
                            return;
                        }
                        let meta = crate::filters::ListMeta {
                            source: crate::filters::LIST_NAME.to_owned(),
                            url: url.clone(),
                            license: crate::filters::LIST_LICENSE.to_owned(),
                            updated_at: crate::filters::now(),
                            hosts: hosts.len(),
                        };
                        match crate::filters::save_cache(&directory, &meta, &hosts) {
                            Ok(()) => {
                                browser.refresh_content_filter();
                                browser.toast(&format!(
                                    "Liste aggiornate: {} host",
                                    meta.hosts
                                ));
                            }
                            Err(error) => {
                                browser.toast(&format!("Lista non salvata: {error}"))
                            }
                        }
                    }
                    Err(error) => {
                        if !quiet {
                            browser.toast(&format!(
                                "Aggiornamento liste non riuscito: {error}"
                            ));
                        }
                    }
                }
            },
        );
    }

    /// Aggiorna la lista in background all'avvio se la copia è vecchia.
    fn schedule_list_update(self: &Rc<Self>) {
        let stale = crate::filters::load_cache(&self.filter_dir())
            .map(|(meta, _)| {
                crate::filters::is_stale(&meta, crate::filters::now(), crate::filters::MAX_AGE)
            })
            .unwrap_or(true);
        if !stale {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local_once(4, move || {
            if let Some(browser) = weak.upgrade() {
                browser.update_filter_lists(true);
            }
        });
    }

    /// Identificatore e descrizione leggibile del permesso richiesto.
    fn permission_kind(request: &webkit::PermissionRequest) -> Option<(&'static str, &'static str)> {
        if request.is::<webkit::GeolocationPermissionRequest>() {
            return Some(("geolocation", "la geolocalizzazione"));
        }
        if request.is::<webkit::NotificationPermissionRequest>() {
            return Some(("notifications", "le notifiche"));
        }
        if request.is::<webkit::UserMediaPermissionRequest>() {
            let Ok(media) = request.clone().downcast::<webkit::UserMediaPermissionRequest>() else {
                return Some(("media", "dispositivi multimediali"));
            };
            let audio = media.is_for_audio_device();
            let video = media.is_for_video_device();
            return Some(match (audio, video) {
                (true, true) => ("camera-microphone", "fotocamera e microfono"),
                (true, false) => ("microphone", "il microfono"),
                (false, true) => ("camera", "la fotocamera"),
                (false, false) => ("media", "dispositivi multimediali"),
            });
        }
        if request.is::<webkit::ClipboardPermissionRequest>() {
            return Some(("clipboard", "gli appunti"));
        }
        if request.is::<webkit::PointerLockPermissionRequest>() {
            return Some(("pointer-lock", "il blocco del puntatore"));
        }
        if request.is::<webkit::DeviceInfoPermissionRequest>() {
            return Some(("device-info", "informazioni sui dispositivi"));
        }
        if request.is::<webkit::MediaKeySystemPermissionRequest>() {
            return Some(("media-keys", "chiavi DRM"));
        }
        if request.is::<webkit::XRPermissionRequest>() {
            return Some(("xr", "realtà estesa"));
        }
        None
    }

    /// Decide un permesso: usa la scelta memorizzata o chiede all'utente.
    fn handle_permission_request(
        self: &Rc<Self>,
        webview: &WebView,
        request: &webkit::PermissionRequest,
    ) {
        let Some((kind, label)) = Self::permission_kind(request) else {
            // Permesso non riconosciuto: si nega, come da comportamento sicuro.
            request.deny();
            return;
        };

        let origin = webview
            .uri()
            .and_then(|uri| origin_of(&uri))
            .unwrap_or_default();
        if origin.is_empty() {
            request.deny();
            return;
        }

        match self.storage.permission(&origin, kind).ok().flatten().as_deref() {
            Some("allow") => {
                request.allow();
                return;
            }
            Some("deny") => {
                request.deny();
                return;
            }
            _ => {}
        }

        let host = url::Url::parse(&origin)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| origin.clone());
        let dialog = adw::AlertDialog::new(
            Some(&format!("{host} chiede {label}")),
            Some(
                "La scelta viene ricordata per questo sito. Puoi cambiarla da \
                 Impostazioni → Privacy → Permessi per sito.",
            ),
        );
        dialog.add_response("deny", "Blocca");
        dialog.add_response("allow", "Consenti");
        dialog.set_response_appearance("allow", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("deny"));
        dialog.set_close_response("deny");

        let request = request.clone();
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |dialog, response| {
            dialog.close();
            let allowed = response == "allow";
            if allowed {
                request.allow();
            } else {
                request.deny();
            }
            let Some(browser) = weak.upgrade() else {
                return;
            };
            let decision = if allowed { "allow" } else { "deny" };
            if let Err(error) = browser.storage.set_permission(&origin, kind, decision) {
                browser.toast(&format!("Permesso non memorizzato: {error}"));
            }
        });

        dialog.present(Some(&self.window));
    }

    /// Cancella i dati indicati in **tutte** le sessioni (profilo storico e
    /// spazi isolati) e riporta l'esito reale dell'operazione.
    fn clear_website_data(self: &Rc<Self>, types: webkit::WebsiteDataTypes, label: &str) {
        let mut managers = Vec::new();
        if let Some(manager) = self.network_session.website_data_manager() {
            managers.push(manager);
        }
        for session in self.workspace_sessions.borrow().values() {
            if let Some(manager) = session.website_data_manager() {
                managers.push(manager);
            }
        }
        if managers.is_empty() {
            self.toast("Gestore dati non disponibile");
            return;
        }

        // `clear` accetta solo callback `Send`: i risultati tornano al thread UI
        // tramite un canale, così il messaggio non dichiara un falso successo.
        let (sender, receiver) = std::sync::mpsc::channel::<Result<(), String>>();
        let total = managers.len();
        for manager in managers {
            let sender = sender.clone();
            manager.clear(
                types,
                glib::TimeSpan::from_seconds(0),
                None::<&gio::Cancellable>,
                move |result| {
                    let _ = sender.send(result.map_err(|error| error.to_string()));
                },
            );
        }

        let label = label.to_owned();
        let weak = Rc::downgrade(self);
        let mut received = 0usize;
        let mut failure: Option<String> = None;
        let mut ticks = 0u32;
        glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
            while let Ok(result) = receiver.try_recv() {
                received += 1;
                if let Err(error) = result {
                    failure.get_or_insert(error);
                }
            }

            if received >= total {
                if let Some(browser) = weak.upgrade() {
                    match &failure {
                        Some(error) => browser
                            .toast(&format!("{label}: cancellazione non riuscita ({error})")),
                        None => browser.toast(&format!("{label}: dati cancellati")),
                    }
                }
                return glib::ControlFlow::Break;
            }

            ticks += 1;
            if ticks > 100 {
                if let Some(browser) = weak.upgrade() {
                    browser.toast(&format!("{label}: esito non confermato"));
                }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    /// Popover «Privacy»: blocco contenuti sempre a portata di mano e
    /// cancellazione rapida dei dati, senza passare dalle impostazioni.
    fn show_privacy_popover(self: &Rc<Self>, button: &gtk::Button) {
        let popover = gtk::Popover::new();
        popover.set_parent(button);
        popover.set_has_arrow(true);
        popover.add_css_class("privacy-popover");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_width_request(280);

        let title = gtk::Label::new(Some("Privacy"));
        title.set_xalign(0.0);
        title.add_css_class("privacy-title");
        content.append(&title);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 1);
        labels.set_hexpand(true);
        let name = gtk::Label::new(Some("Blocco contenuti"));
        name.set_xalign(0.0);
        let subtitle = gtk::Label::new(Some("Annunci, tracker e pop-up"));
        subtitle.set_xalign(0.0);
        subtitle.add_css_class("row-subtitle");
        labels.append(&name);
        labels.append(&subtitle);

        let blocking = gtk::Switch::new();
        blocking.set_active(self.state.borrow().settings.block_ads);
        blocking.set_valign(gtk::Align::Center);
        {
            let weak = Rc::downgrade(self);
            blocking.connect_active_notify(move |switch| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let enabled = switch.is_active();
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.block_ads == enabled {
                        false
                    } else {
                        state.settings.block_ads = enabled;
                        true
                    }
                };
                if changed {
                    browser.refresh_content_filter();
                    browser.save_settings();
                    browser.toast(if enabled {
                        "Blocco contenuti attivo"
                    } else {
                        "Blocco contenuti disattivo"
                    });
                }
            });
        }

        row.append(&labels);
        row.append(&blocking);
        content.append(&row);

        // Eccezione per il sito della scheda attiva.
        if let Some(origin) = self.current_origin() {
            let site_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            site_row.set_margin_top(4);
            let site_labels = gtk::Box::new(gtk::Orientation::Vertical, 1);
            site_labels.set_hexpand(true);
            let site_name = gtk::Label::new(Some("Su questo sito"));
            site_name.set_xalign(0.0);
            let site_subtitle = gtk::Label::new(Some(&origin));
            site_subtitle.set_xalign(0.0);
            site_subtitle.add_css_class("row-subtitle");
            site_labels.append(&site_name);
            site_labels.append(&site_subtitle);

            let site_switch = gtk::Switch::new();
            site_switch.set_valign(gtk::Align::Center);
            site_switch.set_active(self.site_blocking_enabled(&origin));
            site_switch.set_sensitive(blocking.is_active());
            {
                let weak = Rc::downgrade(self);
                let origin = origin.clone();
                let popover = popover.clone();
                site_switch.connect_active_notify(move |switch| {
                    let Some(browser) = weak.upgrade() else {
                        return;
                    };
                    if browser.site_blocking_enabled(&origin) == switch.is_active() {
                        return;
                    }
                    popover.popdown();
                    browser.toggle_site_blocking(&origin);
                });
            }
            site_row.append(&site_labels);
            site_row.append(&site_switch);
            content.append(&site_row);
        }

        let status = gtk::Label::new(Some(&self.filter_status()));
        status.add_css_class("row-subtitle");
        status.set_wrap(true);
        status.set_xalign(0.0);
        status.set_max_width_chars(42);
        content.append(&status);

        let refresh_lists = gtk::Button::with_label("Aggiorna liste");
        refresh_lists.add_css_class("flat");
        refresh_lists.add_css_class("privacy-action");
        refresh_lists.set_halign(gtk::Align::Fill);
        {
            let weak = Rc::downgrade(self);
            let popover = popover.clone();
            refresh_lists.connect_clicked(move |_| {
                popover.popdown();
                if let Some(browser) = weak.upgrade() {
                    browser.toast("Aggiornamento liste in corso…");
                    browser.update_filter_lists(false);
                }
            });
        }
        content.append(&refresh_lists);

        let exceptions = self.block_exceptions().len();
        let exceptions_button = gtk::Button::with_label(&format!("Eccezioni ({exceptions})…"));
        exceptions_button.add_css_class("flat");
        exceptions_button.add_css_class("privacy-action");
        exceptions_button.set_halign(gtk::Align::Fill);
        {
            let weak = Rc::downgrade(self);
            let popover = popover.clone();
            exceptions_button.connect_clicked(move |_| {
                popover.popdown();
                if let Some(browser) = weak.upgrade() {
                    browser.open_block_exceptions();
                }
            });
        }
        content.append(&exceptions_button);

        let clear = gtk::Button::with_label("Cancella dati di navigazione");
        clear.add_css_class("flat");
        clear.add_css_class("privacy-action");
        clear.set_halign(gtk::Align::Fill);
        {
            let weak = Rc::downgrade(self);
            let popover = popover.clone();
            clear.connect_clicked(move |_| {
                popover.popdown();
                if let Some(browser) = weak.upgrade() {
                    browser.clear_browsing_data_prompt();
                }
            });
        }

        let settings = gtk::Button::with_label("Impostazioni privacy…");
        settings.add_css_class("flat");
        settings.add_css_class("privacy-action");
        settings.set_halign(gtk::Align::Fill);
        {
            let weak = Rc::downgrade(self);
            let popover = popover.clone();
            settings.connect_clicked(move |_| {
                popover.popdown();
                if let Some(browser) = weak.upgrade() {
                    browser.open_preferences();
                }
            });
        }

        content.append(&clear);
        content.append(&settings);
        popover.set_child(Some(&content));
        popover.connect_closed(|popover| popover.unparent());
        popover.popup();
    }

    fn clear_browsing_data_prompt(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Cancellare i dati di navigazione?"),
            Some(
                "Vengono rimossi cronologia, cache, cookie, localStorage, IndexedDB e \
                 service worker. Segnalibri, download e permessi per sito restano, \
                 salvo scelta diversa qui sotto.",
            ),
        );

        let options = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let remember = gtk::CheckButton::with_label("Dimentica anche i permessi per sito");
        remember.set_active(false);
        let explain = gtk::Label::new(Some(
            "Geolocalizzazione, fotocamera, microfono, notifiche e appunti \
             torneranno a essere richiesti da ogni sito.",
        ));
        explain.set_xalign(0.0);
        explain.set_wrap(true);
        explain.set_margin_start(24);
        explain.add_css_class("row-subtitle");
        options.append(&remember);
        options.append(&explain);
        dialog.set_extra_child(Some(&options));

        dialog.add_response("cancel", "Annulla");
        dialog.add_response("clear", "Cancella");
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |dialog, response| {
            dialog.close();
            if response != "clear" {
                return;
            }
            let Some(browser) = weak.upgrade() else {
                return;
            };
            browser.clear_all_browsing_data(remember.is_active());
        });

        dialog.present(Some(&self.window));
    }

    fn clear_all_browsing_data(self: &Rc<Self>, include_permissions: bool) {
        if let Err(error) = self.storage.clear_history() {
            self.toast(&format!("Cronologia non cancellata: {error}"));
        }
        if include_permissions {
            match self.storage.clear_permissions() {
                Ok(()) => self.toast("Permessi per sito dimenticati"),
                Err(error) => self.toast(&format!("Permessi non cancellati: {error}")),
            }
        }
        self.clear_website_data(
            webkit::WebsiteDataTypes::MEMORY_CACHE
                | webkit::WebsiteDataTypes::DISK_CACHE
                | webkit::WebsiteDataTypes::COOKIES
                | webkit::WebsiteDataTypes::LOCAL_STORAGE
                | webkit::WebsiteDataTypes::SESSION_STORAGE
                | webkit::WebsiteDataTypes::INDEXEDDB_DATABASES
                | webkit::WebsiteDataTypes::SERVICE_WORKER_REGISTRATIONS,
            "Dati di navigazione",
        );
    }

    /// Selettore rapido di schede su tutti gli spazi (Ctrl+Shift+A).
    fn open_tab_search(self: &Rc<Self>) {
        let window = adw::Window::new();
        window.set_title(Some("Cerca scheda"));
        window.set_transient_for(Some(&self.window));
        window.set_modal(true);
        window.set_default_size(480, 460);

        let toolbar_view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        toolbar_view.add_top_bar(&header);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);

        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some("Filtra per titolo o indirizzo"));
        content.append(&entry);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.add_css_class("dialog-list");
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&list)
            .build();
        content.append(&scroll);
        toolbar_view.set_content(Some(&content));
        window.set_content(Some(&toolbar_view));

        let populate: Rc<dyn Fn(&str)> = {
            let weak = Rc::downgrade(self);
            let list = list.clone();
            Rc::new(move |query: &str| {
                list.remove_all();
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let query = query.trim().to_lowercase();
                let entries = {
                    let state = browser.state.borrow();
                    state
                        .workspaces
                        .iter()
                        .flat_map(|workspace| {
                            workspace.tabs.iter().filter_map(|id| {
                                state.tab(*id).map(|tab| {
                                    (
                                        workspace.name.clone(),
                                        tab.id,
                                        tab.title.clone(),
                                        tab.uri.clone(),
                                    )
                                })
                            })
                        })
                        .filter(|(_, _, title, uri)| {
                            query.is_empty()
                                || title.to_lowercase().contains(&query)
                                || uri.to_lowercase().contains(&query)
                        })
                        .collect::<Vec<_>>()
                };

                if entries.is_empty() {
                    add_empty_state(&list, "Nessuna scheda corrisponde.");
                    return;
                }

                for (workspace, tab_id, title, uri) in entries {
                    let row = gtk::ListBoxRow::new();
                    row.set_widget_name(&tab_id.to_string());

                    let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
                    content.set_margin_top(8);
                    content.set_margin_bottom(8);
                    content.set_margin_start(12);
                    content.set_margin_end(12);

                    let title_label = gtk::Label::new(Some(if title.is_empty() {
                        &uri
                    } else {
                        &title
                    }));
                    title_label.set_xalign(0.0);
                    title_label.set_ellipsize(pango::EllipsizeMode::End);
                    title_label.add_css_class("row-title");

                    let subtitle = gtk::Label::new(Some(&format!("{workspace} · {uri}")));
                    subtitle.set_xalign(0.0);
                    subtitle.set_ellipsize(pango::EllipsizeMode::Middle);
                    subtitle.add_css_class("row-subtitle");

                    content.append(&title_label);
                    content.append(&subtitle);
                    row.set_child(Some(&content));
                    list.append(&row);
                }

                if let Some(first) = list.row_at_index(0) {
                    list.select_row(Some(&first));
                }
            })
        };
        populate("");

        {
            let populate = populate.clone();
            entry.connect_changed(move |entry| populate(&entry.text()));
        }

        {
            let weak = Rc::downgrade(self);
            let window = window.clone();
            list.connect_row_activated(move |_, row| {
                let Ok(tab_id) = row.widget_name().parse::<TabId>() else {
                    return;
                };
                if let Some(browser) = weak.upgrade() {
                    browser.activate_tab(tab_id);
                }
                window.close();
            });
        }

        {
            let list = list.clone();
            let weak = Rc::downgrade(self);
            let window = window.clone();
            entry.connect_activate(move |_| {
                let Some(row) = list.selected_row().or_else(|| list.row_at_index(0)) else {
                    return;
                };
                let Ok(tab_id) = row.widget_name().parse::<TabId>() else {
                    return;
                };
                if let Some(browser) = weak.upgrade() {
                    browser.activate_tab(tab_id);
                }
                window.close();
            });
        }

        {
            let window_for_key = window.clone();
            let key = gtk::EventControllerKey::new();
            key.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    window_for_key.close();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            window.add_controller(key);
        }

        window.present();
        entry.grab_focus();
    }

    fn open_block_exceptions(self: &Rc<Self>) {
        self.open_list_window("Eccezioni al blocco contenuti", |browser, list| {
            fill_block_exceptions(browser, list);
        });
    }

    fn open_permissions(self: &Rc<Self>) {
        self.open_list_window("Permessi per sito", |browser, list| {
            let entries = browser.storage.permissions().unwrap_or_default();
            if entries.is_empty() {
                add_empty_state(
                    list,
                    "Nessun permesso memorizzato: ogni sito chiede il consenso.",
                );
                return;
            }

            let clear_all_row = adw::ActionRow::new();
            clear_all_row.set_title("Dimentica tutti i permessi");
            clear_all_row.set_subtitle("I siti torneranno a chiedere il consenso");
            let clear_all = gtk::Button::with_label("Dimentica");
            clear_all.add_css_class("flat");
            clear_all.set_valign(gtk::Align::Center);
            {
                let weak = Rc::downgrade(browser);
                let list = list.clone();
                clear_all.connect_clicked(move |_| {
                    let Some(browser) = weak.upgrade() else {
                        return;
                    };
                    match browser.storage.clear_permissions() {
                        Ok(()) => {
                            list.remove_all();
                            add_empty_state(&list, "Nessun permesso memorizzato.");
                            browser.toast("Permessi dimenticati");
                        }
                        Err(error) => {
                            browser.toast(&format!("Permessi non cancellati: {error}"))
                        }
                    }
                });
            }
            clear_all_row.add_suffix(&clear_all);
            list.append(&clear_all_row);

            for entry in entries {
                let row = adw::ActionRow::new();
                row.set_title(&entry.origin);
                let state = if entry.decision == "allow" {
                    "Consentito"
                } else {
                    "Bloccato"
                };
                row.set_subtitle(&format!(
                    "{} · {state}",
                    permission_label(&entry.kind)
                ));

                let forget = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text("Dimentica questo permesso")
                    .valign(gtk::Align::Center)
                    .build();
                forget.add_css_class("flat");
                {
                    let weak = Rc::downgrade(browser);
                    let list = list.clone();
                    let origin = entry.origin.clone();
                    let kind = entry.kind.clone();
                    let row_for_remove = row.clone();
                    forget.connect_clicked(move |_| {
                        let Some(browser) = weak.upgrade() else {
                            return;
                        };
                        match browser.storage.remove_permission(&origin, &kind) {
                            Ok(()) => {
                                list.remove(&row_for_remove);
                                browser.toast("Permesso dimenticato");
                            }
                            Err(error) => {
                                browser.toast(&format!("Permesso non rimosso: {error}"))
                            }
                        }
                    });
                }
                row.add_suffix(&forget);
                list.append(&row);
            }
        });
    }

    fn open_extensions(self: &Rc<Self>) {
        self.open_list_window("Estensioni", |browser, list| {
            let folder_row = adw::ActionRow::new();
            folder_row.set_title("Cartella estensioni");
            folder_row.set_subtitle(&browser.paths.extensions_dir.to_string_lossy());
            let open_folder = gtk::Button::with_label("Apri");
            open_folder.add_css_class("flat");
            open_folder.set_valign(gtk::Align::Center);
            {
                let directory = browser.paths.extensions_dir.clone();
                open_folder.connect_clicked(move |_| launch_directory(&directory));
            }
            folder_row.add_suffix(&open_folder);
            list.append(&folder_row);

            let extensions = crate::extensions::load_all(&browser.paths.extensions_dir);
            if extensions.is_empty() {
                add_empty_state(
                    list,
                    "Nessuna estensione. Crea una cartella con manifest.json dentro la cartella estensioni.",
                );
                return;
            }

            for extension in extensions {
                let row = adw::ActionRow::new();
                row.set_title(&extension.name);
                row.set_subtitle(&extension.subtitle());

                row.set_tooltip_text(Some(&format!(
                    "Cartella: {} — siti: {}",
                    extension.id,
                    extension.patterns().join(", ")
                )));
                let badge = gtk::Label::new(Some(&extension.patterns().join(", ")));
                badge.add_css_class("row-subtitle");
                badge.set_valign(gtk::Align::Center);
                badge.set_ellipsize(pango::EllipsizeMode::End);
                badge.set_max_width_chars(28);
                row.add_suffix(&badge);
                list.append(&row);
            }
        });
    }

    // ----------------------------------------------------------------
    // Stampa, modalità lettore, zoom per sito
    // ----------------------------------------------------------------

    fn print_active(self: &Rc<Self>) {
        let Some(webview) = self.active_webview() else {
            self.toast("Nessuna pagina da stampare");
            return;
        };
        let operation = webkit::PrintOperation::new(&webview);
        let _ = operation.run_dialog(Some(&self.window));
    }

    fn toggle_reader(self: &Rc<Self>) {
        let Some(tab_id) = self.active_tab_id() else {
            return;
        };
        let Some(webview) = self.active_webview() else {
            self.toast("Apri una pagina prima di usare la modalità lettore");
            return;
        };

        let reader_active = self
            .state
            .borrow()
            .tab(tab_id)
            .map(|tab| tab.reader)
            .unwrap_or(false);

        if reader_active {
            {
                let mut state = self.state.borrow_mut();
                if let Some(tab) = state.tab_mut(tab_id) {
                    tab.reader = false;
                }
            }
            webview.reload();
            self.toast("Modalità lettore disattivata");
            return;
        }

        let weak = Rc::downgrade(self);
        webview.evaluate_javascript(
            READER_JS,
            None,
            None,
            None::<&gio::Cancellable>,
            move |result| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(_) => {
                        {
                            let mut state = browser.state.borrow_mut();
                            if let Some(tab) = state.tab_mut(tab_id) {
                                tab.reader = true;
                            }
                        }
                        browser.toast("Modalità lettore: Ctrl+Alt+R per uscire");
                    }
                    Err(error) => {
                        browser.toast(&format!("Modalità lettore non disponibile: {error}"))
                    }
                }
            },
        );
    }

    fn change_zoom(self: &Rc<Self>, delta: f64) {
        let Some(webview) = self.active_webview() else {
            return;
        };
        let next = (((webview.zoom_level() + delta).clamp(0.3, 3.0)) * 100.0).round() / 100.0;
        webview.set_zoom_level(next);

        let uri = webview
            .uri()
            .map(|value| value.to_string())
            .unwrap_or_default();
        if let Some(origin) = origin_of(&uri) {
            let _ = self.storage.set_site_zoom(&origin, next);
            self.toast(&format!("Zoom {:.0}% per {origin}", next * 100.0));
        } else {
            self.toast(&format!("Zoom {:.0}%", next * 100.0));
        }
    }

    fn reset_zoom(self: &Rc<Self>) {
        let Some(webview) = self.active_webview() else {
            return;
        };
        webview.set_zoom_level(1.0);
        let uri = webview
            .uri()
            .map(|value| value.to_string())
            .unwrap_or_default();
        if let Some(origin) = origin_of(&uri) {
            let _ = self.storage.clear_site_zoom(&origin);
        }
        self.toast("Zoom ripristinato al 100%");
    }

    fn new_workspace(self: &Rc<Self>) {
        {
            let mut state = self.state.borrow_mut();
            let id = state.next_workspace_id;
            state.next_workspace_id += 1;
            let name = format!("Spazio {}", state.workspaces.len() + 1);
            state.workspaces.push(Workspace {
                id,
                name,
                tabs: Vec::new(),
                active_tab: None,
                groups: Vec::new(),
                // Spazio nuovo: profilo dati nuovo, quindi cookie e sessioni
                // separati da quelli degli altri spazi.
                profile: new_profile_id(),
            });
            state.active_workspace = id;
        }
        self.rebuild_workspace_menu();
        self.new_tab(None, true, false);
        self.save_session();
    }

    fn switch_workspace(self: &Rc<Self>, workspace_id: WorkspaceId) {
        let active_tab = {
            let mut state = self.state.borrow_mut();
            let Some(workspace) = state.workspace(workspace_id) else {
                return;
            };
            let active = workspace.active_tab.or_else(|| workspace.tabs.first().copied());
            state.active_workspace = workspace_id;
            active
        };

        self.rebuild_workspace_menu();
        self.rebuild_tab_list();
        if let Some(tab_id) = active_tab {
            self.activate_tab(tab_id);
        } else {
            self.new_tab(None, true, false);
        }
        self.save_session();
    }

    fn rename_workspace(self: &Rc<Self>) {
        let current_name = self
            .state
            .borrow()
            .active_workspace()
            .name
            .clone();
        self.prompt_text("Rinomina workspace", "Nome", &current_name, {
            let weak = Rc::downgrade(self);
            move |name| {
                if let Some(browser) = weak.upgrade() {
                    let workspace_id = browser.state.borrow().active_workspace;
                    {
                        let mut state = browser.state.borrow_mut();
                        if let Some(workspace) = state.workspace_mut(workspace_id) {
                            workspace.name = name;
                        }
                    }
                    browser.rebuild_workspace_menu();
                    browser.save_session();
                }
            }
        });
    }

    fn delete_workspace(self: &Rc<Self>) {
        let (workspace_id, workspace_name, count) = {
            let state = self.state.borrow();
            let workspace = state.active_workspace();
            (workspace.id, workspace.name.clone(), state.workspaces.len())
        };

        if count <= 1 {
            self.toast("Devi mantenere almeno un workspace");
            return;
        }

        let heading = format!("Eliminare “{workspace_name}”?");
        let dialog = adw::AlertDialog::new(
            Some(&heading),
            Some("Tutte le schede di questo workspace verranno chiuse."),
        );
        dialog.add_response("cancel", "Annulla");
        dialog.add_response("delete", "Elimina");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |dialog: &adw::AlertDialog, response: &str| {
            if response == "delete" {
                if let Some(browser) = weak.upgrade() {
                    browser.remove_workspace(workspace_id);
                }
            }
            dialog.close();
        });
        dialog.present(Some(&self.window));
    }

    fn remove_workspace(self: &Rc<Self>, workspace_id: WorkspaceId) {
        let (removed_ids, active_tab) = {
            let mut state = self.state.borrow_mut();
            if state.workspaces.len() <= 1 {
                return;
            }
            let Some(index) = state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
            else {
                return;
            };
            let removed = state.workspaces.remove(index);
            let next_index = index.min(state.workspaces.len() - 1);
            state.active_workspace = state.workspaces[next_index].id;
            let active_tab = state.workspaces[next_index]
                .active_tab
                .or_else(|| state.workspaces[next_index].tabs.first().copied());
            (removed.tabs, active_tab)
        };

        // Se la scheda di destra è fra quelle eliminate, sganciala prima di
        // distruggere le WebView: altrimenti il riquadro resterebbe con una
        // pagina orfana di uno spazio che non esiste più.
        if let Some(split_id) = *self.split_tab.borrow() {
            if removed_ids.contains(&split_id) {
                self.detach_split_tab(split_id);
            }
        }

        for tab_id in removed_ids {
            let webview = self
                .state
                .borrow_mut()
                .tabs
                .remove(&tab_id)
                .and_then(|tab| tab.webview);
            if let Some(webview) = webview {
                self.stack.remove(&webview);
                self.split_stack.remove(&webview);
            }
            self.tab_widgets.borrow_mut().remove(&tab_id);
        }

        self.rebuild_workspace_menu();
        self.rebuild_tab_list();
        if let Some(tab_id) = active_tab {
            self.activate_tab(tab_id);
        } else {
            self.new_tab(None, true, false);
        }
        self.save_session();
    }

    fn rebuild_workspace_menu(&self) {
        let (active_id, active_name, workspaces) = {
            let state = self.state.borrow();
            (
                state.active_workspace,
                state.active_workspace().name.clone(),
                state
                    .workspaces
                    .iter()
                    .map(|workspace| (workspace.id, workspace.name.clone()))
                    .collect::<Vec<_>>(),
            )
        };

        let menu = gio::Menu::new();
        let workspace_section = gio::Menu::new();
        for (id, name) in workspaces {
            let label = if id == active_id {
                format!("• {name}")
            } else {
                format!("  {name}")
            };
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(
                Some("win.switch-workspace"),
                Some(&id.to_string().to_variant()),
            );
            workspace_section.append_item(&item);
        }
        menu.append_section(None, &workspace_section);

        let actions = gio::Menu::new();
        actions.append(Some("Nuovo workspace"), Some("win.new-workspace"));
        actions.append(
            Some("Rinomina workspace corrente"),
            Some("win.rename-workspace"),
        );
        actions.append(
            Some("Elimina workspace corrente"),
            Some("win.delete-workspace"),
        );
        menu.append_section(None, &actions);

        self.workspace_button.set_menu_model(Some(&menu));
        self.workspace_button.set_label(&active_name);
        self.workspace_button
            .set_tooltip_text(Some(&format!("Workspace: {active_name}")));
    }

    fn toggle_bookmark(&self) {
        let (uri, title, is_start_page) = {
            let state = self.state.borrow();
            let Some(tab_id) = state.active_tab_id() else {
                return;
            };
            let Some(tab) = state.tab(tab_id) else {
                return;
            };
            (tab.uri.clone(), tab.title.clone(), tab.is_start_page)
        };

        if is_start_page || uri.is_empty() || !is_history_url(&uri) {
            self.toast("Questa pagina non può essere aggiunta ai segnalibri");
            return;
        }

        let bookmarked = self.storage.is_bookmarked(&uri).unwrap_or(false);
        let result = if bookmarked {
            self.storage.remove_bookmark_by_url(&uri)
        } else {
            self.storage.add_bookmark(&uri, &title)
        };

        match result {
            Ok(()) => {
                self.sync_toolbar();
                self.toast(if bookmarked {
                    "Segnalibro rimosso"
                } else {
                    "Pagina aggiunta ai segnalibri"
                });
            }
            Err(error) => self.toast(&format!("Errore segnalibri: {error}")),
        }
    }

    fn open_bookmarks(self: &Rc<Self>) {
        self.open_list_window("Segnalibri", |browser, list| {
            let bookmarks = browser.storage.bookmarks().unwrap_or_default();
            if bookmarks.is_empty() {
                add_empty_state(list, "Nessun segnalibro. Usa Ctrl+D su una pagina.");
                return;
            }
            for bookmark in bookmarks {
                browser.add_bookmark_row(list, bookmark);
            }
        });
    }

    fn add_bookmark_row(self: &Rc<Self>, list: &gtk::ListBox, bookmark: Bookmark) {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("dialog-row");

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(12);
        content.set_margin_end(8);

        let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
        labels.set_hexpand(true);
        let title = gtk::Label::new(Some(if bookmark.title.is_empty() {
            &bookmark.url
        } else {
            &bookmark.title
        }));
        title.set_xalign(0.0);
        title.set_ellipsize(pango::EllipsizeMode::End);
        title.add_css_class("row-title");
        let url = gtk::Label::new(Some(&bookmark.url));
        url.set_xalign(0.0);
        url.set_ellipsize(pango::EllipsizeMode::Middle);
        url.add_css_class("row-subtitle");
        labels.append(&title);
        labels.append(&url);

        let open = gtk::Button::builder()
            .icon_name("go-next-symbolic")
            .tooltip_text("Apri in una nuova scheda")
            .has_frame(false)
            .build();
        {
            let weak = Rc::downgrade(self);
            let url = bookmark.url.clone();
            open.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    browser.new_tab(Some(url.clone()), true, false);
                }
            });
        }

        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Rimuovi segnalibro")
            .has_frame(false)
            .build();
        {
            let weak = Rc::downgrade(self);
            let list = list.clone();
            let row = row.clone();
            let id = bookmark.id;
            remove.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    let _ = browser.storage.remove_bookmark(id);
                    list.remove(&row);
                    browser.sync_toolbar();
                }
            });
        }

        content.append(&labels);
        content.append(&open);
        content.append(&remove);
        row.set_child(Some(&content));
        list.append(&row);
    }

    fn open_history(self: &Rc<Self>) {
        let window = adw::Window::new();
        window.set_title(Some("Cronologia"));
        window.set_transient_for(Some(&self.window));
        window.set_modal(true);
        window.set_default_size(680, 560);

        let toolbar_view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        toolbar_view.add_top_bar(&header);

        let order = gtk::DropDown::from_strings(&["Più recenti", "Più visitati"]);
        order.set_tooltip_text(Some("Ordina la cronologia"));
        order.set_selected(0);
        header.pack_end(&order);

        let clear = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Cancella la cronologia")
            .build();
        header.pack_end(&clear);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("dialog-list");

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&list)
            .build();

        toolbar_view.set_content(Some(&scroll));
        window.set_content(Some(&toolbar_view));

        let reload: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

        let refresh: Rc<dyn Fn()> = {
            let weak = Rc::downgrade(self);
            let list = list.clone();
            let order = order.clone();
            Rc::new(move || {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                list.remove_all();
                let mode = if order.selected() == 1 {
                    HistoryOrder::MostVisited
                } else {
                    HistoryOrder::Recent
                };
                let entries = browser.storage.history(mode, 300).unwrap_or_default();
                if entries.is_empty() {
                    add_empty_state(&list, "La cronologia è vuota.");
                    return;
                }
                for entry in entries {
                    browser.add_history_row(&list, entry);
                }
            })
        };
        *reload.borrow_mut() = Some(refresh.clone());

        {
            let reload = reload.clone();
            order.connect_selected_notify(move |_| {
                if let Some(refresh) = reload.borrow().clone() {
                    refresh();
                }
            });
        }
        {
            let weak = Rc::downgrade(self);
            let reload = reload.clone();
            clear.connect_clicked(move |_| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                match browser.storage.clear_history() {
                    Ok(()) => browser.toast("Cronologia cancellata"),
                    Err(error) => browser.toast(&format!("Errore: {error}")),
                }
                if let Some(refresh) = reload.borrow().clone() {
                    refresh();
                }
            });
        }

        refresh();
        window.present();
    }

    fn add_history_row(self: &Rc<Self>, list: &gtk::ListBox, entry: HistoryEntry) {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("dialog-row");

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(12);
        content.set_margin_end(8);

        let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
        labels.set_hexpand(true);
        let title = gtk::Label::new(Some(if entry.title.is_empty() {
            &entry.url
        } else {
            &entry.title
        }));
        title.set_xalign(0.0);
        title.set_ellipsize(pango::EllipsizeMode::End);
        title.add_css_class("row-title");
        let url = gtk::Label::new(Some(&entry.url));
        url.set_xalign(0.0);
        url.set_ellipsize(pango::EllipsizeMode::Middle);
        url.add_css_class("row-subtitle");
        labels.append(&title);
        labels.append(&url);

        let visit_text = if entry.visits == 1 {
            "1 visita".to_owned()
        } else {
            format!("{} visite", entry.visits)
        };
        let visits = gtk::Label::new(Some(&visit_text));
        visits.add_css_class("row-subtitle");
        visits.set_valign(gtk::Align::Center);

        let open = gtk::Button::builder()
            .icon_name("go-next-symbolic")
            .tooltip_text("Apri in una nuova scheda")
            .has_frame(false)
            .build();
        {
            let weak = Rc::downgrade(self);
            let url = entry.url.clone();
            open.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    browser.new_tab(Some(url.clone()), true, false);
                }
            });
        }

        content.append(&labels);
        content.append(&visits);
        content.append(&open);
        row.set_child(Some(&content));
        list.append(&row);
    }

    fn open_downloads(self: &Rc<Self>) {
        self.open_list_window("Download", |browser, list| {
            let entries = browser.storage.downloads().unwrap_or_default();
            if entries.is_empty() {
                add_empty_state(list, "Nessun download.");
                return;
            }
            for entry in entries {
                browser.add_download_row(list, entry);
            }
        });
    }

    fn add_download_row(self: &Rc<Self>, list: &gtk::ListBox, entry: DownloadEntry) {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("dialog-row");

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(12);
        content.set_margin_end(8);

        let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
        labels.set_hexpand(true);
        let title = gtk::Label::new(Some(&entry.filename));
        title.set_xalign(0.0);
        title.set_ellipsize(pango::EllipsizeMode::End);
        title.add_css_class("row-title");
        let path = gtk::Label::new(Some(&entry.destination));
        path.set_xalign(0.0);
        path.set_ellipsize(pango::EllipsizeMode::Middle);
        path.add_css_class("row-subtitle");
        labels.append(&title);
        labels.append(&path);

        let status = gtk::Label::new(Some(entry.status_label()));
        status.set_valign(gtk::Align::Center);
        if entry.status == "failed" {
            status.add_css_class("download-failed");
        } else {
            status.add_css_class("row-subtitle");
        }

        let open_folder = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("Apri cartella")
            .has_frame(false)
            .build();
        {
            let destination = PathBuf::from(&entry.destination);
            open_folder.connect_clicked(move |_| {
                if let Some(parent) = destination.parent() {
                    let uri = url::Url::from_directory_path(parent)
                        .map(|url| url.to_string())
                        .unwrap_or_default();
                    if !uri.is_empty() {
                        let _ = gio::AppInfo::launch_default_for_uri(
                            &uri,
                            None::<&gio::AppLaunchContext>,
                        );
                    }
                }
            });
        }

        let open_file = gtk::Button::builder()
            .icon_name("document-open-symbolic")
            .tooltip_text("Apri file")
            .has_frame(false)
            .build();
        {
            let destination = entry.destination.clone();
            open_file.connect_clicked(move |_| {
                if let Ok(uri) = url::Url::from_file_path(&destination) {
                    let _ = gio::AppInfo::launch_default_for_uri(
                        uri.as_str(),
                        None::<&gio::AppLaunchContext>,
                    );
                }
            });
        }

        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Rimuovi dalla lista")
            .has_frame(false)
            .build();
        {
            let weak = Rc::downgrade(self);
            let list = list.clone();
            let row_for_remove = row.clone();
            let id = entry.id;
            remove.connect_clicked(move |_| {
                if let Some(browser) = weak.upgrade() {
                    let _ = browser.storage.remove_download(id);
                    list.remove(&row_for_remove);
                }
            });
        }

        content.append(&labels);
        content.append(&status);
        content.append(&open_file);
        content.append(&open_folder);
        content.append(&remove);
        row.set_child(Some(&content));
        list.append(&row);
    }

    // ------------------------------------------------------------ Importazione

    /// Sceglie un file esportato da un altro browser e lo importa.
    fn import_data(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::new();
        dialog.set_title("Importa dati");
        dialog.set_accept_label(Some("Importa"));
        dialog.set_modal(true);

        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Cronologia, segnalibri, account"));
        for pattern in [
            "*.csv",
            "*.json",
            "*.html",
            "*.htm",
            "*.sqlite",
            "*.db",
            "*History*",
            "*places.sqlite",
            "*Bookmarks*",
            "*logins*",
        ] {
            filter.add_pattern(pattern);
        }
        let every = gtk::FileFilter::new();
        every.set_name(Some("Tutti i file"));
        every.add_pattern("*");

        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        filters.append(&every);
        dialog.set_filters(Some(&filters));
        dialog.set_default_filter(Some(&filter));

        let weak = Rc::downgrade(self);
        dialog.open(Some(&self.window), None::<&gio::Cancellable>, move |result| {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            let Ok(file) = result else {
                return;
            };
            let Some(path) = file.path() else {
                browser.show_import_summary(
                    "Importazione non riuscita",
                    "Il file scelto non si trova sul disco.",
                );
                return;
            };
            browser.run_import(&path);
        });
    }

    fn run_import(self: &Rc<Self>, path: &Path) {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.show_import_summary(
                    "Importazione non riuscita",
                    &format!("Non riesco a leggere il file: {error}"),
                );
                return;
            }
        };
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        match crate::import::parse(&bytes, name) {
            Ok(data) => self.apply_import(data),
            Err(error) => {
                self.show_import_summary("Importazione non riuscita", &format!("{error:#}"))
            }
        }
    }

    fn apply_import(self: &Rc<Self>, data: crate::import::ImportData) {
        if data.is_empty() && data.notes.is_empty() {
            self.show_import_summary(
                "Nessun dato",
                "Il file non conteneva elementi da importare.",
            );
            return;
        }

        let mut lines: Vec<String> = Vec::new();

        let visits = data
            .history
            .iter()
            .map(|visit| (visit.url.clone(), visit.title.clone(), visit.visited_at))
            .collect::<Vec<(String, String, Option<i64>)>>();
        match self.storage.import_history(&visits) {
            Ok(count) => lines.push(format!("Cronologia: {count} visite")),
            Err(error) => lines.push(format!("Cronologia non importata: {error}")),
        }

        let bookmarks = data
            .bookmarks
            .iter()
            .map(|bookmark| (bookmark.url.clone(), bookmark.title.clone()))
            .collect::<Vec<(String, String)>>();
        match self.storage.add_bookmarks_bulk(&bookmarks) {
            Ok(count) => lines.push(format!("Segnalibri: {count}")),
            Err(error) => lines.push(format!("Segnalibri non importati: {error}")),
        }

        if !data.credentials.is_empty() {
            match crate::keyring::passwords_key() {
                Ok(key) => {
                    let mut rows = Vec::new();
                    for credential in &data.credentials {
                        match crate::crypto::encrypt(&key, credential.password.as_bytes()) {
                            Ok((nonce, secret)) => rows.push((
                                credential.host(),
                                credential.url.clone(),
                                credential.username.clone(),
                                nonce.to_vec(),
                                secret,
                            )),
                            Err(error) => {
                                lines.push(format!("Password non cifrata: {error}"))
                            }
                        }
                    }
                    match self.storage.save_credentials(&rows) {
                        Ok(count) => lines.push(format!("Account e password: {count}")),
                        Err(error) => lines.push(format!("Account non salvati: {error}")),
                    }
                }
                Err(error) => lines.push(format!(
                    "Password non importate: {error}. Restano fuori dal database invece \
                     di essere salvate in chiaro."
                )),
            }
        }

        lines.extend(data.notes);
        if !data.credentials.is_empty() {
            lines.push(
                "Le password sono sul dispositivo, cifrate con una chiave del portachiavi \
                 di sistema; le copi da «Account e password»."
                    .to_owned(),
            );
        }
        self.show_import_summary("Importazione completata", &lines.join("\n"));
        self.sync_toolbar();
    }

    fn show_import_summary(&self, heading: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_response("close", "Chiudi");
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        let dialog_weak = dialog.clone();
        dialog.connect_response(None, move |_, _| {
            dialog_weak.close();
        });
        dialog.present(Some(&self.window));
    }

    /// Elenco degli account importati; la password si copia negli appunti
    /// senza mai mostrarla a schermo.
    fn open_credentials(self: &Rc<Self>) {
        self.open_list_window("Account e password", |browser, list| {
            let key = crate::keyring::passwords_key();
            let entries = browser.storage.credentials().unwrap_or_default();
            if entries.is_empty() {
                add_empty_state(
                    list,
                    "Nessun account salvato. Usa «Importa dati…» per aggiungerne.",
                );
                return;
            }
            for entry in entries {
                browser.add_credential_row(list, entry, key.clone());
            }
        });
    }

    fn add_credential_row(
        self: &Rc<Self>,
        list: &gtk::ListBox,
        entry: StoredCredential,
        key: std::result::Result<[u8; 32], String>,
    ) {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("dialog-row");

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(12);
        content.set_margin_end(8);

        let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
        labels.set_hexpand(true);
        let host = gtk::Label::new(Some(&entry.host));
        // L'indirizzo completo resta nel suggerimento: più account sullo stesso
        // host (porti o percorsi diversi) restano distinguibili.
        host.set_tooltip_text(Some(&entry.url));
        host.set_xalign(0.0);
        host.set_ellipsize(pango::EllipsizeMode::End);
        host.add_css_class("row-title");
        let username_text = if entry.username.is_empty() {
            "senza nome utente"
        } else {
            entry.username.as_str()
        };
        let username = gtk::Label::new(Some(username_text));
        username.set_xalign(0.0);
        username.set_ellipsize(pango::EllipsizeMode::Middle);
        username.add_css_class("row-subtitle");
        labels.append(&host);
        labels.append(&username);

        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text("Copia la password")
            .has_frame(false)
            .build();
        {
            let weak = Rc::downgrade(self);
            let nonce = entry.nonce.clone();
            let secret = entry.secret.clone();
            copy.connect_clicked(move |_| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let key = match key.clone() {
                    Ok(key) => key,
                    Err(error) => {
                        browser.toast(&format!("Password non leggibile: {error}"));
                        return;
                    }
                };
                let Ok(nonce) = <[u8; 12]>::try_from(nonce.as_slice()) else {
                    browser.toast("La password salvata è danneggiata");
                    return;
                };
                let password =
                    String::from_utf8_lossy(&crate::crypto::decrypt(&key, &nonce, &secret))
                        .to_string();
                browser.window.clipboard().set_text(&password);
                browser.toast("Password copiata negli appunti");
            });
        }

        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Elimina account")
            .has_frame(false)
            .build();
        {
            let weak = Rc::downgrade(self);
            let list = list.clone();
            let row_for_remove = row.clone();
            let id = entry.id;
            remove.connect_clicked(move |_| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                if let Err(error) = browser.storage.remove_credential(id) {
                    browser.toast(&format!("Eliminazione non riuscita: {error}"));
                    return;
                }
                list.remove(&row_for_remove);
            });
        }

        content.append(&labels);
        content.append(&copy);
        content.append(&remove);
        row.set_child(Some(&content));
        list.append(&row);
    }

    fn open_list_window<F>(self: &Rc<Self>, title: &str, populate: F)
    where
        F: FnOnce(&Rc<Self>, &gtk::ListBox) + 'static,
    {
        let window = adw::Window::new();
        window.set_title(Some(title));
        window.set_transient_for(Some(&self.window));
        window.set_modal(true);
        window.set_default_size(620, 520);

        let toolbar_view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        toolbar_view.add_top_bar(&header);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("dialog-list");

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&list)
            .build();

        toolbar_view.set_content(Some(&scroll));
        window.set_content(Some(&toolbar_view));
        populate(self, &list);
        window.present();
    }

    fn open_preferences(self: &Rc<Self>) {
        let prefs = adw::PreferencesDialog::new();
        prefs.set_title("Impostazioni");
        prefs.set_content_width(720);
        prefs.set_content_height(760);
        prefs.set_search_enabled(false);

        let page = adw::PreferencesPage::new();
        page.set_title("Generali");
        page.set_icon_name(Some("emblem-system-symbolic"));

        // ------------------------------------------------------------ Ricerca
        let search_group = adw::PreferencesGroup::new();
        search_group.set_title("Motore di ricerca");
        search_group.set_description(Some(
            "DuckDuckGo resta sempre disponibile. Nei motori personalizzati usa {query} come segnaposto. \
             La parola chiave permette di scrivere, per esempio, «g rust» nella barra degli indirizzi.",
        ));

        let combo = adw::ComboRow::new();
        combo.set_title("Motore predefinito");
        search_group.add(&combo);

        let engines_list = gtk::ListBox::new();
        engines_list.set_selection_mode(gtk::SelectionMode::None);
        engines_list.add_css_class("boxed-list");
        search_group.add(&engines_list);

        let add_row = adw::ActionRow::new();
        add_row.set_title("Aggiungi motore di ricerca");
        add_row.set_subtitle("Nome, parola chiave facoltativa e URL con {query}");
        let add_button = gtk::Button::with_label("Aggiungi");
        add_button.set_valign(gtk::Align::Center);
        add_button.add_css_class("flat");
        add_row.add_suffix(&add_button);
        search_group.add(&add_row);
        page.add(&search_group);

        // ------------------------------------------------------------ Aspetto
        let appearance = adw::PreferencesGroup::new();
        appearance.set_title("Aspetto");

        let theme_row = adw::ComboRow::new();
        theme_row.set_title("Tema");
        theme_row.set_subtitle("«Segui il sistema» usa il tema chiaro o scuro di Wayland");
        let theme_model = gtk::StringList::new(&["Segui il sistema", "Chiaro", "Scuro"]);
        theme_row.set_model(Some(&theme_model));
        theme_row.set_selected(self.state.borrow().settings.theme.index());
        appearance.add(&theme_row);
        page.add(&appearance);

        // ------------------------------------------------------------ Navigazione
        let navigation = adw::PreferencesGroup::new();
        navigation.set_title("Navigazione");

        let start_page_value = self.state.borrow().settings.start_page.clone();
        let start_row = adw::ActionRow::new();
        start_row.set_title("Pagina iniziale personalizzata");
        start_row.set_subtitle(if start_page_value.trim().is_empty() {
            "Pagina interna di Peats"
        } else {
            start_page_value.as_str()
        });

        let edit_start = gtk::Button::with_label("Modifica");
        edit_start.add_css_class("flat");
        edit_start.set_valign(gtk::Align::Center);
        {
            let weak = Rc::downgrade(self);
            let current = start_page_value.clone();
            edit_start.connect_clicked(move |_| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let weak_inner = Rc::downgrade(&browser);
                let current = current.clone();
                browser.prompt_text(
                    "Pagina iniziale",
                    "URL della pagina iniziale personalizzata",
                    &current,
                    move |value| {
                        let Some(browser) = weak_inner.upgrade() else {
                            return;
                        };
                        let template = browser.search_template();
                        let normalized = resolve_input(&value, &template).unwrap_or(value);
                        {
                            let mut state = browser.state.borrow_mut();
                            state.settings.start_page = normalized;
                        }
                        browser.save_settings();
                        browser.toast("Pagina iniziale aggiornata");
                    },
                );
            });
        }

        let reset_start = gtk::Button::builder()
            .icon_name("edit-undo-symbolic")
            .tooltip_text("Torna alla pagina iniziale interna")
            .valign(gtk::Align::Center)
            .build();
        reset_start.add_css_class("flat");
        {
            let weak = Rc::downgrade(self);
            reset_start.connect_clicked(move |_| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                {
                    let mut state = browser.state.borrow_mut();
                    state.settings.start_page.clear();
                }
                browser.save_settings();
                browser.toast("Pagina iniziale interna ripristinata");
            });
        }

        start_row.add_suffix(&edit_start);
        start_row.add_suffix(&reset_start);
        navigation.add(&start_row);

        let suggestions_row = adw::SwitchRow::new();
        suggestions_row.set_title("Suggerimenti nella barra indirizzi");
        suggestions_row.set_subtitle(
            "Schede aperte, segnalibri e cronologia sempre; la ricerca rapida \
             interroga DuckDuckGo solo con questa opzione attiva",
        );
        suggestions_row.set_active(self.state.borrow().settings.search_suggestions);
        navigation.add(&suggestions_row);
        page.add(&navigation);

        // ---------------------------------------------------------- Prestazioni
        let performance = adw::PreferencesGroup::new();
        performance.set_title("Prestazioni");
        performance.set_description(Some(
            "Le schede inattive possono liberare memoria; i dati della sessione restano su disco.",
        ));

        let unload_row = adw::SwitchRow::new();
        unload_row.set_title("Scarica le schede inattive");
        unload_row.set_subtitle(
            "Dopo 10 minuti senza uso la scheda viene scaricata e ricaricata alla riapertura",
        );
        unload_row.set_active(self.state.borrow().settings.unload_inactive_tabs);
        performance.add(&unload_row);

        let retention_days = self.state.borrow().settings.history_retention_days;
        let retention_adjustment =
            gtk::Adjustment::new(retention_days as f64, 0.0, 3650.0, 30.0, 90.0, 0.0);
        let retention_row = adw::SpinRow::new(Some(&retention_adjustment), 30.0, 0);
        retention_row.set_title("Conserva la cronologia");
        retention_row.set_subtitle("Giorni mantenuti (0 = nessun limite)");
        performance.add(&retention_row);
        page.add(&performance);

        // ------------------------------------------------------------ Privacy
        let privacy = adw::PreferencesGroup::new();
        privacy.set_title("Privacy");
        privacy.set_description(Some(
            "Filtro incorporato per annunci e tracker, applicato a tutte le schede.",
        ));

        let ads_row = adw::SwitchRow::new();
        ads_row.set_title("Blocco contenuti di base");
        ads_row.set_subtitle("Blocca annunci, tracker e finestre pop-up pubblicitarie");
        ads_row.set_active(self.state.borrow().settings.block_ads);
        privacy.add(&ads_row);

        let cosmetic_row = adw::SwitchRow::new();
        cosmetic_row.set_title("Nascondi gli spazi pubblicitari");
        cosmetic_row.set_subtitle(
            "Regole CSS che eliminano i contenitori vuoti lasciati dal blocco",
        );
        cosmetic_row.set_active(self.state.borrow().settings.cosmetic_blocking);
        privacy.add(&cosmetic_row);

        let third_party_row = adw::SwitchRow::new();
        third_party_row.set_title("Blocca cookie di terze parti");
        third_party_row.set_subtitle(
            "Il motore rifiuta i cookie dei domini diversi dalla pagina visitata",
        );
        third_party_row.set_active(self.state.borrow().settings.block_third_party_cookies);
        privacy.add(&third_party_row);

        let signals_row = adw::SwitchRow::new();
        signals_row.set_title("Segnali di privacy per le pagine");
        signals_row.set_subtitle(
            "Global Privacy Control e Do Not Track via JavaScript: non sono header HTTP",
        );
        signals_row.set_active(self.state.borrow().settings.privacy_signals);
        privacy.add(&signals_row);

        let consent_row = adw::SwitchRow::new();
        consent_row.set_title("Gestione banner cookie");
        consent_row.set_subtitle(&format!(
            "Clicca solo il rifiuto previsto da: {}",
            crate::consent::cmp_names().join(", ")
        ));
        consent_row.set_active(self.state.borrow().settings.cookie_banner_helper);
        privacy.add(&consent_row);

        privacy.add(&self.data_row(
            "Eccezioni al blocco contenuti",
            "Siti in cui la protezione è disattivata",
            "Gestisci",
            |browser| browser.open_block_exceptions(),
        ));
        privacy.add(&self.data_row(
            "Permessi per sito",
            "Geolocalizzazione, fotocamera, microfono, notifiche e appunti",
            "Gestisci",
            |browser| browser.open_permissions(),
        ));
        page.add(&privacy);

        {
            let weak = Rc::downgrade(self);
            ads_row.connect_active_notify(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let enabled = row.is_active();
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.block_ads == enabled {
                        false
                    } else {
                        state.settings.block_ads = enabled;
                        true
                    }
                };
                if changed {
                    browser.refresh_content_filter();
                    browser.save_settings();
                    browser.toast(if enabled {
                        "Blocco contenuti attivo"
                    } else {
                        "Blocco contenuti disattivo"
                    });
                }
            });
        }

        {
            let weak = Rc::downgrade(self);
            cosmetic_row.connect_active_notify(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let enabled = row.is_active();
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.cosmetic_blocking == enabled {
                        false
                    } else {
                        state.settings.cosmetic_blocking = enabled;
                        true
                    }
                };
                if changed {
                    browser.refresh_content_filter();
                    browser.save_settings();
                    browser.toast(if enabled {
                        "Spazi pubblicitari nascosti"
                    } else {
                        "Spazi pubblicitari visibili di nuovo"
                    });
                }
            });
        }

        {
            let weak = Rc::downgrade(self);
            unload_row.connect_active_notify(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let enabled = row.is_active();
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.unload_inactive_tabs == enabled {
                        false
                    } else {
                        state.settings.unload_inactive_tabs = enabled;
                        true
                    }
                };
                if changed {
                    browser.save_settings();
                    browser.toast(if enabled {
                        "Le schede inattive verranno scaricate"
                    } else {
                        "Le schede restano in memoria"
                    });
                }
            });
        }

        {
            let weak = Rc::downgrade(self);
            retention_row.connect_value_notify(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let days = row.value() as i64;
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.history_retention_days == days {
                        false
                    } else {
                        state.settings.history_retention_days = days;
                        true
                    }
                };
                if !changed {
                    return;
                }
                browser.save_settings();
                match browser.storage.prune_history(days) {
                    Ok(removed) if removed > 0 => {
                        browser.toast(&format!("Cronologia: {removed} visite rimosse"))
                    }
                    Ok(_) => browser.toast("Impostazione della cronologia salvata"),
                    Err(error) => browser.toast(&format!("Potatura non riuscita: {error}")),
                }
            });
        }

        {
            let weak = Rc::downgrade(self);
            third_party_row.connect_active_notify(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let enabled = row.is_active();
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.block_third_party_cookies == enabled {
                        false
                    } else {
                        state.settings.block_third_party_cookies = enabled;
                        true
                    }
                };
                if changed {
                    browser.apply_network_privacy();
                    browser.save_settings();
                    browser.toast(if enabled {
                        "Cookie di terze parti bloccati"
                    } else {
                        "Cookie di terze parti consentiti"
                    });
                }
            });
        }

        {
            let weak = Rc::downgrade(self);
            signals_row.connect_active_notify(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let enabled = row.is_active();
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.privacy_signals == enabled {
                        false
                    } else {
                        state.settings.privacy_signals = enabled;
                        true
                    }
                };
                if changed {
                    browser.refresh_privacy_scripts();
                    browser.save_settings();
                    browser.toast(if enabled {
                        "Segnali di privacy attivi"
                    } else {
                        "Segnali di privacy disattivati"
                    });
                }
            });
        }

        {
            let weak = Rc::downgrade(self);
            consent_row.connect_active_notify(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let enabled = row.is_active();
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.cookie_banner_helper == enabled {
                        false
                    } else {
                        state.settings.cookie_banner_helper = enabled;
                        true
                    }
                };
                if changed {
                    browser.refresh_privacy_scripts();
                    browser.save_settings();
                    browser.toast(if enabled {
                        "Gestione banner cookie attiva"
                    } else {
                        "Gestione banner cookie disattiva"
                    });
                }
            });
        }

        {
            let weak = Rc::downgrade(self);
            suggestions_row.connect_active_notify(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let enabled = row.is_active();
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.search_suggestions == enabled {
                        false
                    } else {
                        state.settings.search_suggestions = enabled;
                        true
                    }
                };
                if changed {
                    browser.save_settings();
                    if !enabled {
                        browser.close_suggestions();
                    }
                    browser.toast(if enabled {
                        "Suggerimenti di ricerca attivi"
                    } else {
                        "Suggerimenti di ricerca disattivati"
                    });
                }
            });
        }

        // ------------------------------------------------------------ Dati
        let data_group = adw::PreferencesGroup::new();
        data_group.set_title("Dati e cache");
        data_group.set_description(Some(
            "Pagine interne: about:settings · about:history · about:downloads · \
             about:bookmarks · about:extensions · about:sessions · about:closed-tabs",
        ));
        data_group.add(&self.data_row(
            "Importa dati",
            "Cronologia, segnalibri e account/password da Chrome, Firefox o file CSV, JSON e HTML",
            "Importa…",
            |browser| browser.import_data(),
        ));
        data_group.add(&self.data_row(
            "Account e password",
            "Voci salvate sul dispositivo, cifrate con la chiave del portachiavi di sistema",
            "Apri",
            |browser| browser.open_credentials(),
        ));
        data_group.add(&self.data_row(
            "Cancella la cache",
            "Rimuove le risorse salvate su disco e in memoria",
            "Cancella",
            |browser| {
                browser.clear_website_data(
                    webkit::WebsiteDataTypes::MEMORY_CACHE | webkit::WebsiteDataTypes::DISK_CACHE,
                    "Cache",
                );
            },
        ));
        data_group.add(&self.data_row(
            "Cancella cookie e dati dei siti",
            "Cookie, localStorage, IndexedDB e service worker",
            "Cancella",
            |browser| {
                browser.clear_website_data(
                    webkit::WebsiteDataTypes::COOKIES
                        | webkit::WebsiteDataTypes::LOCAL_STORAGE
                        | webkit::WebsiteDataTypes::SESSION_STORAGE
                        | webkit::WebsiteDataTypes::INDEXEDDB_DATABASES
                        | webkit::WebsiteDataTypes::SERVICE_WORKER_REGISTRATIONS,
                    "Cookie e dati dei siti",
                );
            },
        ));
        data_group.add(&self.data_row(
            "Cancella decisioni sui certificati (HSTS)",
            "Dimentica le eccezioni HSTS memorizzate",
            "Cancella",
            |browser| {
                browser.clear_website_data(webkit::WebsiteDataTypes::HSTS_CACHE, "Decisioni HSTS");
            },
        ));
        data_group.add(&self.data_row(
            "Cancella account e password",
            "Rimuove le voci importate dal database locale",
            "Cancella",
            |browser| match browser.storage.clear_credentials() {
                Ok(count) => browser.toast(&format!("{count} account rimossi")),
                Err(error) => browser.toast(&format!("Errore: {error}")),
            },
        ));
        data_group.add(&self.data_row(
            "Cancella la cronologia",
            "Rimuove tutte le visite registrate",
            "Cancella",
            |browser| match browser.storage.clear_history() {
                Ok(()) => browser.toast("Cronologia cancellata"),
                Err(error) => browser.toast(&format!("Errore: {error}")),
            },
        ));
        page.add(&data_group);

        // ------------------------------------------------------- Aggiornamenti
        let updates = adw::PreferencesGroup::new();
        updates.set_title("Aggiornamenti");
        updates.set_description(Some(
            "Peats legge un manifest JSON (version, url del pacchetto RPM, sha256, notes) \
             dall'indirizzo indicato e propone l'installazione quando la versione è più \
             recente. Il controllo automatico avviene una volta al giorno.",
        ));

        let check_row = adw::SwitchRow::new();
        check_row.set_title("Controlla gli aggiornamenti");
        check_row.set_subtitle("Una richiesta al giorno solo all'indirizzo indicato qui sotto");
        check_row.set_active(self.state.borrow().settings.check_updates);
        updates.add(&check_row);

        let update_url_row = adw::EntryRow::new();
        update_url_row.set_title("Manifest degli aggiornamenti (HTTPS)");
        update_url_row.set_show_apply_button(true);
        update_url_row.set_text(&self.state.borrow().settings.update_url);
        updates.add(&update_url_row);

        updates.add(&self.data_row(
            "Controlla adesso",
            "Verifica subito se c'è una versione più recente",
            "Controlla",
            |browser| browser.maybe_check_updates(true),
        ));
        page.add(&updates);

        {
            let weak = Rc::downgrade(self);
            check_row.connect_active_notify(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let enabled = row.is_active();
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.check_updates == enabled {
                        false
                    } else {
                        state.settings.check_updates = enabled;
                        true
                    }
                };
                if changed {
                    browser.save_settings();
                    browser.toast(if enabled {
                        "Controllo aggiornamenti attivo"
                    } else {
                        "Controllo aggiornamenti disattivato"
                    });
                }
            });
        }

        {
            let weak = Rc::downgrade(self);
            update_url_row.connect_apply(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let url = row.text().trim().to_owned();
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.update_url == url {
                        false
                    } else {
                        state.settings.update_url = url;
                        true
                    }
                };
                if changed {
                    browser.save_settings();
                    browser.toast("Indirizzo del manifest salvato");
                }
            });
        }

        prefs.add(&page);

        // ------------------------------------------------------------ Comportamento
        let refresh_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let syncing = Rc::new(Cell::new(false));

        let refresh: Rc<dyn Fn()> = {
            let weak = Rc::downgrade(self);
            let combo = combo.clone();
            let engines_list = engines_list.clone();
            let refresh_slot = refresh_slot.clone();
            let syncing = syncing.clone();
            Rc::new(move || {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let (engines, selected) = {
                    let state = browser.state.borrow();
                    (
                        state.settings.engines.clone(),
                        state.settings.default_engine.clone(),
                    )
                };

                syncing.set(true);
                let names = engines
                    .iter()
                    .map(|engine| engine.name.as_str())
                    .collect::<Vec<_>>();
                combo.set_model(Some(&gtk::StringList::new(&names)));
                combo.set_selected(
                    engines
                        .iter()
                        .position(|engine| engine.id == selected)
                        .unwrap_or(0) as u32,
                );
                syncing.set(false);

                engines_list.remove_all();
                for engine in &engines {
                    let row = adw::ActionRow::new();
                    row.set_title(&engine.name);
                    let keyword = engine
                        .keyword_token()
                        .map(|token| format!("!{token}"))
                        .unwrap_or_default();
                    let subtitle = if keyword.is_empty() {
                        engine.template.clone()
                    } else {
                        format!("{keyword} · {}", engine.template)
                    };
                    row.set_subtitle(&subtitle);

                    if engine.builtin {
                        let badge = gtk::Label::new(Some("integrato"));
                        badge.add_css_class("row-subtitle");
                        badge.set_valign(gtk::Align::Center);
                        row.add_suffix(&badge);
                        engines_list.append(&row);
                        continue;
                    }

                    let edit = gtk::Button::builder()
                        .icon_name("document-edit-symbolic")
                        .tooltip_text("Modifica motore")
                        .valign(gtk::Align::Center)
                        .build();
                    edit.add_css_class("flat");
                    {
                        let weak = Rc::downgrade(&browser);
                        let refresh_slot = refresh_slot.clone();
                        let engine_id = engine.id.clone();
                        edit.connect_clicked(move |_| {
                            let Some(browser) = weak.upgrade() else {
                                return;
                            };
                            let existing = {
                                let state = browser.state.borrow();
                                state.settings.find_engine(&engine_id).cloned()
                            };
                            let Some(existing) = existing else {
                                return;
                            };
                            let weak_inner = weak.clone();
                            let refresh_slot = refresh_slot.clone();
                            browser.prompt_engine(Some(existing), move |edited| {
                                let Some(browser) = weak_inner.upgrade() else {
                                    return;
                                };
                                let name = edited.name.clone();
                                {
                                    let mut state = browser.state.borrow_mut();
                                    if let Some(item) = state
                                        .settings
                                        .engines
                                        .iter_mut()
                                        .find(|item| item.id == edited.id)
                                    {
                                        *item = edited;
                                    }
                                }
                                browser.save_settings();
                                if let Some(refresh) = refresh_slot.borrow().clone() {
                                    refresh();
                                }
                                browser.toast(&format!("Motore aggiornato: {name}"));
                            });
                        });
                    }

                    let remove = gtk::Button::builder()
                        .icon_name("user-trash-symbolic")
                        .tooltip_text("Elimina motore")
                        .valign(gtk::Align::Center)
                        .build();
                    remove.add_css_class("flat");
                    {
                        let weak = Rc::downgrade(&browser);
                        let refresh_slot = refresh_slot.clone();
                        let engine_id = engine.id.clone();
                        let engine_name = engine.name.clone();
                        remove.connect_clicked(move |_| {
                            let Some(browser) = weak.upgrade() else {
                                return;
                            };
                            let (removed, was_default) = {
                                let mut state = browser.state.borrow_mut();
                                let was_default = state.settings.default_engine == engine_id;
                                let removed = state.settings.remove_engine(&engine_id);
                                (removed, was_default)
                            };
                            if !removed {
                                return;
                            }
                            browser.save_settings();
                            if let Some(refresh) = refresh_slot.borrow().clone() {
                                refresh();
                            }
                            if was_default {
                                browser.toast(&format!(
                                    "«{engine_name}» rimosso. Motore predefinito: DuckDuckGo"
                                ));
                            } else {
                                browser.toast(&format!("Motore rimosso: {engine_name}"));
                            }
                        });
                    }

                    row.add_suffix(&edit);
                    row.add_suffix(&remove);
                    engines_list.append(&row);
                }
            })
        };
        *refresh_slot.borrow_mut() = Some(refresh.clone());
        refresh();

        {
            let weak = Rc::downgrade(self);
            let syncing = syncing.clone();
            combo.connect_selected_notify(move |combo| {
                if syncing.get() {
                    return;
                }
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let index = combo.selected() as usize;
                let engine = {
                    let state = browser.state.borrow();
                    state.settings.engines.get(index).cloned()
                };
                let Some(engine) = engine else {
                    return;
                };
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.default_engine == engine.id {
                        false
                    } else {
                        state.settings.default_engine = engine.id.clone();
                        true
                    }
                };
                if changed {
                    browser.save_settings();
                    browser.toast(&format!("Motore predefinito: {}", engine.name));
                }
            });
        }

        {
            let weak = Rc::downgrade(self);
            let refresh_slot = refresh_slot.clone();
            add_button.connect_clicked(move |_| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let weak_inner = Rc::downgrade(&browser);
                let refresh_slot = refresh_slot.clone();
                browser.prompt_engine(None, move |engine| {
                    let Some(browser) = weak_inner.upgrade() else {
                        return;
                    };
                    let name = engine.name.clone();
                    {
                        let mut state = browser.state.borrow_mut();
                        state.settings.engines.push(engine);
                    }
                    browser.save_settings();
                    if let Some(refresh) = refresh_slot.borrow().clone() {
                        refresh();
                    }
                    browser.toast(&format!("Motore aggiunto: {name}"));
                });
            });
        }

        {
            let weak = Rc::downgrade(self);
            theme_row.connect_selected_notify(move |row| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                let preference = ThemePreference::from_index(row.selected());
                let changed = {
                    let mut state = browser.state.borrow_mut();
                    if state.settings.theme == preference {
                        false
                    } else {
                        state.settings.theme = preference;
                        true
                    }
                };
                if changed {
                    browser.apply_theme();
                    browser.save_settings();
                    browser.toast(&format!("Tema: {}", preference.label()));
                }
            });
        }

        prefs.present(Some(&self.window));
    }

    /// Riga delle preferenze con un pulsante che esegue un'azione sul browser.
    fn data_row<F>(
        self: &Rc<Self>,
        title: &str,
        subtitle: &str,
        button_label: &str,
        action: F,
    ) -> adw::ActionRow
    where
        F: Fn(&Rc<Self>) + 'static,
    {
        let row = adw::ActionRow::new();
        row.set_title(title);
        row.set_subtitle(subtitle);
        let button = gtk::Button::with_label(button_label);
        button.add_css_class("flat");
        button.set_valign(gtk::Align::Center);
        let weak = Rc::downgrade(self);
        let action = Rc::new(action);
        button.connect_clicked(move |_| {
            if let Some(browser) = weak.upgrade() {
                action(&browser);
            }
        });
        row.add_suffix(&button);
        row
    }

    fn prompt_engine<F>(&self, existing: Option<SearchEngine>, on_accept: F)
    where
        F: Fn(SearchEngine) + 'static,
    {
        let editing = existing.is_some();
        let window = adw::Window::new();
        window.set_title(Some(if editing {
            "Modifica motore di ricerca"
        } else {
            "Nuovo motore di ricerca"
        }));
        window.set_transient_for(Some(&self.window));
        window.set_modal(true);
        window.set_default_size(520, 400);
        window.set_resizable(false);

        let toolbar_view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        toolbar_view.add_top_bar(&header);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);

        let name_label = gtk::Label::new(Some("Nome"));
        name_label.set_xalign(0.0);
        name_label.add_css_class("row-title");
        let name_entry = gtk::Entry::new();
        name_entry.set_placeholder_text(Some("Esempio: Kagi"));
        name_entry.set_hexpand(true);

        let keyword_label = gtk::Label::new(Some("Parola chiave (facoltativa)"));
        keyword_label.set_xalign(0.0);
        keyword_label.add_css_class("row-title");
        let keyword_entry = gtk::Entry::new();
        keyword_entry.set_placeholder_text(Some("Esempio: k per «k rust»"));
        keyword_entry.set_hexpand(true);

        let template_label = gtk::Label::new(Some("URL di ricerca"));
        template_label.set_xalign(0.0);
        template_label.add_css_class("row-title");
        let template_entry = gtk::Entry::new();
        template_entry.set_placeholder_text(Some("https://example.com/?q={query}"));
        template_entry.set_hexpand(true);

        let error_label = gtk::Label::new(None);
        error_label.set_xalign(0.0);
        error_label.add_css_class("form-error");
        error_label.set_visible(false);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Annulla");
        let accept = gtk::Button::with_label(if editing { "Salva" } else { "Aggiungi" });
        accept.add_css_class("suggested-action");
        actions.append(&cancel);
        actions.append(&accept);

        content.append(&name_label);
        content.append(&name_entry);
        content.append(&keyword_label);
        content.append(&keyword_entry);
        content.append(&template_label);
        content.append(&template_entry);
        content.append(&error_label);
        content.append(&actions);
        toolbar_view.set_content(Some(&content));
        window.set_content(Some(&toolbar_view));

        let mut engine_id = new_engine_id();
        if let Some(existing) = &existing {
            engine_id = existing.id.clone();
            name_entry.set_text(&existing.name);
            keyword_entry.set_text(&existing.keyword);
            template_entry.set_text(&existing.template);
        }

        {
            let window = window.clone();
            cancel.connect_clicked(move |_| window.close());
        }
        {
            let window = window.clone();
            let name_entry = name_entry.clone();
            let keyword_entry = keyword_entry.clone();
            let template_entry = template_entry.clone();
            let error_label = error_label.clone();
            let on_accept = Rc::new(on_accept);
            let engine_id = engine_id.clone();
            accept.connect_clicked(move |_| {
                let name = name_entry.text().trim().to_string();
                let template = template_entry.text().trim().to_string();
                let keyword_input = keyword_entry.text().trim().to_string();

                if name.is_empty() {
                    error_label.set_text("Inserisci un nome per il motore.");
                    error_label.set_visible(true);
                    return;
                }
                if !template.contains("{query}") {
                    error_label
                        .set_text("L'URL di ricerca deve contenere il segnaposto {query}.");
                    error_label.set_visible(true);
                    return;
                }
                if !template.starts_with("http://") && !template.starts_with("https://") {
                    error_label
                        .set_text("L'URL di ricerca deve iniziare con http:// o https://.");
                    error_label.set_visible(true);
                    return;
                }

                let keyword = if keyword_input.is_empty() {
                    String::new()
                } else {
                    match crate::model::valid_keyword(&keyword_input) {
                        Some(keyword) => keyword,
                        None => {
                            error_label.set_text(
                                "La parola chiave non può contenere spazi né caratteri come . / : ?",
                            );
                            error_label.set_visible(true);
                            return;
                        }
                    }
                };

                let engine = SearchEngine {
                    id: engine_id.clone(),
                    name,
                    template,
                    keyword,
                    builtin: false,
                };
                if !engine.is_valid_template() {
                    error_label.set_text("Motore non valido.");
                    error_label.set_visible(true);
                    return;
                }

                on_accept(engine);
                window.close();
            });
        }
        {
            let accept = accept.clone();
            let template_entry = template_entry.clone();
            template_entry.connect_activate(move |_| accept.emit_clicked());
        }

        window.present();
        name_entry.grab_focus();
    }

    fn open_about(&self) {
        let dialog = gtk::AboutDialog::new();
        dialog.set_program_name(Some(APP_NAME));
        dialog.set_version(Some(env!("CARGO_PKG_VERSION")));
        dialog.set_comments(Some(
            "Un browser leggero e ispirato a Zen, scritto in Rust con GTK4 e WebKitGTK.",
        ));
        dialog.set_transient_for(Some(&self.window));
        dialog.set_modal(true);
        dialog.present();
    }

    fn prompt_text<F>(&self, title: &str, label: &str, initial: &str, on_accept: F)
    where
        F: Fn(String) + 'static,
    {
        let window = adw::Window::new();
        window.set_title(Some(title));
        window.set_transient_for(Some(&self.window));
        window.set_modal(true);
        window.set_default_size(420, 180);
        window.set_resizable(false);

        let toolbar_view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        toolbar_view.add_top_bar(&header);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);

        let field_label = gtk::Label::new(Some(label));
        field_label.set_xalign(0.0);
        field_label.add_css_class("row-title");
        let entry = gtk::Entry::new();
        entry.set_text(initial);
        entry.set_hexpand(true);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Annulla");
        let accept = gtk::Button::with_label("Salva");
        accept.add_css_class("suggested-action");
        actions.append(&cancel);
        actions.append(&accept);

        content.append(&field_label);
        content.append(&entry);
        content.append(&actions);
        toolbar_view.set_content(Some(&content));
        window.set_content(Some(&toolbar_view));

        {
            let window = window.clone();
            cancel.connect_clicked(move |_| window.close());
        }
        {
            let window = window.clone();
            let entry = entry.clone();
            accept.connect_clicked(move |_| {
                let value = entry.text().trim().to_string();
                if !value.is_empty() {
                    on_accept(value);
                    window.close();
                }
            });
        }
        {
            let accept = accept.clone();
            entry.connect_activate(move |_| accept.emit_clicked());
        }

        window.present();
        entry.grab_focus();
        entry.select_region(0, -1);
    }

    fn session_snapshot(&self) -> Option<Session> {
        let state = self.state.borrow();
        if state.workspaces.is_empty() {
            return None;
        }

        let active_workspace = state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == state.active_workspace)
            .unwrap_or(0);

        let workspaces = state
            .workspaces
            .iter()
            .map(|workspace| {
                let tabs = workspace
                    .tabs
                    .iter()
                    .filter_map(|tab_id| state.tab(*tab_id))
                    .map(|tab| TabSession {
                        uri: if tab.is_start_page {
                            String::new()
                        } else {
                            tab.uri.clone()
                        },
                        title: tab.title.clone(),
                        pinned: tab.pinned,
                        group: tab.group,
                    })
                    .collect::<Vec<_>>();
                let active_tab = workspace
                    .active_tab
                    .and_then(|active| workspace.tabs.iter().position(|id| *id == active))
                    .unwrap_or(0);
                WorkspaceSession {
                    name: workspace.name.clone(),
                    active_tab,
                    tabs,
                    groups: workspace.groups.clone(),
                    profile: workspace.profile.clone(),
                }
            })
            .collect();

        Some(Session {
            version: 1,
            name: String::new(),
            active_workspace,
            workspaces,
        })
    }

    fn save_session(&self) {
        // Una finestra privata non lascia tracce sul disco.
        if self.private {
            return;
        }
        if let Some(session) = self.session_snapshot() {
            if let Err(error) = save_json(&self.paths.session, &session) {
                eprintln!("cannot save session: {error}");
            }
        }
    }

    fn save_settings(&self) {
        let settings = self.state.borrow().settings.clone();
        if let Err(error) = save_json(&self.paths.settings, &settings) {
            eprintln!("cannot save settings: {error}");
        }
    }

    fn toast(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_timeout(3);
        self.toast_overlay.add_toast(toast);
    }

    /// Toast con un'azione, usato quando c'è qualcosa che l'utente può fare
    /// subito (per esempio riprovare un caricamento fallito).
    fn toast_with_action(
        self: &Rc<Self>,
        message: &str,
        label: &str,
        action: impl Fn(&Rc<Self>) + 'static,
    ) {
        let toast = adw::Toast::new(message);
        toast.set_timeout(6);
        toast.set_button_label(Some(label));
        let weak = Rc::downgrade(self);
        toast.connect_button_clicked(move |_| {
            if let Some(browser) = weak.upgrade() {
                action(&browser);
            }
        });
        self.toast_overlay.add_toast(toast);
    }
}

fn build_primary_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let tabs = gio::Menu::new();
    tabs.append(Some("Nuova scheda"), Some("win.new-tab"));
    tabs.append(Some("Riapri scheda chiusa"), Some("win.reopen-closed-tab"));
    tabs.append(Some("Schede chiuse di recente"), Some("win.closed-tabs"));
    tabs.append(Some("Cerca scheda…"), Some("win.tab-search"));
    tabs.append(Some("Duplica scheda"), Some("win.duplicate-tab"));
    tabs.append(Some("Fissa o rimuovi scheda"), Some("win.toggle-pin"));
    tabs.append(Some("Chiudi scheda"), Some("win.close-tab"));
    menu.append_section(None, &tabs);

    let sessions = gio::Menu::new();
    sessions.append(Some("Salva sessione…"), Some("win.save-session"));
    sessions.append(Some("Apri sessione…"), Some("win.open-session"));
    menu.append_section(None, &sessions);

    let page = gio::Menu::new();
    page.append(Some("Stampa / Salva PDF…"), Some("win.print"));
    page.append(Some("Modalità lettore"), Some("win.reader-mode"));
    page.append(Some("Zoom avanti"), Some("win.zoom-in"));
    page.append(Some("Zoom indietro"), Some("win.zoom-out"));
    page.append(Some("Zoom normale"), Some("win.zoom-reset"));
    page.append(Some("Vista affiancata"), Some("win.toggle-split"));
    page.append(Some("Inverti riquadri"), Some("win.swap-split"));
    menu.append_section(None, &page);

    let privacy = gio::Menu::new();
    privacy.append(
        Some("Cancella dati di navigazione…"),
        Some("win.clear-data"),
    );
    privacy.append(Some("Permessi per sito…"), Some("win.permissions"));
    privacy.append(
        Some("Blocco contenuti su questo sito"),
        Some("win.toggle-site-blocking"),
    );
    menu.append_section(None, &privacy);

    let navigation = gio::Menu::new();
    navigation.append(Some("Segnalibri"), Some("win.bookmarks"));
    navigation.append(Some("Cronologia"), Some("win.history"));
    navigation.append(Some("Download"), Some("win.downloads"));
    navigation.append(Some("Importa dati…"), Some("win.import-data"));
    navigation.append(Some("Account e password"), Some("win.credentials"));
    menu.append_section(None, &navigation);

    let app = gio::Menu::new();
    app.append(Some("Estensioni"), Some("win.extensions"));
    app.append(Some("Nuova finestra privata"), Some("win.private-window"));
    app.append(Some("Impostazioni"), Some("win.preferences"));
    app.append(Some("Informazioni su Peats"), Some("win.about"));
    menu.append_section(None, &app);

    let window = gio::Menu::new();
    window.append(Some("Chiudi finestra"), Some("win.close"));
    window.append(Some("Esci"), Some("win.quit"));
    menu.append_section(None, &window);

    menu
}

/// Voce di menu in un popover: chiude il popover al clic.
fn menu_button(popover: &gtk::Popover, label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("flat");
    button.set_halign(gtk::Align::Fill);
    let popover = popover.clone();
    button.connect_clicked(move |_| popover.popdown());
    button
}

fn session_slug(name: &str) -> String {
    let mut slug = String::new();
    for character in name.chars() {
        if character.is_alphanumeric() {
            for lower in character.to_lowercase() {
                slug.push(lower);
            }
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }

    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        format!("sessione-{}", std::process::id())
    } else {
        slug
    }
}

fn launch_directory(path: &Path) {
    if let Ok(uri) = url::Url::from_directory_path(path) {
        let _ = gio::AppInfo::launch_default_for_uri(
            uri.as_str(),
            None::<&gio::AppLaunchContext>,
        );
    }
}

fn sidebar_action_button(icon_name: &str, tooltip: &str, action: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(tooltip)
        .build();
    button.set_action_name(Some(action));
    button.add_css_class("flat");
    button.add_css_class("sidebar-action");
    button
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn add_empty_state(list: &gtk::ListBox, message: &str) {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(false);
    let label = gtk::Label::new(Some(message));
    label.add_css_class("empty-state");
    label.set_margin_top(36);
    label.set_margin_bottom(36);
    label.set_margin_start(18);
    label.set_margin_end(18);
    row.set_child(Some(&label));
    list.append(&row);
}

fn create_network_session(paths: &Paths, profile: &str) -> NetworkSession {
    // Il profilo `default` conserva le cartelle storiche; ogni spazio ha la
    // propria coppia di cartelle, quindi cookie, localStorage, IndexedDB,
    // credenziali e cache non si mescolano fra spazi.
    let (data_directory, cache_directory) = if profile == DEFAULT_PROFILE || profile.is_empty() {
        (paths.data_dir.join("webkit"), paths.cache_dir.join("webkit"))
    } else {
        (
            paths.data_dir.join("webkit-profiles").join(profile),
            paths.cache_dir.join("webkit-profiles").join(profile),
        )
    };
    let _ = fs::create_dir_all(&data_directory);
    let _ = fs::create_dir_all(&cache_directory);
    let session = NetworkSession::new(
        data_directory.to_str(),
        cache_directory.to_str(),
    );
    session.set_persistent_credential_storage_enabled(true);
    session
}

/// ITP e politica dei cookie applicati a una sessione di rete.
fn apply_session_privacy(session: &NetworkSession, block_third_party: bool) {
    // ITP (Intelligent Tracking Prevention) è esposto da WebKitGTK 6 come
    // `webkit_network_session_set_itp_enabled`.
    session.set_itp_enabled(true);
    if let Some(cookies) = session.cookie_manager() {
        cookies.set_accept_policy(if block_third_party {
            webkit::CookieAcceptPolicy::NoThirdParty
        } else {
            webkit::CookieAcceptPolicy::Always
        });
    }
}

/// Identificatore di un nuovo profilo dati per uno spazio.
/// Sessione effimera per le finestre private: niente cookie, cache o
/// credenziali su disco.
fn create_private_session() -> NetworkSession {
    let session = NetworkSession::new_ephemeral();
    session.set_persistent_credential_storage_enabled(false);
    session
}

fn new_profile_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("ws-{nanos}")
}

fn new_engine_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("custom-{nanos}")
}

fn title_from_uri(uri: &str) -> String {
    url::Url::parse(uri)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.trim_start_matches("www.").to_owned()))
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| uri.to_owned())
}

/// Nome del file da salvare: quello suggerito dal server, altrimenti l'ultimo
/// segmento dell'URL, altrimenti un nome generico.
fn download_filename(suggested: &str, uri: &str) -> String {
    let suggested = suggested.trim();
    if !suggested.is_empty() {
        return sanitize_filename(suggested);
    }

    url::Url::parse(uri)
        .ok()
        .and_then(|url| {
            url.path_segments().and_then(|segments| {
                let mut segments = segments;
                segments
                    .rfind(|segment| !segment.is_empty())
                    .map(|segment| segment.replace("%20", " "))
            })
        })
        .filter(|segment| !segment.is_empty())
        .map(|segment| sanitize_filename(&segment))
        .unwrap_or_else(|| "download".to_owned())
}

fn permission_label(kind: &str) -> &'static str {
    match kind {
        "geolocation" => "Geolocalizzazione",
        "notifications" => "Notifiche",
        "camera" => "Fotocamera",
        "microphone" => "Microfono",
        "camera-microphone" => "Fotocamera e microfono",
        "media" => "Dispositivi multimediali",
        "clipboard" => "Appunti",
        "pointer-lock" => "Blocco del puntatore",
        "device-info" => "Informazioni dispositivo",
        "media-keys" => "Chiavi DRM",
        "xr" => "Realtà estesa",
        _ => "Altro",
    }
}

fn sanitize_filename(name: &str) -> String {
    let name = Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let name = name.trim().trim_start_matches('.');
    if name.is_empty() {
        "download".to_owned()
    } else {
        name.to_owned()
    }
}

fn unique_download_path(directory: &Path, filename: &str) -> PathBuf {
    let _ = fs::create_dir_all(directory);
    let candidate = directory.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("download");
    let extension = path.extension().and_then(|extension| extension.to_str());

    for index in 1..10_000 {
        let name = match extension {
            Some(extension) if !extension.is_empty() => {
                format!("{stem} ({index}).{extension}")
            }
            _ => format!("{stem} ({index})"),
        };
        let candidate = directory.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }

    directory.join(format!("{stem}-{}.download", std::process::id()))
}

fn start_page_html(search_template: &str) -> String {
    // Il modello di ricerca arriva come JSON e viene inserito nello script:
    // niente `format!` sul documento, così le graffe di CSS e JavaScript
    // restano leggibili.
    let template = serde_json::to_string(search_template).unwrap_or_else(|_| {
        "\"https://duckduckgo.com/?q={query}\"".to_owned()
    });

    let mut html = String::with_capacity(9000);
    html.push_str(
        r#"<!doctype html>
<html lang="it">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Nuova scheda</title>
<style>
  :root {
    color-scheme: light dark;
    --bg: #f2f4fb;
    --fg: #15161c;
    --muted: #5d6070;
    --card: rgba(255, 255, 255, .74);
    --card-border: rgba(21, 22, 28, .08);
    --chip: rgba(255, 255, 255, .6);
    --accent: #4d7cf7;
    --accent-2: #a06bff;
    --shadow: 0 28px 80px rgba(31, 41, 90, .16);
    --ring: rgba(77, 124, 247, .28);
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #0f1015;
      --fg: #f2f3f8;
      --muted: #a4a7b8;
      --card: rgba(28, 29, 38, .68);
      --card-border: rgba(255, 255, 255, .10);
      --chip: rgba(255, 255, 255, .06);
      --accent: #79a6ff;
      --accent-2: #c3a1ff;
      --shadow: 0 28px 80px rgba(0, 0, 0, .55);
      --ring: rgba(121, 166, 255, .3);
    }
  }
  * { box-sizing: border-box; }
  html, body { height: 100%; margin: 0; }
  body {
    background: var(--bg);
    color: var(--fg);
    font-family: system-ui, -apple-system, "Segoe UI", Cantarell, sans-serif;
    display: grid;
    place-items: center;
    padding: 32px 24px;
    overflow: hidden;
  }

  /* Alone colorato di sfondo: due macchie che si muovono lentamente. */
  .aura {
    position: fixed;
    inset: -20vmax;
    pointer-events: none;
    z-index: 0;
  }
  .aura span {
    position: absolute;
    width: 52vmax;
    height: 52vmax;
    border-radius: 50%;
    filter: blur(70px);
    opacity: .55;
  }
  .aura span:first-child {
    top: 4%;
    left: 6%;
    background: radial-gradient(circle at 30% 30%, var(--accent), transparent 62%);
    animation: drift-a 22s ease-in-out infinite alternate;
  }
  .aura span:last-child {
    right: 2%;
    bottom: 0%;
    background: radial-gradient(circle at 60% 40%, var(--accent-2), transparent 64%);
    animation: drift-b 26s ease-in-out infinite alternate;
  }
  @keyframes drift-a {
    from { transform: translate3d(0, 0, 0) scale(1); }
    to   { transform: translate3d(6vmax, 4vmax, 0) scale(1.12); }
  }
  @keyframes drift-b {
    from { transform: translate3d(0, 0, 0) scale(1.05); }
    to   { transform: translate3d(-5vmax, -3vmax, 0) scale(.95); }
  }

  main {
    position: relative;
    z-index: 1;
    width: min(660px, 94vw);
    animation: rise .55s cubic-bezier(.22, .9, .3, 1) both;
  }
  @keyframes rise {
    from { opacity: 0; transform: translateY(14px); }
    to   { opacity: 1; transform: translateY(0); }
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-bottom: 26px;
  }
  .mark {
    width: 58px;
    height: 58px;
    border-radius: 19px;
    display: grid;
    place-items: center;
    background: linear-gradient(150deg, var(--accent), var(--accent-2));
    box-shadow: 0 16px 40px var(--ring);
  }
  .mark svg { width: 32px; height: 32px; display: block; }
  .wordmark { display: flex; flex-direction: column; gap: 4px; }
  h1 {
    margin: 0;
    font-size: 32px;
    line-height: 1;
    letter-spacing: -.035em;
  }
  .tagline {
    color: var(--muted);
    font-size: 13.5px;
    min-height: 18px;
  }

  form {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 8px 8px 16px;
    background: var(--card);
    border: 1px solid var(--card-border);
    border-radius: 20px;
    box-shadow: var(--shadow);
    backdrop-filter: blur(18px) saturate(1.2);
    transition: box-shadow .18s ease, border-color .18s ease,
      transform .18s ease;
  }
  form:focus-within {
    border-color: var(--accent);
    box-shadow: var(--shadow), 0 0 0 4px var(--ring);
    transform: translateY(-1px);
  }
  .search-icon {
    width: 20px;
    height: 20px;
    flex: none;
    opacity: .55;
  }
  input {
    flex: 1;
    min-width: 0;
    border: 0;
    outline: 0;
    background: transparent;
    color: var(--fg);
    font-size: 17px;
    padding: 13px 2px;
  }
  input::placeholder { color: var(--muted); }
  button {
    border: 0;
    border-radius: 14px;
    background: linear-gradient(150deg, var(--accent), var(--accent-2));
    color: #fff;
    padding: 12px 20px;
    font-size: 14.5px;
    font-weight: 650;
    cursor: pointer;
    transition: filter .16s ease, transform .16s ease;
  }
  button:hover { filter: brightness(1.07); transform: translateY(-1px); }
  button:active { transform: translateY(0); filter: brightness(.96); }

  .links {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    margin-top: 22px;
  }
  .links a {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 9px 14px;
    border-radius: 999px;
    background: var(--chip);
    border: 1px solid var(--card-border);
    color: var(--fg);
    font-size: 13px;
    text-decoration: none;
    backdrop-filter: blur(10px);
    transition: transform .16s ease, background-color .16s ease,
      border-color .16s ease;
  }
  .links a:hover {
    transform: translateY(-2px);
    border-color: var(--accent);
    background: var(--card);
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
  }
  .dot-wiki { background: #6b7280; }
  .dot-github { background: #8957e5; }
  .dot-dd { background: #de5833; }
  .dot-yt { background: #ff0033; }

  .hint {
    margin-top: 28px;
    color: var(--muted);
    font-size: 12.5px;
    text-align: center;
    line-height: 1.9;
  }
  kbd {
    border: 1px solid var(--card-border);
    border-bottom-width: 2px;
    border-radius: 7px;
    padding: 1px 7px;
    font: inherit;
    font-size: 11.5px;
    background: var(--chip);
  }

  @media (prefers-reduced-motion: reduce) {
    .aura span,
    main,
    form,
    .links a,
    button {
      animation: none !important;
      transition: none !important;
    }
  }
</style>
</head>
<body>
<div class="aura" aria-hidden="true"><span></span><span></span></div>
<main>
  <div class="brand">
    <span class="mark" aria-hidden="true">
      <svg viewBox="0 0 32 32" fill="none" xmlns="http://www.w3.org/2000/svg">
        <circle cx="16" cy="16" r="10.5" stroke="white" stroke-width="2.6"/>
        <path d="M16 5.5a10.5 10.5 0 0 1 8.4 16.9" stroke="white" stroke-width="2.6" stroke-linecap="round" opacity=".65"/>
        <circle cx="16" cy="16" r="3.2" fill="white"/>
      </svg>
    </span>
    <div class="wordmark">
      <h1>Peats</h1>
      <div class="tagline" id="tagline">Pronto quando vuoi</div>
    </div>
  </div>

  <form id="search">
    <svg class="search-icon" viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <circle cx="11" cy="11" r="6.5" stroke="currentColor" stroke-width="1.9"/>
      <path d="m16 16 4 4" stroke="currentColor" stroke-width="1.9" stroke-linecap="round"/>
    </svg>
    <input id="query" type="search" autocomplete="off" autofocus
           placeholder="Cerca sul web o inserisci un indirizzo">
    <button type="submit">Cerca</button>
  </form>

  <nav class="links" aria-label="Scorciatoie">
    <a href="https://it.wikipedia.org"><span class="dot dot-wiki"></span>Wikipedia</a>
    <a href="https://github.com"><span class="dot dot-github"></span>GitHub</a>
    <a href="https://duckduckgo.com"><span class="dot dot-dd"></span>DuckDuckGo</a>
    <a href="https://www.youtube.com"><span class="dot dot-yt"></span>YouTube</a>
  </nav>

  <p class="hint">
    <kbd>Ctrl</kbd> + <kbd>L</kbd> barra indirizzi ·
    <kbd>Ctrl</kbd> + <kbd>T</kbd> nuova scheda ·
    <kbd>Ctrl</kbd> + <kbd>Shift</kbd> + <kbd>S</kbd> vista affiancata
  </p>
</main>
<script>
  const SEARCH_TEMPLATE = "#,
    );
    html.push_str(&template);
    html.push_str(
        r#";
  const form = document.getElementById('search');
  const input = document.getElementById('query');
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    const query = input.value.trim();
    if (!query) return;
    const template = SEARCH_TEMPLATE.includes('{query}')
      ? SEARCH_TEMPLATE
      : 'https://duckduckgo.com/?q={query}';
    window.location.href = template.replace('{query}', encodeURIComponent(query));
  });

  // Saluto coerente con l'ora del sistema, solo testo.
  const hour = new Date().getHours();
  const greeting = hour < 6 ? 'Buonanotte'
    : hour < 13 ? 'Buongiorno'
    : hour < 18 ? 'Buon pomeriggio'
    : 'Buonasera';
  document.getElementById('tagline').textContent = greeting + ' — dove vuoi andare?';
</script>
</body>
</html>"#,
    );
    html
}


/// Elenco delle eccezioni al blocco contenuti, con rimozione singola o totale.
fn fill_block_exceptions(browser: &Rc<Browser>, list: &gtk::ListBox) {
    list.remove_all();
    let entries = browser.block_exceptions();
    if entries.is_empty() {
        add_empty_state(
            list,
            "Nessuna eccezione: il blocco contenuti è attivo su tutti i siti.",
        );
        return;
    }

    let clear_all_row = adw::ActionRow::new();
    clear_all_row.set_title("Rimuovi tutte le eccezioni");
    clear_all_row.set_subtitle("Il blocco torna attivo su tutti i siti");
    let clear_all = gtk::Button::with_label("Rimuovi");
    clear_all.add_css_class("flat");
    clear_all.set_valign(gtk::Align::Center);
    {
        let weak = Rc::downgrade(browser);
        let list = list.clone();
        clear_all.connect_clicked(move |_| {
            let Some(browser) = weak.upgrade() else {
                return;
            };
            match browser.storage.clear_block_exceptions() {
                Ok(()) => {
                    browser.refresh_content_filter();
                    fill_block_exceptions(&browser, &list);
                    browser.toast("Eccezioni rimosse");
                }
                Err(error) => browser.toast(&format!("Eccezioni non rimosse: {error}")),
            }
        });
    }
    clear_all_row.add_suffix(&clear_all);
    list.append(&clear_all_row);

    for origin in entries {
        let row = adw::ActionRow::new();
        row.set_title(&origin);
        row.set_subtitle("Blocco contenuti disattivato");

        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Riattiva il blocco per questo sito")
            .valign(gtk::Align::Center)
            .build();
        remove.add_css_class("flat");
        {
            let weak = Rc::downgrade(browser);
            let list = list.clone();
            let origin = origin.clone();
            remove.connect_clicked(move |_| {
                let Some(browser) = weak.upgrade() else {
                    return;
                };
                match browser.storage.remove_block_exception(&origin) {
                    Ok(()) => {
                        browser.refresh_content_filter();
                        fill_block_exceptions(&browser, &list);
                        browser.toast("Eccezione rimossa");
                    }
                    Err(error) => browser.toast(&format!("Eccezione non rimossa: {error}")),
                }
            });
        }
        row.add_suffix(&remove);
        list.append(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_filename_prefers_server_suggestion() {
        assert_eq!(
            download_filename("report.pdf", "https://example.com/a/b.zip"),
            "report.pdf"
        );
    }

    #[test]
    fn download_filename_falls_back_to_url_segment() {
        assert_eq!(
            download_filename("", "http://127.0.0.1:8765/peats-test.txt"),
            "peats-test.txt"
        );
        assert_eq!(
            download_filename("   ", "https://example.com/file%20name.txt"),
            "file name.txt"
        );
    }

    #[test]
    fn download_filename_rejects_path_traversal() {
        assert_eq!(download_filename("../../etc/passwd", "https://x/y"), "passwd");
        assert_eq!(download_filename("", "https://example.com/"), "download");
    }
}
