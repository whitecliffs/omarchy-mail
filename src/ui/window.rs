use crate::database::Database;
use crate::mail;
use crate::models::{Account, Message, SecurityMode, ServerConfig};
use crate::theme;
use adw::prelude::*;
use gtk::gdk;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

struct AppState {
    window: adw::ApplicationWindow,
    database: Database,
    accounts: RefCell<Vec<Account>>,
    messages: RefCell<Vec<Message>>,
    sidebar: gtk::Box,
    message_list: gtk::ListBox,
    reader: gtk::Box,
    search_entry: gtk::SearchEntry,
    selected_message: RefCell<Option<i64>>,
    status: gtk::Label,
    demo_mode: bool,
}

pub fn build_window(application: &adw::Application) {
    let database =
        Database::open_default().expect("Omarchy Mail could not open its local database");
    let accounts = database.load_accounts().unwrap_or_default();
    let demo_mode = std::env::var("OMARCHY_MAIL_DEMO").as_deref() == Ok("1");
    let messages = if demo_mode {
        Message::demo_messages()
    } else {
        database.list_messages(None, "Inbox").unwrap_or_default()
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

    let (header, compose, refresh, settings) = build_header(&status);

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.add_css_class("mail-sidebar");
    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&sidebar)
        .build();

    let message_list = gtk::ListBox::new();
    message_list.set_selection_mode(gtk::SelectionMode::Single);
    message_list.set_activate_on_single_click(true);
    message_list.add_css_class("mail-message-list");
    let message_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&message_list)
        .build();

    let middle = gtk::Box::new(gtk::Orientation::Vertical, 0);
    middle.add_css_class("mail-middle");
    let (filter_bar, search) = build_filter_bar();
    middle.append(&filter_bar);
    middle.append(&message_scroll);

    let reader = gtk::Box::new(gtk::Orientation::Vertical, 0);
    reader.add_css_class("mail-reader");
    reader.set_vexpand(true);

    let content = gtk::Paned::new(gtk::Orientation::Horizontal);
    content.set_wide_handle(true);
    content.set_position(258);
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
        messages: RefCell::new(messages),
        sidebar,
        message_list,
        reader,
        search_entry: search.clone(),
        selected_message: RefCell::new(None),
        status,
        demo_mode,
    });

    // The middle pane is inserted after the state exists so its selection can
    // route into the reader without keeping a second source of truth.
    content.set_end_child(Some(&build_middle_and_reader(state.clone(), &middle)));
    connect_header_actions(state.clone(), &compose, &refresh, &settings);
    let state_for_search = state.clone();
    search.connect_search_changed(move |entry| {
        render_messages(&state_for_search, entry.text().as_str())
    });
    render_sidebar(&state);
    render_messages(&state, "");
    render_reader(&state, None);

    theme::install();
    connect_keyboard_shortcuts(&state);
    window.present();

    if !state.accounts.borrow().is_empty() {
        let state_for_startup_sync = state.clone();
        glib::idle_add_local_once(move || sync_all(state_for_startup_sync, false));
        let state_for_periodic_sync = state.clone();
        glib::timeout_add_local(Duration::from_secs(300), move || {
            sync_all(state_for_periodic_sync.clone(), true);
            glib::ControlFlow::Continue
        });
    }
}

fn build_header(status: &gtk::Label) -> (adw::HeaderBar, gtk::Button, gtk::Button, gtk::Button) {
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Omarchy Mail", "Email without the clutter");
    header.set_title_widget(Some(&title));

    let compose = icon_button("mail-message-new-symbolic", "Compose a new message");
    compose.set_widget_name("compose-button");
    header.pack_start(&compose);

    let refresh = icon_button("view-refresh-symbolic", "Synchronise mail");
    refresh.set_widget_name("refresh-button");
    header.pack_end(&refresh);
    let settings = icon_button("emblem-system-symbolic", "Settings");
    settings.set_widget_name("settings-button");
    header.pack_end(&settings);
    header.pack_end(status);
    (header, compose, refresh, settings)
}

