use crate::database::Database;
use crate::mail;
use crate::models::{
    Account, AttachmentInfo, AuthMethod, MailFolder, Message, PendingSend, SecurityMode,
    ServerConfig,
};
use crate::preferences;
use crate::theme;
use adw::prelude::*;
use chrono::Datelike;
use gtk::gdk;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use webkit6::prelude::*;

const MESSAGE_PAGE_SIZE: usize = 100;

struct AppState {
    window: adw::ApplicationWindow,
    database: Database,
    accounts: RefCell<Vec<Account>>,
    preferences: RefCell<preferences::Preferences>,
    folders: RefCell<Vec<MailFolder>>,
    messages: RefCell<Vec<Message>>,
    sidebar: gtk::Box,
    message_list: gtk::ListBox,
    middle: gtk::Box,
    sidebar_scroll: gtk::ScrolledWindow,
    reader: gtk::Box,
    navigation: gtk::Button,
    search_entry: gtk::SearchEntry,
    scope: RefCell<MailScope>,
    search_filters: RefCell<SearchFilters>,
    selected_message: RefCell<Option<i64>>,
    account_expanded: RefCell<HashMap<i64, bool>>,
    narrow_mode: Cell<bool>,
    mobile_mode: Cell<bool>,
    sidebar_revealed: Cell<bool>,
    allowed_remote_images: RefCell<std::collections::HashSet<i64>>,
    monitor_sender: async_channel::Sender<mail::sync::SyncReport>,
    monitor_stops: RefCell<HashMap<i64, Arc<AtomicBool>>>,
    outbox_sender: async_channel::Sender<mail::sync::OutboxReport>,
    outbox_stops: RefCell<HashMap<i64, Arc<AtomicBool>>>,
    status: gtk::Label,
    selection_bar: gtk::Box,
    selection_count: gtk::Label,
    selected_messages: RefCell<HashSet<i64>>,
    load_more: gtk::Button,
    message_offset: Cell<usize>,
    has_more_messages: Cell<bool>,
    demo_mode: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MailScope {
    Unified(String),
    Account { id: i64, folder: String },
}

#[derive(Clone, Debug)]
enum FolderOperation {
    Create {
        account_id: i64,
        local_name: String,
        remote_name: String,
    },
    Rename {
        account_id: i64,
        old_local_name: String,
        old_remote_name: String,
        new_local_name: String,
        new_remote_name: String,
    },
    Delete {
        account_id: i64,
        local_name: String,
        remote_name: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SearchFilters {
    unread: bool,
    starred: bool,
    attachments: bool,
    account_id: Option<i64>,
    folder: Option<String>,
    after: Option<String>,
    before: Option<String>,
}

pub fn build_window(application: &adw::Application) {
    let database =
        Database::open_default().expect("Omarchy Mail could not open its local database");
    let accounts = database.load_accounts().unwrap_or_default();
    let folders = database.load_folders().unwrap_or_default();
    let preferences = preferences::load();
    let demo_mode = std::env::var("OMARCHY_MAIL_DEMO").as_deref() == Ok("1");
    let (monitor_sender, monitor_receiver) = async_channel::unbounded();
    let (outbox_sender, outbox_receiver) = async_channel::unbounded();
    let messages = if demo_mode {
        Message::demo_messages()
    } else {
        database
            .list_messages_filtered_page(None, Some("Inbox"), false, MESSAGE_PAGE_SIZE, 0)
            .unwrap_or_default()
    };

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Omarchy Mail")
        .default_width(1440)
        .default_height(900)
        .build();
    window.add_css_class("omarchy-mail-window");

    let status = gtk::Label::new(Some("Ready"));
    status.add_css_class("mail-status");
    status.set_margin_start(12);
    status.set_margin_end(12);

    let (header, compose, refresh, settings, navigation) = build_header(&status);

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.add_css_class("mail-sidebar");
    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&sidebar)
        .build();

    let message_list = gtk::ListBox::new();
    message_list.set_selection_mode(gtk::SelectionMode::Multiple);
    message_list.set_activate_on_single_click(true);
    message_list.add_css_class("mail-message-list");
    let message_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&message_list)
        .build();

    let middle = gtk::Box::new(gtk::Orientation::Vertical, 0);
    middle.add_css_class("mail-middle");
    let (filter_bar, search, filter_button) = build_filter_bar();
    middle.append(&filter_bar);
    let (
        selection_bar,
        selection_count,
        mark_read,
        mark_unread,
        star_selected,
        archive_selected,
        trash_selected,
        clear_selection,
    ) = build_selection_bar();
    middle.append(&selection_bar);
    middle.append(&message_scroll);
    let load_more = gtk::Button::with_label("Load more messages");
    load_more.set_halign(gtk::Align::Center);
    load_more.set_margin_top(8);
    load_more.set_margin_bottom(10);
    load_more.add_css_class("mail-load-more");
    load_more.set_visible(false);
    middle.append(&load_more);

    let reader = gtk::Box::new(gtk::Orientation::Vertical, 0);
    reader.add_css_class("mail-reader");
    reader.set_vexpand(true);

    let content = gtk::Paned::new(gtk::Orientation::Horizontal);
    content.set_wide_handle(true);
    content.set_position(preferences.sidebar_width.clamp(220, 420));
    content.set_start_child(Some(&sidebar_scroll));
    content.set_resize_start_child(false);
    content.set_shrink_start_child(true);
    content.set_resize_end_child(true);
    content.set_shrink_end_child(true);

    let shell = gtk::Box::new(gtk::Orientation::Vertical, 0);
    shell.add_css_class("mail-shell");
    shell.append(&header);
    shell.append(&content);
    shell.set_vexpand(true);
    window.set_content(Some(&shell));

    let state = Rc::new(AppState {
        window: window.clone(),
        database,
        accounts: RefCell::new(accounts),
        preferences: RefCell::new(preferences),
        folders: RefCell::new(folders),
        messages: RefCell::new(messages),
        sidebar,
        middle: middle.clone(),
        sidebar_scroll,
        message_list,
        reader,
        navigation: navigation.clone(),
        search_entry: search.clone(),
        scope: RefCell::new(MailScope::Unified("Inbox".into())),
        search_filters: RefCell::new(SearchFilters::default()),
        selected_message: RefCell::new(None),
        account_expanded: RefCell::new(HashMap::new()),
        narrow_mode: Cell::new(false),
        mobile_mode: Cell::new(false),
        sidebar_revealed: Cell::new(false),
        allowed_remote_images: RefCell::new(std::collections::HashSet::new()),
        monitor_sender,
        monitor_stops: RefCell::new(HashMap::new()),
        outbox_sender,
        outbox_stops: RefCell::new(HashMap::new()),
        status,
        selection_bar,
        selection_count,
        selected_messages: RefCell::new(HashSet::new()),
        load_more,
        message_offset: Cell::new(if demo_mode {
            Message::demo_messages().len()
        } else {
            MESSAGE_PAGE_SIZE
        }),
        has_more_messages: Cell::new(!demo_mode),
        demo_mode,
    });

    // The middle pane is inserted after the state exists so its selection can
    // route into the reader without keeping a second source of truth.
    let reader_pane = build_middle_and_reader(state.clone(), &middle);
    content.set_end_child(Some(&reader_pane));
    connect_pane_persistence(&state, &content, &reader_pane);
    connect_header_actions(state.clone(), &compose, &refresh, &settings, &navigation);
    let state_for_search = state.clone();
    search.connect_search_changed(move |entry| {
        render_messages(&state_for_search, entry.text().as_str())
    });
    let state_for_filter = state.clone();
    filter_button.connect_clicked(move |button| open_filter_menu(state_for_filter.clone(), button));
    connect_selection_actions(
        state.clone(),
        &mark_read,
        &mark_unread,
        &star_selected,
        &archive_selected,
        &trash_selected,
        &clear_selection,
    );
    let state_for_load_more = state.clone();
    state
        .load_more
        .connect_clicked(move |_| load_more_messages(&state_for_load_more));
    render_sidebar(&state);
    render_messages(&state, "");
    render_reader(&state, None);

    theme::install();
    connect_keyboard_shortcuts(&state);
    let state_for_size = state.clone();
    let window_for_size = window.clone();
    glib::timeout_add_local(Duration::from_millis(250), move || {
        apply_responsive_layout(&state_for_size, window_for_size.width());
        glib::ControlFlow::Continue
    });
    apply_responsive_layout(&state, window.width());
    window.present();
    listen_for_monitor_reports(state.clone(), monitor_receiver);
    listen_for_outbox_reports(state.clone(), outbox_receiver);
    start_account_monitors(state);
}

fn build_header(
    status: &gtk::Label,
) -> (
    adw::HeaderBar,
    gtk::Button,
    gtk::Button,
    gtk::Button,
    gtk::Button,
) {
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Omarchy Mail", "Email without the clutter");
    header.set_title_widget(Some(&title));

    let compose = icon_button("mail-message-new-symbolic", "Compose a new message");
    compose.set_widget_name("compose-button");
    header.pack_start(&compose);
    let navigation = icon_button("sidebar-show-symbolic", "Show folders and accounts");
    navigation.set_widget_name("navigation-button");
    navigation.set_visible(false);
    header.pack_start(&navigation);

    let refresh = icon_button("view-refresh-symbolic", "Synchronise mail");
    refresh.set_widget_name("refresh-button");
    header.pack_end(&refresh);
    let settings = icon_button("emblem-system-symbolic", "Settings");
    settings.set_widget_name("settings-button");
    header.pack_end(&settings);
    header.pack_end(status);
    (header, compose, refresh, settings, navigation)
}

fn connect_header_actions(
    state: Rc<AppState>,
    compose: &gtk::Button,
    refresh: &gtk::Button,
    settings: &gtk::Button,
    navigation: &gtk::Button,
) {
    let state_for_compose = state.clone();
    compose.connect_clicked(move |_| open_compose(state_for_compose.clone()));
    let state_for_refresh = state.clone();
    refresh.connect_clicked(move |_| sync_all(state_for_refresh.clone(), true));
    let state_for_navigation = state.clone();
    navigation.connect_clicked(move |_| {
        if state_for_navigation.mobile_mode.get() {
            let visible = !state_for_navigation.sidebar_revealed.get();
            state_for_navigation.sidebar_revealed.set(visible);
            state_for_navigation.sidebar_scroll.set_visible(visible);
        }
    });
    let state_for_settings = state;
    settings.connect_clicked(move |_| open_settings(state_for_settings.clone()));
}

fn build_middle_and_reader(state: Rc<AppState>, middle: &gtk::Box) -> gtk::Paned {
    let reader = state.reader.clone();
    let pane = gtk::Paned::new(gtk::Orientation::Horizontal);
    pane.set_wide_handle(true);
    pane.set_position(
        state
            .preferences
            .borrow()
            .message_list_width
            .clamp(300, 900),
    );
    pane.set_start_child(Some(middle));
    pane.set_end_child(Some(&reader));
    pane.set_resize_start_child(false);
    pane.set_shrink_start_child(true);
    pane.set_resize_end_child(true);
    pane.set_shrink_end_child(true);

    let message_list = state.message_list.clone();
    let state_for_selected_rows = state.clone();
    message_list.connect_selected_rows_changed(move |list| {
        let selected = list
            .selected_rows()
            .into_iter()
            .filter_map(|row| {
                row.widget_name()
                    .strip_prefix("message-row-")
                    .and_then(|id| id.parse::<i64>().ok())
            })
            .collect::<HashSet<_>>();
        state_for_selected_rows.selected_messages.replace(selected);
        update_selection_summary(&state_for_selected_rows);
    });
    let state_for_selection = state.clone();
    message_list.connect_row_selected(move |_, row| {
        let Some(row) = row else { return };
        let id = row
            .widget_name()
            .strip_prefix("message-row-")
            .and_then(|id| id.parse::<i64>().ok());
        state_for_selection.selected_message.replace(id);
        let message = id.and_then(|id| {
            state_for_selection
                .messages
                .borrow()
                .iter()
                .find(|message| message.id == id)
                .cloned()
        });
        render_reader(&state_for_selection, message);
    });
    pane
}

fn connect_pane_persistence(
    state: &Rc<AppState>,
    sidebar_pane: &gtk::Paned,
    message_pane: &gtk::Paned,
) {
    let pending_sidebar = Rc::new(RefCell::new(None));
    let state_for_sidebar = state.clone();
    let pending_for_sidebar = pending_sidebar.clone();
    let sidebar_pane_for_timer = sidebar_pane.clone();
    sidebar_pane.connect_position_notify(move |_| {
        if pending_for_sidebar.borrow().is_some() {
            return;
        }
        let state = state_for_sidebar.clone();
        let pending = pending_for_sidebar.clone();
        let pane = sidebar_pane_for_timer.clone();
        let source = glib::timeout_add_local_once(Duration::from_millis(700), move || {
            pending.borrow_mut().take();
            let position = pane.position();
            if position <= 0 {
                return;
            }
            let result = {
                let mut preferences = state.preferences.borrow_mut();
                preferences.sidebar_width = position;
                preferences::save(&preferences)
            };
            if let Err(error) = result {
                set_status(&state, &format!("Couldn’t save pane width: {error}"));
            }
        });
        *pending_for_sidebar.borrow_mut() = Some(source);
    });

    let pending_message = Rc::new(RefCell::new(None));
    let state_for_message = state.clone();
    let pending_for_message = pending_message.clone();
    let message_pane_for_timer = message_pane.clone();
    message_pane.connect_position_notify(move |_| {
        if pending_for_message.borrow().is_some() {
            return;
        }
        let state = state_for_message.clone();
        let pending = pending_for_message.clone();
        let pane = message_pane_for_timer.clone();
        let source = glib::timeout_add_local_once(Duration::from_millis(700), move || {
            pending.borrow_mut().take();
            let position = pane.position();
            if position <= 0 {
                return;
            }
            let result = {
                let mut preferences = state.preferences.borrow_mut();
                preferences.message_list_width = position;
                preferences::save(&preferences)
            };
            if let Err(error) = result {
                set_status(&state, &format!("Couldn’t save pane width: {error}"));
            }
        });
        *pending_for_message.borrow_mut() = Some(source);
    });
}

fn apply_responsive_layout(state: &Rc<AppState>, width: i32) {
    let narrow = width > 0 && width < 980;
    let mobile = width > 0 && width < 700;
    if mobile != state.mobile_mode.get() {
        state.sidebar_revealed.set(false);
    }
    state.narrow_mode.set(narrow);
    state.mobile_mode.set(mobile);
    state
        .sidebar_scroll
        .set_visible(!mobile || state.sidebar_revealed.get());
    state.navigation.set_visible(mobile);
    if narrow {
        let has_selection = state.selected_message.borrow().is_some();
        state.middle.set_visible(!has_selection);
        state.reader.set_visible(has_selection);
    } else {
        state.middle.set_visible(true);
        state.reader.set_visible(true);
    }
}

fn connect_keyboard_shortcuts(state: &Rc<AppState>) {
    let controller = gtk::EventControllerKey::new();
    let state_for_key = state.clone();
    controller.connect_key_pressed(move |_, key, _, modifiers| {
        let control = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        if control && key == gdk::Key::n {
            open_compose(state_for_key.clone());
            return glib::Propagation::Stop;
        }
        if control && key == gdk::Key::f {
            state_for_key.search_entry.grab_focus();
            return glib::Propagation::Stop;
        }
        if control && key == gdk::Key::r {
            sync_all(state_for_key.clone(), true);
            return glib::Propagation::Stop;
        }
        if control && key == gdk::Key::a && state_for_key.message_list.has_focus() {
            state_for_key.message_list.select_all();
            return glib::Propagation::Stop;
        }
        if key == gdk::Key::Escape && !state_for_key.selected_messages.borrow().is_empty() {
            clear_selected_rows(&state_for_key);
            return glib::Propagation::Stop;
        }
        if key == gdk::Key::Delete && state_for_key.selected_messages.borrow().len() > 1 {
            apply_bulk_move(&state_for_key, "Trash");
            return glib::Propagation::Stop;
        }
        if key == gdk::Key::a && state_for_key.selected_messages.borrow().len() > 1 {
            apply_bulk_move(&state_for_key, "Archive");
            return glib::Propagation::Stop;
        }
        if state_for_key.message_list.has_focus() {
            if let Some(message) = state_for_key.selected_message.borrow().and_then(|id| {
                state_for_key
                    .messages
                    .borrow()
                    .iter()
                    .find(|message| message.id == id)
                    .cloned()
            }) {
                match key {
                    gdk::Key::r => {
                        open_compose_with_context(
                            state_for_key.clone(),
                            Some(ComposeContext::Reply {
                                message,
                                reply_all: false,
                            }),
                        );
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::f => {
                        open_compose_with_context(
                            state_for_key.clone(),
                            Some(ComposeContext::Forward(message)),
                        );
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }
        }
        let Some(message_id) = *state_for_key.selected_message.borrow() else {
            return glib::Propagation::Proceed;
        };
        match key {
            gdk::Key::a => apply_message_action(&state_for_key, message_id, "archive"),
            gdk::Key::Delete => apply_message_action(&state_for_key, message_id, "trash"),
            gdk::Key::u => apply_message_action(&state_for_key, message_id, "read"),
            gdk::Key::s => apply_message_action(&state_for_key, message_id, "star"),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    state.window.add_controller(controller);
}

fn build_filter_bar() -> (gtk::Box, gtk::SearchEntry, gtk::Button) {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar.add_css_class("mail-filter-bar");

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search messages"));
    search.set_hexpand(true);
    search.set_margin_start(12);
    search.set_margin_top(10);
    search.set_margin_bottom(10);
    search.set_tooltip_text(Some("Search sender, recipients, subject, and message text"));
    search.set_widget_name("message-search");
    bar.append(&search);

    // `view-filter-symbolic` is not provided by the active Omarchy icon theme
    // and falls back to a confusing missing-icon glyph. This native filter
    // glyph is available in the same theme and reads as adjustable filters.
    let filter = icon_button("nautilus-search-filters-symbolic", "Filter messages");
    filter.set_margin_top(10);
    filter.set_margin_bottom(10);
    filter.set_margin_end(12);
    bar.append(&filter);
    (bar, search, filter)
}

fn build_selection_bar() -> (
    gtk::Box,
    gtk::Label,
    gtk::Button,
    gtk::Button,
    gtk::Button,
    gtk::Button,
    gtk::Button,
    gtk::Button,
) {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.add_css_class("mail-selection-bar");
    bar.set_margin_start(12);
    bar.set_margin_end(12);
    bar.set_margin_top(2);
    bar.set_margin_bottom(8);
    let count = gtk::Label::new(Some("0 selected"));
    count.set_xalign(0.0);
    count.set_hexpand(true);
    count.add_css_class("mail-reader-meta");
    bar.append(&count);

    let mark_read = gtk::Button::with_label("Read");
    mark_read.set_tooltip_text(Some("Mark selected messages as read"));
    let mark_unread = gtk::Button::with_label("Unread");
    mark_unread.set_tooltip_text(Some("Mark selected messages as unread"));
    let star = gtk::Button::with_label("Star");
    star.set_tooltip_text(Some("Star selected messages"));
    let archive = gtk::Button::with_label("Archive");
    archive.set_tooltip_text(Some("Archive selected messages"));
    let trash = gtk::Button::with_label("Trash");
    trash.set_tooltip_text(Some("Move selected messages to Trash"));
    trash.add_css_class("destructive-action");
    let clear = gtk::Button::with_label("Clear");
    clear.set_tooltip_text(Some("Clear message selection"));
    for button in [&mark_read, &mark_unread, &star, &archive, &trash, &clear] {
        button.set_has_frame(false);
        bar.append(button);
    }
    bar.set_visible(false);
    (
        bar,
        count,
        mark_read,
        mark_unread,
        star,
        archive,
        trash,
        clear,
    )
}

fn connect_selection_actions(
    state: Rc<AppState>,
    mark_read: &gtk::Button,
    mark_unread: &gtk::Button,
    star: &gtk::Button,
    archive: &gtk::Button,
    trash: &gtk::Button,
    clear: &gtk::Button,
) {
    let state_for_read = state.clone();
    mark_read.connect_clicked(move |_| apply_bulk_flag(&state_for_read, "read", true));
    let state_for_unread = state.clone();
    mark_unread.connect_clicked(move |_| apply_bulk_flag(&state_for_unread, "read", false));
    let state_for_star = state.clone();
    star.connect_clicked(move |_| apply_bulk_flag(&state_for_star, "star", true));
    let state_for_archive = state.clone();
    archive.connect_clicked(move |_| apply_bulk_move(&state_for_archive, "Archive"));
    let state_for_trash = state.clone();
    trash.connect_clicked(move |_| apply_bulk_move(&state_for_trash, "Trash"));
    clear.connect_clicked(move |_| clear_selected_rows(&state));
}

fn update_selection_summary(state: &AppState) {
    let count = state.selected_messages.borrow().len();
    state.selection_count.set_text(&format!("{count} selected"));
    state.selection_bar.set_visible(count > 0);
}

fn clear_selected_rows(state: &Rc<AppState>) {
    state.message_list.unselect_all();
    state.selected_messages.borrow_mut().clear();
    update_selection_summary(state);
}

fn selected_message_ids(state: &Rc<AppState>) -> Vec<i64> {
    state.selected_messages.borrow().iter().copied().collect()
}

fn open_filter_menu(state: Rc<AppState>, button: &gtk::Button) {
    let popover = gtk::Popover::new();
    popover.set_parent(button);
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu.set_margin_top(6);
    menu.set_margin_bottom(6);
    menu.set_margin_start(6);
    menu.set_margin_end(6);
    let heading = gtk::Label::new(Some("FILTER SEARCH"));
    heading.set_xalign(0.0);
    heading.add_css_class("mail-section-label");
    menu.append(&heading);

    let filter_rows: [(&str, bool, fn(&mut SearchFilters, bool)); 3] = [
        (
            "Unread",
            state.search_filters.borrow().unread,
            |filters: &mut SearchFilters, active| filters.unread = active,
        ),
        (
            "Starred",
            state.search_filters.borrow().starred,
            |filters: &mut SearchFilters, active| filters.starred = active,
        ),
        (
            "With attachments",
            state.search_filters.borrow().attachments,
            |filters: &mut SearchFilters, active| filters.attachments = active,
        ),
    ];
    for (label, selected, setter) in filter_rows {
        let item = gtk::CheckButton::with_label(label);
        item.set_active(selected);
        let state_for_item = state.clone();
        item.connect_toggled(move |item| {
            setter(
                &mut state_for_item.search_filters.borrow_mut(),
                item.is_active(),
            );
            render_messages(&state_for_item, state_for_item.search_entry.text().as_str());
        });
        menu.append(&item);
    }

    let divider = gtk::Separator::new(gtk::Orientation::Horizontal);
    divider.set_margin_top(5);
    divider.set_margin_bottom(5);
    menu.append(&divider);

    let accounts = state.accounts.borrow().clone();
    let mut account_labels = vec!["All accounts".to_string()];
    account_labels.extend(accounts.iter().map(|account| {
        if account.display_name.trim().is_empty() {
            account.email.clone()
        } else {
            format!("{} <{}>", account.display_name, account.email)
        }
    }));
    let account_refs = account_labels
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let account_selector = gtk::DropDown::from_strings(&account_refs);
    account_selector.set_hexpand(true);
    if let Some(account_id) = state.search_filters.borrow().account_id {
        if let Some(index) = accounts
            .iter()
            .position(|account| account.id == Some(account_id))
        {
            account_selector.set_selected((index + 1) as u32);
        }
    }
    let account_row = form_row("Account", &account_selector);
    menu.append(&account_row);
    let state_for_account = state.clone();
    account_selector.connect_selected_notify(move |selector| {
        let selected = selector.selected() as usize;
        state_for_account.search_filters.borrow_mut().account_id = accounts
            .get(selected.saturating_sub(1))
            .and_then(|account| account.id);
        render_messages(
            &state_for_account,
            state_for_account.search_entry.text().as_str(),
        );
    });

    let mut folder_names = vec!["All folders".to_string()];
    for folder in [
        "Inbox", "Sent", "Drafts", "Archive", "Spam", "Trash", "Outbox",
    ] {
        folder_names.push(folder.to_string());
    }
    for folder in state
        .folders
        .borrow()
        .iter()
        .map(|folder| folder.name.clone())
    {
        if !folder_names
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&folder))
        {
            folder_names.push(folder);
        }
    }
    let folder_refs = folder_names.iter().map(String::as_str).collect::<Vec<_>>();
    let folder_selector = gtk::DropDown::from_strings(&folder_refs);
    folder_selector.set_hexpand(true);
    if let Some(folder) = state.search_filters.borrow().folder.as_deref() {
        if let Some(index) = folder_names.iter().position(|known| known == folder) {
            folder_selector.set_selected(index as u32);
        }
    }
    let folder_row = form_row("Folder", &folder_selector);
    menu.append(&folder_row);
    let state_for_folder = state.clone();
    folder_selector.connect_selected_notify(move |selector| {
        state_for_folder.search_filters.borrow_mut().folder = folder_names
            .get(selector.selected() as usize)
            .filter(|folder| folder.as_str() != "All folders")
            .cloned();
        render_messages(
            &state_for_folder,
            state_for_folder.search_entry.text().as_str(),
        );
    });

    let date_heading = gtk::Label::new(Some("DATE (YYYY-MM-DD)"));
    date_heading.set_xalign(0.0);
    date_heading.add_css_class("mail-section-label");
    date_heading.set_margin_top(6);
    menu.append(&date_heading);
    for (placeholder, field) in [("After date", true), ("Before date", false)] {
        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some(placeholder));
        let current = if field {
            state
                .search_filters
                .borrow()
                .after
                .clone()
                .unwrap_or_default()
        } else {
            state
                .search_filters
                .borrow()
                .before
                .clone()
                .unwrap_or_default()
        };
        entry.set_text(&current);
        let state_for_date = state.clone();
        entry.connect_changed(move |entry| {
            let value = entry.text().trim().to_string();
            let value = (!value.is_empty()).then_some(value);
            if field {
                state_for_date.search_filters.borrow_mut().after = value;
            } else {
                state_for_date.search_filters.borrow_mut().before = value;
            }
            render_messages(&state_for_date, state_for_date.search_entry.text().as_str());
        });
        menu.append(&entry);
    }

    let clear = gtk::Button::with_label("Clear filters");
    clear.set_halign(gtk::Align::End);
    let state_for_clear = state.clone();
    let popover_for_clear = popover.clone();
    clear.connect_clicked(move |_| {
        state_for_clear
            .search_filters
            .replace(SearchFilters::default());
        render_messages(
            &state_for_clear,
            state_for_clear.search_entry.text().as_str(),
        );
        popover_for_clear.popdown();
    });
    menu.append(&clear);
    popover.set_child(Some(&menu));
    popover.popup();
}

fn render_sidebar(state: &Rc<AppState>) {
    clear(&state.sidebar);
    let inner = gtk::Box::new(gtk::Orientation::Vertical, 8);
    inner.set_margin_start(14);
    inner.set_margin_end(14);
    inner.set_margin_top(18);
    inner.set_margin_bottom(18);

    let unified = gtk::Label::new(Some("UNIFIED"));
    unified.set_xalign(0.0);
    unified.add_css_class("mail-section-label");
    inner.append(&unified);

    let inbox_messages = if state.demo_mode {
        state.messages.borrow().clone()
    } else {
        state
            .database
            .list_messages_filtered(None, Some("Inbox"), false)
            .unwrap_or_else(|_| state.messages.borrow().clone())
    };
    let unread = if state.demo_mode {
        inbox_messages
            .iter()
            .filter(|message| message.unread)
            .count()
    } else {
        state
            .database
            .unread_count(None, "Inbox")
            .unwrap_or_else(|_| {
                inbox_messages
                    .iter()
                    .filter(|message| message.unread)
                    .count()
            })
    };
    let outbox_count = if state.demo_mode {
        0
    } else {
        state
            .database
            .pending_sends(None)
            .map(|sends| sends.len())
            .unwrap_or_default()
    };
    for (label, icon, count) in [
        ("Inbox", "mail-unread-symbolic", Some(unread)),
        ("Starred", "starred-symbolic", None),
        ("Sent", "mail-send-symbolic", None),
        ("Drafts", "document-save-symbolic", None),
        ("Outbox", "mail-send-symbolic", Some(outbox_count)),
        ("Archive", "archive-symbolic", None),
        ("Trash", "user-trash-symbolic", None),
    ] {
        inner.append(&sidebar_action_row(
            state,
            label,
            icon,
            count,
            MailScope::Unified(label.into()),
        ));
    }
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_top(12);
    separator.set_margin_bottom(6);
    inner.append(&separator);

    let accounts_heading = gtk::Label::new(Some("ACCOUNTS"));
    accounts_heading.set_xalign(0.0);
    accounts_heading.add_css_class("mail-section-label");
    inner.append(&accounts_heading);

    let accounts = state.accounts.borrow();
    if accounts.is_empty() {
        let hint = gtk::Label::new(Some("Add an account to see its folders here."));
        hint.set_wrap(true);
        hint.set_xalign(0.0);
        hint.add_css_class("mail-empty-body");
        inner.append(&hint);
    } else {
        for account in accounts.iter() {
            inner.append(&account_expander(account, state.clone()));
        }
    }
    drop(accounts);

    let add_account = gtk::Button::with_label("Add account");
    add_account.add_css_class("mail-accent-button");
    add_account.set_margin_top(12);
    let state_for_account = state.clone();
    add_account.connect_clicked(move |_| open_account_dialog(state_for_account.clone()));
    inner.append(&add_account);

    state.sidebar.append(&inner);
}

fn account_expander(account: &Account, state: Rc<AppState>) -> gtk::Expander {
    let title = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let dot = gtk::Label::new(Some("●"));
    dot.add_css_class("mail-unread-dot");
    title.append(&dot);
    let name = gtk::Label::new(Some(if account.display_name.is_empty() {
        &account.email
    } else {
        &account.display_name
    }));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.add_css_class("mail-account-title");
    title.append(&name);
    let expander = gtk::Expander::new(None);
    expander.set_label_widget(Some(&title));

    let Some(account_id) = account.id else {
        expander.set_expanded(true);
        return expander;
    };
    let expanded = state
        .account_expanded
        .borrow()
        .get(&account_id)
        .copied()
        .unwrap_or_else(|| {
            !state
                .preferences
                .borrow()
                .account_is_collapsed(&account.email)
        });
    expander.set_expanded(expanded);
    let known_folders = state
        .folders
        .borrow()
        .iter()
        .filter(|folder| folder.account_id == account_id)
        .cloned()
        .collect::<Vec<_>>();
    let folders = gtk::Box::new(gtk::Orientation::Vertical, 2);
    folders.set_margin_start(24);
    for name in [
        "Inbox", "Drafts", "Sent", "Outbox", "Archive", "Spam", "Trash",
    ] {
        let count = if name == "Outbox" {
            state
                .database
                .pending_sends(Some(account_id))
                .map(|sends| Some(sends.len()))
                .unwrap_or(Some(0))
        } else {
            state
                .database
                .unread_count(Some(account_id), name)
                .ok()
                .or_else(|| {
                    known_folders
                        .iter()
                        .find(|folder| folder.name.eq_ignore_ascii_case(name))
                        .map(|folder| folder.unread_count as usize)
                })
        };
        folders.append(&sidebar_action_row(
            &state,
            name,
            "folder-symbolic",
            count,
            MailScope::Account {
                id: account_id,
                folder: name.into(),
            },
        ));
    }
    let custom = known_folders
        .iter()
        .filter(|folder| folder.kind == "custom")
        .collect::<Vec<_>>();
    if !custom.is_empty() {
        let custom_expander = gtk::Expander::new(Some("Folders"));
        let custom_rows = gtk::Box::new(gtk::Orientation::Vertical, 2);
        custom_rows.set_margin_start(12);
        for folder in custom {
            custom_rows.append(&sidebar_action_row(
                &state,
                &folder.name,
                "folder-symbolic",
                state
                    .database
                    .unread_count(Some(account_id), &folder.name)
                    .ok()
                    .or(Some(folder.unread_count as usize)),
                MailScope::Account {
                    id: account_id,
                    folder: folder.name.clone(),
                },
            ));
        }
        custom_expander.set_child(Some(&custom_rows));
        folders.append(&custom_expander);
    }
    let new_folder = gtk::Button::with_label("New folder");
    new_folder.set_has_frame(false);
    new_folder.set_halign(gtk::Align::Start);
    new_folder.set_margin_start(12);
    new_folder.set_margin_top(4);
    new_folder.add_css_class("mail-sidebar-secondary");
    let state_for_new_folder = state.clone();
    new_folder
        .connect_clicked(move |_| open_new_folder_dialog(state_for_new_folder.clone(), account_id));
    folders.append(&new_folder);
    expander.set_child(Some(&folders));
    let account_email = account.email.clone();
    let state_for_expander = state.clone();
    expander.connect_expanded_notify(move |expander| {
        let expanded = expander.is_expanded();
        state_for_expander
            .account_expanded
            .borrow_mut()
            .insert(account_id, expanded);
        let mut preferences = state_for_expander.preferences.borrow_mut();
        preferences.set_account_collapsed(&account_email, !expanded);
        let _ = preferences::save(&preferences);
    });
    expander
}

fn sidebar_row(label: &str, icon: &str, count: Option<usize>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.add_css_class("mail-sidebar-row");
    row.set_margin_top(1);
    row.set_margin_bottom(1);
    row.set_margin_start(4);
    row.set_margin_end(4);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(17);
    row.append(&image);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    row.append(&text);
    if let Some(count) = count.filter(|count| *count > 0) {
        let count_label = gtk::Label::new(Some(&count.to_string()));
        count_label.add_css_class("mail-count");
        row.append(&count_label);
    }
    row
}

fn sidebar_action_row(
    state: &Rc<AppState>,
    label: &str,
    icon: &str,
    count: Option<usize>,
    scope: MailScope,
) -> gtk::Button {
    let row = sidebar_row(label, icon, count);
    if *state.scope.borrow() == scope {
        row.add_css_class("selected");
    }
    let button = gtk::Button::new();
    button.set_has_frame(false);
    button.set_halign(gtk::Align::Fill);
    button.set_child(Some(&row));
    button.set_tooltip_text(Some(&format!("Show {label}")));
    let state_for_click = state.clone();
    let scope_for_click = scope.clone();
    button.connect_clicked(move |_| select_scope(&state_for_click, scope_for_click.clone()));
    if let MailScope::Account { id, folder } = &scope {
        let account_id = *id;
        let local_name = folder.clone();
        let state_for_menu = state.clone();
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        gesture.connect_pressed(move |gesture, _, x, y| {
            let Some(widget) = gesture.widget() else {
                return;
            };
            let Ok(button) = widget.downcast::<gtk::Button>() else {
                return;
            };
            open_folder_menu(
                state_for_menu.clone(),
                &button,
                account_id,
                &local_name,
                x,
                y,
            );
        });
        button.add_controller(gesture);
    }
    button
}

fn select_scope(state: &Rc<AppState>, scope: MailScope) {
    let account_scope = match &scope {
        MailScope::Account { id, folder } => Some((*id, folder.clone())),
        MailScope::Unified(_) => None,
    };
    state.scope.replace(scope);
    state.selected_message.replace(None);
    load_messages_for_scope(state);
    render_sidebar(state);
    render_messages(state, state.search_entry.text().as_str());
    render_reader(state, None);
    if let Some((account_id, folder)) = account_scope.filter(|(_, folder)| folder != "Outbox") {
        sync_folder_for_scope(state, account_id, &folder);
    }
}

fn load_messages_for_scope(state: &Rc<AppState>) {
    if state.demo_mode {
        return;
    }
    state.message_offset.set(0);
    state.has_more_messages.set(false);
    let scope = state.scope.borrow().clone();
    if let Some(account_id) = match scope {
        MailScope::Unified(folder) if folder == "Outbox" => Some(None),
        MailScope::Account { id, folder } if folder == "Outbox" => Some(Some(id)),
        _ => None,
    } {
        state
            .messages
            .replace(load_outbox_messages(state, account_id));
        state.message_offset.set(state.messages.borrow().len());
        return;
    }
    let scope = state.scope.borrow().clone();
    let result = match scope {
        MailScope::Unified(folder) if folder == "Starred" => state
            .database
            .list_messages_filtered_page(None, None, true, MESSAGE_PAGE_SIZE, 0),
        MailScope::Unified(folder) => state.database.list_messages_filtered_page(
            None,
            Some(&folder),
            false,
            MESSAGE_PAGE_SIZE,
            0,
        ),
        MailScope::Account { id, folder } => state.database.list_messages_filtered_page(
            Some(id),
            Some(&folder),
            false,
            MESSAGE_PAGE_SIZE,
            0,
        ),
    };
    if let Ok(messages) = result {
        state
            .has_more_messages
            .set(messages.len() == MESSAGE_PAGE_SIZE);
        state.message_offset.set(messages.len());
        state.messages.replace(messages);
    }
}

fn load_more_messages(state: &Rc<AppState>) {
    if state.demo_mode {
        return;
    }
    let query = state.search_entry.text();
    if !query.trim().is_empty() {
        set_status(state, "Search results are shown from the local index");
        return;
    }
    let scope = state.scope.borrow().clone();
    let (account_id, folder, starred_only) = match &scope {
        MailScope::Unified(folder) if folder == "Starred" => (None, None, true),
        MailScope::Unified(folder) => (None, Some(folder.clone()), false),
        MailScope::Account { id, folder } => (Some(*id), Some(folder.clone()), false),
    };
    if folder.as_deref() == Some("Outbox") {
        return;
    }
    let offset = state.message_offset.get();
    state.load_more.set_sensitive(false);
    let database = state.database.clone();
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = database.list_messages_filtered_page(
            account_id,
            folder.as_deref(),
            starred_only,
            MESSAGE_PAGE_SIZE,
            offset,
        );
        let _ = sender.send_blocking(result);
    });
    let state_for_result = state.clone();
    glib::MainContext::default().spawn_local(async move {
        state_for_result.load_more.set_sensitive(true);
        match receiver.recv().await {
            Ok(Ok(messages)) => {
                if *state_for_result.scope.borrow() != scope {
                    return;
                }
                let loaded = messages.len();
                state_for_result.messages.borrow_mut().extend(messages);
                state_for_result
                    .message_offset
                    .set(offset.saturating_add(loaded));
                state_for_result
                    .has_more_messages
                    .set(loaded == MESSAGE_PAGE_SIZE);
                render_messages(
                    &state_for_result,
                    state_for_result.search_entry.text().as_str(),
                );
                if loaded == 0 {
                    set_status(&state_for_result, "All messages loaded");
                } else {
                    set_status(&state_for_result, &format!("Loaded {loaded} more messages"));
                }
            }
            Ok(Err(error)) => {
                set_status(
                    &state_for_result,
                    &format!("Couldn’t load more messages: {error}"),
                );
            }
            Err(_) => set_status(
                &state_for_result,
                "The message loading worker stopped unexpectedly.",
            ),
        }
    });
}

fn load_outbox_messages(state: &Rc<AppState>, account_id: Option<i64>) -> Vec<Message> {
    state
        .database
        .pending_sends(account_id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|send| {
            state
                .accounts
                .borrow()
                .iter()
                .find(|account| account.id == Some(send.account_id))
                .cloned()
                .map(|account| pending_send_message(&send, &account))
        })
        .collect()
}

fn pending_send_message(send: &PendingSend, account: &Account) -> Message {
    let mut recipients = send.to.clone();
    if !send.cc.is_empty() {
        recipients.push_str(&format!("\nCc: {}", send.cc.join(", ")));
    }
    if !send.bcc.is_empty() {
        recipients.push_str(&format!("\nBcc: {}", send.bcc.join(", ")));
    }
    let attachments = send
        .attachments
        .iter()
        .map(|attachment| AttachmentInfo {
            filename: attachment.filename.clone(),
            content_type: "application/octet-stream".into(),
            size: std::fs::metadata(&attachment.path)
                .map(|metadata| metadata.len())
                .unwrap_or_default(),
            cache_path: attachment.path.clone(),
            content_id: None,
        })
        .collect::<Vec<_>>();
    Message {
        id: -send.id,
        account_id: Some(send.account_id),
        folder: "Outbox".into(),
        remote_uid: None,
        uidvalidity: None,
        message_id: None,
        thread_key: Some(format!("outbox:{}", send.id)),
        sender_name: account.display_name.clone(),
        sender_email: account.email.clone(),
        recipients,
        subject: if send.subject.trim().is_empty() {
            "(no subject)".into()
        } else {
            send.subject.clone()
        },
        preview: send
            .body
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(160)
            .collect(),
        body: send.body.clone(),
        body_html: send.body_html.clone(),
        received_at: send.created_at.clone(),
        unread: false,
        starred: false,
        has_attachments: !attachments.is_empty(),
        attachments,
        thread_size: 1,
    }
}

fn sync_folder_for_scope(state: &Rc<AppState>, account_id: i64, local_name: &str) {
    let Some(account) = state
        .accounts
        .borrow()
        .iter()
        .find(|account| account.id == Some(account_id))
        .cloned()
    else {
        return;
    };
    let remote_name = state
        .folders
        .borrow()
        .iter()
        .find(|folder| folder.account_id == account_id && folder.name == local_name)
        .map(|folder| folder.remote_name.clone())
        .unwrap_or_else(|| default_remote_folder(local_name).to_string());
    set_status(state, &format!("Loading {local_name}…"));
    let (sender, receiver) = async_channel::bounded(1);
    crate::mail::sync::spawn_folder_sync(
        account,
        state.database.clone(),
        remote_name,
        local_name.to_string(),
        sender,
    );
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        match receiver.recv().await {
            Ok(report) if report.error.is_none() => {
                load_messages_for_scope(&state);
                render_sidebar(&state);
                render_messages(&state, state.search_entry.text().as_str());
                let suffix = if report.skipped_messages == 0 {
                    String::new()
                } else {
                    format!(" · skipped {} malformed", report.skipped_messages)
                };
                set_status(
                    &state,
                    &format!(
                        "{} ready · {} new{suffix}",
                        report.folder, report.new_messages
                    ),
                );
            }
            Ok(report) => set_status(
                &state,
                &format!(
                    "Couldn’t load {}: {}",
                    report.folder,
                    report.error.unwrap_or_default()
                ),
            ),
            Err(_) => set_status(&state, "The folder worker stopped unexpectedly."),
        }
    });
}

fn default_remote_folder(local_name: &str) -> &str {
    match local_name {
        "Inbox" => "INBOX",
        "Drafts" => "Drafts",
        "Sent" => "Sent",
        "Archive" => "Archive",
        "Spam" => "Spam",
        "Trash" => "Trash",
        other => other,
    }
}

fn render_messages(state: &Rc<AppState>, query: &str) {
    state.message_list.unselect_all();
    state.selected_messages.borrow_mut().clear();
    update_selection_summary(state);
    clear(&state.message_list);
    state.load_more.set_visible(false);
    let raw_query = query.trim();
    let query = raw_query.to_lowercase();
    let scope = state.scope.borrow().clone();
    let filters = state.search_filters.borrow().clone();
    let is_outbox_scope = matches!(
        &scope,
        MailScope::Unified(folder) | MailScope::Account { folder, .. } if folder == "Outbox"
    );
    let messages = if raw_query.is_empty() || state.demo_mode || is_outbox_scope {
        state.messages.borrow().clone()
    } else {
        state
            .database
            .search_messages_page(raw_query, MESSAGE_PAGE_SIZE, 0)
            .unwrap_or_else(|_| state.messages.borrow().clone())
    };
    let can_load_more = raw_query.is_empty()
        && !state.demo_mode
        && !is_outbox_scope
        && state.has_more_messages.get();
    let mut visible = messages
        .into_iter()
        .filter(|message| {
            let in_scope = match &scope {
                MailScope::Unified(folder) if folder == "Starred" => message.starred,
                MailScope::Unified(folder) => message.folder.eq_ignore_ascii_case(folder),
                MailScope::Account { id, folder } => {
                    message.account_id == Some(*id) && message.folder.eq_ignore_ascii_case(folder)
                }
            };
            in_scope
                && search_filters_match(message, &filters)
                && search_text_matches(message, &query)
        })
        .collect::<Vec<_>>();
    if visible.is_empty() {
        let (title, subtitle) = if !raw_query.is_empty() {
            ("No messages found", "Try a different search.")
        } else if is_outbox_scope {
            (
                "Outbox is clear",
                "Messages waiting to send will appear here.",
            )
        } else if matches!(&scope, MailScope::Unified(folder) if folder == "Inbox") {
            ("You’re all caught up", "New messages will appear here.")
        } else {
            ("No messages yet", "There’s nothing here to show.")
        };
        append_empty_message_state(&state.message_list, title, subtitle);
        state.load_more.set_visible(can_load_more);
        return;
    }
    visible.sort_by(compare_received_newest);
    let visible = if state.preferences.borrow().conversation_view {
        group_messages(visible)
    } else {
        visible
    };
    for message in visible.iter() {
        let (row, star) = message_row(message);
        let message_id = message.id;
        let state_for_star = state.clone();
        star.connect_clicked(move |_| {
            if message_id >= 0 {
                apply_message_action(&state_for_star, message_id, "star");
            }
        });

        if message_id >= 0 {
            let gesture = gtk::GestureClick::new();
            gesture.set_button(3);
            let state_for_menu = state.clone();
            gesture.connect_pressed(move |gesture, _, x, y| {
                let Some(widget) = gesture.widget() else {
                    return;
                };
                let Ok(row) = widget.downcast::<gtk::ListBoxRow>() else {
                    return;
                };
                open_message_menu(state_for_menu.clone(), &row, message_id, x, y);
            });
            row.add_controller(gesture);
        }
        state.message_list.append(&row);
    }
    state.load_more.set_visible(can_load_more);
}

fn search_filters_match(message: &Message, filters: &SearchFilters) -> bool {
    filters
        .account_id
        .is_none_or(|account_id| message.account_id == Some(account_id))
        && filters
            .folder
            .as_deref()
            .is_none_or(|folder| message.folder.eq_ignore_ascii_case(folder))
        && (!filters.unread || message.unread)
        && (!filters.starred || message.starred)
        && (!filters.attachments || message.has_attachments)
        && filters
            .after
            .as_deref()
            .is_none_or(|date| message_date_matches(message, date, true))
        && filters
            .before
            .as_deref()
            .is_none_or(|date| message_date_matches(message, date, false))
}

fn search_text_matches(message: &Message, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    let fields = [
        message.sender_name.as_str(),
        message.sender_email.as_str(),
        message.recipients.as_str(),
        message.subject.as_str(),
        message.body.as_str(),
    ]
    .iter()
    .map(|value| value.to_lowercase())
    .collect::<Vec<_>>();
    query
        .split_whitespace()
        .all(|term| fields.iter().any(|field| field.contains(term)))
}

fn message_date_matches(message: &Message, value: &str, after: bool) -> bool {
    let Ok(filter_date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") else {
        return true;
    };
    let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(&message.received_at) else {
        return false;
    };
    if after {
        timestamp.date_naive() >= filter_date
    } else {
        timestamp.date_naive() <= filter_date
    }
}

fn append_empty_message_state(list: &gtk::ListBox, title: &str, subtitle: &str) {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(false);
    let empty = gtk::Box::new(gtk::Orientation::Vertical, 8);
    empty.set_halign(gtk::Align::Center);
    empty.set_valign(gtk::Align::Center);
    empty.set_margin_top(72);
    empty.set_margin_bottom(72);
    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("mail-empty-title");
    let subtitle_label = gtk::Label::new(Some(subtitle));
    subtitle_label.add_css_class("mail-empty-body");
    empty.append(&title_label);
    empty.append(&subtitle_label);
    row.set_child(Some(&empty));
    list.append(&row);
}

fn conversation_identity(message: &Message) -> (Option<i64>, String, String) {
    (
        message.account_id,
        message.folder.clone(),
        message
            .thread_key
            .clone()
            .unwrap_or_else(|| format!("message:{}", message.id)),
    )
}

fn group_messages(messages: Vec<Message>) -> Vec<Message> {
    let mut grouped: Vec<((Option<i64>, String, String), Message, usize)> = Vec::new();
    for message in messages {
        let identity = conversation_identity(&message);
        if let Some((_, representative, count)) = grouped
            .iter_mut()
            .find(|(known_identity, _, _)| *known_identity == identity)
        {
            *count += 1;
            representative.unread |= message.unread;
            representative.starred |= message.starred;
            representative.has_attachments |= message.has_attachments;
            representative.thread_size = *count as u32;
        } else {
            let mut representative = message;
            representative.thread_size = 1;
            grouped.push((identity, representative, 1));
        }
    }
    grouped
        .into_iter()
        .map(|(_, representative, _)| representative)
        .collect()
}

fn sync_all(state: Rc<AppState>, notify: bool) {
    let accounts = state
        .accounts
        .borrow()
        .iter()
        .filter(|account| account.enabled)
        .cloned()
        .collect::<Vec<_>>();
    if accounts.is_empty() {
        set_status(&state, "Add an account before synchronising");
        return;
    }

    set_status(&state, "Synchronising…");
    let account_count = accounts.len();
    let (sender, receiver) = async_channel::bounded(account_count);
    for account in accounts {
        crate::mail::sync::spawn_account_sync(account, state.database.clone(), sender.clone());
    }
    drop(sender);

    glib::MainContext::default().spawn_local(async move {
        let mut completed = 0;
        let mut errors = Vec::new();
        let mut skipped_messages = 0;
        while let Ok(report) = receiver.recv().await {
            completed += 1;
            skipped_messages += report.skipped_messages;
            if let Some(error) = report.error {
                errors.push(format!("{}: {error}", report.email));
            } else if notify
                && report.new_messages > 0
                && notifications_allowed(&state, report.account_id)
            {
                crate::mail::sync::notify_new_mail(
                    &report.email,
                    report.new_messages,
                    report.newest_message.as_ref(),
                );
            }
            if let Ok(folders) = state.database.load_folders() {
                state.folders.replace(folders);
            }
            load_messages_for_scope(&state);
            render_sidebar(&state);
            render_messages(&state, state.search_entry.text().as_str());
            set_status(&state, &format!("Synchronised {completed}/{account_count}"));
        }
        if errors.is_empty() {
            if skipped_messages == 0 {
                set_status(&state, "All accounts are up to date");
            } else {
                set_status(
                    &state,
                    &format!(
                        "All accounts are up to date · skipped {skipped_messages} malformed message(s)"
                    ),
                );
            }
        } else {
            set_status(
                &state,
                &format!("Sync needs attention: {}", errors.join(" · ")),
            );
        }
    });
}

fn notifications_allowed(state: &AppState, account_id: Option<i64>) -> bool {
    state.preferences.borrow().notifications_enabled
        && state
            .accounts
            .borrow()
            .iter()
            .find(|account| account.id == account_id)
            .map(|account| account.notify)
            .unwrap_or(true)
}

fn start_account_monitors(state: Rc<AppState>) {
    let accounts = state
        .accounts
        .borrow()
        .iter()
        .filter(|account| account.enabled)
        .cloned()
        .collect::<Vec<_>>();
    for account in accounts {
        start_account_monitor(&state, account);
    }
}