fn connect_header_actions(
    state: Rc<AppState>,
    compose: &gtk::Button,
    refresh: &gtk::Button,
    settings: &gtk::Button,
) {
    let state_for_compose = state.clone();
    compose.connect_clicked(move |_| open_compose(state_for_compose.clone()));
    let state_for_refresh = state.clone();
    refresh.connect_clicked(move |_| sync_all(state_for_refresh.clone(), true));
    let state_for_settings = state;
    settings.connect_clicked(move |_| open_settings(state_for_settings.clone()));
}

fn build_middle_and_reader(state: Rc<AppState>, middle: &gtk::Box) -> gtk::Paned {
    let reader = state.reader.clone();
    let pane = gtk::Paned::new(gtk::Orientation::Horizontal);
    pane.set_wide_handle(true);
    pane.set_position(440);
    pane.set_start_child(Some(middle));
    pane.set_end_child(Some(&reader));
    pane.set_resize_start_child(false);
    pane.set_shrink_start_child(true);
    pane.set_resize_end_child(true);
    pane.set_shrink_end_child(true);

    let message_list = state.message_list.clone();
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

fn build_filter_bar() -> (gtk::Box, gtk::SearchEntry) {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar.add_css_class("mail-filter-bar");
    bar.set_margin_start(12);
    bar.set_margin_end(12);
    bar.set_margin_top(10);
    bar.set_margin_bottom(10);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search messages"));
    search.set_hexpand(true);
    search.set_tooltip_text(Some("Search sender, recipients, subject, and message text"));
    search.set_widget_name("message-search");
    bar.append(&search);

    let filter = icon_button("view-filter-symbolic", "Filter messages");
    bar.append(&filter);
    (bar, search)
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

    let messages = state.messages.borrow();
    let unread = messages.iter().filter(|message| message.unread).count();
    for (label, icon, count) in [
        ("Inbox", "mail-unread-symbolic", Some(unread)),
        ("Starred", "starred-symbolic", None),
        ("Sent", "mail-send-symbolic", None),
        ("Drafts", "document-save-symbolic", None),
        ("Archive", "archive-symbolic", None),
        ("Trash", "user-trash-symbolic", None),
    ] {
        inner.append(&sidebar_row(label, icon, count));
    }
    drop(messages);

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
            inner.append(&account_expander(account));
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

fn account_expander(account: &Account) -> gtk::Expander {
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
    expander.set_expanded(true);

    let folders = gtk::Box::new(gtk::Orientation::Vertical, 2);
    folders.set_margin_start(24);
    for folder in ["Inbox", "Drafts", "Sent", "Archive", "Spam", "Trash"] {
        folders.append(&sidebar_row(folder, "folder-symbolic", None));
    }
    expander.set_child(Some(&folders));
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

fn render_messages(state: &Rc<AppState>, query: &str) {
    clear(&state.message_list);
    let query = query.trim().to_lowercase();
    let messages = state.messages.borrow();
    for message in messages.iter().filter(|message| {
        query.is_empty()
            || [
                message.sender_name.as_str(),
                message.sender_email.as_str(),
                message.recipients.as_str(),
                message.subject.as_str(),
                message.body.as_str(),
            ]
            .iter()
            .any(|value| value.to_lowercase().contains(&query))
    }) {
        let (row, star) = message_row(message);
        let message_id = message.id;
        let state_for_star = state.clone();
        star.connect_clicked(move |_| {
            let next = {
                let mut messages = state_for_star.messages.borrow_mut();
                let Some(message) = messages.iter_mut().find(|message| message.id == message_id)
                else {
                    return;
                };
                message.starred = !message.starred;
                message.starred
            };
            let database = state_for_star.database.clone();
            std::thread::spawn(move || {
                let _ = database.set_starred(message_id, next);
            });
            render_sidebar(&state_for_star);
            render_messages(&state_for_star, "");
        });

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
        state.message_list.append(&row);
    }
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
        while let Ok(report) = receiver.recv().await {
            completed += 1;
            if let Some(error) = report.error {
                errors.push(format!("{}: {error}", report.email));
            } else if notify && report.new_messages > 0 {
                crate::mail::sync::notify_new_mail(&report.email, report.new_messages);
            }
            if let Ok(messages) = state.database.list_messages(None, "Inbox") {
                state.messages.replace(messages);
                render_sidebar(&state);
                render_messages(&state, "");
            }
            set_status(&state, &format!("Synchronised {completed}/{account_count}"));
        }
        if errors.is_empty() {
            set_status(&state, "All accounts are up to date");
        } else {
            set_status(
                &state,
                &format!("Sync needs attention: {}", errors.join(" · ")),
            );
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
    let indicator = gtk::Label::new(Some(if message.unread { "●" } else { " " }));
    indicator.add_css_class("mail-unread-dot");
    indicator.set_valign(gtk::Align::Start);
    layout.append(&indicator);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 3);
    text.set_hexpand(true);
    let sender = gtk::Label::new(Some(&message.sender_name));
    sender.set_xalign(0.0);
    sender.add_css_class("mail-sender");
    text.append(&sender);
    let subject = gtk::Label::new(Some(&message.subject));
    subject.set_xalign(0.0);
    subject.set_ellipsize(gtk::pango::EllipsizeMode::End);
    subject.add_css_class("mail-subject");
    text.append(&subject);
    let preview = gtk::Label::new(Some(&message.preview));
    preview.set_xalign(0.0);
    preview.set_ellipsize(gtk::pango::EllipsizeMode::End);
    preview.add_css_class("mail-preview");
    text.append(&preview);
    layout.append(&text);

    let trailing = gtk::Box::new(gtk::Orientation::Vertical, 4);
    trailing.set_valign(gtk::Align::Start);
    let date = gtk::Label::new(Some(&message.received_at));
    date.add_css_class("mail-date");
    trailing.append(&date);
    let markers = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let star = icon_button(
        if message.starred {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        },
        "Star or unstar message",
    );
    star.add_css_class("mail-star");
    star.set_widget_name(&format!("star-{}", message.id));
    markers.append(&star);
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
    trailing.append(&markers);
    layout.append(&trailing);
    row.set_child(Some(&layout));
    (row, star)
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
    for (label, action) in [
        ("Mark read / unread", "read"),
        ("Star / unstar", "star"),
        ("Archive", "archive"),
        ("Move to Trash", "trash"),
    ] {
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

fn apply_message_action(state: &Rc<AppState>, message_id: i64, action: &str) {
    let mut persist = None;
    match action {
        "read" => {
            let unread = {
                let mut messages = state.messages.borrow_mut();
                let Some(message) = messages.iter_mut().find(|message| message.id == message_id)
                else {
                    return;
                };
                message.unread = !message.unread;
                message.unread
            };
            persist = Some(("read", unread));
        }
        "star" => {
            let starred = {
                let mut messages = state.messages.borrow_mut();
                let Some(message) = messages.iter_mut().find(|message| message.id == message_id)
                else {
                    return;
                };
                message.starred = !message.starred;
                message.starred
            };
            persist = Some(("star", starred));
        }
        "archive" | "trash" => {
            let folder = if action == "archive" {
                "Archive"
            } else {
                "Trash"
            };
            state
                .messages
                .borrow_mut()
                .retain(|message| message.id != message_id);
            let database = state.database.clone();
            let folder = folder.to_string();
            std::thread::spawn(move || {
                let _ = database.move_message(message_id, &folder);
                let _ = database.queue_action(
                    None,
                    Some(message_id),
                    "move",
                    &serde_json::json!({ "folder": folder }).to_string(),
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
        }
        _ => {}
    }
    if let Some((kind, value)) = persist {
        let database = state.database.clone();
        std::thread::spawn(move || {
            if kind == "read" {
                let _ = database.set_unread(message_id, value);
            } else {
                let _ = database.set_starred(message_id, value);
            }
        });
        set_status(state, "Message updated");
    }
    render_sidebar(state);
    render_messages(state, "");
}

fn render_reader(state: &Rc<AppState>, message: Option<Message>) {
    clear(&state.reader);
    if let Some(message) = message {
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_start(34);
        content.set_margin_end(34);
        content.set_margin_top(30);
        content.set_margin_bottom(34);

        let subject = gtk::Label::new(Some(&message.subject));
        subject.set_xalign(0.0);
        subject.set_wrap(true);
        subject.add_css_class("mail-reader-subject");
        content.append(&subject);

        let sender_line = gtk::Box::new(gtk::Orientation::Horizontal, 7);
        sender_line.set_margin_top(16);
        let sender = gtk::Label::new(Some(&message.sender_name));
        sender.add_css_class("mail-reader-sender");
        sender_line.append(&sender);
        let email = gtk::Label::new(Some(&format!("<{}>", message.sender_email)));
        email.add_css_class("mail-reader-meta");
        sender_line.append(&email);
        let date = gtk::Label::new(Some(&message.received_at));
        date.add_css_class("mail-reader-meta");
        date.set_hexpand(true);
        date.set_xalign(1.0);
        sender_line.append(&date);
        content.append(&sender_line);

        let recipients = gtk::Label::new(Some(&format!("To {}", message.recipients)));
        recipients.set_xalign(0.0);
        recipients.add_css_class("mail-reader-meta");
        content.append(&recipients);

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
            let action = match label {
                "Archive" => Some("archive"),
                "Delete" => Some("trash"),
                _ => None,
            };
            button.connect_clicked(move |_| {
                if let Some(action) = action {
                    apply_message_action(&state, reader_message_id, action);
                } else {
                    set_status(&state, &format!("{label_owned} ready"));
                }
            });
            actions.append(&button);
        }
        content.append(&actions);

        let rule = gtk::Separator::new(gtk::Orientation::Horizontal);
        rule.set_margin_top(22);
        rule.set_margin_bottom(24);
        content.append(&rule);

        let body_text = if message.body.contains('<') {
            crate::mail::mime::html_to_text(&message.body)
        } else {
            message.body.clone()
        };
        let body = gtk::Label::new(Some(&body_text));
        body.set_xalign(0.0);
        body.set_yalign(0.0);
        body.set_wrap(true);
        body.set_selectable(true);
        body.add_css_class("mail-reader-body");
        content.append(&body);

        if message.has_attachments {
            let attachments = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            attachments.set_margin_top(30);
            let chip = gtk::Label::new(Some("  attachment  "));
            chip.add_css_class("mail-attachment-chip");
            attachments.append(&chip);
            content.append(&attachments);
        }
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
        let title = gtk::Label::new(Some("Select a message to read it"));
        title.add_css_class("mail-empty-title");
        let body = gtk::Label::new(Some("Your conversations will appear here."));
        body.add_css_class("mail-empty-body");
        empty.append(&title);
        empty.append(&body);
        state.reader.append(&empty);
    }
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
        "We’ll keep your password in the system keyring. It will never be written to Omarchy Mail’s database.",
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
        .placeholder_text("Password or app password")
        .hexpand(true)
        .build();
    password.set_visibility(false);
    root.append(&form_row("Email address", &email));
    root.append(&form_row("Display name", &display_name));
    root.append(&form_row("Password", &password));

    let advanced = gtk::Expander::new(Some("Connection details"));
    advanced.set_margin_top(12);
    let details = gtk::Box::new(gtk::Orientation::Vertical, 8);
    details.set_margin_top(10);
    let incoming_host = gtk::Entry::builder()
        .text("imap.example.com")
        .hexpand(true)
        .build();
    let incoming_port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    incoming_port.set_value(993.0);
    let outgoing_host = gtk::Entry::builder()
        .text("smtp.example.com")
        .hexpand(true)
        .build();
    let outgoing_port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    outgoing_port.set_value(465.0);
    details.append(&form_row("IMAP server", &incoming_host));
    details.append(&form_row("IMAP port", &incoming_port));
    details.append(&form_row("SMTP server", &outgoing_host));
    details.append(&form_row("SMTP port", &outgoing_port));
    let note = gtk::Label::new(Some(
        "TLS is used by default. Provider-specific OAuth sign-in will be added without storing provider secrets in the app.",
    ));
    note.set_wrap(true);
    note.add_css_class("mail-empty-body");
    details.append(&note);
    advanced.set_child(Some(&details));
    root.append(&advanced);

    email.connect_changed({
        let incoming_host = incoming_host.clone();
        let incoming_port = incoming_port.clone();
        let outgoing_host = outgoing_host.clone();
        let outgoing_port = outgoing_port.clone();
        move |entry| {
            if mail::valid_email(entry.text().as_str()) {
                let (imap_host, imap_port, smtp_host, smtp_port) =
                    mail::discover_servers(entry.text().as_str());
                incoming_host.set_text(&imap_host);
                incoming_port.set_value(imap_port as f64);
                outgoing_host.set_text(&smtp_host);
                outgoing_port.set_value(smtp_port as f64);
            }
        }
    });

    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("mail-danger");
    error.set_margin_top(14);
    root.append(&error);

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
        if secret.is_empty() {
            error.set_text("Enter your password or app password.");
            return;
        }
        let name = if display_name.text().trim().is_empty() {
            address.split('@').next().unwrap_or("Account").to_string()
        } else {
            display_name.text().trim().to_string()
        };
        let mut account = Account::new(&address, name);
        account.incoming = ServerConfig {
            hostname: incoming_host.text().trim().to_string(),
            port: incoming_port.value_as_int().max(1) as u16,
            security: SecurityMode::Tls,
            username: address.clone(),
        };
        account.outgoing = ServerConfig {
            hostname: outgoing_host.text().trim().to_string(),
            port: outgoing_port.value_as_int().max(1) as u16,
            security: if outgoing_port.value_as_int() == 587 {
                SecurityMode::StartTls
            } else {
                SecurityMode::Tls
            },
            username: address.clone(),
        };
        button.set_sensitive(false);
        error.set_text("Saving securely…");
        let button_for_async = button.clone();
        let error_for_async = error.clone();

        let database = state_for_save.database.clone();
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = mail::credentials::store_password(&address, &secret)
                .map_err(|error| error.to_string())
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
                    state.accounts.borrow_mut().push(account);
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
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let label = gtk::Label::new(Some(&account.email));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.add_css_class("mail-reader-meta");
        let remove = gtk::Button::with_label("Remove");
        remove.add_css_class("mail-danger");
        let state_for_remove = state.clone();
        remove.connect_clicked(move |button| {
            let Some(account_id) = account.id else {
                set_status(&state_for_remove, "This account has no local id yet");
                return;
            };
            button.set_sensitive(false);
            let email = account.email.clone();
            let database = state_for_remove.database.clone();
            let (sender, receiver) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = mail::credentials::delete_password(&email)
                    .map_err(|error| error.to_string())
                    .and_then(|_| {
                        database
                            .delete_account(account_id)
                            .map_err(|error| error.to_string())
                    });
                let _ = sender.send_blocking(result);
            });
            let state = state_for_remove.clone();
            glib::MainContext::default().spawn_local(async move {
                match receiver.recv().await {
                    Ok(Ok(())) => {
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
        row.append(&label);
        row.append(&remove);
        account_list.append(&row);
    }
    root.append(&account_list);

    let reading = gtk::Label::new(Some("READING"));
    reading.set_xalign(0.0);
    reading.add_css_class("mail-section-label");
    reading.set_margin_top(24);
    root.append(&reading);
    root.append(&switch_row("Block remote images by default", true));
    root.append(&switch_row("Group messages into conversations", true));

    let composing = gtk::Label::new(Some("COMPOSING"));
    composing.set_xalign(0.0);
    composing.add_css_class("mail-section-label");
    composing.set_margin_top(24);
    root.append(&composing);
    root.append(&switch_row("Ask before sending in plain text", false));

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
    if state.accounts.borrow().is_empty() && !state.demo_mode {
        open_account_dialog(state);
        return;
    }
    let window = adw::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title("New message")
        .default_width(760)
        .default_height(620)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_margin_start(26);
    root.set_margin_end(26);
    root.set_margin_top(24);
    root.set_margin_bottom(24);
    let title = gtk::Label::new(Some("New message"));
    title.set_xalign(0.0);
    title.add_css_class("mail-reader-subject");
    root.append(&title);
    let to = gtk::Entry::builder().placeholder_text("Recipients").build();
    let cc = gtk::Entry::builder().placeholder_text("Cc / Bcc").build();
    let subject = gtk::Entry::builder().placeholder_text("Subject").build();
    root.append(&to);
    root.append(&cc);
    root.append(&subject);
    for entry in [&to, &cc, &subject] {
        entry.set_margin_top(8);
    }
    let body = gtk::TextView::new();
    body.set_wrap_mode(gtk::WrapMode::WordChar);
    body.set_vexpand(true);
    body.set_top_margin(16);
    body.set_bottom_margin(16);
    body.set_left_margin(12);
    body.set_right_margin(12);
    root.append(&body);
    let compose_status = gtk::Label::new(None);
    compose_status.set_xalign(0.0);
    compose_status.set_wrap(true);
    compose_status.add_css_class("mail-status");
    root.append(&compose_status);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let attach = gtk::Button::with_label("Attach file");
    let draft = gtk::Button::with_label("Save draft");
    let send = gtk::Button::with_label("Send");
    send.add_css_class("mail-accent-button");
    toolbar.append(&attach);
    toolbar.append(&draft);
    toolbar.append(&send);
    toolbar.set_halign(gtk::Align::End);
    root.append(&toolbar);
    let state_for_draft = state.clone();
    draft.connect_clicked(move |_| set_status(&state_for_draft, "Draft saved locally"));
    let state_for_send = state.clone();
    let window_for_send = window.clone();
    send.connect_clicked(move |button| {
        let to_value = to.text().trim().to_string();
        if to_value.is_empty() {
            compose_status.set_text("Add at least one recipient.");
            return;
        }
        let subject_value = subject.text().to_string();
        let body_buffer = body.buffer();
        let body_value = body_buffer
            .text(&body_buffer.start_iter(), &body_buffer.end_iter(), true)
            .to_string();
        if state_for_send.demo_mode {
            set_status(&state_for_send, "Preview message queued");
            window_for_send.close();
            return;
        }
        let Some(account) = state_for_send.accounts.borrow().first().cloned() else {
            compose_status.set_text("Add an account before sending.");
            return;
        };
        let cc_values = cc
            .text()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        button.set_sensitive(false);
        compose_status.set_text("Sending securely…");
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = mail::credentials::load_password(&account.email)
                .map_err(|error| error.to_string())
                .and_then(|password| {
                    mail::smtp::send_text(
                        &account,
                        &password,
                        &to_value,
                        &cc_values,
                        &subject_value,
                        &body_value,
                    )
                    .map_err(|error| error.to_string())
                });
            let _ = sender.send_blocking(result);
        });
        let state = state_for_send.clone();
        let window = window_for_send.clone();
        let button = button.clone();
        let compose_status = compose_status.clone();
        glib::MainContext::default().spawn_local(async move {
            match receiver.recv().await {
                Ok(Ok(())) => {
                    set_status(&state, "Message sent");
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
    let _ = attach;
    window.set_content(Some(&root));
    window.present();
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

fn switch_row(label: &str, active: bool) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_margin_top(10);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    let switcher = gtk::Switch::new();
    switcher.set_active(active);
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