fn start_account_monitor(state: &Rc<AppState>, account: Account) {
    let Some(account_id) = account.id else {
        return;
    };
    if state.monitor_stops.borrow().contains_key(&account_id) {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    state
        .monitor_stops
        .borrow_mut()
        .insert(account_id, stop.clone());
    let outbox_account = account.clone();
    mail::sync::spawn_account_monitor(
        account,
        state.database.clone(),
        state.monitor_sender.clone(),
        stop,
    );
    start_account_outbox_monitor(state, outbox_account);
}

fn start_account_outbox_monitor(state: &Rc<AppState>, account: Account) {
    let Some(account_id) = account.id else {
        return;
    };
    if state.outbox_stops.borrow().contains_key(&account_id) {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    state
        .outbox_stops
        .borrow_mut()
        .insert(account_id, stop.clone());
    mail::sync::spawn_outbox_monitor(
        account,
        state.database.clone(),
        state.outbox_sender.clone(),
        stop,
    );
}

fn listen_for_monitor_reports(
    state: Rc<AppState>,
    receiver: async_channel::Receiver<mail::sync::SyncReport>,
) {
    glib::MainContext::default().spawn_local(async move {
        while let Ok(report) = receiver.recv().await {
            let should_notify = !report.initial
                && report.new_messages > 0
                && report.error.is_none()
                && notifications_allowed(&state, report.account_id);
            if should_notify {
                mail::sync::notify_new_mail(
                    &report.email,
                    report.new_messages,
                    report.newest_message.as_ref(),
                );
            }

            if let Ok(folders) = state.database.load_folders() {
                state.folders.replace(folders);
            }
            load_messages_for_scope(&state);
            render_sidebar(&state);
            render_messages(&state, state.search_entry.text().as_str());

            if let Some(error) = report.error {
                set_status(&state, &format!("{}: {error}", report.email));
            } else if report.new_messages > 0 {
                set_status(
                    &state,
                    &format!("{} · {} new", report.email, report.new_messages),
                );
            } else {
                let suffix = if report.skipped_messages == 0 {
                    String::new()
                } else {
                    format!(" · skipped {} malformed", report.skipped_messages)
                };
                set_status(
                    &state,
                    &format!(
                        "{} is up to date · {} cached{suffix}",
                        report.email, report.fetched
                    ),
                );
            }
        }
    });
}

fn listen_for_outbox_reports(
    state: Rc<AppState>,
    receiver: async_channel::Receiver<mail::sync::OutboxReport>,
) {
    glib::MainContext::default().spawn_local(async move {
        while let Ok(report) = receiver.recv().await {
            if report.sent > 0 {
                refresh_cached_view(&state);
                set_status(
                    &state,
                    &format!(
                        "{} queued message{} sent",
                        report.sent,
                        if report.sent == 1 { "" } else { "s" }
                    ),
                );
            }
            if let Some(error) = report.error {
                set_status(&state, &format!("{} · {error}", report.email));
            }
        }
    });
}

fn message_row(message: &Message) -> (gtk::ListBoxRow, gtk::Button) {
    let row = gtk::ListBoxRow::new();
    row.set_widget_name(&format!("message-row-{}", message.id));
    row.add_css_class("mail-message-row");
    if message.unread {
        row.add_css_class("unread");
    }

    let layout = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    layout.set_hexpand(true);
    layout.set_size_request(0, -1);
    let indicator = gtk::Label::new(Some(if message.unread { "●" } else { " " }));
    indicator.add_css_class("mail-unread-dot");
    indicator.set_valign(gtk::Align::Start);
    indicator.set_size_request(12, -1);
    layout.append(&indicator);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 4);
    text.set_hexpand(true);
    text.set_size_request(0, -1);
    let sender = gtk::Label::new(Some(&message.sender_name));
    sender.set_xalign(0.0);
    sender.set_single_line_mode(true);
    sender.set_ellipsize(gtk::pango::EllipsizeMode::End);
    sender.set_width_chars(1);
    sender.set_size_request(0, -1);
    sender.add_css_class("mail-sender");
    text.append(&sender);

    let subject = gtk::Label::new(Some(&message.subject));
    subject.set_xalign(0.0);
    subject.set_single_line_mode(true);
    subject.set_ellipsize(gtk::pango::EllipsizeMode::End);
    subject.set_width_chars(1);
    subject.set_size_request(0, -1);
    subject.add_css_class("mail-subject");
    text.append(&subject);
    let preview = gtk::Label::new(Some(&message.preview));
    preview.set_xalign(0.0);
    preview.set_single_line_mode(true);
    preview.set_ellipsize(gtk::pango::EllipsizeMode::End);
    preview.set_width_chars(1);
    preview.set_size_request(0, -1);
    preview.add_css_class("mail-preview");
    text.append(&preview);
    layout.append(&text);

    let trailing = gtk::Box::new(gtk::Orientation::Vertical, 4);
    trailing.set_valign(gtk::Align::Start);
    trailing.set_size_request(96, -1);
    let date = gtk::Label::new(Some(&format_message_date(&message.received_at)));
    date.set_single_line_mode(true);
    date.set_halign(gtk::Align::End);
    date.set_xalign(1.0);
    date.add_css_class("mail-date");
    trailing.append(&date);
    let markers = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    markers.set_halign(gtk::Align::End);
    if message.has_attachments {
        let attachment = gtk::Image::from_icon_name("mail-attachment-symbolic");
        attachment.add_css_class("mail-attachment");
        markers.append(&attachment);
    }
    if message.thread_size > 1 {
        let thread = gtk::Label::new(Some(&message.thread_size.to_string()));
        thread.add_css_class("mail-count");
        markers.append(&thread);
    }
    let star = icon_button(
        if message.starred {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        },
        "Star or unstar message",
    );
    star.add_css_class("mail-star");
    star.set_sensitive(message.id >= 0);
    star.set_widget_name(&format!("star-{}", message.id));
    markers.append(&star);
    trailing.append(&markers);
    layout.append(&trailing);
    row.set_child(Some(&layout));
    (row, star)
}

fn format_message_date(value: &str) -> String {
    format_message_date_at(value, chrono::Local::now().date_naive())
}

fn format_message_date_at(value: &str, today: chrono::NaiveDate) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) else {
        return value.to_string();
    };
    let local = parsed.with_timezone(&chrono::Local);
    let date = local.date_naive();
    if date == today {
        format!("Today, {}", local.format("%H:%M"))
    } else if date == today - chrono::Duration::days(1) {
        "Yesterday".into()
    } else if date.year() == today.year() {
        local.format("%-d %b").to_string()
    } else {
        local.format("%-d %b %Y").to_string()
    }
}

fn format_message_datetime(value: &str) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) else {
        return value.to_string();
    };
    parsed
        .with_timezone(&chrono::Local)
        .format("%a, %-d %B %Y at %H:%M")
        .to_string()
}

fn open_folder_menu(
    state: Rc<AppState>,
    parent: &gtk::Button,
    account_id: i64,
    local_name: &str,
    x: f64,
    y: f64,
) {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(true);
    popover.set_parent(parent);
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu.set_margin_top(6);
    menu.set_margin_bottom(6);
    menu.set_margin_start(6);
    menu.set_margin_end(6);

    let new_folder = gtk::Button::with_label("New folder…");
    new_folder.set_has_frame(false);
    let state_for_new = state.clone();
    let popover_for_new = popover.clone();
    new_folder.connect_clicked(move |_| {
        open_new_folder_dialog(state_for_new.clone(), account_id);
        popover_for_new.popdown();
    });
    menu.append(&new_folder);

    let folder = state
        .folders
        .borrow()
        .iter()
        .find(|folder| {
            folder.account_id == account_id
                && folder.name.eq_ignore_ascii_case(local_name)
                && folder.kind == "custom"
        })
        .cloned();
    if let Some(folder) = folder {
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.set_margin_top(4);
        separator.set_margin_bottom(4);
        menu.append(&separator);

        let rename = gtk::Button::with_label("Rename folder…");
        rename.set_has_frame(false);
        let state_for_rename = state.clone();
        let popover_for_rename = popover.clone();
        let folder_for_rename = folder.clone();
        rename.connect_clicked(move |_| {
            open_rename_folder_dialog(state_for_rename.clone(), folder_for_rename.clone());
            popover_for_rename.popdown();
        });
        menu.append(&rename);

        let delete = gtk::Button::with_label("Delete folder…");
        delete.set_has_frame(false);
        let state_for_delete = state.clone();
        let popover_for_delete = popover.clone();
        delete.connect_clicked(move |_| {
            open_delete_folder_dialog(state_for_delete.clone(), folder.clone());
            popover_for_delete.popdown();
        });
        menu.append(&delete);
    }
    popover.set_child(Some(&menu));
    popover.popup();
}

fn open_new_folder_dialog(state: Rc<AppState>, account_id: i64) {
    open_folder_name_dialog(state, account_id, None);
}

fn open_rename_folder_dialog(state: Rc<AppState>, folder: MailFolder) {
    open_folder_name_dialog(state, folder.account_id, Some(folder));
}

fn open_folder_name_dialog(state: Rc<AppState>, account_id: i64, existing: Option<MailFolder>) {
    let renaming = existing.is_some();
    let window = adw::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title(if renaming {
            "Rename folder"
        } else {
            "Create folder"
        })
        .default_width(460)
        .default_height(260)
        .build();
    window.add_css_class("mail-dialog");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_margin_start(28);
    root.set_margin_end(28);
    root.set_margin_top(26);
    root.set_margin_bottom(26);

    let title = gtk::Label::new(Some(if renaming {
        "Rename folder"
    } else {
        "Create a folder"
    }));
    title.set_xalign(0.0);
    title.add_css_class("mail-reader-subject");
    root.append(&title);
    let hint = gtk::Label::new(Some(if renaming {
        "Choose a new name for this custom mailbox. The server will keep its folder hierarchy."
    } else {
        "The new mailbox will be created on your email server and appear under this account."
    }));
    hint.set_xalign(0.0);
    hint.set_wrap(true);
    hint.add_css_class("mail-empty-body");
    hint.set_margin_top(6);
    hint.set_margin_bottom(16);
    root.append(&hint);

    let name = gtk::Entry::builder()
        .placeholder_text("Folder name")
        .hexpand(true)
        .build();
    if let Some(folder) = &existing {
        name.set_text(&folder.name);
    }
    root.append(&form_row("Name", &name));

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.add_css_class("mail-danger");
    status.set_margin_top(12);
    root.append(&status);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    actions.set_margin_top(18);
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label(if renaming { "Rename" } else { "Create" });
    save.add_css_class("mail-accent-button");
    actions.append(&cancel);
    actions.append(&save);
    root.append(&actions);
    window.set_content(Some(&root));

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());

    let state_for_save = state.clone();
    let window_for_save = window.clone();
    let existing_for_save = existing.clone();
    let name_for_save = name.clone();
    let status_for_save = status.clone();
    save.connect_clicked(move |button| {
        let new_name = name_for_save.text().trim().to_string();
        if new_name.is_empty() {
            status_for_save.set_text("Enter a folder name.");
            return;
        }
        let operation = if let Some(folder) = &existing_for_save {
            if folder.name.eq_ignore_ascii_case(&new_name) {
                status_for_save.set_text("Choose a different folder name.");
                return;
            }
            FolderOperation::Rename {
                account_id,
                old_local_name: folder.name.clone(),
                old_remote_name: folder.remote_name.clone(),
                new_local_name: new_name.clone(),
                new_remote_name: renamed_remote_name(&folder.remote_name, &new_name),
            }
        } else {
            FolderOperation::Create {
                account_id,
                local_name: new_name.clone(),
                remote_name: new_name,
            }
        };
        start_folder_operation(
            state_for_save.clone(),
            operation,
            window_for_save.clone(),
            button.clone(),
            status_for_save.clone(),
        );
    });

    window.present();
}

fn open_delete_folder_dialog(state: Rc<AppState>, folder: MailFolder) {
    let window = adw::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title("Delete folder")
        .default_width(460)
        .default_height(260)
        .build();
    window.add_css_class("mail-dialog");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_margin_start(28);
    root.set_margin_end(28);
    root.set_margin_top(26);
    root.set_margin_bottom(26);
    let title = gtk::Label::new(Some("Delete this folder?"));
    title.set_xalign(0.0);
    title.add_css_class("mail-reader-subject");
    root.append(&title);
    let hint = gtk::Label::new(Some(&format!(
        "“{}” and its messages will be removed from the server. This cannot be undone.",
        folder.name
    )));
    hint.set_xalign(0.0);
    hint.set_wrap(true);
    hint.add_css_class("mail-empty-body");
    hint.set_margin_top(8);
    root.append(&hint);
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.add_css_class("mail-danger");
    status.set_margin_top(12);
    root.append(&status);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    actions.set_margin_top(18);
    let cancel = gtk::Button::with_label("Cancel");
    let delete = gtk::Button::with_label("Delete folder");
    delete.add_css_class("destructive-action");
    actions.append(&cancel);
    actions.append(&delete);
    root.append(&actions);
    window.set_content(Some(&root));

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    let operation = FolderOperation::Delete {
        account_id: folder.account_id,
        local_name: folder.name.clone(),
        remote_name: folder.remote_name.clone(),
    };
    let state_for_delete = state.clone();
    let window_for_delete = window.clone();
    let status_for_delete = status.clone();
    delete.connect_clicked(move |button| {
        start_folder_operation(
            state_for_delete.clone(),
            operation.clone(),
            window_for_delete.clone(),
            button.clone(),
            status_for_delete.clone(),
        );
    });
    window.present();
}

fn renamed_remote_name(old_remote_name: &str, new_local_name: &str) -> String {
    old_remote_name
        .rsplit_once('/')
        .map(|(parent, _)| format!("{parent}/{new_local_name}"))
        .unwrap_or_else(|| new_local_name.to_string())
}

fn start_folder_operation(
    state: Rc<AppState>,
    operation: FolderOperation,
    window: adw::Window,
    button: gtk::Button,
    status: gtk::Label,
) {
    let account_id = match &operation {
        FolderOperation::Create { account_id, .. }
        | FolderOperation::Rename { account_id, .. }
        | FolderOperation::Delete { account_id, .. } => *account_id,
    };
    let Some(account) = state
        .accounts
        .borrow()
        .iter()
        .find(|account| account.id == Some(account_id))
        .cloned()
    else {
        status.set_text("This account is no longer available.");
        return;
    };
    button.set_sensitive(false);
    status.set_text("Updating the mail server…");
    set_status(&state, "Updating folders…");
    let database = state.database.clone();
    let operation_for_worker = operation.clone();
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result =
            mail::credentials::load_auth_material(&account.email, "imap", &account.incoming.auth)
                .map_err(|error| {
                    mail::credentials::friendly_load_error("IMAP", &account.incoming.auth, &error)
                })
                .and_then(|auth| {
                    let server_result = match &operation_for_worker {
                        FolderOperation::Create { remote_name, .. } => {
                            mail::imap::create_folder(&account, &auth, remote_name)
                        }
                        FolderOperation::Rename {
                            old_remote_name,
                            new_remote_name,
                            ..
                        } => mail::imap::rename_folder(
                            &account,
                            &auth,
                            old_remote_name,
                            new_remote_name,
                        ),
                        FolderOperation::Delete { remote_name, .. } => {
                            mail::imap::delete_folder(&account, &auth, remote_name)
                        }
                    };
                    server_result.map_err(|error| error.to_string())
                })
                .and_then(|_| match &operation_for_worker {
                    FolderOperation::Create {
                        account_id,
                        local_name,
                        remote_name,
                    } => database
                        .create_folder(&MailFolder {
                            account_id: *account_id,
                            name: local_name.clone(),
                            remote_name: remote_name.clone(),
                            kind: "custom".into(),
                            unread_count: 0,
                        })
                        .map_err(|error| error.to_string()),
                    FolderOperation::Rename {
                        account_id,
                        old_remote_name,
                        new_local_name,
                        new_remote_name,
                        ..
                    } => database
                        .rename_folder(
                            *account_id,
                            old_remote_name,
                            new_local_name,
                            new_remote_name,
                        )
                        .map_err(|error| error.to_string()),
                    FolderOperation::Delete {
                        account_id,
                        local_name,
                        remote_name,
                    } => database
                        .delete_folder(*account_id, remote_name, local_name)
                        .map_err(|error| error.to_string()),
                });
        let _ = sender.send_blocking(result);
    });
    let state_for_result = state.clone();
    glib::MainContext::default().spawn_local(async move {
        match receiver.recv().await {
            Ok(Ok(())) => {
                apply_folder_operation_to_state(&state_for_result, &operation);
                window.close();
                set_status(
                    &state_for_result,
                    match operation {
                        FolderOperation::Create { .. } => "Folder created",
                        FolderOperation::Rename { .. } => "Folder renamed",
                        FolderOperation::Delete { .. } => "Folder deleted",
                    },
                );
            }
            Ok(Err(error)) => {
                button.set_sensitive(true);
                status.set_text(&format!("Couldn’t update this folder: {error}"));
                set_status(
                    &state_for_result,
                    "The folder change could not be completed",
                );
            }
            Err(_) => {
                button.set_sensitive(true);
                status.set_text("The folder worker stopped unexpectedly.");
            }
        }
    });
}

fn apply_folder_operation_to_state(state: &Rc<AppState>, operation: &FolderOperation) {
    match operation {
        FolderOperation::Create {
            account_id,
            local_name,
            remote_name,
        } => {
            state.folders.borrow_mut().push(MailFolder {
                account_id: *account_id,
                name: local_name.clone(),
                remote_name: remote_name.clone(),
                kind: "custom".into(),
                unread_count: 0,
            });
        }
        FolderOperation::Rename {
            account_id,
            old_local_name,
            old_remote_name,
            new_local_name,
            new_remote_name,
        } => {
            if let Some(folder) = state.folders.borrow_mut().iter_mut().find(|folder| {
                folder.account_id == *account_id
                    && (folder.remote_name == *old_remote_name
                        || folder.name.eq_ignore_ascii_case(old_local_name))
            }) {
                folder.name = new_local_name.clone();
                folder.remote_name = new_remote_name.clone();
            }
            let mut scope = state.scope.borrow_mut();
            if *scope
                == (MailScope::Account {
                    id: *account_id,
                    folder: old_local_name.clone(),
                })
            {
                *scope = MailScope::Account {
                    id: *account_id,
                    folder: new_local_name.clone(),
                };
            }
        }
        FolderOperation::Delete {
            account_id,
            local_name,
            remote_name,
        } => {
            state.folders.borrow_mut().retain(|folder| {
                !(folder.account_id == *account_id
                    && (folder.remote_name == *remote_name
                        || folder.name.eq_ignore_ascii_case(local_name)))
            });
            if *state.scope.borrow()
                == (MailScope::Account {
                    id: *account_id,
                    folder: local_name.clone(),
                })
            {
                state.scope.replace(MailScope::Account {
                    id: *account_id,
                    folder: "Inbox".into(),
                });
            }
            state.selected_message.replace(None);
        }
    }
    refresh_cached_view(state);
}

fn open_message_menu(state: Rc<AppState>, row: &gtk::ListBoxRow, message_id: i64, x: f64, y: f64) {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(true);
    popover.set_parent(row);
    let point = gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
    popover.set_pointing_to(Some(&point));
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu.set_margin_top(6);
    menu.set_margin_bottom(6);
    menu.set_margin_start(6);
    menu.set_margin_end(6);
    for (label, action) in [("Mark read / unread", "read"), ("Star / unstar", "star")] {
        let button = gtk::Button::with_label(label);
        button.set_has_frame(false);
        let state = state.clone();
        let popover = popover.clone();
        button.connect_clicked(move |_| {
            apply_message_action(&state, message_id, action);
            popover.popdown();
        });
        menu.append(&button);
    }
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_top(4);
    separator.set_margin_bottom(4);
    menu.append(&separator);
    for (label, action) in [("Move to…", "move"), ("Copy to…", "copy")] {
        let button = gtk::Button::with_label(label);
        button.set_has_frame(false);
        let state = state.clone();
        let row = row.clone();
        let popover = popover.clone();
        button.connect_clicked(move |_| {
            open_message_destination_menu(state.clone(), &row, message_id, action);
            popover.popdown();
        });
        menu.append(&button);
    }
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_top(4);
    separator.set_margin_bottom(4);
    menu.append(&separator);
    for (label, action) in [("Archive", "archive"), ("Move to Trash", "trash")] {
        let button = gtk::Button::with_label(label);
        button.set_has_frame(false);
        let state = state.clone();
        let popover = popover.clone();
        button.connect_clicked(move |_| {
            apply_message_action(&state, message_id, action);
            popover.popdown();
        });
        menu.append(&button);
    }
    popover.set_child(Some(&menu));
    popover.popup();
}

fn account_destination_folders(state: &Rc<AppState>, account_id: i64) -> Vec<MailFolder> {
    let mut folders = state
        .folders
        .borrow()
        .iter()
        .filter(|folder| folder.account_id == account_id)
        .cloned()
        .collect::<Vec<_>>();
    for (name, remote_name, kind) in [
        ("Inbox", "INBOX", "inbox"),
        ("Drafts", "Drafts", "drafts"),
        ("Sent", "Sent", "sent"),
        ("Archive", "Archive", "archive"),
        ("Spam", "Spam", "spam"),
        ("Trash", "Trash", "trash"),
    ] {
        if !folders.iter().any(|folder| {
            folder.name.eq_ignore_ascii_case(name)
                || folder.remote_name.eq_ignore_ascii_case(remote_name)
        }) {
            folders.push(MailFolder {
                account_id,
                name: name.into(),
                remote_name: remote_name.into(),
                kind: kind.into(),
                unread_count: 0,
            });
        }
    }
    folders.sort_by_key(|folder| (folder.kind == "custom", folder.name.to_ascii_lowercase()));
    folders
}

fn open_message_destination_menu(
    state: Rc<AppState>,
    row: &gtk::ListBoxRow,
    message_id: i64,
    action: &str,
) {
    let Some(message) = state
        .messages
        .borrow()
        .iter()
        .find(|message| message.id == message_id)
        .cloned()
    else {
        return;
    };
    let Some(account_id) = message.account_id else {
        set_status(&state, "This message is not linked to an email account");
        return;
    };
    let popover = gtk::Popover::new();
    popover.set_has_arrow(true);
    popover.set_parent(row);
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu.set_margin_top(6);
    menu.set_margin_bottom(6);
    menu.set_margin_start(6);
    menu.set_margin_end(6);
    let heading = gtk::Label::new(Some(if action == "move" {
        "Move message to"
    } else {
        "Copy message to"
    }));
    heading.set_xalign(0.0);
    heading.add_css_class("mail-section-label");
    heading.set_margin_start(8);
    heading.set_margin_end(8);
    heading.set_margin_bottom(4);
    menu.append(&heading);

    let destinations = account_destination_folders(&state, account_id);
    let mut added = false;
    for folder in destinations {
        if folder.name.eq_ignore_ascii_case(&message.folder) {
            continue;
        }
        added = true;
        let button = gtk::Button::with_label(&folder.name);
        button.set_has_frame(false);
        button.set_halign(gtk::Align::Fill);
        let state_for_action = state.clone();
        let popover_for_action = popover.clone();
        let action = action.to_string();
        let target = folder.name.clone();
        button.connect_clicked(move |_| {
            apply_message_transfer(&state_for_action, message_id, &action, &target);
            popover_for_action.popdown();
        });
        menu.append(&button);
    }
    if !added {
        let empty = gtk::Label::new(Some("No other folders available"));
        empty.add_css_class("mail-empty-body");
        menu.append(&empty);
    }
    popover.set_child(Some(&menu));
    popover.popup();
}

fn apply_message_transfer(
    state: &Rc<AppState>,
    message_id: i64,
    action: &str,
    target_folder: &str,
) {
    let Some(message) = state
        .messages
        .borrow()
        .iter()
        .find(|message| message.id == message_id)
        .cloned()
    else {
        return;
    };
    if message.folder.eq_ignore_ascii_case(target_folder) {
        set_status(state, "The message is already in that folder");
        return;
    }
    let (Some(account_id), Some(remote_uid), Some(uidvalidity)) =
        (message.account_id, message.remote_uid, message.uidvalidity)
    else {
        set_status(
            state,
            "This message cannot be moved or copied on the server",
        );
        return;
    };
    if action != "move" && action != "copy" {
        return;
    }

    if action == "move" {
        if let Some(message) = state
            .messages
            .borrow_mut()
            .iter_mut()
            .find(|message| message.id == message_id)
        {
            message.folder = target_folder.to_string();
        }
        let database = state.database.clone();
        let target = target_folder.to_string();
        let source = message.folder.clone();
        std::thread::spawn(move || {
            let _ = database.move_message(message_id, &target);
            let _ = database.queue_action(
                Some(account_id),
                Some(message_id),
                "move",
                &serde_json::json!({
                    "folder": target,
                    "source_folder": source,
                })
                .to_string(),
            );
        });
        state.selected_message.replace(None);
        render_reader(state, None);
        set_status(state, &format!("Moved to {target_folder} — ready to undo"));
    } else {
        let database = state.database.clone();
        let source = message.folder.clone();
        let target = target_folder.to_string();
        std::thread::spawn(move || {
            let _ = database.queue_action(
                Some(account_id),
                Some(message_id),
                "copy",
                &serde_json::json!({
                    "folder": target,
                    "source_folder": source,
                })
                .to_string(),
            );
        });
        set_status(state, &format!("Copy to {target_folder} queued"));
    }
    let _ = remote_uid;
    let _ = uidvalidity;
    render_sidebar(state);
    render_messages(state, state.search_entry.text().as_str());
}

fn apply_bulk_flag(state: &Rc<AppState>, kind: &str, enabled: bool) {
    let ids = selected_message_ids(state);
    if ids.is_empty() {
        return;
    }
    let updates = {
        let mut messages = state.messages.borrow_mut();
        messages
            .iter_mut()
            .filter(|message| ids.contains(&message.id) && message.id >= 0)
            .map(|message| {
                let value = if kind == "read" {
                    message.unread = !enabled;
                    !enabled
                } else {
                    message.starred = enabled;
                    enabled
                };
                (
                    message.id,
                    message.account_id,
                    message.folder.clone(),
                    value,
                )
            })
            .collect::<Vec<_>>()
    };
    if updates.is_empty() {
        clear_selected_rows(state);
        return;
    }
    let database = state.database.clone();
    let kind_for_worker = kind.to_string();
    std::thread::spawn(move || {
        for (message_id, account_id, folder, value) in updates {
            if kind_for_worker == "read" {
                let _ = database.set_unread(message_id, value);
            } else {
                let _ = database.set_starred(message_id, value);
            }
            if let Some(account_id) = account_id {
                let _ = database.queue_action(
                    Some(account_id),
                    Some(message_id),
                    &kind_for_worker,
                    &serde_json::json!({ "value": value, "folder": folder }).to_string(),
                );
            }
        }
    });
    clear_selected_rows(state);
    render_sidebar(state);
    render_messages(state, state.search_entry.text().as_str());
    set_status(
        state,
        match (kind, enabled) {
            ("read", true) => "Selected messages marked read",
            ("read", false) => "Selected messages marked unread",
            ("star", true) => "Selected messages starred",
            _ => "Selected messages updated",
        },
    );
}

fn apply_bulk_move(state: &Rc<AppState>, target_folder: &str) {
    let ids = selected_message_ids(state);
    if ids.is_empty() {
        return;
    }
    let updates = {
        let mut messages = state.messages.borrow_mut();
        messages
            .iter_mut()
            .filter(|message| {
                ids.contains(&message.id)
                    && message.id >= 0
                    && message.account_id.is_some()
                    && message.remote_uid.is_some()
                    && message.uidvalidity.is_some()
                    && !message.folder.eq_ignore_ascii_case(target_folder)
            })
            .map(|message| {
                let source_folder = message.folder.clone();
                let account_id = message.account_id.expect("filtered account id");
                message.folder = target_folder.to_string();
                (
                    message.id,
                    account_id,
                    source_folder,
                    target_folder.to_string(),
                )
            })
            .collect::<Vec<_>>()
    };
    if updates.is_empty() {
        clear_selected_rows(state);
        set_status(state, "The selected messages are already there");
        return;
    }
    let database = state.database.clone();
    std::thread::spawn(move || {
        for (message_id, account_id, source_folder, target_folder) in updates {
            let _ = database.move_message(message_id, &target_folder);
            let _ = database.queue_action(
                Some(account_id),
                Some(message_id),
                "move",
                &serde_json::json!({
                    "folder": target_folder,
                    "source_folder": source_folder,
                })
                .to_string(),
            );
        }
    });
    clear_selected_rows(state);
    state.selected_message.replace(None);
    render_reader(state, None);
    render_sidebar(state);
    render_messages(state, state.search_entry.text().as_str());
    set_status(
        state,
        if target_folder == "Trash" {
            "Selected messages moved to Trash"
        } else {
            "Selected messages archived"
        },
    );
}

fn apply_message_action(state: &Rc<AppState>, message_id: i64, action: &str) {
    let mut persist = None;
    match action {
        "read" => {
            let (unread, folder) = {
                let mut messages = state.messages.borrow_mut();
                let Some(message) = messages.iter_mut().find(|message| message.id == message_id)
                else {
                    return;
                };
                message.unread = !message.unread;
                (message.unread, message.folder.clone())
            };
            persist = Some(("read", unread, folder));
        }
        "star" => {
            let (starred, folder) = {
                let mut messages = state.messages.borrow_mut();
                let Some(message) = messages.iter_mut().find(|message| message.id == message_id)
                else {
                    return;
                };
                message.starred = !message.starred;
                (message.starred, message.folder.clone())
            };
            persist = Some(("star", starred, folder));
        }
        "archive" | "trash" => {
            let folder = if action == "archive" {
                "Archive"
            } else {
                "Trash"
            };
            let (account_id, source_folder) = {
                let mut messages = state.messages.borrow_mut();
                let Some(message) = messages.iter_mut().find(|message| message.id == message_id)
                else {
                    return;
                };
                let account_id = message.account_id;
                let source_folder = message.folder.clone();
                message.folder = folder.to_string();
                (account_id, source_folder)
            };
            let database = state.database.clone();
            let folder = folder.to_string();
            std::thread::spawn(move || {
                let _ = database.move_message(message_id, &folder);
                let _ = database.queue_action(
                    account_id,
                    Some(message_id),
                    "move",
                    &serde_json::json!({
                        "folder": folder,
                        "source_folder": source_folder,
                    })
                    .to_string(),
                );
            });
            set_status(
                state,
                if action == "archive" {
                    "Archived — ready to undo"
                } else {
                    "Moved to Trash — ready to undo"
                },
            );
            state.selected_message.replace(None);
            render_reader(state, None);
        }
        _ => {}
    }
    if let Some((kind, value, folder)) = persist {
        let database = state.database.clone();
        let account_id = state
            .messages
            .borrow()
            .iter()
            .find(|message| message.id == message_id)
            .and_then(|message| message.account_id);
        std::thread::spawn(move || {
            if kind == "read" {
                let _ = database.set_unread(message_id, value);
            } else {
                let _ = database.set_starred(message_id, value);
            }
            let _ = database.queue_action(
                account_id,
                Some(message_id),
                kind,
                &serde_json::json!({ "value": value, "folder": folder }).to_string(),
            );
        });
        set_status(state, "Message updated");
    }
    render_sidebar(state);
    render_messages(state, state.search_entry.text().as_str());
}

fn render_reader(state: &Rc<AppState>, message: Option<Message>) {
    if state.narrow_mode.get() {
        state.middle.set_visible(message.is_none());
        state.reader.set_visible(message.is_some());
    }
    clear(&state.reader);
    if let Some(message) = message {
        let conversation = conversation_messages(state, &message);
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_start(34);
        content.set_margin_end(34);
        content.set_margin_top(30);
        content.set_margin_bottom(34);

        if state.narrow_mode.get() {
            let back = icon_button("go-previous-symbolic", "Back to messages");
            back.set_label("Messages");
            back.set_use_underline(false);
            back.set_halign(gtk::Align::Start);
            let state_for_back = state.clone();
            back.connect_clicked(move |_| {
                state_for_back.selected_message.replace(None);
                state_for_back.message_list.unselect_all();
                render_reader(&state_for_back, None);
            });
            content.append(&back);
        }

        let subject = gtk::Label::new(Some(&message.subject));
        subject.set_xalign(0.0);
        subject.set_wrap(true);
        subject.add_css_class("mail-reader-subject");
        content.append(&subject);

        let sender_line = gtk::Box::new(gtk::Orientation::Horizontal, 7);
        sender_line.set_margin_top(16);
        let sender_name = if message.folder == "Drafts" {
            "Draft"
        } else {
            message.sender_name.as_str()
        };
        let sender = gtk::Label::new(Some(sender_name));
        sender.add_css_class("mail-reader-sender");
        sender_line.append(&sender);
        let sender_address = if message.folder == "Drafts" {
            "saved locally".to_string()
        } else {
            format!("<{}>", message.sender_email)
        };
        let email = gtk::Label::new(Some(&sender_address));
        email.add_css_class("mail-reader-meta");
        sender_line.append(&email);
        let date = gtk::Label::new(Some(&format_message_datetime(&message.received_at)));
        date.add_css_class("mail-reader-meta");
        date.set_hexpand(true);
        date.set_xalign(1.0);
        sender_line.append(&date);
        content.append(&sender_line);

        let recipients = gtk::Label::new(Some(&format!("To {}", message.recipients)));
        recipients.set_xalign(0.0);
        recipients.add_css_class("mail-reader-meta");
        content.append(&recipients);

        if let Some(body_html) = message.body_html.as_deref() {
            if let Some(notice) = remote_image_block_notice(state, &message, body_html) {
                content.append(&notice);
            }
        }

        let pending_send = if message.folder == "Outbox" {
            message.id.checked_neg().and_then(|send_id| {
                state
                    .database
                    .pending_sends(None)
                    .ok()?
                    .into_iter()
                    .find(|send| send.id == send_id)
            })
        } else {
            None
        };
        if let Some(send) = pending_send {
            append_outbox_controls(&content, state, &send);
        } else if message.folder == "Drafts" {
            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            actions.set_margin_top(20);

            let edit = icon_button("document-edit-symbolic", "Continue editing this draft");
            edit.set_label("Edit draft");
            edit.set_use_underline(false);
            let state_for_edit = state.clone();
            let draft_for_edit = message.clone();
            edit.connect_clicked(move |_| {
                open_compose_with_context(
                    state_for_edit.clone(),
                    Some(ComposeContext::Draft(draft_for_edit.clone())),
                );
            });
            actions.append(&edit);

            let delete = icon_button("user-trash-symbolic", "Delete this draft");
            delete.set_label("Delete");
            delete.set_use_underline(false);
            let state_for_delete = state.clone();
            let draft_id = message.id;
            delete.connect_clicked(move |_| delete_local_message(&state_for_delete, draft_id));
            actions.append(&delete);
            content.append(&actions);
        } else {
            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            actions.set_margin_top(20);
            let reader_message_id = message.id;
            for (label, icon) in [
                ("Reply", "mail-reply-sender-symbolic"),
                ("Reply all", "mail-reply-all-symbolic"),
                ("Forward", "mail-forward-symbolic"),
                ("Archive", "archive-symbolic"),
                ("Delete", "user-trash-symbolic"),
            ] {
                let button = icon_button(icon, label);
                button.set_label(label);
                button.set_use_underline(false);
                let state = state.clone();
                let label_owned = label.to_string();
                let message_for_compose = message.clone();
                let action = match label {
                    "Archive" => Some("archive"),
                    "Delete" => Some("trash"),
                    _ => None,
                };
                button.connect_clicked(move |_| {
                    if let Some(action) = action {
                        apply_message_action(&state, reader_message_id, action);
                    } else if label_owned == "Reply" {
                        open_compose_with_context(
                            state.clone(),
                            Some(ComposeContext::Reply {
                                message: message_for_compose.clone(),
                                reply_all: false,
                            }),
                        );
                    } else if label_owned == "Reply all" {
                        open_compose_with_context(
                            state.clone(),
                            Some(ComposeContext::Reply {
                                message: message_for_compose.clone(),
                                reply_all: true,
                            }),
                        );
                    } else if label_owned == "Forward" {
                        open_compose_with_context(
                            state.clone(),
                            Some(ComposeContext::Forward(message_for_compose.clone())),
                        );
                    } else {
                        set_status(&state, &format!("{label_owned} ready"));
                    }
                });
                actions.append(&button);
            }
            content.append(&actions);
        }

        let rule = gtk::Separator::new(gtk::Orientation::Horizontal);
        rule.set_margin_top(22);
        rule.set_margin_bottom(24);
        content.append(&rule);

        if conversation.len() > 1 {
            let conversation_label = gtk::Label::new(Some(&format!(
                "Conversation · {} messages",
                conversation.len()
            )));
            conversation_label.set_xalign(0.0);
            conversation_label.add_css_class("mail-conversation-label");
            conversation_label.set_margin_bottom(8);
            content.append(&conversation_label);

            for related in conversation
                .iter()
                .filter(|related| related.id != message.id)
            {
                let title = format!(
                    "{}  ·  {}",
                    related.sender_name,
                    format_message_datetime(&related.received_at)
                );
                let expander = gtk::Expander::new(Some(&title));
                expander.add_css_class("mail-conversation-expander");
                expander.set_margin_bottom(8);
                expander.set_expanded(false);
                let older = gtk::Box::new(gtk::Orientation::Vertical, 0);
                older.set_margin_start(10);
                older.set_margin_end(10);
                older.set_margin_top(10);
                older.set_margin_bottom(10);
                append_message_content(&older, state, related);
                expander.set_child(Some(&older));
                content.append(&expander);
            }
        }
        append_message_content(&content, state, &message);
        scroll.set_child(Some(&content));
        state.reader.append(&scroll);
    } else if state.accounts.borrow().is_empty() && !state.demo_mode {
        let welcome = gtk::Box::new(gtk::Orientation::Vertical, 12);
        welcome.set_valign(gtk::Align::Center);
        welcome.set_halign(gtk::Align::Center);
        welcome.set_margin_start(24);
        welcome.set_margin_end(24);
        let title = gtk::Label::new(Some("Welcome to Omarchy Mail"));
        title.add_css_class("mail-empty-title");
        let subtitle = gtk::Label::new(Some("Email without the clutter."));
        subtitle.add_css_class("mail-empty-body");
        let add = gtk::Button::with_label("Add email account");
        add.add_css_class("mail-accent-button");
        let state_for_account = state.clone();
        add.connect_clicked(move |_| open_account_dialog(state_for_account.clone()));
        welcome.append(&title);
        welcome.append(&subtitle);
        welcome.append(&add);
        state.reader.append(&welcome);
    } else {
        let empty = gtk::Box::new(gtk::Orientation::Vertical, 10);
        empty.set_valign(gtk::Align::Center);
        empty.set_halign(gtk::Align::Center);
        empty.set_margin_top(72);
        let title = gtk::Label::new(Some("Select a message to read it"));
        title.add_css_class("mail-empty-title");
        let body = gtk::Label::new(Some("Your conversations will appear here."));
        body.add_css_class("mail-empty-body");
        empty.append(&title);
        empty.append(&body);
        state.reader.append(&empty);
    }
}

fn conversation_messages(state: &Rc<AppState>, selected: &Message) -> Vec<Message> {
    let identity = conversation_identity(selected);
    let mut messages = state
        .messages
        .borrow()
        .iter()
        .filter(|message| conversation_identity(message) == identity)
        .cloned()
        .collect::<Vec<_>>();
    if !messages.iter().any(|message| message.id == selected.id) {
        messages.push(selected.clone());
    }
    messages.sort_by(|left, right| compare_received_newest(right, left));
    messages
}

fn compare_received_newest(left: &Message, right: &Message) -> std::cmp::Ordering {
    match (
        chrono::DateTime::parse_from_rfc3339(&left.received_at),
        chrono::DateTime::parse_from_rfc3339(&right.received_at),
    ) {
        (Ok(left), Ok(right)) => right.cmp(&left),
        _ => std::cmp::Ordering::Equal,
    }
}

fn append_outbox_controls(content: &gtk::Box, state: &Rc<AppState>, send: &PendingSend) {
    let status = gtk::Label::new(Some(if send.retryable {
        "Waiting to send when the connection is available."
    } else {
        "This message needs attention before it can be sent."
    }));
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.add_css_class(if send.retryable {
        "mail-status"
    } else {
        "mail-danger"
    });
    status.set_margin_top(18);
    content.append(&status);
    if let Some(error) = &send.last_error {
        let details = gtk::Label::new(Some(&format!("Last attempt: {error}")));
        details.set_xalign(0.0);
        details.set_wrap(true);
        details.add_css_class("mail-reader-meta");
        details.set_margin_top(6);
        content.append(&details);
    }

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_margin_top(18);
    let retry = gtk::Button::with_label("Retry now");
    retry.add_css_class("mail-accent-button");
    let delete = gtk::Button::with_label("Discard");
    delete.add_css_class("mail-danger");
    let state_for_retry = state.clone();
    let retry_id = send.id;
    retry.connect_clicked(
        move |_| match state_for_retry.database.retry_pending_send(retry_id) {
            Ok(()) => {
                refresh_cached_view(&state_for_retry);
                set_status(&state_for_retry, "Message queued for retry");
            }
            Err(error) => set_status(
                &state_for_retry,
                &format!("Couldn’t retry this message: {error}"),
            ),
        },
    );
    let state_for_delete = state.clone();
    let send_for_delete = send.clone();
    delete.connect_clicked(move |_| {
        match state_for_delete
            .database
            .delete_pending_send(send_for_delete.id)
        {
            Ok(()) => {
                mail::outbox::remove_staged_files(&send_for_delete);
                state_for_delete.selected_message.replace(None);
                refresh_cached_view(&state_for_delete);
                set_status(&state_for_delete, "Outbox message discarded");
            }
            Err(error) => set_status(
                &state_for_delete,
                &format!("Couldn’t discard this message: {error}"),
            ),
        }
    });
    actions.append(&retry);
    actions.append(&delete);
    content.append(&actions);
}

fn append_message_content(content: &gtk::Box, state: &Rc<AppState>, message: &Message) {
    if let Some(body_html) = message.body_html.as_deref() {
        append_html_content(content, state, message, body_html);
    } else {
        let body = gtk::TextView::new();
        body.set_wrap_mode(gtk::WrapMode::WordChar);
        body.set_editable(false);
        body.set_cursor_visible(false);
        body.set_hexpand(true);
        body.set_left_margin(0);
        body.set_right_margin(0);
        body.set_top_margin(0);
        body.set_bottom_margin(0);
        body.add_css_class("mail-reader-body");
        body.buffer().set_text(&message.body);
        content.append(&body);
    }

    let inline_images = message
        .attachments
        .iter()
        .filter(|attachment| {
            is_inline_image(attachment)
                && !message.body_html.as_deref().is_some_and(|html| {
                    html_references_content_id(html, attachment.content_id.as_deref())
                })
        })
        .collect::<Vec<_>>();
    if !inline_images.is_empty() {
        let images = gtk::Box::new(gtk::Orientation::Vertical, 8);
        images.set_margin_top(24);
        images.add_css_class("mail-inline-images");
        let heading = gtk::Label::new(Some("Inline images"));
        heading.set_xalign(0.0);
        heading.add_css_class("mail-reader-meta");
        images.append(&heading);
        for attachment in &inline_images {
            if !attachment_available(attachment) {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let unavailable = gtk::Label::new(Some("Inline image unavailable"));
                unavailable.set_xalign(0.0);
                unavailable.set_hexpand(true);
                unavailable.add_css_class("mail-empty-body");
                row.append(&unavailable);
                let download = gtk::Button::with_label("Download");
                connect_attachment_download(&download, state, message, attachment);
                row.append(&download);
                images.append(&row);
                continue;
            }
            let picture = gtk::Picture::for_filename(&attachment.cache_path);
            picture.add_css_class("mail-inline-image");
            picture.set_alternative_text(Some(&attachment.filename));
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_can_shrink(true);
            picture.set_halign(gtk::Align::Start);
            picture.set_hexpand(true);
            picture.set_margin_bottom(8);
            images.append(&picture);
        }
        content.append(&images);
    }

    let file_attachments = message
        .attachments
        .iter()
        .filter(|attachment| !is_inline_image(attachment))
        .collect::<Vec<_>>();
    if !file_attachments.is_empty() {
        let attachments = gtk::Box::new(gtk::Orientation::Vertical, 8);
        attachments.set_margin_top(30);
        let heading = gtk::Label::new(Some("Attachments"));
        heading.set_xalign(0.0);
        heading.add_css_class("mail-reader-meta");
        attachments.append(&heading);
        for attachment in file_attachments {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row.add_css_class("mail-attachment-chip");
            let icon = gtk::Image::from_icon_name("mail-attachment-symbolic");
            row.append(&icon);
            let label = gtk::Label::new(Some(&format_attachment_label(attachment)));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            row.append(&label);
            let save = gtk::Button::with_label(if attachment_available(attachment) {
                "Save"
            } else {
                "Download"
            });
            if attachment_available(attachment) {
                let state_for_attachment = state.clone();
                let attachment = attachment.clone();
                save.connect_clicked(move |_| {
                    save_attachment(state_for_attachment.clone(), attachment.clone())
                });
            } else {
                connect_attachment_download(&save, state, message, attachment);
            }
            row.append(&save);
            attachments.append(&row);
        }
        content.append(&attachments);
    } else if message.has_attachments && inline_images.is_empty() {
        let attachments = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        attachments.set_margin_top(30);
        let chip = gtk::Label::new(Some("  attachment unavailable  "));
        chip.add_css_class("mail-attachment-chip");
        attachments.append(&chip);
        content.append(&attachments);
    }
}

fn append_html_content(
    content: &gtk::Box,
    state: &Rc<AppState>,
    message: &Message,
    body_html: &str,
) {
    let remote_allowed = remote_images_allowed(state, message);
    let settings = webkit6::Settings::builder()
        .allow_file_access_from_file_urls(false)
        .allow_universal_access_from_file_urls(false)
        .allow_top_navigation_to_data_urls(false)
        .auto_load_images(remote_allowed)
        .enable_javascript(false)
        .enable_javascript_markup(false)
        .enable_media(false)
        .enable_webgl(false)
        .enable_html5_database(false)
        .enable_html5_local_storage(false)
        .build();
    let webview = webkit6::WebView::builder().settings(&settings).build();
    webview.set_hexpand(true);
    webview.set_vexpand(true);
    webview.set_size_request(-1, 320);
    webview.add_css_class("mail-html-webview");
    if let Ok(background) = webkit6::gdk::RGBA::parse(&theme::ThemePalette::load().background) {
        webview.set_background_color(&background);
    }
    webview.connect_decide_policy(|_, decision, decision_type| {
        if matches!(
            decision_type,
            webkit6::PolicyDecisionType::NavigationAction
                | webkit6::PolicyDecisionType::NewWindowAction
        ) {
            if let Some(navigation) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() {
                let mut action = navigation.navigation_action();
                let uri = action
                    .as_mut()
                    .and_then(|action| action.request())
                    .and_then(|request| request.uri())
                    .map(|uri| uri.to_string());
                if let Some(ref uri) = uri
                    && (uri.starts_with("https://")
                        || uri.starts_with("http://")
                        || uri.starts_with("mailto:"))
                {
                    let _ =
                        gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
                    decision.ignore();
                    return true;
                }
                // load_html() uses this local document URI. It must be allowed
                // through the policy callback or the reader stays blank.
                if uri.as_deref() == Some("about:blank") {
                    decision.use_();
                    return true;
                }
                decision.ignore();
                return true;
            }
        }
        false
    });
    let prepared = crate::mail::mime::prepare_html_for_webview(
        body_html,
        &message.attachments,
        remote_allowed,
        &theme::ThemePalette::load().foreground,
    );
    webview.load_html(&prepared, Some("about:blank"));
    content.append(&webview);
}

fn append_html_children(
    parent: &gtk::Box,
    state: &Rc<AppState>,
    message: &Message,
    children: &[crate::mail::mime::HtmlNode],
) {
    let mut inline_markup = String::new();
    for child in children {
        if is_html_block(child) {
            append_inline_markup(parent, state, message, &mut inline_markup);
            parent.append(&render_html_block(state, message, child));
        } else {
            inline_markup.push_str(&crate::mail::mime::html_node_markup(child));
        }
    }
    append_inline_markup(parent, state, message, &mut inline_markup);
}

fn append_inline_markup(
    parent: &gtk::Box,
    state: &Rc<AppState>,
    message: &Message,
    markup: &mut String,
) {
    if markup.is_empty() || markup.trim().is_empty() {
        markup.clear();
        return;
    }
    append_html_inline(parent, state, message, markup);
    markup.clear();
}

fn render_html_block(
    state: &Rc<AppState>,
    message: &Message,
    node: &crate::mail::mime::HtmlNode,
) -> gtk::Widget {
    let crate::mail::mime::HtmlNode::Element {
        name,
        attributes,
        children,
    } = node
    else {
        let fallback = gtk::Box::new(gtk::Orientation::Vertical, 0);
        append_html_children(&fallback, state, message, std::slice::from_ref(node));
        return fallback.upcast();
    };

    match name.as_str() {
        "table" => render_html_table(state, message, attributes, children),
        "tr" => render_html_row(state, message, attributes, children),
        "td" | "th" => render_html_cell(state, message, name, attributes, children),
        "ul" | "ol" => render_html_list(state, message, name, attributes, children),
        "li" => render_html_list_item(state, message, attributes, children),
        "hr" => {
            let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
            separator.set_margin_top(8);
            separator.set_margin_bottom(8);
            separator.upcast()
        }
        "blockquote" => {
            let block = gtk::Box::new(gtk::Orientation::Vertical, 0);
            block.add_css_class("mail-html-blockquote");
            block.set_margin_start(16);
            block.set_margin_top(5);
            block.set_margin_bottom(7);
            apply_html_box_layout(&block, attributes);
            append_html_children(&block, state, message, children);
            block.upcast()
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let heading = gtk::Box::new(gtk::Orientation::Vertical, 0);
            heading.set_margin_top(8);
            heading.set_margin_bottom(5);
            apply_html_box_layout(&heading, attributes);
            append_html_children(&heading, state, message, children);
            heading.upcast()
        }
        "p" => {
            let paragraph = gtk::Box::new(gtk::Orientation::Vertical, 0);
            paragraph.set_margin_top(3);
            paragraph.set_margin_bottom(8);
            apply_html_box_layout(&paragraph, attributes);
            append_html_children(&paragraph, state, message, children);
            paragraph.upcast()
        }
        "center" => {
            let center = gtk::Box::new(gtk::Orientation::Vertical, 0);
            center.set_halign(gtk::Align::Center);
            center.set_hexpand(true);
            apply_html_box_layout(&center, attributes);
            append_html_children(&center, state, message, children);
            center.upcast()
        }
        _ => {
            let block = gtk::Box::new(gtk::Orientation::Vertical, 0);
            apply_html_box_layout(&block, attributes);
            append_html_children(&block, state, message, children);
            block.upcast()
        }
    }
}

fn render_html_table(
    state: &Rc<AppState>,
    message: &Message,
    attributes: &[(String, String)],
    children: &[crate::mail::mime::HtmlNode],
) -> gtk::Widget {
    let table = gtk::Box::new(gtk::Orientation::Vertical, 0);
    table.set_hexpand(true);
    table.add_css_class("mail-html-table");
    apply_html_box_layout(&table, attributes);
    append_html_table_rows(&table, state, message, children);
    table.upcast()
}

fn append_html_table_rows(
    table: &gtk::Box,
    state: &Rc<AppState>,
    message: &Message,
    children: &[crate::mail::mime::HtmlNode],
) {
    for child in children {
        if let Some((name, attributes, nested)) = html_element_parts(child) {
            if name == "tr" {
                table.append(&render_html_row(state, message, attributes, nested));
            } else if matches!(name, "tbody" | "thead" | "tfoot" | "table") {
                append_html_table_rows(table, state, message, nested);
            }
        }
    }
}

fn render_html_row(
    state: &Rc<AppState>,
    message: &Message,
    attributes: &[(String, String)],
    children: &[crate::mail::mime::HtmlNode],
) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.set_hexpand(true);
    row.set_valign(gtk::Align::Start);
    row.add_css_class("mail-html-row");
    apply_html_box_layout(&row, attributes);
    for child in children {
        if let Some((name, cell_attributes, cell_children)) = html_element_parts(child)
            && matches!(name, "td" | "th")
        {
            row.append(&render_html_cell(
                state,
                message,
                name,
                cell_attributes,
                cell_children,
            ));
        }
    }
    row.upcast()
}

fn render_html_cell(
    state: &Rc<AppState>,
    message: &Message,
    name: &str,
    attributes: &[(String, String)],
    children: &[crate::mail::mime::HtmlNode],
) -> gtk::Widget {
    let cell = gtk::Box::new(gtk::Orientation::Vertical, 0);
    cell.set_hexpand(true);
    cell.set_valign(gtk::Align::Start);
    cell.add_css_class("mail-html-cell");
    if name == "th" {
        cell.add_css_class("mail-html-header-cell");
    }
    apply_html_box_layout(&cell, attributes);
    append_html_children(&cell, state, message, children);
    cell.upcast()
}

fn render_html_list(
    state: &Rc<AppState>,
    message: &Message,
    name: &str,
    attributes: &[(String, String)],
    children: &[crate::mail::mime::HtmlNode],
) -> gtk::Widget {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.set_margin_top(3);
    list.set_margin_bottom(8);
    apply_html_box_layout(&list, attributes);
    let mut index = 1;
    for child in children {
        if let Some((child_name, child_attributes, child_children)) = html_element_parts(child)
            && child_name == "li"
        {
            let item = render_html_list_item_with_marker(
                state,
                message,
                child_attributes,
                child_children,
                if name == "ol" {
                    let marker = format!("{index}. ");
                    index += 1;
                    marker
                } else {
                    "• ".into()
                },
            );
            list.append(&item);
        }
    }
    list.upcast()
}

fn render_html_list_item(
    state: &Rc<AppState>,
    message: &Message,
    attributes: &[(String, String)],
    children: &[crate::mail::mime::HtmlNode],
) -> gtk::Widget {
    render_html_list_item_with_marker(state, message, attributes, children, "• ".into()).upcast()
}

fn render_html_list_item_with_marker(
    state: &Rc<AppState>,
    message: &Message,
    attributes: &[(String, String)],
    children: &[crate::mail::mime::HtmlNode],
    marker: String,
) -> gtk::Box {
    let item = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    item.set_hexpand(true);
    item.set_valign(gtk::Align::Start);
    let marker_label = gtk::Label::new(Some(&marker));
    marker_label.set_valign(gtk::Align::Start);
    item.append(&marker_label);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.set_hexpand(true);
    apply_html_box_layout(&body, attributes);
    append_html_children(&body, state, message, children);
    item.append(&body);
    item
}

fn is_html_block(node: &crate::mail::mime::HtmlNode) -> bool {
    html_element_parts(node).is_some_and(|(name, _, _)| {
        matches!(
            name,
            "blockquote"
                | "center"
                | "div"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "hr"
                | "li"
                | "ol"
                | "p"
                | "table"
                | "tbody"
                | "td"
                | "tfoot"
                | "th"
                | "thead"
                | "tr"
                | "ul"
        )
    })
}

fn html_element_parts(
    node: &crate::mail::mime::HtmlNode,
) -> Option<(&str, &[(String, String)], &[crate::mail::mime::HtmlNode])> {
    match node {
        crate::mail::mime::HtmlNode::Element {
            name,
            attributes,
            children,
        } => Some((name, attributes, children)),
        _ => None,
    }
}

fn apply_html_box_layout(widget: &impl IsA<gtk::Widget>, attributes: &[(String, String)]) {
    let padding = html_style_value(attributes, "padding")
        .and_then(parse_html_length)
        .or_else(|| html_attribute(attributes, "cellpadding").and_then(parse_html_length));
    if let Some(padding) = padding {
        widget.set_margin_start(padding);
        widget.set_margin_end(padding);
        widget.set_margin_top(padding);
        widget.set_margin_bottom(padding);
    }
    let align = html_attribute(attributes, "align")
        .or_else(|| html_style_value(attributes, "text-align"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    match align.as_str() {
        "center" => widget.set_halign(gtk::Align::Center),
        "right" | "end" => widget.set_halign(gtk::Align::End),
        "left" | "start" => widget.set_halign(gtk::Align::Start),
        _ => {}
    }
}

fn html_attribute<'a>(attributes: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(attribute, _)| attribute == name)
        .map(|(_, value)| value.as_str())
}

fn html_style_value<'a>(attributes: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let style = html_attribute(attributes, "style")?;
    style.split(';').find_map(|declaration| {
        let (property, value) = declaration.split_once(':')?;
        property
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim())
    })
}

fn parse_html_length(value: &str) -> Option<i32> {
    let value = value.trim().strip_suffix("px").unwrap_or(value.trim());
    if value.contains('%') {
        return None;
    }
    let length = value.parse::<i32>().ok()?;
    (0..=2400).contains(&length).then_some(length)
}

fn append_html_inline(content: &gtk::Box, state: &Rc<AppState>, message: &Message, markup: &str) {
    let fragments = crate::mail::mime::html_fragments(markup);
    let contains_image = fragments
        .iter()
        .any(|fragment| matches!(fragment, crate::mail::mime::HtmlFragment::Image { .. }));

    if !contains_image {
        let rendered = fragments
            .into_iter()
            .filter_map(|fragment| match fragment {
                crate::mail::mime::HtmlFragment::Markup(markup) => Some(markup),
                crate::mail::mime::HtmlFragment::Image { .. } => None,
            })
            .collect::<String>();
        if rendered.trim().is_empty() {
            return;
        }
        let body = gtk::Label::new(None);
        body.set_use_markup(true);
        body.set_markup(&rendered);
        body.set_xalign(0.0);
        body.set_yalign(0.0);
        body.set_wrap(true);
        body.set_selectable(true);
        body.set_hexpand(true);
        body.set_margin_top(2);
        body.set_margin_bottom(2);
        body.add_css_class("mail-reader-body");
        content.append(&body);
        return;
    }

    let body = gtk::TextView::new();
    body.set_wrap_mode(gtk::WrapMode::WordChar);
    body.set_editable(false);
    body.set_cursor_visible(false);
    body.set_hexpand(true);
    body.set_vexpand(false);
    body.set_size_request(-1, 28);
    body.set_left_margin(0);
    body.set_right_margin(0);
    body.set_top_margin(0);
    body.set_bottom_margin(0);
    body.set_pixels_above_lines(2);
    body.set_pixels_below_lines(2);
    body.add_css_class("mail-reader-body");

    let buffer = body.buffer();
    let remote_allowed = remote_images_allowed(state, message);
    for fragment in fragments {
        match fragment {
            crate::mail::mime::HtmlFragment::Markup(markup) => {
                let mut end = buffer.end_iter();
                buffer.insert_markup(&mut end, &markup);
            }
            crate::mail::mime::HtmlFragment::Image { src, alt } => {
                if let Some(attachment) = inline_attachment_for_source(message, &src) {
                    if attachment_available(attachment) {
                        queue_inline_image(
                            &buffer,
                            gio::File::for_path(&attachment.cache_path),
                            &alt,
                        );
                    } else {
                        insert_image_placeholder(&buffer, &alt, "Inline image unavailable");
                    }
                } else if remote_allowed {
                    queue_inline_image(&buffer, gio::File::for_uri(&src), &alt);
                } else {
                    insert_image_placeholder(&buffer, &alt, "Remote image blocked");
                }
            }
        }
    }
    content.append(&body);
}

fn remote_images_allowed(state: &Rc<AppState>, message: &Message) -> bool {
    let sender_allowed = state
        .preferences
        .borrow()
        .remote_images_allowed_for_sender(&message.sender_email);
    !state.preferences.borrow().block_remote_images
        || sender_allowed
        || state.allowed_remote_images.borrow().contains(&message.id)
}

fn remote_image_block_notice(
    state: &Rc<AppState>,
    message: &Message,
    body_html: &str,
) -> Option<gtk::Box> {
    let urls = crate::mail::mime::remote_image_urls(body_html);
    if urls.is_empty() || remote_images_allowed(state, message) {
        return None;
    }

    let notice = gtk::Box::new(gtk::Orientation::Vertical, 6);
    notice.set_margin_top(14);
    notice.set_margin_bottom(2);
    notice.add_css_class("mail-remote-image-notice");

    let heading = gtk::Label::new(Some("Remote images blocked"));
    heading.set_xalign(0.0);
    heading.add_css_class("mail-reader-meta");
    notice.append(&heading);
    let details = gtk::Label::new(Some(&format!(
        "{} image{} hidden to protect your privacy.",
        urls.len(),
        if urls.len() == 1 { " is" } else { "s are" }
    )));
    details.set_xalign(0.0);
    details.set_wrap(true);
    details.add_css_class("mail-empty-body");
    notice.append(&details);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let load = gtk::Button::with_label("Load for this message");
    let state_for_message = state.clone();
    let message_id = message.id;
    load.connect_clicked(move |_| {
        state_for_message
            .allowed_remote_images
            .borrow_mut()
            .insert(message_id);
        rerender_selected_reader(&state_for_message);
    });
    actions.append(&load);
    let always = gtk::Button::with_label("Always allow from sender");
    let state_for_sender = state.clone();
    let sender_email = message.sender_email.clone();
    always.connect_clicked(move |_| {
        let result = {
            let mut preferences = state_for_sender.preferences.borrow_mut();
            preferences.allow_remote_images_for_sender(&sender_email);
            preferences::save(&preferences)
        };
        if let Err(error) = result {
            set_status(
                &state_for_sender,
                &format!("Couldn’t save image preference: {error}"),
            );
        }
        rerender_selected_reader(&state_for_sender);
    });
    actions.append(&always);
    notice.append(&actions);

    Some(notice)
}

fn inline_attachment_for_source<'a>(
    message: &'a Message,
    source: &str,
) -> Option<&'a AttachmentInfo> {
    let content_id = source.strip_prefix("cid:")?.trim_matches(['<', '>']);
    message.attachments.iter().find(|attachment| {
        attachment
            .content_id
            .as_deref()
            .is_some_and(|known| known.eq_ignore_ascii_case(content_id))
    })
}

fn html_references_content_id(html: &str, content_id: Option<&str>) -> bool {
    let Some(content_id) = content_id else {
        return false;
    };
    let content_id = content_id.trim_matches(['<', '>']);
    html.to_ascii_lowercase()
        .contains(&format!("cid:{}", content_id.to_ascii_lowercase()))
}

fn insert_image_placeholder(buffer: &gtk::TextBuffer, alt: &str, label: &str) {
    let mut end = buffer.end_iter();
    let placeholder = image_placeholder_text(alt, label);
    buffer.insert_markup(&mut end, &glib::markup_escape_text(&placeholder));
}

fn image_placeholder_text(alt: &str, label: &str) -> String {
    if alt.trim().is_empty() {
        format!("[{label}]")
    } else {
        format!("[{label}: {}]", alt.trim())
    }
}

fn queue_inline_image(buffer: &gtk::TextBuffer, file: gio::File, alt: &str) {
    let mut end = buffer.end_iter();
    let offset = end.offset();
    buffer.insert(&mut end, "\u{FFFC}");
    let start = buffer.iter_at_offset(offset);
    let mark = buffer.create_mark(None, &start, true);
    let buffer_for_image = buffer.clone();
    let alt = alt.to_string();
    file.load_bytes_async(None::<&gio::Cancellable>, move |result| {
        let texture = match result {
            Ok((bytes, _)) if bytes.len() <= 8 * 1024 * 1024 => {
                gdk::Texture::from_bytes(&bytes).ok()
            }
            _ => None,
        };
        replace_inline_image(&buffer_for_image, &mark, texture.as_ref(), &alt);
    });
}

fn replace_inline_image(
    buffer: &gtk::TextBuffer,
    mark: &gtk::TextMark,
    texture: Option<&gdk::Texture>,
    alt: &str,
) {
    let mut start = buffer.iter_at_mark(mark);
    let mut end = start.clone();
    end.forward_char();
    buffer.delete(&mut start, &mut end);
    if let Some(texture) = texture {
        buffer.insert_paintable(&mut start, texture);
    } else {
        let placeholder = image_placeholder_text(alt, "Image unavailable");
        buffer.insert_markup(&mut start, &glib::markup_escape_text(&placeholder));
    }
    buffer.delete_mark(mark);
}

fn rerender_selected_reader(state: &Rc<AppState>) {
    let Some(id) = *state.selected_message.borrow() else {
        return;
    };
    let message = state
        .messages
        .borrow()
        .iter()
        .find(|message| message.id == id)
        .cloned();
    render_reader(state, message);
}

fn is_inline_image(attachment: &AttachmentInfo) -> bool {
    attachment.content_id.is_some()
        && attachment
            .content_type
            .to_ascii_lowercase()
            .starts_with("image/")
}

fn attachment_available(attachment: &AttachmentInfo) -> bool {
    !attachment.cache_path.is_empty() && Path::new(&attachment.cache_path).is_file()
}

fn connect_attachment_download(
    button: &gtk::Button,
    state: &Rc<AppState>,
    message: &Message,
    attachment: &AttachmentInfo,
) {
    let state = state.clone();
    let message = message.clone();
    let attachment = attachment.clone();
    button.connect_clicked(move |button| {
        button.set_sensitive(false);
        button.set_label("Downloading…");
        download_attachment(
            state.clone(),
            message.clone(),
            attachment.clone(),
            button.clone(),
        );
    });
}

fn download_attachment(
    state: Rc<AppState>,
    message: Message,
    attachment: AttachmentInfo,
    button: gtk::Button,
) {
    if state.demo_mode {
        button.set_sensitive(true);
        button.set_label("Download");
        set_status(&state, "Preview messages have no remote attachments");
        return;
    }
    let Some(account_id) = message.account_id else {
        button.set_sensitive(true);
        button.set_label("Download");
        set_status(&state, "This message has no source account");
        return;
    };
    let Some(account) = state
        .accounts
        .borrow()
        .iter()
        .find(|account| account.id == Some(account_id))
        .cloned()
    else {
        button.set_sensitive(true);
        button.set_label("Download");
        set_status(&state, "The source account is no longer configured");
        return;
    };
    let remote_name = state
        .folders
        .borrow()
        .iter()
        .find(|folder| folder.account_id == account_id && folder.name == message.folder)
        .map(|folder| folder.remote_name.clone())
        .or_else(|| {
            matches!(
                message.folder.as_str(),
                "Inbox" | "Drafts" | "Sent" | "Archive" | "Spam" | "Trash"
            )
            .then(|| default_remote_folder(&message.folder).to_string())
        });
    let Some(remote_name) = remote_name else {
        button.set_sensitive(true);
        button.set_label("Download");
        set_status(
            &state,
            "Refresh this account before downloading from that folder",
        );
        return;
    };
    set_status(&state, &format!("Downloading {}…", attachment.filename));
    let (sender, receiver) = async_channel::bounded(1);
    mail::sync::spawn_message_fetch(
        account,
        state.database.clone(),
        message,
        remote_name,
        sender,
    );
    glib::MainContext::default().spawn_local(async move {
        match receiver.recv().await {
            Ok(report) if report.error.is_none() => {
                refresh_cached_view(&state);
                if report
                    .message
                    .as_ref()
                    .map(|message| {
                        message
                            .attachments
                            .iter()
                            .any(|candidate| same_attachment(candidate, &attachment))
                    })
                    .unwrap_or(false)
                {
                    set_status(&state, "Attachment downloaded");
                } else {
                    set_status(
                        &state,
                        "The message downloaded, but that attachment was not found",
                    );
                }
            }
            Ok(report) => {
                button.set_sensitive(true);
                button.set_label("Download");
                set_status(
                    &state,
                    &format!(
                        "Couldn’t download attachment: {}",
                        report.error.unwrap_or_else(|| "unknown error".into())
                    ),
                );
            }
            Err(_) => {
                button.set_sensitive(true);
                button.set_label("Download");
                set_status(&state, "The attachment worker stopped unexpectedly.");
            }
        }
    });
}

fn same_attachment(left: &AttachmentInfo, right: &AttachmentInfo) -> bool {
    match (&left.content_id, &right.content_id) {
        (Some(left), Some(right)) => left == right,
        _ => left.filename == right.filename && left.content_type == right.content_type,
    }
}

fn refresh_cached_view(state: &Rc<AppState>) {
    load_messages_for_scope(state);
    render_sidebar(state);
    render_messages(state, state.search_entry.text().as_str());
    let selected_id = *state.selected_message.borrow();
    let selected = selected_id.and_then(|id| {
        state
            .messages
            .borrow()
            .iter()
            .find(|message| message.id == id)
            .cloned()
    });
    render_reader(state, selected);
}

fn format_attachment_label(attachment: &AttachmentInfo) -> String {
    let size = if attachment.size < 1024 {
        format!("{} B", attachment.size)
    } else if attachment.size < 1024 * 1024 {
        format!("{:.1} KB", attachment.size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", attachment.size as f64 / (1024.0 * 1024.0))
    };
    format!("{}  ·  {}", attachment.filename, size)
}

fn save_attachment(state: Rc<AppState>, attachment: AttachmentInfo) {
    if !attachment_available(&attachment) {
        set_status(
            &state,
            "This attachment is not available in the local cache",
        );
        return;
    }
    let dialog = gtk::FileDialog::builder()
        .title("Save attachment")
        .accept_label("Save")
        .initial_name(&attachment.filename)
        .build();
    let window = state.window.clone();
    dialog.save(
        Some(&window),
        None::<&gio::Cancellable>,
        move |result| match result {
            Ok(file) => {
                let Some(destination) = file.path() else {
                    set_status(&state, "That destination is not available locally");
                    return;
                };
                let source = PathBuf::from(&attachment.cache_path);
                let (sender, receiver) = async_channel::bounded(1);
                std::thread::spawn(move || {
                    let result = std::fs::copy(source, destination)
                        .map(|_| ())
                        .map_err(|error| error.to_string());
                    let _ = sender.send_blocking(result);
                });
                let state_for_result = state.clone();
                glib::MainContext::default().spawn_local(async move {
                    match receiver.recv().await {
                        Ok(Ok(())) => set_status(&state_for_result, "Attachment saved"),
                        Ok(Err(error)) => set_status(
                            &state_for_result,
                            &format!("Couldn’t save attachment: {error}"),
                        ),
                        Err(_) => {
                            set_status(&state_for_result, "The save worker stopped unexpectedly.")
                        }
                    }
                });
            }
            Err(error) if error.matches(gio::IOErrorEnum::Cancelled) => {}
            Err(error) => set_status(&state, &format!("Couldn’t save attachment: {error}")),
        },
    );
}

struct AccountEditorFields {
    display_name: gtk::Entry,
    incoming_host: gtk::Entry,
    incoming_username: gtk::Entry,
    incoming_port: gtk::SpinButton,
    incoming_security: gtk::DropDown,
    incoming_auth: gtk::DropDown,
    incoming_secret: gtk::Entry,
    outgoing_host: gtk::Entry,
    outgoing_username: gtk::Entry,
    outgoing_port: gtk::SpinButton,
    outgoing_security: gtk::DropDown,
    outgoing_auth: gtk::DropDown,
    outgoing_secret: gtk::Entry,
}

impl AccountEditorFields {
    fn new(account: &Account) -> Self {
        let display_name = gtk::Entry::builder()
            .text(&account.display_name)
            .hexpand(true)
            .build();
        let incoming_host = gtk::Entry::builder()
            .text(&account.incoming.hostname)
            .hexpand(true)
            .build();
        let incoming_username = gtk::Entry::builder()
            .text(&account.incoming.username)
            .hexpand(true)
            .build();
        let incoming_port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
        incoming_port.set_value(account.incoming.port as f64);
        let incoming_security = gtk::DropDown::from_strings(&["TLS", "STARTTLS", "None"]);
        incoming_security.set_selected(security_index(&account.incoming.security));
        let incoming_auth = gtk::DropDown::from_strings(&["Password", "OAuth2 access token"]);
        incoming_auth.set_selected(auth_index(&account.incoming.auth));
        let incoming_secret = secret_entry("Leave empty to keep the stored IMAP credential");

        let outgoing_host = gtk::Entry::builder()
            .text(&account.outgoing.hostname)
            .hexpand(true)
            .build();
        let outgoing_username = gtk::Entry::builder()
            .text(&account.outgoing.username)
            .hexpand(true)
            .build();
        let outgoing_port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
        outgoing_port.set_value(account.outgoing.port as f64);
        let outgoing_security = gtk::DropDown::from_strings(&["TLS", "STARTTLS", "None"]);
        outgoing_security.set_selected(security_index(&account.outgoing.security));
        let outgoing_auth = gtk::DropDown::from_strings(&["Password", "OAuth2 access token"]);
        outgoing_auth.set_selected(auth_index(&account.outgoing.auth));
        let outgoing_secret = secret_entry("Leave empty to keep the stored SMTP credential");

        let incoming_secret_for_auth = incoming_secret.clone();
        incoming_auth.connect_selected_notify(move |auth| {
            incoming_secret_for_auth.set_placeholder_text(Some(if auth.selected() == 1 {
                "Leave empty to keep the stored IMAP token"
            } else {
                "Leave empty to keep the stored IMAP password"
            }));
        });
        let outgoing_secret_for_auth = outgoing_secret.clone();
        outgoing_auth.connect_selected_notify(move |auth| {
            outgoing_secret_for_auth.set_placeholder_text(Some(if auth.selected() == 1 {
                "Leave empty to keep the stored SMTP token"
            } else {
                "Leave empty to keep the stored SMTP password"
            }));
        });

        Self {
            display_name,
            incoming_host,
            incoming_username,
            incoming_port,
            incoming_security,
            incoming_auth,
            incoming_secret,
            outgoing_host,
            outgoing_username,
            outgoing_port,
            outgoing_security,
            outgoing_auth,
            outgoing_secret,
        }
    }

    fn view(&self) -> gtk::Box {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&form_row("Display name", &self.display_name));

        let credentials = gtk::Label::new(Some(
            "Passwords and access tokens are never shown here. Enter a new value only when you want to replace the stored credential.",
        ));
        credentials.set_xalign(0.0);
        credentials.set_wrap(true);
        credentials.add_css_class("mail-empty-body");
        credentials.set_margin_top(14);
        root.append(&credentials);
        root.append(&form_row("New IMAP credential", &self.incoming_secret));
        root.append(&form_row("New SMTP credential", &self.outgoing_secret));

        let advanced = gtk::Expander::new(Some("Connection details"));
        advanced.set_margin_top(14);
        let details = gtk::Box::new(gtk::Orientation::Vertical, 8);
        details.set_margin_top(10);
        details.append(&form_row("IMAP authentication", &self.incoming_auth));
        details.append(&form_row("IMAP server", &self.incoming_host));
        details.append(&form_row("IMAP username", &self.incoming_username));
        details.append(&form_row("IMAP port", &self.incoming_port));
        details.append(&form_row("IMAP security", &self.incoming_security));
        details.append(&form_row("SMTP authentication", &self.outgoing_auth));
        details.append(&form_row("SMTP server", &self.outgoing_host));
        details.append(&form_row("SMTP username", &self.outgoing_username));
        details.append(&form_row("SMTP port", &self.outgoing_port));
        details.append(&form_row("SMTP security", &self.outgoing_security));
        advanced.set_child(Some(&details));
        root.append(&advanced);
        root
    }

    fn account(&self, original: &Account) -> Result<Account, String> {
        let display_name = if self.display_name.text().trim().is_empty() {
            original
                .email
                .split('@')
                .next()
                .unwrap_or("Account")
                .to_string()
        } else {
            self.display_name.text().trim().to_string()
        };
        let incoming = read_server_config(
            "IMAP",
            &self.incoming_host,
            &self.incoming_username,
            &self.incoming_port,
            &self.incoming_security,
            &self.incoming_auth,
        )?;
        let outgoing = read_server_config(
            "SMTP",
            &self.outgoing_host,
            &self.outgoing_username,
            &self.outgoing_port,
            &self.outgoing_security,
            &self.outgoing_auth,
        )?;
        Ok(Account {
            id: original.id,
            email: original.email.clone(),
            display_name,
            incoming,
            outgoing,
            enabled: original.enabled,
            notify: original.notify,
        })
    }
}

fn secret_entry(placeholder: &str) -> gtk::Entry {
    let entry = gtk::Entry::builder()
        .placeholder_text(placeholder)
        .hexpand(true)
        .build();
    entry.set_visibility(false);
    entry
}

fn auth_index(auth: &AuthMethod) -> u32 {
    match auth {
        AuthMethod::Password => 0,
        AuthMethod::OAuth2 => 1,
    }
}

fn security_index(security: &SecurityMode) -> u32 {
    match security {
        SecurityMode::Tls => 0,
        SecurityMode::StartTls => 1,
        SecurityMode::None => 2,
    }
}

fn auth_method_for_index(index: u32) -> AuthMethod {
    if index == 1 {
        AuthMethod::OAuth2
    } else {
        AuthMethod::Password
    }
}

fn security_for_index(index: u32) -> SecurityMode {
    match index {
        1 => SecurityMode::StartTls,
        2 => SecurityMode::None,
        _ => SecurityMode::Tls,
    }
}

fn read_server_config(
    protocol: &str,
    host: &gtk::Entry,
    username: &gtk::Entry,
    port: &gtk::SpinButton,
    security: &gtk::DropDown,
    auth: &gtk::DropDown,
) -> Result<ServerConfig, String> {
    let hostname = host.text().trim().to_string();
    if hostname.is_empty() {
        return Err(format!("Enter an {protocol} server hostname."));
    }
    let username = username.text().trim().to_string();
    if username.is_empty() {
        return Err(format!("Enter an {protocol} username."));
    }
    Ok(ServerConfig {
        hostname,
        port: port.value_as_int().clamp(1, 65535) as u16,
        security: security_for_index(security.selected()),
        username,
        auth: auth_method_for_index(auth.selected()),
    })
}

fn auth_material_for_secret(method: &AuthMethod, secret: &str) -> mail::credentials::AuthMaterial {
    match method {
        AuthMethod::Password => mail::credentials::AuthMaterial::Password(secret.to_string()),
        AuthMethod::OAuth2 => {
            mail::credentials::AuthMaterial::OAuth2AccessToken(secret.to_string())
        }
    }
}

fn oauth_protocols(
    email: &str,
    incoming_auth: &gtk::DropDown,
    outgoing_auth: &gtk::DropDown,
) -> Vec<String> {
    if mail::oauth::provider_for_email(email).is_none() {
        return Vec::new();
    }
    let mut protocols = Vec::new();
    if incoming_auth.selected() == 1 {
        protocols.push("imap".to_string());
    }
    if outgoing_auth.selected() == 1 {
        protocols.push("smtp".to_string());
    }
    protocols
}

fn update_oauth_button(
    button: &gtk::Button,
    email: &str,
    incoming_auth: &gtk::DropDown,
    outgoing_auth: &gtk::DropDown,
) {
    let available = !oauth_protocols(email, incoming_auth, outgoing_auth).is_empty();
    button.set_visible(available);
    if available && let Some(provider) = mail::oauth::provider_for_email(email) {
        button.set_label(&format!("Authorize with {} in browser…", provider.label()));
    }
}

fn start_oauth_authorization(
    email: String,
    protocols: Vec<String>,
    button: gtk::Button,
    status: gtk::Label,
) {
    if protocols.is_empty() {
        status.set_text("Choose OAuth2 for IMAP or SMTP first.");
        return;
    }
    let request = match mail::oauth::begin(&email) {
        Ok(request) => request,
        Err(error) => {
            status.set_text(&error.to_string());
            return;
        }
    };
    if let Err(error) = gio::AppInfo::launch_default_for_uri(
        &request.authorization_url,
        None::<&gio::AppLaunchContext>,
    ) {
        status.set_text(&format!("Couldn’t open the browser: {error}"));
        return;
    }

    button.set_sensitive(false);
    status.set_text("Waiting for browser authorization…");
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = mail::oauth::complete(request)
            .map_err(|error| error.to_string())
            .and_then(|tokens| {
                for protocol in protocols {
                    mail::credentials::store_oauth2_tokens(&email, &protocol, &tokens)
                        .map_err(|error| error.to_string())?;
                }
                Ok::<(), String>(())
            });
        let _ = sender.send_blocking(result);
    });

    glib::MainContext::default().spawn_local(async move {
        match receiver.recv().await {
            Ok(Ok(())) => status.set_text(
                "Browser sign-in complete. Save the account to use the new authorization.",
            ),
            Ok(Err(error)) => status.set_text(&format!("Browser sign-in failed: {error}")),
            Err(_) => status.set_text("The browser sign-in worker stopped unexpectedly."),
        }
        button.set_sensitive(true);
    });
}

fn open_account_editor(state: Rc<AppState>, original: Account) {
    let window = adw::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title("Edit email account")
        .default_width(620)
        .default_height(720)
        .build();
    window.add_css_class("mail-dialog");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_margin_start(28);
    root.set_margin_end(28);
    root.set_margin_top(26);
    root.set_margin_bottom(26);

    let title = gtk::Label::new(Some("Edit email account"));
    title.set_xalign(0.0);
    title.add_css_class("mail-reader-subject");
    root.append(&title);
    let identity = gtk::Label::new(Some(&format!(
        "{} · {}",
        original.email, original.incoming.hostname
    )));
    identity.set_xalign(0.0);
    identity.add_css_class("mail-empty-body");
    identity.set_ellipsize(gtk::pango::EllipsizeMode::End);
    root.append(&identity);

    let scrolled = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let fields = Rc::new(AccountEditorFields::new(&original));
    let fields_view = fields.view();
    scrolled.set_child(Some(&fields_view));
    root.append(&scrolled);

    let oauth_button = gtk::Button::with_label("Authorize in browser…");
    oauth_button.set_halign(gtk::Align::Start);
    oauth_button.add_css_class("mail-secondary-button");
    oauth_button.set_margin_top(12);
    root.append(&oauth_button);

    let status = gtk::Label::new(Some(
        "The email address is the account identity and cannot be changed here.",
    ));
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.add_css_class("mail-empty-body");
    status.set_margin_top(14);
    root.append(&status);

    update_oauth_button(
        &oauth_button,
        &original.email,
        &fields.incoming_auth,
        &fields.outgoing_auth,
    );
    let oauth_button_for_imap = oauth_button.clone();
    let fields_for_imap_oauth = fields.clone();
    let original_email_for_imap_oauth = original.email.clone();
    fields.incoming_auth.connect_selected_notify(move |_| {
        update_oauth_button(
            &oauth_button_for_imap,
            &original_email_for_imap_oauth,
            &fields_for_imap_oauth.incoming_auth,
            &fields_for_imap_oauth.outgoing_auth,
        );
    });
    let oauth_button_for_smtp = oauth_button.clone();
    let fields_for_smtp_oauth = fields.clone();
    let original_email_for_smtp_oauth = original.email.clone();
    fields.outgoing_auth.connect_selected_notify(move |_| {
        update_oauth_button(
            &oauth_button_for_smtp,
            &original_email_for_smtp_oauth,
            &fields_for_smtp_oauth.incoming_auth,
            &fields_for_smtp_oauth.outgoing_auth,
        );
    });
    let original_email_for_oauth = original.email.clone();
    let fields_for_oauth = fields.clone();
    let oauth_status = status.clone();
    oauth_button.connect_clicked(move |button| {
        let protocols = oauth_protocols(
            &original_email_for_oauth,
            &fields_for_oauth.incoming_auth,
            &fields_for_oauth.outgoing_auth,
        );
        start_oauth_authorization(
            original_email_for_oauth.clone(),
            protocols,
            button.clone(),
            oauth_status.clone(),
        );
    });

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    actions.set_margin_top(18);
    let cancel = gtk::Button::with_label("Cancel");
    let check = gtk::Button::with_label("Check IMAP connection");
    let save = gtk::Button::with_label("Save changes");
    save.add_css_class("mail-accent-button");
    actions.append(&cancel);
    actions.append(&check);
    actions.append(&save);
    root.append(&actions);
    window.set_content(Some(&root));

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());

    let original_for_check = original.clone();
    let fields_for_check = fields.clone();
    let status_for_check = status.clone();
    check.connect_clicked(move |button| {
        let account = match fields_for_check.account(&original_for_check) {
            Ok(account) => account,
            Err(error) => {
                status_for_check.set_text(&error);
                return;
            }
        };
        let supplied_secret = fields_for_check.incoming_secret.text().to_string();
        let method = account.incoming.auth.clone();
        let email = account.email.clone();
        button.set_sensitive(false);
        status_for_check.set_text("Checking IMAP connection…");
        let button_for_async = button.clone();
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = if supplied_secret.is_empty() {
                mail::credentials::load_auth_material(&email, "imap", &method).map_err(|error| {
                    mail::credentials::friendly_load_error("IMAP", &method, &error)
                })
            } else {
                Ok(auth_material_for_secret(&method, &supplied_secret))
            }
            .and_then(|auth| {
                mail::imap::test_connection(&account, &auth).map_err(|error| error.to_string())
            });
            let _ = sender.send_blocking(result);
        });
        let status = status_for_check.clone();
        glib::MainContext::default().spawn_local(async move {
            match receiver.recv().await {
                Ok(Ok(())) => status.set_text(
                    "IMAP connection successful. Your settings are accepted by the server.",
                ),
                Ok(Err(error)) => status.set_text(&format!("Couldn’t connect to IMAP: {error}")),
                Err(_) => status.set_text("The connection check stopped unexpectedly."),
            }
            button_for_async.set_sensitive(true);
        });
    });

    let state_for_save = state.clone();
    let original_for_save = original.clone();
    let fields_for_save = fields.clone();
    let window_for_save = window.clone();
    let status_for_save = status.clone();
    save.connect_clicked(move |button| {
        let updated = match fields_for_save.account(&original_for_save) {
            Ok(account) => account,
            Err(error) => {
                status_for_save.set_text(&error);
                return;
            }
        };
        let imap_secret = fields_for_save.incoming_secret.text().to_string();
        let smtp_secret = fields_for_save.outgoing_secret.text().to_string();
        let database = state_for_save.database.clone();
        let original_id = original_for_save.id;
        let email = updated.email.clone();
        let imap_method = updated.incoming.auth.clone();
        let smtp_method = updated.outgoing.auth.clone();
        button.set_sensitive(false);
        status_for_save.set_text("Saving account securely…");
        let button_for_async = button.clone();
        let status_for_async = status_for_save.clone();
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = if imap_secret.is_empty() {
                Ok(())
            } else {
                mail::credentials::store_auth_material(&email, "imap", &imap_method, &imap_secret)
                    .map_err(|error| error.to_string())
            }
            .and_then(|_| {
                if smtp_secret.is_empty() {
                    Ok(())
                } else {
                    mail::credentials::store_auth_material(
                        &email,
                        "smtp",
                        &smtp_method,
                        &smtp_secret,
                    )
                    .map_err(|error| error.to_string())
                }
            })
            .and_then(|_| {
                database
                    .save_account(&updated)
                    .map(|id| {
                        let mut updated = updated;
                        updated.id = Some(id);
                        updated
                    })
                    .map_err(|error| error.to_string())
            });
            let _ = sender.send_blocking(result);
        });
        let state = state_for_save.clone();
        let window = window_for_save.clone();
        glib::MainContext::default().spawn_local(async move {
            match receiver.recv().await {
                Ok(Ok(updated)) => {
                    if let Some(account_id) = original_id {
                        if let Some(stop) = state.monitor_stops.borrow_mut().remove(&account_id) {
                            stop.store(true, Ordering::Relaxed);
                        }
                        if let Some(stop) = state.outbox_stops.borrow_mut().remove(&account_id) {
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                    let monitor_account = updated.clone();
                    if let Some(account_id) = updated.id {
                        state
                            .accounts
                            .borrow_mut()
                            .retain(|stored| stored.id != Some(account_id));
                    }
                    state.accounts.borrow_mut().push(updated);
                    start_account_monitor(&state, monitor_account);
                    render_sidebar(&state);
                    window.close();
                    set_status(&state, "Account updated securely");
                }
                Ok(Err(error)) => {
                    button_for_async.set_sensitive(true);
                    status_for_async.set_text(&format!("Couldn’t save this account: {error}"));
                }
                Err(_) => {
                    button_for_async.set_sensitive(true);
                    status_for_async.set_text("The account update worker stopped unexpectedly.");
                }
            }
        });
    });

    window.present();
}

fn open_account_dialog(state: Rc<AppState>) {
    let dialog = adw::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title("Add email account")
        .default_width(560)
        .default_height(620)
        .build();
    dialog.add_css_class("mail-dialog");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_margin_start(28);
    root.set_margin_end(28);
    root.set_margin_top(26);
    root.set_margin_bottom(26);
    let title = gtk::Label::new(Some("Add email account"));
    title.set_xalign(0.0);
    title.add_css_class("mail-reader-subject");
    root.append(&title);
    let hint = gtk::Label::new(Some(
        "We’ll keep your password or access token in the system keyring. It will never be written to Omarchy Mail’s database.",
    ));
    hint.set_xalign(0.0);
    hint.set_wrap(true);
    hint.add_css_class("mail-empty-body");
    hint.set_margin_top(6);
    hint.set_margin_bottom(20);
    root.append(&hint);

    let email = gtk::Entry::builder()
        .placeholder_text("you@example.com")
        .hexpand(true)
        .build();
    let display_name = gtk::Entry::builder()
        .placeholder_text("Your name")
        .hexpand(true)
        .build();
    let password = gtk::Entry::builder()
        .placeholder_text("Password, app password, or OAuth2 token")
        .hexpand(true)
        .build();
    password.set_visibility(false);
    root.append(&form_row("Email address", &email));
    root.append(&form_row("Display name", &display_name));
    root.append(&form_row("Password / token", &password));

    let advanced = gtk::Expander::new(Some("Connection details"));
    advanced.set_margin_top(12);
    let details = gtk::Box::new(gtk::Orientation::Vertical, 8);
    details.set_margin_top(10);
    let incoming_host = gtk::Entry::builder()
        .text("imap.example.com")
        .hexpand(true)
        .build();
    let incoming_auth = gtk::DropDown::from_strings(&["Password", "OAuth2 access token"]);
    incoming_auth.set_selected(0);
    incoming_auth.connect_selected_notify({
        let password = password.clone();
        move |auth| {
            password.set_placeholder_text(Some(if auth.selected() == 1 {
                "Paste IMAP OAuth2 access token"
            } else {
                "Password or app password"
            }));
        }
    });
    let incoming_username = gtk::Entry::builder()
        .placeholder_text("IMAP username (defaults to email)")
        .hexpand(true)
        .build();
    let incoming_port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    incoming_port.set_value(993.0);
    let incoming_security = gtk::DropDown::from_strings(&["TLS", "STARTTLS", "None"]);
    incoming_security.set_selected(0);
    let outgoing_host = gtk::Entry::builder()
        .text("smtp.example.com")
        .hexpand(true)
        .build();
    let outgoing_auth = gtk::DropDown::from_strings(&["Password", "OAuth2 access token"]);
    outgoing_auth.set_selected(0);
    let outgoing_username = gtk::Entry::builder()
        .placeholder_text("SMTP username (defaults to email)")
        .hexpand(true)
        .build();
    let outgoing_port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    outgoing_port.set_value(465.0);
    let outgoing_security = gtk::DropDown::from_strings(&["TLS", "STARTTLS", "None"]);
    outgoing_security.set_selected(0);
    let outgoing_password = gtk::Entry::builder()
        .placeholder_text("Leave empty to reuse IMAP password")
        .hexpand(true)
        .build();
    outgoing_password.set_visibility(false);
    outgoing_auth.connect_selected_notify({
        let outgoing_password = outgoing_password.clone();
        move |auth| {
            outgoing_password.set_placeholder_text(Some(if auth.selected() == 1 {
                "Paste SMTP OAuth2 access token"
            } else {
                "Leave empty to reuse IMAP password"
            }));
        }
    });
    details.append(&form_row("IMAP authentication", &incoming_auth));
    details.append(&form_row("IMAP server", &incoming_host));
    details.append(&form_row("IMAP username", &incoming_username));
    details.append(&form_row("IMAP port", &incoming_port));
    details.append(&form_row("IMAP security", &incoming_security));
    details.append(&form_row("SMTP authentication", &outgoing_auth));
    details.append(&form_row("SMTP server", &outgoing_host));
    details.append(&form_row("SMTP username", &outgoing_username));
    details.append(&form_row("SMTP port", &outgoing_port));
    details.append(&form_row("SMTP security", &outgoing_security));
    details.append(&form_row("SMTP password / token", &outgoing_password));
    let note = gtk::Label::new(Some(
        "TLS is used by default. IMAP and SMTP usernames may differ. For Gmail or Microsoft accounts, choose OAuth2 above and authorize securely in your browser.",
    ));
    note.set_wrap(true);
    note.add_css_class("mail-empty-body");
    details.append(&note);
    advanced.set_child(Some(&details));
    root.append(&advanced);

    let oauth_button = gtk::Button::with_label("Authorize in browser…");
    oauth_button.set_halign(gtk::Align::Start);
    oauth_button.add_css_class("mail-secondary-button");
    oauth_button.set_margin_top(12);
    oauth_button.set_visible(false);
    root.append(&oauth_button);

    let oauth_button_for_imap = oauth_button.clone();
    let email_for_imap_oauth = email.clone();
    let outgoing_auth_for_imap_oauth = outgoing_auth.clone();
    incoming_auth.connect_selected_notify(move |auth| {
        update_oauth_button(
            &oauth_button_for_imap,
            email_for_imap_oauth.text().as_str(),
            auth,
            &outgoing_auth_for_imap_oauth,
        );
    });
    let oauth_button_for_smtp = oauth_button.clone();
    let email_for_smtp_oauth = email.clone();
    let incoming_auth_for_smtp_oauth = incoming_auth.clone();
    outgoing_auth.connect_selected_notify(move |auth| {
        update_oauth_button(
            &oauth_button_for_smtp,
            email_for_smtp_oauth.text().as_str(),
            &incoming_auth_for_smtp_oauth,
            auth,
        );
    });

    email.connect_changed({
        let incoming_host = incoming_host.clone();
        let incoming_port = incoming_port.clone();
        let outgoing_host = outgoing_host.clone();
        let outgoing_port = outgoing_port.clone();
        let outgoing_security = outgoing_security.clone();
        let incoming_username = incoming_username.clone();
        let outgoing_username = outgoing_username.clone();
        let oauth_button = oauth_button.clone();
        let incoming_auth = incoming_auth.clone();
        let outgoing_auth = outgoing_auth.clone();
        move |entry| {
            update_oauth_button(
                &oauth_button,
                entry.text().as_str(),
                &incoming_auth,
                &outgoing_auth,
            );
            if mail::valid_email(entry.text().as_str()) {
                let (imap_host, imap_port, smtp_host, smtp_port) =
                    mail::discover_servers(entry.text().as_str());
                incoming_host.set_text(&imap_host);
                incoming_port.set_value(imap_port as f64);
                outgoing_host.set_text(&smtp_host);
                outgoing_port.set_value(smtp_port as f64);
                outgoing_security.set_selected(if smtp_port == 587 { 1 } else { 0 });
                if incoming_username.text().is_empty() {
                    incoming_username.set_text(entry.text().as_str());
                }
                if outgoing_username.text().is_empty() {
                    outgoing_username.set_text(entry.text().as_str());
                }
            }
        }
    });

    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("mail-danger");
    error.set_margin_top(14);
    root.append(&error);

    let email_for_oauth = email.clone();
    let incoming_auth_for_oauth = incoming_auth.clone();
    let outgoing_auth_for_oauth = outgoing_auth.clone();
    let error_for_oauth = error.clone();
    oauth_button.connect_clicked(move |button| {
        let protocols = oauth_protocols(
            email_for_oauth.text().as_str(),
            &incoming_auth_for_oauth,
            &outgoing_auth_for_oauth,
        );
        start_oauth_authorization(
            email_for_oauth.text().trim().to_string(),
            protocols,
            button.clone(),
            error_for_oauth.clone(),
        );
    });

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    actions.set_margin_top(20);
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Add account");
    save.add_css_class("mail-accent-button");
    actions.append(&cancel);
    actions.append(&save);
    root.append(&actions);
    dialog.set_content(Some(&root));

    let dialog_for_cancel = dialog.clone();
    cancel.connect_clicked(move |_| dialog_for_cancel.close());
    let state_for_save = state.clone();
    let dialog_for_save = dialog.clone();
    save.connect_clicked(move |button| {
        let address = email.text().trim().to_string();
        if !mail::valid_email(&address) {
            error.set_text("Enter a valid email address.");
            return;
        }
        let secret = password.text().to_string();
        let auth_method_for_index = |index| {
            if index == 1 {
                AuthMethod::OAuth2
            } else {
                AuthMethod::Password
            }
        };
        let incoming_auth_method = auth_method_for_index(incoming_auth.selected());
        let outgoing_auth_method = auth_method_for_index(outgoing_auth.selected());
        if incoming_auth_method == AuthMethod::Password && secret.is_empty() {
            error.set_text("Enter your password or app password.");
            return;
        }
        if outgoing_auth_method == AuthMethod::Password
            && incoming_auth_method == AuthMethod::OAuth2
            && outgoing_password.text().is_empty()
        {
            error.set_text(
                "Enter an SMTP password when IMAP uses OAuth2, or choose OAuth2 for SMTP too.",
            );
            return;
        }
        let name = if display_name.text().trim().is_empty() {
            address.split('@').next().unwrap_or("Account").to_string()
        } else {
            display_name.text().trim().to_string()
        };
        let incoming_login = if incoming_username.text().trim().is_empty() {
            address.clone()
        } else {
            incoming_username.text().trim().to_string()
        };
        let outgoing_login = if outgoing_username.text().trim().is_empty() {
            address.clone()
        } else {
            outgoing_username.text().trim().to_string()
        };
        let outgoing_secret = if outgoing_password.text().is_empty() {
            if outgoing_auth_method == AuthMethod::Password {
                secret.clone()
            } else {
                String::new()
            }
        } else {
            outgoing_password.text().to_string()
        };
        let security_for_index = |index| match index {
            1 => SecurityMode::StartTls,
            2 => SecurityMode::None,
            _ => SecurityMode::Tls,
        };
        let mut account = Account::new(&address, name);
        account.incoming = ServerConfig {
            hostname: incoming_host.text().trim().to_string(),
            port: incoming_port.value_as_int().max(1) as u16,
            security: security_for_index(incoming_security.selected()),
            username: incoming_login.clone(),
            auth: incoming_auth_method,
        };
        account.outgoing = ServerConfig {
            hostname: outgoing_host.text().trim().to_string(),
            port: outgoing_port.value_as_int().max(1) as u16,
            security: security_for_index(outgoing_security.selected()),
            username: outgoing_login.clone(),
            auth: outgoing_auth_method,
        };
        button.set_sensitive(false);
        error.set_text("Saving securely…");
        let button_for_async = button.clone();
        let error_for_async = error.clone();

        let database = state_for_save.database.clone();
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = if secret.is_empty() {
                Ok(())
            } else {
                mail::credentials::store_auth_material(
                    &address,
                    "imap",
                    &account.incoming.auth,
                    &secret,
                )
                .map_err(|error| error.to_string())
            }
            .and_then(|_| {
                if outgoing_secret.is_empty() {
                    Ok(())
                } else {
                    mail::credentials::store_auth_material(
                        &address,
                        "smtp",
                        &account.outgoing.auth,
                        &outgoing_secret,
                    )
                    .map_err(|error| error.to_string())
                }
            })
            .and_then(|_| {
                database
                    .save_account(&account)
                    .map_err(|error| error.to_string())
                    .map(|id| {
                        let mut account = account;
                        account.id = Some(id);
                        account
                    })
            });
            let _ = sender.send_blocking(result);
        });

        let state = state_for_save.clone();
        let dialog = dialog_for_save.clone();
        glib::MainContext::default().spawn_local(async move {
            match receiver.recv().await {
                Ok(Ok(account)) => {
                    let monitor_account = account.clone();
                    state.accounts.borrow_mut().push(account);
                    start_account_monitor(&state, monitor_account);
                    dialog.close();
                    render_sidebar(&state);
                    render_reader(&state, None);
                    set_status(&state, "Account added securely");
                }
                Ok(Err(message)) => {
                    button_for_async.set_sensitive(true);
                    error_for_async.set_text(&format!("Couldn’t add this account: {message}"));
                }
                Err(_) => {
                    button_for_async.set_sensitive(true);
                    error_for_async.set_text("The account setup worker stopped unexpectedly.");
                }
            }
        });
    });

    dialog.present();
}

fn open_settings(state: Rc<AppState>) {
    let window = adw::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title("Omarchy Mail settings")
        .default_width(540)
        .default_height(520)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_margin_start(28);
    root.set_margin_end(28);
    root.set_margin_top(26);
    root.set_margin_bottom(26);
    let title = gtk::Label::new(Some("Settings"));
    title.set_xalign(0.0);
    title.add_css_class("mail-reader-subject");
    root.append(&title);

    let accounts = gtk::Label::new(Some("ACCOUNTS"));
    accounts.set_xalign(0.0);
    accounts.add_css_class("mail-section-label");
    accounts.set_margin_top(24);
    root.append(&accounts);
    let account_summary = gtk::Label::new(Some(&format!(
        "{} configured account(s)",
        state.accounts.borrow().len()
    )));
    account_summary.set_xalign(0.0);
    account_summary.add_css_class("mail-empty-body");
    root.append(&account_summary);

    let account_list = gtk::Box::new(gtk::Orientation::Vertical, 8);
    account_list.set_margin_top(12);
    for account in state.accounts.borrow().iter().cloned() {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let identity = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let label = gtk::Label::new(Some(if account.display_name.trim().is_empty() {
            &account.email
        } else {
            &account.display_name
        }));
        label.set_xalign(0.0);
        label.add_css_class("mail-reader-meta");
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let details = gtk::Label::new(Some(&format!(
            "{} · {} · {}",
            account.email, account.incoming.hostname, account.outgoing.hostname
        )));
        details.set_xalign(0.0);
        details.add_css_class("mail-empty-body");
        details.set_ellipsize(gtk::pango::EllipsizeMode::End);
        identity.append(&label);
        identity.append(&details);
        identity.set_hexpand(true);
        header.append(&identity);
        let edit = gtk::Button::with_label("Edit");
        edit.set_tooltip_text(Some("Edit server details or re-enter credentials"));
        let settings_window_for_edit = window.clone();
        let state_for_edit = state.clone();
        let account_for_edit = account.clone();
        edit.connect_clicked(move |_| {
            settings_window_for_edit.close();
            open_account_editor(state_for_edit.clone(), account_for_edit.clone());
        });
        header.append(&edit);
        let notify_label = gtk::Label::new(Some("Notify"));
        notify_label.add_css_class("mail-reader-meta");
        let notify = gtk::Switch::new();
        notify.set_active(account.notify);
        notify.set_tooltip_text(Some("Allow new-mail notifications for this account"));
        let state_for_notify = state.clone();
        let account_for_notify = account.clone();
        notify.connect_active_notify(move |switcher| {
            let mut updated = account_for_notify.clone();
            updated.notify = switcher.is_active();
            let database = state_for_notify.database.clone();
            let state = state_for_notify.clone();
            let (sender, receiver) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = database
                    .save_account(&updated)
                    .map(|_| updated)
                    .map_err(|error| error.to_string());
                let _ = sender.send_blocking(result);
            });
            glib::MainContext::default().spawn_local(async move {
                match receiver.recv().await {
                    Ok(Ok(updated)) => {
                        if let Some(stored) = state
                            .accounts
                            .borrow_mut()
                            .iter_mut()
                            .find(|stored| stored.id == updated.id)
                        {
                            stored.notify = updated.notify;
                        }
                    }
                    Ok(Err(error)) => set_status(
                        &state,
                        &format!("Couldn’t save notification setting: {error}"),
                    ),
                    Err(_) => set_status(
                        &state,
                        "The notification setting worker stopped unexpectedly.",
                    ),
                }
            });
        });
        header.append(&notify_label);
        header.append(&notify);
        let signature_value = state
            .preferences
            .borrow()
            .signatures
            .get(&account.email)
            .cloned()
            .unwrap_or_default();
        let signature = gtk::TextView::new();
        signature.set_wrap_mode(gtk::WrapMode::WordChar);
        signature.set_size_request(-1, 44);
        signature.buffer().set_text(&signature_value);
        signature.add_css_class("mail-settings-signature");
        signature.set_hexpand(true);
        signature.set_tooltip_text(Some(
            "Signature (optional; leave empty to use the account identity)",
        ));
        let state_for_signature = state.clone();
        let email_for_signature = account.email.clone();
        let signature_for_callback = signature.clone();
        signature.buffer().connect_changed(move |_| {
            let result = {
                let mut preferences = state_for_signature.preferences.borrow_mut();
                let value = text_view_contents(&signature_for_callback);
                if value.trim().is_empty() {
                    preferences.signatures.remove(&email_for_signature);
                } else {
                    preferences
                        .signatures
                        .insert(email_for_signature.clone(), value);
                }
                preferences::save(&preferences)
            };
            if let Err(error) = result {
                set_status(
                    &state_for_signature,
                    &format!("Couldn’t save preferences: {error}"),
                );
            }
        });
        let remove = gtk::Button::with_label("Remove");
        remove.add_css_class("mail-danger");
        let state_for_remove = state.clone();
        let account_email = account.email.clone();
        remove.connect_clicked(move |button| {
            let Some(account_id) = account.id else {
                set_status(&state_for_remove, "This account has no local id yet");
                return;
            };
            button.set_sensitive(false);
            let database = state_for_remove.database.clone();
            let queued_sends = database.pending_sends(Some(account_id)).unwrap_or_default();
            let draft_ids = database
                .list_messages(Some(account_id), "Drafts")
                .unwrap_or_default()
                .into_iter()
                .map(|draft| draft.id)
                .collect::<Vec<_>>();
            let account_email = account_email.clone();
            let (sender, receiver) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = mail::credentials::delete_auth_materials(&account_email, "imap")
                    .map_err(|error| error.to_string())
                    .and_then(|_| {
                        mail::credentials::delete_auth_materials(&account_email, "smtp")
                            .map_err(|error| error.to_string())
                    })
                    .and_then(|_| {
                        database
                            .delete_account(account_id)
                            .map_err(|error| error.to_string())
                    })
                    .map(|_| {
                        for send in queued_sends {
                            mail::outbox::remove_staged_files(&send);
                        }
                        for draft_id in draft_ids {
                            mail::outbox::remove_draft_files(draft_id);
                        }
                    });
                let _ = sender.send_blocking(result);
            });
            let state = state_for_remove.clone();
            glib::MainContext::default().spawn_local(async move {
                match receiver.recv().await {
                    Ok(Ok(())) => {
                        if let Some(stop) = state.monitor_stops.borrow_mut().remove(&account_id) {
                            stop.store(true, Ordering::Relaxed);
                        }
                        if let Some(stop) = state.outbox_stops.borrow_mut().remove(&account_id) {
                            stop.store(true, Ordering::Relaxed);
                        }
                        state
                            .accounts
                            .borrow_mut()
                            .retain(|stored| stored.id != Some(account_id));
                        render_sidebar(&state);
                        set_status(&state, "Account removed; local mail cache cleared");
                    }
                    Ok(Err(error)) => {
                        set_status(&state, &format!("Couldn’t remove account: {error}"))
                    }
                    Err(_) => set_status(&state, "The account removal worker stopped unexpectedly"),
                }
            });
        });
        header.append(&remove);
        row.append(&header);
        row.append(&signature);
        account_list.append(&row);
    }
    root.append(&account_list);

    let reading = gtk::Label::new(Some("READING"));
    reading.set_xalign(0.0);
    reading.add_css_class("mail-section-label");
    reading.set_margin_top(24);
    root.append(&reading);
    root.append(&preference_switch_row(
        &state,
        "Block remote images by default",
        state.preferences.borrow().block_remote_images,
        |preferences, active| preferences.block_remote_images = active,
    ));
    root.append(&preference_switch_row(
        &state,
        "Group messages into conversations",
        state.preferences.borrow().conversation_view,
        |preferences, active| preferences.conversation_view = active,
    ));

    let notifications = gtk::Label::new(Some("NOTIFICATIONS"));
    notifications.set_xalign(0.0);
    notifications.add_css_class("mail-section-label");
    notifications.set_margin_top(24);
    root.append(&notifications);
    root.append(&preference_switch_row(
        &state,
        "New mail notifications",
        state.preferences.borrow().notifications_enabled,
        |preferences, active| preferences.notifications_enabled = active,
    ));

    let composing = gtk::Label::new(Some("COMPOSING"));
    composing.set_xalign(0.0);
    composing.add_css_class("mail-section-label");
    composing.set_margin_top(24);
    root.append(&composing);
    root.append(&preference_switch_row(
        &state,
        "Ask before sending in plain text",
        state.preferences.borrow().plain_text_warning,
        |preferences, active| preferences.plain_text_warning = active,
    ));

    let close = gtk::Button::with_label("Done");
    close.set_halign(gtk::Align::End);
    close.set_margin_top(26);
    let window_for_close = window.clone();
    close.connect_clicked(move |_| window_for_close.close());
    root.append(&close);
    window.set_content(Some(&root));
    window.present();
}

fn open_compose(state: Rc<AppState>) {
    open_compose_with_context(state, None);
}

enum ComposeContext {
    Reply { message: Message, reply_all: bool },
    Forward(Message),
    Draft(Message),
}

struct ComposerFormatting {
    tag_table: gtk::TextTagTable,
    bold: gtk::TextTag,
    italic: gtk::TextTag,
    underline: gtk::TextTag,
    link_tags: RefCell<HashMap<String, gtk::TextTag>>,
    next_link: Cell<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ComposerMark {
    bold: bool,
    italic: bool,
    underline: bool,
    link: Option<String>,
}

fn build_composer_formatting(buffer: &gtk::TextBuffer) -> Rc<ComposerFormatting> {
    let tag_table = buffer.tag_table();
    let bold = gtk::TextTag::builder()
        .name("compose-bold")
        .weight(700)
        .build();
    let italic = gtk::TextTag::builder()
        .name("compose-italic")
        .style(gtk::pango::Style::Italic)
        .build();
    let underline = gtk::TextTag::builder()
        .name("compose-underline")
        .underline(gtk::pango::Underline::Single)
        .build();
    tag_table.add(&bold);
    tag_table.add(&italic);
    tag_table.add(&underline);
    Rc::new(ComposerFormatting {
        tag_table,
        bold,
        italic,
        underline,
        link_tags: RefCell::new(HashMap::new()),
        next_link: Cell::new(0),
    })
}

fn composer_link_tag(formatting: &Rc<ComposerFormatting>, url: &str) -> gtk::TextTag {
    if let Some(tag) = formatting.link_tags.borrow().get(url) {
        return tag.clone();
    }
    let name = format!("compose-link-{}", formatting.next_link.get());
    formatting.next_link.set(formatting.next_link.get() + 1);
    let tag = gtk::TextTag::builder()
        .name(&name)
        .underline(gtk::pango::Underline::Single)
        .build();
    formatting.tag_table.add(&tag);
    formatting
        .link_tags
        .borrow_mut()
        .insert(url.to_string(), tag.clone());
    tag
}

fn composer_mark(iter: &gtk::TextIter, formatting: &Rc<ComposerFormatting>) -> ComposerMark {
    let link = iter.tags().into_iter().find_map(|tag| {
        let name = tag.name()?.to_string();
        if !name.starts_with("compose-link-") {
            return None;
        }
        formatting
            .link_tags
            .borrow()
            .iter()
            .find(|(_, candidate)| candidate.name().as_deref() == Some(name.as_str()))
            .map(|(url, _)| url.clone())
    });
    ComposerMark {
        bold: iter.has_tag(&formatting.bold),
        italic: iter.has_tag(&formatting.italic),
        underline: iter.has_tag(&formatting.underline),
        link,
    }
}

fn composer_tags(mark: &ComposerMark, formatting: &Rc<ComposerFormatting>) -> Vec<gtk::TextTag> {
    let mut tags = Vec::new();
    if let Some(url) = mark.link.as_deref() {
        tags.push(composer_link_tag(formatting, url));
    }
    if mark.bold {
        tags.push(formatting.bold.clone());
    }
    if mark.italic {
        tags.push(formatting.italic.clone());
    }
    if mark.underline {
        tags.push(formatting.underline.clone());
    }
    tags
}

fn insert_composer_text(
    buffer: &gtk::TextBuffer,
    formatting: &Rc<ComposerFormatting>,
    mark: &ComposerMark,
    text: &str,
) {
    if text.is_empty() {
        return;
    }
    let tags = composer_tags(mark, formatting);
    let tag_refs = tags.iter().collect::<Vec<_>>();
    let mut end = buffer.end_iter();
    buffer.insert_with_tags(&mut end, text, &tag_refs);
}

fn decode_composer_entities(value: &str) -> String {
    value
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn composer_href(raw_tag: &str) -> Option<String> {
    let lower = raw_tag.to_ascii_lowercase();
    let start = lower.find("href=")? + "href=".len();
    let value = raw_tag[start..].trim_start();
    let value = if let Some(quoted) = value.strip_prefix('"') {
        quoted.split_once('"')?.0
    } else if let Some(quoted) = value.strip_prefix('\'') {
        quoted.split_once('\'')?.0
    } else {
        value.split_whitespace().next()?
    };
    let value = decode_composer_entities(value);
    (value.starts_with("https://") || value.starts_with("http://") || value.starts_with("mailto:"))
        .then_some(value)
}

fn composer_ends_with_newline(buffer: &gtk::TextBuffer) -> bool {
    let mut end = buffer.end_iter();
    end.backward_char() && end.char() == '\n'
}

fn composer_add_line_break(buffer: &gtk::TextBuffer) {
    if !buffer.start_iter().is_end() && !composer_ends_with_newline(buffer) {
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, "\n");
    }
}

fn insert_composer_html(buffer: &gtk::TextBuffer, formatting: &Rc<ComposerFormatting>, html: &str) {
    let safe = mail::mime::sanitize_html(html);
    let mut mark = ComposerMark::default();
    let mut position = 0;
    while position < safe.len() {
        let Some(relative_start) = safe[position..].find('<') else {
            insert_composer_text(
                buffer,
                formatting,
                &mark,
                &decode_composer_entities(&safe[position..]),
            );
            break;
        };
        let start = position + relative_start;
        insert_composer_text(
            buffer,
            formatting,
            &mark,
            &decode_composer_entities(&safe[position..start]),
        );
        let Some(relative_end) = safe[start..].find('>') else {
            insert_composer_text(
                buffer,
                formatting,
                &mark,
                &decode_composer_entities(&safe[start..]),
            );
            break;
        };
        let end = start + relative_end;
        let raw_tag = &safe[start + 1..end];
        let normalized = raw_tag.trim().to_ascii_lowercase();
        let closing = normalized.starts_with('/');
        let name = normalized
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or_default();
        match (closing, name) {
            (_, "br") => {
                let mut end_iter = buffer.end_iter();
                buffer.insert(&mut end_iter, "\n");
            }
            (false, "a") => mark.link = composer_href(raw_tag),
            (true, "a") => mark.link = None,
            (false, "b") | (false, "strong") => mark.bold = true,
            (true, "b") | (true, "strong") => mark.bold = false,
            (false, "i") | (false, "em") => mark.italic = true,
            (true, "i") | (true, "em") => mark.italic = false,
            (false, "u") => mark.underline = true,
            (true, "u") => mark.underline = false,
            (true, "p") | (true, "div") | (true, "li") => composer_add_line_break(buffer),
            _ => {}
        }
        position = end + 1;
    }
}

fn composer_html_body(buffer: &gtk::TextBuffer, formatting: &Rc<ComposerFormatting>) -> String {
    let mut output = String::new();
    let mut current = ComposerMark::default();
    let mut iter = buffer.start_iter();
    while !iter.is_end() {
        let mark = composer_mark(&iter, formatting);
        if mark != current {
            if current.underline {
                output.push_str("</u>");
            }
            if current.italic {
                output.push_str("</em>");
            }
            if current.bold {
                output.push_str("</strong>");
            }
            if current.link.is_some() {
                output.push_str("</a>");
            }
            if mark.link.is_some() {
                let url = mark.link.as_deref().unwrap_or_default();
                output.push_str("<a href=\"");
                output.push_str(&glib::markup_escape_text(url));
                output.push_str("\">");
            }
            if mark.bold {
                output.push_str("<strong>");
            }
            if mark.italic {
                output.push_str("<em>");
            }
            if mark.underline {
                output.push_str("<u>");
            }
            current = mark;
        }
        if iter.char() == '\n' {
            if current.underline {
                output.push_str("</u>");
            }
            if current.italic {
                output.push_str("</em>");
            }
            if current.bold {
                output.push_str("</strong>");
            }
            if current.link.is_some() {
                output.push_str("</a>");
            }
            output.push_str("<br>\n");
            current = ComposerMark::default();
        } else {
            output.push_str(&glib::markup_escape_text(
                iter.char().encode_utf8(&mut [0; 4]),
            ));
        }
        if !iter.forward_char() {
            break;
        }
    }
    if current.underline {
        output.push_str("</u>");
    }
    if current.italic {
        output.push_str("</em>");
    }
    if current.bold {
        output.push_str("</strong>");
    }
    if current.link.is_some() {
        output.push_str("</a>");
    }
    if output.trim().is_empty() {
        String::new()
    } else {
        format!("<div>{output}</div>")
    }
}

fn composer_contents(
    body: &gtk::TextView,
    html_mode: bool,
    formatting: &Rc<ComposerFormatting>,
) -> (String, Option<String>) {
    let plain = text_view_contents(body);
    let html = html_mode
        .then(|| mail::mime::sanitize_html(&composer_html_body(&body.buffer(), formatting)))
        .filter(|html| !html.trim().is_empty());
    (plain, html)
}

fn toggle_composer_tag(body: &gtk::TextView, tag: &gtk::TextTag) -> bool {
    let Some((start, end)) = body.buffer().selection_bounds() else {
        return false;
    };
    if start.offset() == end.offset() {
        return false;
    }
    let mut cursor = start.clone();
    let mut fully_tagged = true;
    while cursor.offset() < end.offset() {
        if !cursor.has_tag(tag) {
            fully_tagged = false;
            break;
        }
        if !cursor.forward_char() {
            break;
        }
    }
    if fully_tagged {
        body.buffer().remove_tag(tag, &start, &end);
    } else {
        body.buffer().apply_tag(tag, &start, &end);
    }
    true
}

fn toggle_composer_bullet(body: &gtk::TextView) {
    let buffer = body.buffer();
    let Some(insert_mark) = buffer.mark("insert") else {
        return;
    };
    let cursor = buffer.iter_at_mark(&insert_mark);
    let mut start = cursor.clone();
    start.set_line_offset(0);
    let mut end = cursor.clone();
    end.forward_to_line_end();
    let line = start.text(&end).to_string();
    if line.starts_with("• ") {
        let mut remove_end = start.clone();
        remove_end.forward_chars(2);
        buffer.delete(&mut start, &mut remove_end);
    } else {
        buffer.insert(&mut start, "• ");
    }
}

fn open_composer_link_dialog(
    parent: &adw::Window,
    body: &gtk::TextView,
    formatting: &Rc<ComposerFormatting>,
    status: &gtk::Label,
    on_changed: &Rc<dyn Fn()>,
) {
    let Some((start, end)) = body.buffer().selection_bounds() else {
        status.set_text("Select some text before adding a link.");
        return;
    };
    if start.offset() == end.offset() {
        status.set_text("Select some text before adding a link.");
        return;
    }
    let dialog = gtk::Dialog::builder()
        .transient_for(parent)
        .modal(true)
        .title("Add link")
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Add link", gtk::ResponseType::Accept);
    let content = dialog.content_area();
    content.set_spacing(10);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    let entry = gtk::Entry::builder()
        .placeholder_text("https://example.com")
        .build();
    content.append(&gtk::Label::new(Some("Link address")));
    content.append(&entry);
    let buffer = body.buffer();
    let start_offset = start.offset();
    let end_offset = end.offset();
    let formatting = formatting.clone();
    let status = status.clone();
    let on_changed = on_changed.clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept {
            let url = entry.text().trim().to_string();
            if !(url.starts_with("https://")
                || url.starts_with("http://")
                || url.starts_with("mailto:"))
            {
                status.set_text("Use an http(s) or mailto link.");
                return;
            }
            let tag = composer_link_tag(&formatting, &url);
            let start = buffer.iter_at_offset(start_offset);
            let end = buffer.iter_at_offset(end_offset);
            buffer.apply_tag(&tag, &start, &end);
            status.set_text("Link added");
            on_changed();
        }
        dialog.close();
    });
    dialog.present();
}

#[derive(Debug)]
enum SendDisposition {
    Sent,
    Queued { retryable: bool },
}

fn open_compose_with_context(state: Rc<AppState>, context: Option<ComposeContext>) {
    if state.accounts.borrow().is_empty() && !state.demo_mode {
        open_account_dialog(state);
        return;
    }

    let (
        initial_account_id,
        initial_draft_id,
        initial_to,
        initial_cc,
        initial_bcc,
        initial_subject,
        initial_body,
        initial_body_html,
        initial_attachments,
    ) = match context {
        Some(ComposeContext::Reply { message, reply_all }) => {
            let subject = if message.subject.to_lowercase().starts_with("re:") {
                message.subject.clone()
            } else {
                format!("Re: {}", message.subject)
            };
            let cc = if reply_all {
                message.recipients
            } else {
                String::new()
            };
            let quoted = message
                .body
                .lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            let body = format!(
                "\n\nOn {}, {} wrote:\n{}",
                message.received_at, message.sender_name, quoted
            );
            (
                message.account_id,
                None,
                message.sender_email,
                cc,
                String::new(),
                subject,
                body,
                None,
                Vec::new(),
            )
        }
        Some(ComposeContext::Forward(message)) => {
            let subject = if message.subject.to_lowercase().starts_with("fwd:") {
                message.subject.clone()
            } else {
                format!("Fwd: {}", message.subject)
            };
            let body = format!(
                "\n\n---------- Forwarded message ----------\nFrom: {} <{}>\nDate: {}\nSubject: {}\n\n{}",
                message.sender_name,
                message.sender_email,
                message.received_at,
                message.subject,
                message.body
            );
            (
                message.account_id,
                None,
                String::new(),
                String::new(),
                String::new(),
                subject,
                body,
                None,
                Vec::new(),
            )
        }
        Some(ComposeContext::Draft(message)) => {
            let recipients = parse_draft_recipients(&message.recipients);
            (
                message.account_id,
                Some(message.id),
                recipients.to,
                recipients.cc,
                recipients.bcc,
                message.subject,
                message.body,
                message.body_html,
                message.attachments,
            )
        }
        None => (
            None,
            None,
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            None,
            Vec::new(),
        ),
    };
    let compose_title = if initial_draft_id.is_some() {
        "Edit draft"
    } else {
        "New message"
    };

    let window = adw::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title(compose_title)
        .default_width(760)
        .default_height(620)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_margin_start(26);
    root.set_margin_end(26);
    root.set_margin_top(24);
    root.set_margin_bottom(24);
    let title = gtk::Label::new(Some(compose_title));
    title.set_xalign(0.0);
    title.add_css_class("mail-reader-subject");
    root.append(&title);
    let to = gtk::Entry::builder().placeholder_text("Recipients").build();
    let cc = gtk::Entry::builder()
        .placeholder_text("Carbon copy recipients")
        .build();
    let bcc = gtk::Entry::builder()
        .placeholder_text("Blind carbon copy recipients")
        .build();
    let subject = gtk::Entry::builder().placeholder_text("Subject").build();
    let account_labels = if state.demo_mode && state.accounts.borrow().is_empty() {
        vec!["Preview account <demo@example.com>".to_string()]
    } else {
        state
            .accounts
            .borrow()
            .iter()
            .map(|account| {
                if account.display_name.is_empty() {
                    account.email.clone()
                } else {
                    format!("{} <{}>", account.display_name, account.email)
                }
            })
            .collect::<Vec<_>>()
    };
    let account_label_refs = account_labels
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let account_selector = gtk::DropDown::from_strings(&account_label_refs);
    account_selector.set_hexpand(true);
    account_selector.set_selected(0);
    if let Some(account_id) = initial_account_id {
        if let Some(index) = state
            .accounts
            .borrow()
            .iter()
            .position(|account| account.id == Some(account_id))
        {
            account_selector.set_selected(index as u32);
        }
    }
    root.append(&form_row("From", &account_selector));
    to.set_text(&initial_to);
    cc.set_text(&initial_cc);
    bcc.set_text(&initial_bcc);
    subject.set_text(&initial_subject);
    root.append(&to);
    let recipient_options = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    recipient_options.set_margin_top(6);
    let cc_toggle = gtk::ToggleButton::with_label("Cc");
    let bcc_toggle = gtk::ToggleButton::with_label("Bcc");
    cc_toggle.set_active(!initial_cc.trim().is_empty());
    bcc_toggle.set_active(!initial_bcc.trim().is_empty());
    recipient_options.append(&cc_toggle);
    recipient_options.append(&bcc_toggle);
    root.append(&recipient_options);
    let cc_row = form_row("Cc", &cc);
    let bcc_row = form_row("Bcc", &bcc);
    cc_row.set_visible(cc_toggle.is_active());
    bcc_row.set_visible(bcc_toggle.is_active());
    root.append(&cc_row);
    root.append(&bcc_row);
    root.append(&subject);
    for entry in [&to, &cc, &bcc, &subject] {
        entry.set_hexpand(true);
        entry.set_margin_top(8);
    }
    let cc_row_for_toggle = cc_row.clone();
    cc_toggle.connect_toggled(move |button| {
        cc_row_for_toggle.set_visible(button.is_active());
    });
    let bcc_row_for_toggle = bcc_row.clone();
    bcc_toggle.connect_toggled(move |button| {
        bcc_row_for_toggle.set_visible(button.is_active());
    });
    let body = gtk::TextView::new();
    body.add_css_class("mail-compose-body");
    body.set_wrap_mode(gtk::WrapMode::WordChar);
    body.set_vexpand(true);
    body.set_top_margin(16);
    body.set_bottom_margin(16);
    body.set_left_margin(12);
    body.set_right_margin(12);
    let formatting = build_composer_formatting(&body.buffer());
    if let Some(html) = initial_body_html.as_deref() {
        insert_composer_html(&body.buffer(), &formatting, html);
    } else {
        body.buffer().set_text(&initial_body);
    }

    let format_toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    format_toolbar.set_margin_top(12);
    format_toolbar.set_margin_bottom(2);
    let format_label = gtk::Label::new(Some("Format"));
    format_label.add_css_class("mail-reader-meta");
    let format_selector = gtk::DropDown::from_strings(&["Plain text", "HTML"]);
    format_selector.set_selected(if initial_body_html.is_some() { 1 } else { 0 });
    let bold = gtk::Button::with_label("B");
    bold.set_tooltip_text(Some("Bold"));
    let italic = gtk::Button::with_label("I");
    italic.set_tooltip_text(Some("Italic"));
    let underline = gtk::Button::with_label("U");
    underline.set_tooltip_text(Some("Underline"));
    let bullet = gtk::Button::with_label("•");
    bullet.set_tooltip_text(Some("Toggle bullet on this line"));
    let link = gtk::Button::with_label("Link");
    link.set_tooltip_text(Some("Add a link to the selected text"));
    let signature = gtk::Button::with_label("Signature");
    signature.set_tooltip_text(Some("Insert the selected account's signature"));
    for button in [&bold, &italic, &underline, &bullet, &link] {
        button.add_css_class("mail-format-button");
    }
    format_toolbar.append(&format_label);
    format_toolbar.append(&format_selector);
    format_toolbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    format_toolbar.append(&bold);
    format_toolbar.append(&italic);
    format_toolbar.append(&underline);
    format_toolbar.append(&bullet);
    format_toolbar.append(&link);
    format_toolbar.append(&signature);
    let format_controls_enabled = initial_body_html.is_some();
    for button in [&bold, &italic, &underline, &bullet, &link] {
        button.set_sensitive(format_controls_enabled);
    }
    root.append(&format_toolbar);
    root.append(&body);
    let attachment_paths: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(
        initial_attachments
            .iter()
            .filter_map(|attachment| {
                attachment_available(attachment).then(|| PathBuf::from(&attachment.cache_path))
            })
            .collect(),
    ));
    let draft_id: Rc<Cell<Option<i64>>> = Rc::new(Cell::new(initial_draft_id));
    let cancelled = Rc::new(Cell::new(false));
    let discard_on_cancel = initial_draft_id.is_none();
    let close_composer: Rc<dyn Fn()> = {
        let state = state.clone();
        let window = window.clone();
        let draft_id = draft_id.clone();
        let cancelled = cancelled.clone();
        Rc::new(move || {
            cancelled.set(true);
            if discard_on_cancel {
                discard_draft_async(state.clone(), draft_id.get());
            }
            window.close();
        })
    };
    let attachment_list = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    attachment_list.set_hexpand(true);
    root.append(&attachment_list);
    for attachment in &initial_attachments {
        if attachment_available(attachment) {
            append_attachment_chip(&attachment_list, &attachment.filename);
        }
    }
    let compose_status = gtk::Label::new(None);
    compose_status.set_xalign(0.0);
    compose_status.set_wrap(true);
    compose_status.add_css_class("mail-status");
    root.append(&compose_status);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.add_css_class("mail-compose-toolbar");
    let attach = gtk::Button::with_label("Attach file");
    let draft = gtk::Button::with_label("Save draft");
    let cancel = gtk::Button::with_label("Cancel");
    let send = gtk::Button::with_label("Send");
    send.add_css_class("mail-accent-button");
    toolbar.append(&attach);
    toolbar.append(&draft);
    toolbar.append(&cancel);
    toolbar.append(&send);
    toolbar.set_halign(gtk::Align::End);
    root.append(&toolbar);

    let close_for_cancel = close_composer.clone();
    cancel.connect_clicked(move |_| close_for_cancel());
    let close_for_escape = close_composer.clone();
    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    key_controller.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            close_for_escape();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key_controller);

    let file_dialog = gtk::FileDialog::builder()
        .title("Attach a file")
        .accept_label("Attach")
        .build();
    let window_for_attach = window.clone();
    let paths_for_attach = attachment_paths.clone();
    let list_for_attach = attachment_list.clone();
    let status_for_attach = compose_status.clone();
    attach.connect_clicked(move |_| {
        let dialog = file_dialog.clone();
        let paths = paths_for_attach.clone();
        let list = list_for_attach.clone();
        let status = status_for_attach.clone();
        dialog.open(
            Some(&window_for_attach),
            None::<&gio::Cancellable>,
            move |result| match result {
                Ok(file) => {
                    let Some(path) = file.path() else {
                        status.set_text("That file is not available locally.");
                        return;
                    };
                    let name = path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("attachment")
                        .to_string();
                    paths.borrow_mut().push(path);
                    append_attachment_chip(&list, &name);
                    status.set_text("Attachment added");
                }
                Err(error) if error.matches(gio::IOErrorEnum::Cancelled) => {}
                Err(error) => status.set_text(&format!("Couldn’t attach that file: {error}")),
            },
        );
    });

    let state_for_draft = state.clone();
    let attachment_paths_for_draft = attachment_paths.clone();
    let draft_id_for_manual = draft_id.clone();
    let account_selector_for_draft = account_selector.clone();
    let to_for_draft = to.clone();
    let cc_for_draft = cc.clone();
    let bcc_for_draft = bcc.clone();
    let subject_for_draft = subject.clone();
    let body_for_draft = body.clone();
    let format_selector_for_draft = format_selector.clone();
    let formatting_for_draft = formatting.clone();
    let compose_status_for_draft = compose_status.clone();
    let cancelled_for_draft = cancelled.clone();
    draft.connect_clicked(move |_| {
        save_draft_async(
            state_for_draft.clone(),
            draft_id_for_manual.clone(),
            account_selector_for_draft.clone(),
            to_for_draft.clone(),
            cc_for_draft.clone(),
            bcc_for_draft.clone(),
            subject_for_draft.clone(),
            body_for_draft.clone(),
            format_selector_for_draft.clone(),
            formatting_for_draft.clone(),
            attachment_paths_for_draft.clone(),
            cancelled_for_draft.clone(),
            compose_status_for_draft.clone(),
            "Saving draft…",
        );
    });

    let draft_revision = Rc::new(Cell::new(0_u64));
    let schedule_autosave: Rc<dyn Fn()> = {
        let state = state.clone();
        let draft_id = draft_id.clone();
        let account_selector = account_selector.clone();
        let to = to.clone();
        let cc = cc.clone();
        let bcc = bcc.clone();
        let subject = subject.clone();
        let body = body.clone();
        let format_selector = format_selector.clone();
        let formatting = formatting.clone();
        let attachment_paths = attachment_paths.clone();
        let cancelled = cancelled.clone();
        let status = compose_status.clone();
        let revision = draft_revision.clone();
        Rc::new(move || {
            schedule_draft_autosave(
                state.clone(),
                draft_id.clone(),
                account_selector.clone(),
                to.clone(),
                cc.clone(),
                bcc.clone(),
                subject.clone(),
                body.clone(),
                format_selector.clone(),
                formatting.clone(),
                attachment_paths.clone(),
                cancelled.clone(),
                status.clone(),
                revision.clone(),
            )
        })
    };
    let bold_for_format = bold.clone();
    let italic_for_format = italic.clone();
    let underline_for_format = underline.clone();
    let bullet_for_format = bullet.clone();
    let link_for_format = link.clone();
    let schedule_for_format = schedule_autosave.clone();
    format_selector.connect_selected_notify(move |selector| {
        let enabled = selector.selected() == 1;
        bold_for_format.set_sensitive(enabled);
        italic_for_format.set_sensitive(enabled);
        underline_for_format.set_sensitive(enabled);
        bullet_for_format.set_sensitive(enabled);
        link_for_format.set_sensitive(enabled);
        schedule_for_format();
    });

    let body_for_bold = body.clone();
    let formatting_for_bold = formatting.clone();
    let schedule_for_bold = schedule_autosave.clone();
    bold.connect_clicked(move |_| {
        if toggle_composer_tag(&body_for_bold, &formatting_for_bold.bold) {
            schedule_for_bold();
        }
    });
    let body_for_italic = body.clone();
    let formatting_for_italic = formatting.clone();
    let schedule_for_italic = schedule_autosave.clone();
    italic.connect_clicked(move |_| {
        if toggle_composer_tag(&body_for_italic, &formatting_for_italic.italic) {
            schedule_for_italic();
        }
    });
    let body_for_underline = body.clone();
    let formatting_for_underline = formatting.clone();
    let schedule_for_underline = schedule_autosave.clone();
    underline.connect_clicked(move |_| {
        if toggle_composer_tag(&body_for_underline, &formatting_for_underline.underline) {
            schedule_for_underline();
        }
    });
    let body_for_bullet = body.clone();
    let schedule_for_bullet = schedule_autosave.clone();
    bullet.connect_clicked(move |_| {
        toggle_composer_bullet(&body_for_bullet);
        schedule_for_bullet();
    });
    let body_for_link = body.clone();
    let formatting_for_link = formatting.clone();
    let window_for_link = window.clone();
    let status_for_link = compose_status.clone();
    let schedule_for_link = schedule_autosave.clone();
    link.connect_clicked(move |_| {
        open_composer_link_dialog(
            &window_for_link,
            &body_for_link,
            &formatting_for_link,
            &status_for_link,
            &schedule_for_link,
        );
    });
    let body_for_signature = body.clone();
    let state_for_signature = state.clone();
    let account_selector_for_signature = account_selector.clone();
    let status_for_signature = compose_status.clone();
    let schedule_for_signature = schedule_autosave.clone();
    signature.connect_clicked(move |_| {
        let account = state_for_signature
            .accounts
            .borrow()
            .get(account_selector_for_signature.selected() as usize)
            .cloned();
        let Some(account) = account else {
            status_for_signature.set_text("Add an account before inserting a signature.");
            return;
        };
        let signature = state_for_signature
            .preferences
            .borrow()
            .signature_for(&account);
        let current = text_view_contents(&body_for_signature);
        if current.contains(&signature) {
            status_for_signature.set_text("That signature is already in the message.");
            return;
        }
        let buffer = body_for_signature.buffer();
        let mut end = buffer.end_iter();
        if !current.trim().is_empty() {
            buffer.insert(&mut end, "\n\n");
        }
        buffer.insert(&mut end, &signature);
        status_for_signature.set_text("Signature inserted");
        schedule_for_signature();
    });
    for entry in [&to, &cc, &bcc, &subject] {
        let schedule = schedule_autosave.clone();
        entry.connect_changed(move |_| schedule());
    }
    let schedule = schedule_autosave.clone();
    body.buffer().connect_changed(move |_| schedule());

    let state_for_send = state.clone();
    let window_for_send = window.clone();
    let account_selector_for_send = account_selector.clone();
    let attachments_for_send = attachment_paths.clone();
    let draft_id_for_send = draft_id.clone();
    send.connect_clicked(move |button| {
        let to_value = to.text().trim().to_string();
        if to_value.is_empty() {
            compose_status.set_text("Add at least one recipient.");
            return;
        }
        if let Err(error) = mail::validate_recipients(&to_value) {
            compose_status.set_text(&error);
            return;
        }
        let subject_value = subject.text().to_string();
        let (body_value, body_html) =
            composer_contents(&body, format_selector.selected() == 1, &formatting);
        let cc_value = cc.text().trim().to_string();
        let bcc_value = bcc.text().trim().to_string();
        let cc_values = match mail::validate_recipients(&cc_value) {
            Ok(recipients) => recipients,
            Err(error) => {
                compose_status.set_text(&error);
                return;
            }
        };
        let bcc_values = match mail::validate_recipients(&bcc_value) {
            Ok(recipients) => recipients,
            Err(error) => {
                compose_status.set_text(&error);
                return;
            }
        };
        let attachments = attachments_for_send.borrow().clone();
        if state_for_send.demo_mode {
            if let Some(draft_id) = draft_id_for_send.get() {
                let database = state_for_send.database.clone();
                std::thread::spawn(move || {
                    let _ = database.delete_message(draft_id);
                    mail::outbox::remove_draft_files(draft_id);
                });
            }
            set_status(&state_for_send, "Preview message queued");
            window_for_send.close();
            return;
        }
        let Some(account) = state_for_send
            .accounts
            .borrow()
            .get(account_selector_for_send.selected() as usize)
            .cloned()
            .or_else(|| state_for_send.accounts.borrow().first().cloned())
        else {
            compose_status.set_text("Add an account before sending.");
            return;
        };
        let Some(account_id) = account.id else {
            compose_status.set_text("This account is not ready to send mail yet.");
            return;
        };
        button.set_sensitive(false);
        compose_status.set_text("Sending securely…");
        let (sender, receiver) = async_channel::bounded(1);
        let database = state_for_send.database.clone();
        std::thread::spawn(move || {
            let result = mail::credentials::load_auth_material(
                &account.email,
                "smtp",
                &account.outgoing.auth,
            )
            .map_err(|error| {
                mail::credentials::friendly_load_error("SMTP", &account.outgoing.auth, &error)
            })
            .and_then(|auth| {
                match mail::smtp::send_text_with_auth(
                    &account,
                    &auth,
                    &to_value,
                    &cc_values,
                    &bcc_values,
                    &subject_value,
                    &body_value,
                    body_html.as_deref(),
                    &attachments,
                ) {
                    Ok(()) => Ok(SendDisposition::Sent),
                    Err(error) => {
                        let error_text = error.to_string();
                        let retryable = error.is_retryable();
                        mail::outbox::queue_failed_send(
                            &database,
                            account_id,
                            &to_value,
                            &cc_values,
                            &bcc_values,
                            &subject_value,
                            &body_value,
                            body_html.as_deref(),
                            &attachments,
                            &error_text,
                            retryable,
                        )
                        .map(|_| SendDisposition::Queued { retryable })
                        .map_err(|queue_error| {
                            format!("{error_text}; also could not save it to Outbox: {queue_error}")
                        })
                    }
                }
            });
            let _ = sender.send_blocking(result);
        });
        let state = state_for_send.clone();
        let window = window_for_send.clone();
        let button = button.clone();
        let compose_status = compose_status.clone();
        let draft_id = draft_id_for_send.clone();
        glib::MainContext::default().spawn_local(async move {
            match receiver.recv().await {
                Ok(Ok(SendDisposition::Sent)) => {
                    if let Some(draft_id) = draft_id.get() {
                        let database = state.database.clone();
                        std::thread::spawn(move || {
                            let _ = database.delete_message(draft_id);
                            mail::outbox::remove_draft_files(draft_id);
                        });
                    }
                    set_status(&state, "Message sent");
                    window.close();
                }
                Ok(Ok(SendDisposition::Queued { retryable })) => {
                    if let Some(draft_id) = draft_id.get() {
                        let database = state.database.clone();
                        std::thread::spawn(move || {
                            let _ = database.delete_message(draft_id);
                            mail::outbox::remove_draft_files(draft_id);
                        });
                    }
                    refresh_cached_view(&state);
                    set_status(
                        &state,
                        if retryable {
                            "Message saved to Outbox; it will retry when online"
                        } else {
                            "Message saved to Outbox; fix the issue and choose Retry now"
                        },
                    );
                    window.close();
                }
                Ok(Err(error)) => {
                    button.set_sensitive(true);
                    compose_status.set_text(&format!("Couldn’t send this message: {error}"));
                }
                Err(_) => {
                    button.set_sensitive(true);
                    compose_status.set_text("The send worker stopped unexpectedly.");
                }
            }
        });
    });
    let send_for_keyboard = send.clone();
    let send_key_controller = gtk::EventControllerKey::new();
    send_key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    send_key_controller.connect_key_pressed(move |_, key, _, modifiers| {
        if modifiers.contains(gdk::ModifierType::CONTROL_MASK) && key == gdk::Key::Return {
            send_for_keyboard.emit_clicked();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(send_key_controller);
    let _ = attach;
    window.set_content(Some(&root));
    window.present();
}

fn schedule_draft_autosave(
    state: Rc<AppState>,
    draft_id: Rc<Cell<Option<i64>>>,
    account_selector: gtk::DropDown,
    to: gtk::Entry,
    cc: gtk::Entry,
    bcc: gtk::Entry,
    subject: gtk::Entry,
    body: gtk::TextView,
    format_selector: gtk::DropDown,
    formatting: Rc<ComposerFormatting>,
    attachment_paths: Rc<RefCell<Vec<PathBuf>>>,
    cancelled: Rc<Cell<bool>>,
    status: gtk::Label,
    revision: Rc<Cell<u64>>,
) {
    let token = revision.get().wrapping_add(1);
    revision.set(token);
    glib::timeout_add_local_once(Duration::from_secs(2), move || {
        if revision.get() == token {
            save_draft_async(
                state,
                draft_id,
                account_selector,
                to,
                cc,
                bcc,
                subject,
                body,
                format_selector,
                formatting,
                attachment_paths,
                cancelled,
                status,
                "Saving draft…",
            );
        }
    });
}

fn save_draft_async(
    state: Rc<AppState>,
    draft_id: Rc<Cell<Option<i64>>>,
    account_selector: gtk::DropDown,
    to: gtk::Entry,
    cc: gtk::Entry,
    bcc: gtk::Entry,
    subject: gtk::Entry,
    body: gtk::TextView,
    format_selector: gtk::DropDown,
    formatting: Rc<ComposerFormatting>,
    attachment_paths: Rc<RefCell<Vec<PathBuf>>>,
    cancelled: Rc<Cell<bool>>,
    status: gtk::Label,
    status_text: &'static str,
) {
    if cancelled.get() {
        return;
    }
    let to_value = to.text().trim().to_string();
    let cc_value = cc.text().trim().to_string();
    let bcc_value = bcc.text().trim().to_string();
    let recipients = compose_recipient_summary(&to_value, &cc_value, &bcc_value);
    let subject = subject.text().to_string();
    let (body, body_html) = composer_contents(&body, format_selector.selected() == 1, &formatting);
    let source_paths = attachment_paths.borrow().clone();
    if recipients.is_empty()
        && subject.trim().is_empty()
        && body.trim().is_empty()
        && source_paths.is_empty()
    {
        return;
    }
    let account_id = state
        .accounts
        .borrow()
        .get(account_selector.selected() as usize)
        .and_then(|account| account.id);
    let existing_id = draft_id.get();
    let database = state.database.clone();
    status.set_text(status_text);
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = (|| {
            let id = match existing_id {
                Some(existing_id) => existing_id,
                None => database.save_draft_with_html(
                    account_id,
                    &recipients,
                    &subject,
                    &body,
                    body_html.as_deref(),
                )?,
            };
            let staged = mail::outbox::stage_draft_attachments(id, &source_paths)
                .map_err(|error| crate::database::DatabaseError::InvalidValue(error.to_string()))?;
            let metadata = staged
                .iter()
                .map(|attachment| AttachmentInfo {
                    filename: attachment.filename.clone(),
                    content_type: "application/octet-stream".into(),
                    size: std::fs::metadata(&attachment.path)
                        .map(|metadata| metadata.len())
                        .unwrap_or_default(),
                    cache_path: attachment.path.clone(),
                    content_id: None,
                })
                .collect::<Vec<_>>();
            let id = database.update_draft_with_html_and_attachments(
                id,
                account_id,
                &recipients,
                &subject,
                &body,
                body_html.as_deref(),
                &metadata,
            )?;
            Ok::<_, crate::database::DatabaseError>((id, staged))
        })()
        .map_err(|error| error.to_string());
        let _ = sender.send_blocking(result);
    });
    glib::MainContext::default().spawn_local(async move {
        match receiver.recv().await {
            Ok(Ok((id, staged))) => {
                if cancelled.get() {
                    discard_draft_async(state.clone(), Some(id));
                    return;
                }
                draft_id.set(Some(id));
                let staged_paths = staged
                    .iter()
                    .map(|attachment| PathBuf::from(&attachment.path))
                    .collect::<Vec<_>>();
                *attachment_paths.borrow_mut() = staged_paths;
                status.set_text("Draft saved locally");
            }
            Ok(Err(error)) => status.set_text(&format!("Couldn’t save this draft: {error}")),
            Err(_) => status.set_text("The draft worker stopped unexpectedly."),
        }
    });
}

fn compose_recipient_summary(to: &str, cc: &str, bcc: &str) -> String {
    let mut fields = Vec::new();
    if !to.trim().is_empty() {
        fields.push(to.trim().to_string());
    }
    if !cc.trim().is_empty() {
        fields.push(format!("Cc: {}", cc.trim()));
    }
    if !bcc.trim().is_empty() {
        fields.push(format!("Bcc: {}", bcc.trim()));
    }
    fields.join("\n")
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DraftRecipients {
    to: String,
    cc: String,
    bcc: String,
}

fn parse_draft_recipients(value: &str) -> DraftRecipients {
    let mut recipients = DraftRecipients::default();
    for line in value.lines() {
        let line = line.trim();
        let (field, address) = line
            .split_once(':')
            .map(|(field, address)| (Some(field.trim()), address.trim()))
            .unwrap_or((None, line));
        let target = match field.map(str::to_ascii_lowercase).as_deref() {
            Some("cc") => &mut recipients.cc,
            Some("bcc") => &mut recipients.bcc,
            _ => &mut recipients.to,
        };
        if address.is_empty() {
            continue;
        }
        if !target.is_empty() {
            target.push_str("; ");
        }
        target.push_str(address);
    }
    recipients
}

fn delete_local_message(state: &Rc<AppState>, message_id: i64) {
    state
        .messages
        .borrow_mut()
        .retain(|message| message.id != message_id);
    if *state.selected_message.borrow() == Some(message_id) {
        state.selected_message.replace(None);
        render_reader(state, None);
    }
    let database = state.database.clone();
    std::thread::spawn(move || {
        let _ = database.delete_message(message_id);
        mail::outbox::remove_draft_files(message_id);
    });
    set_status(state, "Draft deleted");
    render_sidebar(state);
    render_messages(state, state.search_entry.text().as_str());
}

fn discard_draft_async(state: Rc<AppState>, draft_id: Option<i64>) {
    let Some(draft_id) = draft_id else {
        return;
    };
    let database = state.database.clone();
    std::thread::spawn(move || {
        let _ = database.delete_message(draft_id);
        mail::outbox::remove_draft_files(draft_id);
    });
}

fn text_view_contents(view: &gtk::TextView) -> String {
    let buffer = view.buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), true)
        .to_string()
}

fn form_row(label: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_margin_top(8);
    let label_widget = gtk::Label::new(Some(label));
    label_widget.set_width_chars(17);
    label_widget.set_xalign(0.0);
    label_widget.add_css_class("mail-reader-meta");
    row.append(&label_widget);
    row.append(widget);
    row
}

fn append_attachment_chip(list: &gtk::Box, filename: &str) {
    let chip = gtk::Label::new(Some(&format!("  {filename}  ")));
    chip.add_css_class("mail-attachment-chip");
    chip.set_max_width_chars(34);
    chip.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    list.append(&chip);
}

fn preference_switch_row<F>(state: &Rc<AppState>, label: &str, active: bool, setter: F) -> gtk::Box
where
    F: Fn(&mut preferences::Preferences, bool) + 'static,
{
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_margin_top(10);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    let switcher = gtk::Switch::new();
    switcher.set_active(active);
    let state = state.clone();
    switcher.connect_active_notify(move |switcher| {
        let result = {
            let mut preferences = state.preferences.borrow_mut();
            setter(&mut preferences, switcher.is_active());
            preferences::save(&preferences)
        };
        if let Err(error) = result {
            set_status(&state, &format!("Couldn’t save preferences: {error}"));
        }
    });
    row.append(&text);
    row.append(&switcher);
    row
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.set_valign(gtk::Align::Center);
    button
}

fn clear(widget: &impl IsA<gtk::Widget>) {
    let widget = widget.as_ref();
    while let Some(child) = widget.first_child() {
        if let Ok(list) = widget.clone().downcast::<gtk::ListBox>() {
            list.remove(&child);
        } else if let Ok(container) = widget.clone().downcast::<gtk::Box>() {
            container.remove(&child);
        } else {
            break;
        }
    }
}

fn set_status(state: &AppState, message: &str) {
    state.status.set_text(message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_messages_by_account_folder_and_thread() {
        let mut messages = Message::demo_messages();
        messages[1].thread_key = messages[0].thread_key.clone();
        let grouped = group_messages(messages);

        assert_eq!(grouped.len(), 4);
        assert_eq!(grouped[0].thread_size, 2);
        assert!(grouped[0].unread);
        assert!(grouped[0].starred);
    }

    #[test]
    fn matches_refetched_attachments_by_content_id_or_safe_metadata() {
        let inline = AttachmentInfo {
            filename: "inline-image".into(),
            content_type: "image/png".into(),
            size: 10,
            cache_path: String::new(),
            content_id: Some("logo@example.com".into()),
        };
        let refreshed_inline = AttachmentInfo {
            cache_path: "/tmp/logo.png".into(),
            ..inline.clone()
        };
        assert!(same_attachment(&inline, &refreshed_inline));

        let file = AttachmentInfo {
            filename: "notes.txt".into(),
            content_type: "text/plain".into(),
            size: 5,
            cache_path: String::new(),
            content_id: None,
        };
        let refreshed_file = AttachmentInfo {
            cache_path: "/tmp/notes.txt".into(),
            size: 99,
            ..file.clone()
        };
        assert!(same_attachment(&file, &refreshed_file));
    }

    #[test]
    fn preserves_compose_recipient_fields_without_leaking_empty_rows() {
        assert_eq!(
            compose_recipient_summary(
                "jane@example.com; team@example.com",
                "copy@example.com",
                "archive@example.com"
            ),
            "jane@example.com; team@example.com\nCc: copy@example.com\nBcc: archive@example.com"
        );
        assert_eq!(
            compose_recipient_summary("jane@example.com", "", ""),
            "jane@example.com"
        );
    }

    #[test]
    fn rehydrates_draft_recipient_fields() {
        let recipients = parse_draft_recipients(
            "jane@example.com; team@example.com\nCc: copy@example.com\nBcc: archive@example.com",
        );
        assert_eq!(
            recipients,
            DraftRecipients {
                to: "jane@example.com; team@example.com".into(),
                cc: "copy@example.com".into(),
                bcc: "archive@example.com".into(),
            }
        );
    }

    #[test]
    fn formats_message_dates_for_list_density() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        assert_eq!(
            format_message_date_at("2026-09-01T09:04:00+01:00", today),
            "Today, 09:04"
        );
        assert_eq!(
            format_message_date_at("2026-08-31T21:04:00+01:00", today),
            "Yesterday"
        );
        assert_eq!(
            format_message_date_at("2026-01-02T11:30:00+01:00", today),
            "2 Jan"
        );
        assert_eq!(
            format_message_date_at("2025-12-31T11:30:00+00:00", today),
            "31 Dec 2025"
        );
        assert_eq!(format_message_date_at("not-a-date", today), "not-a-date");
    }

    #[test]
    fn formats_reader_dates_with_localized_full_context() {
        let formatted = format_message_datetime("2026-09-01T09:04:00+00:00");
        assert!(formatted.contains("1 September 2026 at"));
        assert!(formatted.ends_with(":04"));
    }

    #[test]
    fn combines_text_and_local_search_filters() {
        let mut message = Message::demo_messages().remove(0);
        message.sender_name = "Élodie".into();
        message.body = "Café plans for the garden photos".into();
        message.account_id = Some(7);
        message.folder = "Sent".into();
        message.received_at = "2026-08-31T14:32:00+00:00".into();
        let filters = SearchFilters {
            unread: true,
            starred: true,
            attachments: true,
            account_id: Some(7),
            folder: Some("Sent".into()),
            after: Some("2026-08-01".into()),
            before: Some("2026-08-31".into()),
        };
        assert!(search_filters_match(&message, &filters));
        assert!(search_text_matches(&message, "garden photos"));
        assert!(search_text_matches(&message, "CAFÉ"));
        assert!(!search_text_matches(&message, "garden unrelated"));
        assert!(!search_filters_match(
            &message,
            &SearchFilters {
                folder: Some("Inbox".into()),
                ..filters
            }
        ));
    }
}
