//! Productivity features layered on the main window: reminders, snooze,
//! undo send / send later, invitations, unsubscribe, address suggestions,
//! server search, calendar views and editing, Google Tasks, and automatic
//! backups. Kept separate so window.rs only carries small hooks.
use super::*;
use crate::database::Contact;
use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Timelike, Utc, Weekday};

// ============================================================ helpers

fn notify(summary: &str, body: &str) {
    let (summary, body) = (summary.to_string(), body.to_string());
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .icon("org.omarchy.Mail")
            .show();
    });
}

/// A small modal confirmation with one destructive/primary action.
pub(super) fn confirm(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    body: &str,
    action: &str,
    on_confirm: impl Fn() + 'static,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .default_width(420)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
    for margin in [24] {
        root.set_margin_start(margin);
        root.set_margin_end(margin);
        root.set_margin_top(margin);
        root.set_margin_bottom(margin);
    }
    let heading = gtk::Label::new(Some(title));
    heading.set_xalign(0.0);
    heading.add_css_class("mail-reader-subject");
    root.append(&heading);
    let text = gtk::Label::new(Some(body));
    text.set_xalign(0.0);
    text.set_wrap(true);
    root.append(&text);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label(action);
    ok.add_css_class("mail-accent-button");
    buttons.append(&cancel);
    buttons.append(&ok);
    root.append(&buttons);
    dialog.set_content(Some(&root));
    let weak = dialog.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(dialog) = weak.upgrade() {
            dialog.close();
        }
    });
    let weak = dialog.downgrade();
    ok.connect_clicked(move |_| {
        on_confirm();
        if let Some(dialog) = weak.upgrade() {
            dialog.close();
        }
    });
    dialog.present();
}

fn local_text(at: DateTime<Local>) -> String {
    at.format("%a %-d %b, %H:%M").to_string()
}

// ============================================================ background timers

pub(super) fn start_background_timers(state: &Rc<AppState>) {
    if state.demo_mode {
        return;
    }
    let weak = Rc::downgrade(state);
    glib::timeout_add_local(Duration::from_secs(30), move || {
        let Some(state) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        check_reminders(&state);
        glib::ControlFlow::Continue
    });
    let weak = Rc::downgrade(state);
    glib::timeout_add_local(Duration::from_secs(60), move || {
        let Some(state) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        wake_snoozed(&state);
        glib::ControlFlow::Continue
    });
    let weak = Rc::downgrade(state);
    glib::timeout_add_local_once(Duration::from_secs(5), {
        let weak = weak.clone();
        move || {
            if let Some(state) = weak.upgrade() {
                wake_snoozed(&state);
            }
        }
    });
    glib::timeout_add_local(Duration::from_secs(120), move || {
        let Some(state) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        run_auto_backup_if_due(&state);
        glib::ControlFlow::Continue
    });
}

// ============================================================ reminders

/// Events whose reminder is due at `now`: timed events starting within the
/// lead time (or up to a minute ago). All-day events are skipped.
pub(super) fn due_reminders<'a>(
    events: &'a [PlannerEvent],
    now: NaiveDateTime,
    lead_minutes: u32,
) -> Vec<&'a PlannerEvent> {
    events
        .iter()
        .filter(|event| {
            let all_day = event.starts_at.ends_with(" 00:00") && event.ends_at.ends_with(" 00:00");
            let Ok(starts) = NaiveDateTime::parse_from_str(&event.starts_at, "%Y-%m-%d %H:%M")
            else {
                return false;
            };
            !all_day
                && starts >= now - chrono::Duration::minutes(1)
                && starts <= now + chrono::Duration::minutes(lead_minutes as i64)
        })
        .collect()
}

fn check_reminders(state: &Rc<AppState>) {
    let (enabled, lead, subscriptions) = {
        let prefs = state.preferences.borrow();
        (
            prefs.notifications_enabled,
            prefs.reminder_minutes,
            prefs.remind_subscriptions,
        )
    };
    let Some(lead) = lead.filter(|_| enabled) else {
        return;
    };
    let events = visible_calendar_events(state);
    let now = Local::now().naive_local();
    for event in due_reminders(&events, now, lead) {
        if event.subscription && !subscriptions {
            continue;
        }
        let key = format!(
            "{}|{}|{}|{}",
            event.source, event.id, event.title, event.starts_at
        );
        if !state.database.claim_reminder(&key).unwrap_or(false) {
            continue;
        }
        let starts = NaiveDateTime::parse_from_str(&event.starts_at, "%Y-%m-%d %H:%M").ok();
        let minutes = starts.map(|s| (s - now).num_minutes().max(0)).unwrap_or(0);
        let when = if minutes == 0 {
            "Starting now".to_string()
        } else {
            format!("In {minutes} min · {}", &event.starts_at[11..])
        };
        let body = if event.location.trim().is_empty() {
            when
        } else {
            format!("{when} · {}", event.location.trim())
        };
        notify(&event.title, &body);
    }
}

// ============================================================ snooze

/// The quick snooze choices offered at `now`.
pub(super) fn snooze_choices(now: DateTime<Local>) -> Vec<(&'static str, DateTime<Local>)> {
    let at = |date: NaiveDate, hour: u32| {
        Local
            .from_local_datetime(&date.and_hms_opt(hour, 0, 0).unwrap())
            .earliest()
            .unwrap_or(now)
    };
    let today = now.date_naive();
    let mut saturday = today + chrono::Duration::days(1);
    while saturday.weekday() != Weekday::Sat {
        saturday += chrono::Duration::days(1);
    }
    let mut monday = today + chrono::Duration::days(1);
    while monday.weekday() != Weekday::Mon {
        monday += chrono::Duration::days(1);
    }
    let mut choices = vec![("Later today", now + chrono::Duration::hours(3))];
    if now.hour() >= 20 {
        choices.clear();
    }
    choices.push(("Tomorrow morning", at(today + chrono::Duration::days(1), 8)));
    if !matches!(today.weekday(), Weekday::Fri | Weekday::Sat | Weekday::Sun) {
        choices.push(("This weekend", at(saturday, 9)));
    }
    choices.push(("Next week", at(monday, 8)));
    choices
}

fn snooze(state: &Rc<AppState>, message_id: i64, until: DateTime<Local>) {
    let database = state.database.clone();
    let until_text = until.with_timezone(&Utc).to_rfc3339();
    match database.snooze_message(message_id, &until_text) {
        Ok(()) => {
            if *state.selected_message.borrow() == Some(message_id) {
                state.selected_message.replace(None);
                render_reader(state, None);
            }
            refresh_cached_view(state);
            set_status(state, &format!("Snoozed until {}", local_text(until)));
        }
        Err(error) => set_status(state, &format!("Couldn’t snooze: {error}")),
    }
}

fn snooze_button(state: &Rc<AppState>, message_id: i64) -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    button.set_label("Snooze");
    button.set_has_frame(false);
    button.set_tooltip_text(Some("Hide this message until later"));
    let popover = gtk::Popover::new();
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    for margin in [6] {
        menu.set_margin_top(margin);
        menu.set_margin_bottom(margin);
        menu.set_margin_start(margin);
        menu.set_margin_end(margin);
    }
    for (label, until) in snooze_choices(Local::now()) {
        let item = gtk::Button::with_label(&format!("{label} · {}", local_text(until)));
        item.set_has_frame(false);
        let state = state.clone();
        let popover = popover.clone();
        item.connect_clicked(move |_| {
            popover.popdown();
            snooze(&state, message_id, until);
        });
        menu.append(&item);
    }
    let custom = gtk::Button::with_label("Pick date and time…");
    custom.set_has_frame(false);
    {
        let state = state.clone();
        let popover = popover.clone();
        custom.connect_clicked(move |_| {
            popover.popdown();
            let state_for_pick = state.clone();
            pick_date_time(&state.window, "Snooze until", "Snooze", move |until| {
                snooze(&state_for_pick, message_id, until)
            });
        });
    }
    menu.append(&custom);
    popover.set_child(Some(&menu));
    button.set_popover(Some(&popover));
    button
}

fn wake_snoozed(state: &Rc<AppState>) {
    let database = state.database.clone();
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let woke = database
            .wake_snoozed(&Utc::now().to_rfc3339())
            .unwrap_or_default();
        for (id, account_id, folder, _, _) in &woke {
            let _ = database.set_unread(*id, true);
            if account_id.is_some() {
                let _ = database.queue_action(
                    *account_id,
                    Some(*id),
                    "read",
                    &serde_json::json!({ "value": true, "folder": folder }).to_string(),
                );
            }
        }
        let _ = sender.send_blocking(woke);
    });
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let Ok(woke) = receiver.recv().await else {
            return;
        };
        if woke.is_empty() {
            return;
        }
        refresh_cached_view(&state);
        set_status(
            &state,
            &format!("{} snoozed message(s) are back", woke.len()),
        );
        if state.preferences.borrow().notifications_enabled {
            let (_, _, _, subject, sender) = &woke[0];
            notify(
                if sender.trim().is_empty() {
                    "Snoozed message"
                } else {
                    sender
                },
                subject,
            );
        }
    });
}

/// Date + time chooser used by Snooze and Send later.
fn pick_date_time(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    action: &str,
    on_pick: impl Fn(DateTime<Local>) + 'static,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .default_width(420)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    for margin in [22] {
        root.set_margin_start(margin);
        root.set_margin_end(margin);
        root.set_margin_top(margin);
        root.set_margin_bottom(margin);
    }
    let tomorrow = Local::now().date_naive() + chrono::Duration::days(1);
    let (date_row, calendar, _) =
        planner_date_picker(title, false, "day-month-24-hour", Some(tomorrow));
    let (time_row, hour, minute) = planner_time_picker("Time", "08:00");
    root.append(&date_row);
    root.append(&time_row);
    let error = gtk::Label::new(None);
    error.add_css_class("mail-danger");
    error.set_xalign(0.0);
    root.append(&error);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label(action);
    ok.add_css_class("mail-accent-button");
    buttons.append(&cancel);
    buttons.append(&ok);
    root.append(&buttons);
    dialog.set_content(Some(&root));
    let weak = dialog.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(dialog) = weak.upgrade() {
            dialog.close();
        }
    });
    let weak = dialog.downgrade();
    ok.connect_clicked(move |_| {
        let Some((h, m)) = parse_planner_time(&hour, &minute) else {
            error.set_text("Enter a time as HH:MM.");
            return;
        };
        let date = calendar.date();
        let Some(naive) =
            NaiveDate::from_ymd_opt(date.year(), date.month() as u32, date.day_of_month() as u32)
                .and_then(|d| d.and_hms_opt(h as u32, m as u32, 0))
        else {
            return;
        };
        let Some(at) = Local.from_local_datetime(&naive).earliest() else {
            return;
        };
        if at <= Local::now() {
            error.set_text("Choose a time in the future.");
            return;
        }
        on_pick(at);
        if let Some(dialog) = weak.upgrade() {
            dialog.close();
        }
    });
    dialog.present();
}

// ============================================================ reader tools

/// Adds Snooze, Unsubscribe, snooze status and invitation handling for a
/// received message.
pub(super) fn append_message_tools(
    actions: &gtk::Box,
    content: &gtk::Box,
    state: &Rc<AppState>,
    message: &Message,
) {
    if state.demo_mode || message.id < 0 {
        return;
    }
    if let Ok(Some(until)) = state.database.snoozed_until(message.id) {
        let when = DateTime::parse_from_rfc3339(&until)
            .map(|at| local_text(at.with_timezone(&Local)))
            .unwrap_or(until);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_top(14);
        let label = gtk::Label::new(Some(&format!("Snoozed until {when}")));
        label.add_css_class("mail-status");
        row.append(&label);
        let wake = gtk::Button::with_label("Unsnooze");
        let state = state.clone();
        let id = message.id;
        wake.connect_clicked(move |_| {
            let _ = state.database.unsnooze_message(id);
            refresh_cached_view(&state);
            set_status(&state, "Message returned to its folder");
        });
        row.append(&wake);
        content.append(&row);
    } else {
        actions.append(&snooze_button(state, message.id));
    }
    if let Some(header) = message.list_unsubscribe.clone() {
        let unsubscribe = gtk::Button::with_label("Unsubscribe");
        unsubscribe.set_has_frame(false);
        unsubscribe.set_tooltip_text(Some("Stop receiving this mailing list"));
        let state = state.clone();
        let message = message.clone();
        unsubscribe.connect_clicked(move |_| start_unsubscribe(&state, &message, &header));
        actions.append(&unsubscribe);
    }
    append_invitation(content, state, message);
}

// ============================================================ unsubscribe

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct UnsubscribeTargets {
    pub https: Option<String>,
    pub mailto: Option<String>,
}

/// Splits a List-Unsubscribe header into its HTTPS and mailto targets.
pub(super) fn parse_unsubscribe(header: &str) -> UnsubscribeTargets {
    let mut targets = UnsubscribeTargets::default();
    for part in header.split(',') {
        let value = part
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .trim();
        let lower = value.to_ascii_lowercase();
        if lower.starts_with("https://") && targets.https.is_none() {
            targets.https = Some(value.to_string());
        } else if lower.starts_with("mailto:") && targets.mailto.is_none() {
            targets.mailto = Some(value.to_string());
        }
    }
    targets
}

/// (address, subject, body) from a mailto: URI.
pub(super) fn parse_mailto(uri: &str) -> Option<(String, String, String)> {
    let url = url::Url::parse(uri).ok()?;
    let address = url.path().to_string();
    if !address.contains('@') {
        return None;
    }
    let mut subject = "unsubscribe".to_string();
    let mut body = "unsubscribe".to_string();
    for (key, value) in url.query_pairs() {
        match key.to_ascii_lowercase().as_str() {
            "subject" => subject = value.into_owned(),
            "body" => body = value.into_owned(),
            _ => {}
        }
    }
    Some((address, subject, body))
}

fn start_unsubscribe(state: &Rc<AppState>, message: &Message, header: &str) {
    let targets = parse_unsubscribe(header);
    let sender = if message.sender_name.trim().is_empty() {
        message.sender_email.clone()
    } else {
        message.sender_name.clone()
    };
    let (how, run): (String, Box<dyn Fn()>) = if let (true, Some(url)) =
        (message.list_unsubscribe_one_click, targets.https.clone())
    {
        let state = state.clone();
        (
            "Omarchy Mail will send a one-click unsubscribe request to the sender.".into(),
            Box::new(move || {
                let url = url.clone();
                let (tx, rx) = async_channel::bounded(1);
                std::thread::spawn(move || {
                    let result = mail::oauth::http_post_form(&url, "List-Unsubscribe=One-Click");
                    let _ = tx.send_blocking(result.map(|_| ()).map_err(|e| e.to_string()));
                });
                let state = state.clone();
                glib::MainContext::default().spawn_local(async move {
                    match rx.recv().await {
                        Ok(Ok(())) => set_status(&state, "Unsubscribed"),
                        Ok(Err(error)) => {
                            set_status(&state, &format!("Unsubscribe failed: {error}"))
                        }
                        Err(_) => {}
                    }
                });
            }),
        )
    } else if let (Some(mailto), Some(account_id)) = (targets.mailto.clone(), message.account_id) {
        let state = state.clone();
        (
            "Omarchy Mail will send an unsubscribe email to the list.".into(),
            Box::new(move || {
                let Some((address, subject, body)) = parse_mailto(&mailto) else {
                    set_status(&state, "This unsubscribe address is not valid");
                    return;
                };
                let Some(account) = state
                    .accounts
                    .borrow()
                    .iter()
                    .find(|a| a.id == Some(account_id))
                    .cloned()
                else {
                    return;
                };
                let database = state.database.clone();
                std::thread::spawn(move || {
                    if mail::outbox::queue_send_at(
                        &database,
                        account_id,
                        &address,
                        &[],
                        &[],
                        &subject,
                        &body,
                        None,
                        &[],
                        Utc::now(),
                    )
                    .is_ok()
                    {
                        mail::sync::send_due_now(&account, &database);
                    }
                });
                set_status(&state, "Unsubscribe email sent");
            }),
        )
    } else if let Some(url) = targets.https.clone() {
        (
            "The sender’s unsubscribe page will open in your browser.".into(),
            Box::new(move || {
                let _ = gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>);
            }),
        )
    } else {
        set_status(state, "This message has no usable unsubscribe link");
        return;
    };
    confirm(
        &state.window,
        "Unsubscribe?",
        &format!("Stop receiving mail from {sender}? {how}"),
        "Unsubscribe",
        run,
    );
}

// ============================================================ invitations

#[derive(Debug, Clone, serde::Deserialize)]
pub(super) struct Invite {
    pub method: String,
    pub uid: String,
    pub title: String,
    pub notes: String,
    pub location: String,
    pub starts_at: String,
    pub ends_at: String,
    pub organizer_email: String,
    pub organizer_name: String,
    #[serde(default)]
    pub cancelled: bool,
}

fn invitation_file(message: &Message) -> Option<String> {
    message.attachments.iter().find_map(|attachment| {
        let kind = attachment.content_type.to_ascii_lowercase();
        let is_calendar = kind == "text/calendar"
            || kind == "application/ics"
            || attachment.filename.to_ascii_lowercase().ends_with(".ics");
        if !is_calendar || attachment.size > 1_000_000 {
            return None;
        }
        std::fs::read_to_string(&attachment.cache_path).ok()
    })
}

fn find_invite_event(state: &AppState, invite: &Invite) -> Option<CalendarEvent> {
    state
        .database
        .list_calendar_events()
        .ok()?
        .into_iter()
        .find(|event| event.title == invite.title && event.starts_at == invite.starts_at)
}

fn append_invitation(content: &gtk::Box, state: &Rc<AppState>, message: &Message) {
    let Some(ics) = invitation_file(message) else {
        return;
    };
    let banner = gtk::Box::new(gtk::Orientation::Vertical, 8);
    banner.add_css_class("mail-settings-account-card");
    banner.set_margin_top(16);
    banner.set_visible(false);
    content.append(&banner);
    let (tx, rx) = async_channel::bounded(1);
    let ics_for_worker = ics.clone();
    std::thread::spawn(move || {
        let parsed =
            crate::icloud::worker(serde_json::json!({"action": "invite", "ics": ics_for_worker}))
                .and_then(|value| {
                    serde_json::from_value::<Invite>(value).map_err(|e| e.to_string())
                });
        let _ = tx.send_blocking(parsed);
    });
    let state = state.clone();
    let message = message.clone();
    glib::MainContext::default().spawn_local(async move {
        let Ok(Ok(invite)) = rx.recv().await else {
            return;
        };
        if banner.root().is_none() {
            return;
        }
        render_invitation(&banner, &state, &message, &invite, &ics);
        banner.set_visible(true);
    });
}

fn render_invitation(
    banner: &gtk::Box,
    state: &Rc<AppState>,
    message: &Message,
    invite: &Invite,
    ics: &str,
) {
    clear(banner);
    let format = state.preferences.borrow().date_time_format.clone();
    let heading = gtk::Label::new(Some(&format!(
        "{} {}",
        if invite.cancelled {
            "Cancelled:"
        } else {
            "Invitation:"
        },
        invite.title
    )));
    heading.set_xalign(0.0);
    heading.set_wrap(true);
    heading.add_css_class("mail-reader-meta");
    banner.append(&heading);
    let mut lines = vec![format!(
        "{} – {}",
        format_planner_datetime(&invite.starts_at, &format),
        format_planner_datetime(&invite.ends_at, &format)
    )];
    if !invite.location.trim().is_empty() {
        lines.push(invite.location.clone());
    }
    if !invite.organizer_email.is_empty() {
        lines.push(format!(
            "Organiser: {}",
            if invite.organizer_name.is_empty() {
                invite.organizer_email.clone()
            } else {
                format!("{} <{}>", invite.organizer_name, invite.organizer_email)
            }
        ));
    }
    let details = gtk::Label::new(Some(&lines.join("\n")));
    details.set_xalign(0.0);
    details.set_wrap(true);
    banner.append(&details);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let existing = find_invite_event(state, invite);
    if invite.cancelled {
        if let Some(event) = existing {
            let remove = gtk::Button::with_label("Remove from calendar");
            let state = state.clone();
            remove.connect_clicked(move |button| {
                let _ = state.database.delete_calendar_event(event.id);
                start_calendar_sync(state.clone());
                button.set_sensitive(false);
                set_status(&state, "Event removed from your calendar");
            });
            buttons.append(&remove);
        }
    } else {
        let status = gtk::Label::new(Some(if existing.is_some() {
            "In your calendar"
        } else {
            ""
        }));
        status.add_css_class("mail-reader-meta");
        if invite.method == "REQUEST" && !invite.organizer_email.is_empty() {
            for (label, partstat) in [
                ("Accept", "ACCEPTED"),
                ("Maybe", "TENTATIVE"),
                ("Decline", "DECLINED"),
            ] {
                let button = gtk::Button::with_label(label);
                if partstat == "ACCEPTED" {
                    button.add_css_class("mail-accent-button");
                }
                let state = state.clone();
                let message = message.clone();
                let invite = invite.clone();
                let ics = ics.to_string();
                let status = status.clone();
                button.connect_clicked(move |_| {
                    respond_to_invite(&state, &message, &invite, &ics, partstat, &status)
                });
                buttons.append(&button);
            }
        } else if existing.is_none() {
            let add = gtk::Button::with_label("Add to calendar");
            let state = state.clone();
            let message = message.clone();
            let invite = invite.clone();
            let status = status.clone();
            add.connect_clicked(move |_| {
                add_invite_to_calendar(&state, &message, &invite);
                status.set_text("Added to your calendar");
            });
            buttons.append(&add);
        }
        buttons.append(&status);
    }
    banner.append(&buttons);
}

fn add_invite_to_calendar(state: &Rc<AppState>, message: &Message, invite: &Invite) {
    if find_invite_event(state, invite).is_some() {
        return;
    }
    let event = CalendarEvent {
        id: 0,
        title: invite.title.clone(),
        notes: invite.notes.clone(),
        starts_at: invite.starts_at.clone(),
        ends_at: invite.ends_at.clone(),
        location: invite.location.clone(),
        message_id: Some(message.id),
    };
    if state.database.create_calendar_event(&event).is_ok() {
        start_calendar_sync(state.clone());
    }
}

fn respond_to_invite(
    state: &Rc<AppState>,
    message: &Message,
    invite: &Invite,
    ics: &str,
    partstat: &'static str,
    status: &gtk::Label,
) {
    let Some(account) = message.account_id.and_then(|id| {
        state
            .accounts
            .borrow()
            .iter()
            .find(|a| a.id == Some(id))
            .cloned()
    }) else {
        status.set_text("This message is not linked to an email account");
        return;
    };
    if partstat == "DECLINED" {
        if let Some(event) = find_invite_event(state, invite) {
            let _ = state.database.delete_calendar_event(event.id);
        }
    } else {
        add_invite_to_calendar(state, message, invite);
    }
    let verb = match partstat {
        "ACCEPTED" => "Accepted",
        "TENTATIVE" => "Tentatively accepted",
        _ => "Declined",
    };
    status.set_text(&format!("{verb} · sending reply…"));
    let database = state.database.clone();
    let ics = ics.to_string();
    let invite = invite.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = (|| {
            let reply = crate::icloud::worker(serde_json::json!({
                "action": "invite_reply", "ics": ics, "email": account.email,
                "name": account.display_name, "partstat": partstat,
            }))?;
            let text = reply["ics"].as_str().ok_or("Invalid reply")?.to_string();
            let directory =
                std::env::temp_dir().join(format!("omarchy-mail-invite-{}", std::process::id()));
            crate::security::ensure_private_dir(&directory).map_err(|e| e.to_string())?;
            let path = directory.join("invite-reply.ics");
            std::fs::write(&path, text).map_err(|e| e.to_string())?;
            let name = if account.display_name.trim().is_empty() {
                account.email.clone()
            } else {
                account.display_name.clone()
            };
            let body = format!(
                "{name} has {} the invitation: {}",
                verb.to_lowercase(),
                invite.title
            );
            let account_id = account.id.ok_or("Account is not ready")?;
            let queued = mail::outbox::queue_send_at(
                &database,
                account_id,
                &invite.organizer_email,
                &[],
                &[],
                &format!("{verb}: {}", invite.title),
                &body,
                None,
                &[path],
                Utc::now(),
            );
            let _ = std::fs::remove_dir_all(&directory);
            queued.map_err(|e| e.to_string())?;
            let report = mail::sync::send_due_now(&account, &database);
            Ok::<_, String>(report.error)
        })();
        let _ = tx.send_blocking(result);
    });
    let state = state.clone();
    let status = status.clone();
    glib::MainContext::default().spawn_local(async move {
        match rx.recv().await {
            Ok(Ok(None)) => status.set_text(&format!("{verb} · reply sent")),
            Ok(Ok(Some(_))) => status.set_text(&format!("{verb} · reply waiting in Outbox")),
            Ok(Err(error)) => {
                status.set_text(&format!("{verb} · couldn’t send the reply: {error}"))
            }
            Err(_) => {}
        }
        refresh_cached_view(&state);
    });
}

// ============================================================ address suggestions

/// Contacts matching the token being typed, best first.
pub(super) fn match_contacts<'a>(
    contacts: &'a [Contact],
    token: &str,
    limit: usize,
) -> Vec<&'a Contact> {
    let token = token.trim().to_lowercase();
    if token.chars().count() < 2 {
        return Vec::new();
    }
    let mut prefix = Vec::new();
    let mut inner = Vec::new();
    for contact in contacts {
        let email = contact.email.to_lowercase();
        let name = contact.name.to_lowercase();
        if email.starts_with(&token) || name.split_whitespace().any(|word| word.starts_with(&token))
        {
            prefix.push(contact);
        } else if email.contains(&token) || name.contains(&token) {
            inner.push(contact);
        }
    }
    prefix.extend(inner);
    prefix.truncate(limit);
    prefix
}

/// Splits recipient text into (already complete part, token being typed).
pub(super) fn split_recipient_token(text: &str) -> (&str, &str) {
    match text.rfind([',', ';']) {
        Some(index) => (&text[..=index], &text[index + 1..]),
        None => ("", text),
    }
}

fn format_contact(contact: &Contact) -> String {
    if contact.name.is_empty() {
        contact.email.clone()
    } else if contact.name.contains([',', ';', '"', '<', '>']) {
        format!("\"{}\" <{}>", contact.name.replace('"', ""), contact.email)
    } else {
        format!("{} <{}>", contact.name, contact.email)
    }
}

/// Loads the address book once per composer, shared by its recipient fields.
pub(super) fn load_contacts(state: &Rc<AppState>) -> Rc<RefCell<Vec<Contact>>> {
    let contacts = Rc::new(RefCell::new(Vec::new()));
    if state.demo_mode {
        return contacts;
    }
    let database = state.database.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(database.contacts().unwrap_or_default());
    });
    let target = contacts.clone();
    glib::MainContext::default().spawn_local(async move {
        if let Ok(list) = rx.recv().await {
            *target.borrow_mut() = list;
        }
    });
    contacts
}

pub(super) fn attach_address_completion(entry: &gtk::Entry, contacts: Rc<RefCell<Vec<Contact>>>) {
    let popover = gtk::Popover::new();
    popover.set_autohide(false);
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);
    popover.set_can_focus(false);
    popover.set_parent(entry);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    popover.set_child(Some(&list));
    let matches: Rc<RefCell<Vec<Contact>>> = Rc::new(RefCell::new(Vec::new()));
    let suppress = Rc::new(Cell::new(false));

    let accept: Rc<dyn Fn(usize)> = {
        let entry = entry.clone();
        let matches = matches.clone();
        let popover = popover.clone();
        let suppress = suppress.clone();
        Rc::new(move |index| {
            let Some(contact) = matches.borrow().get(index).cloned() else {
                return;
            };
            let text = entry.text().to_string();
            let (done, _) = split_recipient_token(&text);
            let separator = if done.is_empty() { "" } else { " " };
            suppress.set(true);
            entry.set_text(&format!("{done}{separator}{}, ", format_contact(&contact)));
            entry.set_position(-1);
            suppress.set(false);
            popover.popdown();
            entry.grab_focus_without_selecting();
        })
    };
    {
        let accept = accept.clone();
        list.connect_row_activated(move |_, row| accept(row.index().max(0) as usize));
    }
    {
        let popover = popover.clone();
        let list = list.clone();
        let matches = matches.clone();
        entry.connect_changed(move |entry| {
            if suppress.get() {
                return;
            }
            let text = entry.text().to_string();
            let (_, token) = split_recipient_token(&text);
            let found = match_contacts(&contacts.borrow(), token, 6)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            clear(&list);
            if found.is_empty() || !entry.state_flags().contains(gtk::StateFlags::FOCUS_WITHIN) {
                popover.popdown();
                matches.borrow_mut().clear();
                return;
            }
            for contact in &found {
                let label = gtk::Label::new(Some(&format_contact(contact)));
                label.set_xalign(0.0);
                label.set_margin_start(6);
                label.set_margin_end(6);
                list.append(&label);
            }
            list.select_row(list.row_at_index(0).as_ref());
            *matches.borrow_mut() = found;
            popover.popup();
        });
    }
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let popover = popover.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if !popover.is_visible() {
                return glib::Propagation::Proceed;
            }
            let selected = list.selected_row().map(|row| row.index()).unwrap_or(0);
            let count = matches.borrow().len() as i32;
            match key {
                gdk::Key::Down => {
                    list.select_row(list.row_at_index((selected + 1).min(count - 1)).as_ref());
                    glib::Propagation::Stop
                }
                gdk::Key::Up => {
                    list.select_row(list.row_at_index((selected - 1).max(0)).as_ref());
                    glib::Propagation::Stop
                }
                gdk::Key::Tab | gdk::Key::Return | gdk::Key::KP_Enter => {
                    accept(selected.max(0) as usize);
                    glib::Propagation::Stop
                }
                gdk::Key::Escape => {
                    popover.popdown();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
    }
    entry.add_controller(keys);
    let focus = gtk::EventControllerFocus::new();
    {
        let popover = popover.clone();
        focus.connect_leave(move |_| popover.popdown());
    }
    entry.add_controller(focus);
    entry.connect_destroy(move |_| popover.unparent());
}

// ============================================================ server search

pub(super) fn search_on_server(state: &Rc<AppState>) {
    let query = state.search_entry.text().trim().to_string();
    if query.is_empty() || state.demo_mode {
        return;
    }
    let folder_for = |folder: &str| match folder {
        "Starred" | "Outbox" | "Snoozed" => "Inbox".to_string(),
        other => other.to_string(),
    };
    let targets = match &*state.scope.borrow() {
        MailScope::Unified(folder) => state
            .accounts
            .borrow()
            .iter()
            .filter(|account| account.enabled)
            .map(|account| (account.clone(), folder_for(folder)))
            .collect::<Vec<_>>(),
        MailScope::Account { id, folder } => state
            .accounts
            .borrow()
            .iter()
            .filter(|account| account.id == Some(*id))
            .map(|account| (account.clone(), folder_for(folder)))
            .collect(),
    };
    let folders = state.folders.borrow().clone();
    let database = state.database.clone();
    state.server_search.set_sensitive(false);
    set_status(state, &format!("Searching the server for “{query}”…"));
    let (tx, rx) = async_channel::bounded(1);
    let query_for_worker = query.clone();
    std::thread::spawn(move || {
        let mut found = 0;
        let mut errors = Vec::new();
        for (account, local) in targets {
            let Some(remote) = mail::imap::remote_name_for(&folders, account.id, &local) else {
                continue;
            };
            let result = mail::credentials::load_auth_material(
                &account.email,
                "imap",
                &account.incoming.auth,
            )
            .map_err(|error| {
                mail::credentials::friendly_load_error("IMAP", &account.incoming.auth, &error)
            })
            .and_then(|auth| {
                mail::imap::search_folder(&account, &auth, &remote, &local, &query_for_worker, 50)
                    .map_err(|e| e.to_string())
            })
            .and_then(|messages| {
                found += messages.len();
                database
                    .upsert_messages(&messages)
                    .map_err(|e| e.to_string())
            });
            if let Err(error) = result {
                errors.push(format!("{}: {error}", account.email));
            }
        }
        let _ = tx.send_blocking((found, errors));
    });
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let Ok((found, errors)) = rx.recv().await else {
            return;
        };
        state.server_search.set_sensitive(true);
        start_message_search(&state, &query);
        let mut text = format!("Server search found {found} message(s) for “{query}”");
        if !errors.is_empty() {
            text.push_str(&format!(" · {}", errors.join("; ")));
        }
        set_status(&state, &text);
    });
}

// ============================================================ undo send / send later

pub(super) struct Composed {
    pub account: Account,
    pub to: String,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub body_html: Option<String>,
    pub attachments: Vec<PathBuf>,
}

/// Queues a composed message for `send_at`. With `undo_seconds` set, the
/// header Undo button can still pull it back before it leaves.
pub(super) fn queue_composed(
    state: Rc<AppState>,
    window: adw::Window,
    button: gtk::Button,
    status: gtk::Label,
    draft_id: Rc<Cell<Option<i64>>>,
    composed: Composed,
    send_at: DateTime<Utc>,
    undo_seconds: Option<u32>,
) {
    let Some(account_id) = composed.account.id else {
        status.set_text("This account is not ready to send mail yet.");
        return;
    };
    button.set_sensitive(false);
    status.set_text(if undo_seconds.is_some() {
        "Sending…"
    } else {
        "Scheduling…"
    });
    let database = state.database.clone();
    let (tx, rx) = async_channel::bounded(1);
    let account = composed.account.clone();
    std::thread::spawn(move || {
        let result = mail::outbox::queue_send_at(
            &database,
            account_id,
            &composed.to,
            &composed.cc,
            &composed.bcc,
            &composed.subject,
            &composed.body,
            composed.body_html.as_deref(),
            &composed.attachments,
            send_at,
        )
        .map_err(|e| e.to_string());
        let _ = tx.send_blocking(result);
    });
    glib::MainContext::default().spawn_local(async move {
        match rx.recv().await {
            Ok(Ok(send_id)) => {
                if let Some(draft_id) = draft_id.get() {
                    let database = state.database.clone();
                    std::thread::spawn(move || {
                        let _ = database.delete_message(draft_id);
                        mail::outbox::remove_draft_files(draft_id);
                    });
                }
                window.close();
                match undo_seconds {
                    Some(seconds) => arm_undo_send(&state, send_id, account, seconds),
                    None => {
                        refresh_cached_view(&state);
                        set_status(
                            &state,
                            &format!(
                                "Scheduled for {} · it is in Outbox until then",
                                local_text(send_at.with_timezone(&Local))
                            ),
                        );
                    }
                }
            }
            Ok(Err(error)) => {
                button.set_sensitive(true);
                status.set_text(&format!("Couldn’t queue this message: {error}"));
            }
            Err(_) => button.set_sensitive(true),
        }
    });
}

fn arm_undo_send(state: &Rc<AppState>, send_id: i64, account: Account, seconds: u32) {
    // A newer undoable action replaces an older one; the message still sends.
    state.undo_action.replace(None);
    let generation = state.undo_send_generation.get().wrapping_add(1);
    state.undo_send_generation.set(generation);
    state.undo_send.replace(Some((send_id, account.clone())));
    state.undo_button.set_visible(true);
    set_status(state, &format!("Sending in {seconds} seconds…"));
    let state = state.clone();
    glib::timeout_add_local_once(Duration::from_secs(seconds as u64), move || {
        if state.undo_send_generation.get() == generation {
            state.undo_send.replace(None);
            if state.undo_action.borrow().is_none() {
                state.undo_button.set_visible(false);
            }
        }
        let database = state.database.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(mail::sync::send_due_now(&account, &database));
        });
        glib::MainContext::default().spawn_local(async move {
            if let Ok(report) = rx.recv().await {
                refresh_cached_view(&state);
                match report.error {
                    Some(error) => {
                        set_status(&state, &format!("Message waiting in Outbox: {error}"))
                    }
                    None if report.sent > 0 => set_status(&state, "Message sent"),
                    None => {}
                }
            }
        });
    });
}

/// Pulls a queued message back into Drafts and reopens it. Returns false
/// when there was no pending send to undo.
pub(super) fn undo_send(state: &Rc<AppState>) -> bool {
    let Some((send_id, _)) = state.undo_send.borrow_mut().take() else {
        return false;
    };
    state
        .undo_send_generation
        .set(state.undo_send_generation.get().wrapping_add(1));
    state.undo_button.set_visible(false);
    let database = state.database.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = (|| {
            let Some(send) = mail::sync::cancel_pending_send(&database, send_id)? else {
                return Ok(None);
            };
            let recipients =
                compose_recipient_summary(&send.to, &send.cc.join(", "), &send.bcc.join(", "));
            let id = database
                .save_draft_with_html(
                    Some(send.account_id),
                    &recipients,
                    &send.subject,
                    &send.body,
                    send.body_html.as_deref(),
                )
                .map_err(|e| e.to_string())?;
            let paths = send
                .attachments
                .iter()
                .map(|a| PathBuf::from(&a.path))
                .collect::<Vec<_>>();
            let staged =
                mail::outbox::stage_draft_attachments(id, &paths).map_err(|e| e.to_string())?;
            let metadata = staged
                .iter()
                .map(|attachment| AttachmentInfo {
                    filename: attachment.filename.clone(),
                    content_type: "application/octet-stream".into(),
                    size: std::fs::metadata(&attachment.path)
                        .map(|m| m.len())
                        .unwrap_or_default(),
                    cache_path: attachment.path.clone(),
                    content_id: None,
                })
                .collect::<Vec<_>>();
            let id = database
                .update_draft_with_html_and_attachments(
                    id,
                    Some(send.account_id),
                    &recipients,
                    &send.subject,
                    &send.body,
                    send.body_html.as_deref(),
                    &metadata,
                )
                .map_err(|e| e.to_string())?;
            mail::outbox::remove_staged_files(&send);
            let draft = database
                .list_messages(Some(send.account_id), "Drafts")
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|message| message.id == id);
            Ok::<_, String>(draft)
        })();
        let _ = tx.send_blocking(result);
    });
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        match rx.recv().await {
            Ok(Ok(Some(draft))) => {
                refresh_cached_view(&state);
                set_status(&state, "Sending undone · the message is back in Drafts");
                open_compose_with_context(state.clone(), Some(ComposeContext::Draft(draft)));
            }
            Ok(Ok(None)) => set_status(&state, "Too late to undo · the message was already sent"),
            Ok(Err(error)) => set_status(&state, &format!("Couldn’t undo sending: {error}")),
            Err(_) => {}
        }
    });
    true
}

pub(super) fn pick_send_later(window: &adw::Window, on_pick: impl Fn(DateTime<Local>) + 'static) {
    let popover_choices = snooze_choices(Local::now());
    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title("Send later")
        .default_width(360)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    for margin in [20] {
        root.set_margin_start(margin);
        root.set_margin_end(margin);
        root.set_margin_top(margin);
        root.set_margin_bottom(margin);
    }
    let heading = gtk::Label::new(Some("Send later"));
    heading.set_xalign(0.0);
    heading.add_css_class("mail-reader-subject");
    root.append(&heading);
    let on_pick = Rc::new(on_pick);
    for (label, at) in popover_choices
        .into_iter()
        .filter(|(label, _)| *label != "Later today")
    {
        let button = gtk::Button::with_label(&format!("{label} · {}", local_text(at)));
        let on_pick = on_pick.clone();
        let weak = dialog.downgrade();
        button.connect_clicked(move |_| {
            on_pick(at);
            if let Some(dialog) = weak.upgrade() {
                dialog.close();
            }
        });
        root.append(&button);
    }
    let custom = gtk::Button::with_label("Pick date and time…");
    {
        let on_pick = on_pick.clone();
        let weak = dialog.downgrade();
        let parent = window.clone();
        custom.connect_clicked(move |_| {
            if let Some(dialog) = weak.upgrade() {
                dialog.close();
            }
            let on_pick = on_pick.clone();
            pick_date_time(&parent, "Send on", "Schedule", move |at| on_pick(at));
        });
    }
    root.append(&custom);
    dialog.set_content(Some(&root));
    dialog.present();
}

// ============================================================ calendar views

#[derive(Clone, Copy, PartialEq, Eq)]
enum CalendarView {
    Month,
    Week,
    Agenda,
}

fn week_start(date: NaiveDate) -> NaiveDate {
    date - chrono::Duration::days(date.weekday().num_days_from_sunday() as i64)
}

fn shift(view: CalendarView, anchor: NaiveDate, forward: bool) -> NaiveDate {
    let sign = if forward { 1 } else { -1 };
    match view {
        CalendarView::Month => {
            let months = anchor.year() * 12 + anchor.month0() as i32 + sign;
            NaiveDate::from_ymd_opt(months.div_euclid(12), months.rem_euclid(12) as u32 + 1, 1)
                .unwrap_or(anchor)
        }
        CalendarView::Week => anchor + chrono::Duration::days(7 * sign as i64),
        CalendarView::Agenda => anchor + chrono::Duration::days(30 * sign as i64),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_view(
    container: &gtk::Box,
    label: &gtk::Label,
    view: CalendarView,
    anchor: NaiveDate,
    events: &[PlannerEvent],
    format: &str,
    state: Rc<AppState>,
    planner: gtk::Window,
) {
    match view {
        CalendarView::Month => render_calendar_month(
            container,
            label,
            anchor.year() * 12 + anchor.month0() as i32,
            events,
            format,
            state,
            planner,
        ),
        CalendarView::Week => render_week(
            container,
            label,
            week_start(anchor),
            events,
            format,
            state,
            planner,
        ),
        CalendarView::Agenda => {
            render_agenda(container, label, anchor, events, format, state, planner)
        }
    }
}

fn event_button(
    event: &PlannerEvent,
    text: &str,
    state: &Rc<AppState>,
    planner: &gtk::Window,
    format: &str,
) -> gtk::Button {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_lines(3);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_hexpand(true);
    label.add_css_class(if event.subscription {
        "planner-subscription"
    } else {
        "planner-personal"
    });
    let button = gtk::Button::new();
    button.set_has_frame(false);
    button.set_hexpand(true);
    button.set_halign(gtk::Align::Fill);
    button.add_css_class("planner-event");
    button.set_child(Some(&label));
    button.set_tooltip_text(Some(&format!("{}\n{}", event.title, event.source)));
    let event = event.clone();
    let state = state.clone();
    let planner = planner.clone();
    let format = format.to_string();
    button.connect_clicked(move |_| open_event_details(&state, &planner, &event, &format));
    button
}

fn event_time_text(event: &PlannerEvent) -> String {
    if event.starts_at.ends_with(" 00:00") && event.ends_at.ends_with(" 00:00") {
        "All day".into()
    } else {
        event.starts_at.get(11..16).unwrap_or("").to_string()
    }
}

fn render_week(
    container: &gtk::Box,
    label: &gtk::Label,
    start: NaiveDate,
    events: &[PlannerEvent],
    format: &str,
    state: Rc<AppState>,
    planner: gtk::Window,
) {
    clear(container);
    let end = start + chrono::Duration::days(6);
    label.set_text(&format!(
        "{} – {}",
        start.format("%-d %b"),
        end.format("%-d %b %Y")
    ));
    let grid = gtk::Grid::new();
    grid.set_column_homogeneous(true);
    grid.set_column_spacing(4);
    grid.set_vexpand(true);
    let today = Local::now().date_naive();
    for offset in 0..7 {
        let date = start + chrono::Duration::days(offset);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
        column.add_css_class("mail-message-row");
        column.set_vexpand(true);
        let heading = gtk::Label::new(Some(&date.format("%a %-d").to_string()));
        heading.add_css_class("mail-section-label");
        if date == today {
            heading.add_css_class("mail-accent-button");
        }
        column.append(&heading);
        for event in events
            .iter()
            .filter(|event| event_date(event) == Some(date))
        {
            column.append(&event_button(
                event,
                &format!("{} {}", event_time_text(event), event.title),
                &state,
                &planner,
                format,
            ));
        }
        let click = gtk::GestureClick::new();
        let state = state.clone();
        let planner = planner.clone();
        click.connect_pressed(move |gesture, presses, x, y| {
            let mut target = gesture
                .widget()
                .and_then(|w| w.pick(x, y, gtk::PickFlags::DEFAULT));
            while let Some(widget) = target {
                if widget.has_css_class("planner-event") {
                    return;
                }
                target = widget.parent();
            }
            if presses == 2 {
                open_calendar_event_dialog(
                    state.clone(),
                    planner.clone(),
                    None,
                    true,
                    Some(date),
                    None,
                    None,
                );
            }
        });
        column.add_controller(click);
        grid.attach(&column, offset as i32, 0, 1, 1);
    }
    container.append(&grid);
}

fn render_agenda(
    container: &gtk::Box,
    label: &gtk::Label,
    start: NaiveDate,
    events: &[PlannerEvent],
    format: &str,
    state: Rc<AppState>,
    planner: gtk::Window,
) {
    clear(container);
    let end = start + chrono::Duration::days(30);
    label.set_text(&format!(
        "{} – {}",
        start.format("%-d %b"),
        end.format("%-d %b %Y")
    ));
    let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let mut current: Option<NaiveDate> = None;
    for event in events
        .iter()
        .filter(|event| event_date(event).is_some_and(|date| date >= start && date < end))
    {
        let date = event_date(event).unwrap();
        if current != Some(date) {
            current = Some(date);
            let heading = gtk::Label::new(Some(&date.format("%A %-d %B").to_string()));
            heading.set_xalign(0.0);
            heading.set_margin_top(10);
            heading.add_css_class("mail-section-label");
            list.append(&heading);
        }
        let mut text = format!("{}   {}", event_time_text(event), event.title);
        if !event.location.trim().is_empty() {
            text.push_str(&format!(" · {}", event.location.trim()));
        }
        list.append(&event_button(event, &text, &state, &planner, format));
    }
    if current.is_none() {
        let empty = gtk::Label::new(Some("No events in these 30 days."));
        empty.add_css_class("mail-empty-body");
        empty.set_xalign(0.0);
        list.append(&empty);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    container.append(&scroll);
}

pub(super) fn build_calendar_page(state: Rc<AppState>, window: adw::Window) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let sync = gtk::Button::with_label("Refresh calendars");
    let views = gtk::DropDown::from_strings(&["Month", "Week", "Agenda"]);
    views.set_tooltip_text(Some("Calendar view"));
    let previous = gtk::Button::with_label("‹");
    previous.set_tooltip_text(Some("Previous"));
    let today_button = gtk::Button::with_label("Today");
    let label = gtk::Label::new(None);
    label.set_hexpand(true);
    label.set_xalign(0.5);
    label.add_css_class("mail-reader-subject");
    let next = gtk::Button::with_label("›");
    next.set_tooltip_text(Some("Next"));
    let add = gtk::Button::with_label("New event");
    add.add_css_class("mail-accent-button");
    for widget in [
        sync.upcast_ref::<gtk::Widget>(),
        views.upcast_ref(),
        previous.upcast_ref(),
        today_button.upcast_ref(),
        label.upcast_ref(),
        next.upcast_ref(),
        add.upcast_ref(),
    ] {
        controls.append(widget);
    }
    page.append(&controls);
    let legend = gtk::Label::new(Some(
        "● Personal (iCloud / Google)    ◆ Subscriptions (read-only) · double-click a day to add an event",
    ));
    legend.set_xalign(0.0);
    legend.set_wrap(true);
    page.append(&legend);
    let sync_status = gtk::Label::new(Some(&state.calendar_sync_status.borrow()));
    sync_status.set_xalign(0.0);
    sync_status.set_wrap(true);
    page.append(&sync_status);
    let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
    container.set_vexpand(true);
    page.append(&container);

    let view = Rc::new(Cell::new(CalendarView::Month));
    let anchor = Rc::new(Cell::new(Local::now().date_naive()));
    let events = Rc::new(RefCell::new(visible_calendar_events(&state)));
    let format = state.preferences.borrow().date_time_format.clone();
    let redraw: Rc<dyn Fn()> = {
        let container = container.clone();
        let label = label.clone();
        let view = view.clone();
        let anchor = anchor.clone();
        let events = events.clone();
        let state = state.clone();
        let window = window.clone();
        Rc::new(move || {
            render_view(
                &container,
                &label,
                view.get(),
                anchor.get(),
                &events.borrow(),
                &format,
                state.clone(),
                window.clone().upcast(),
            );
        })
    };
    redraw();
    {
        let state = state.clone();
        sync.connect_clicked(move |_| start_calendar_sync(state.clone()));
    }
    {
        let (view, anchor, redraw) = (view.clone(), anchor.clone(), redraw.clone());
        views.connect_selected_notify(move |dropdown| {
            view.set(match dropdown.selected() {
                1 => CalendarView::Week,
                2 => CalendarView::Agenda,
                _ => CalendarView::Month,
            });
            anchor.set(Local::now().date_naive());
            redraw();
        });
    }
    for (button, forward) in [(&previous, false), (&next, true)] {
        let (view, anchor, redraw) = (view.clone(), anchor.clone(), redraw.clone());
        button.connect_clicked(move |_| {
            anchor.set(shift(view.get(), anchor.get(), forward));
            redraw();
        });
    }
    {
        let (anchor, redraw) = (anchor.clone(), redraw.clone());
        today_button.connect_clicked(move |_| {
            anchor.set(Local::now().date_naive());
            redraw();
        });
    }
    {
        let state = state.clone();
        let window = window.clone();
        add.connect_clicked(move |_| {
            open_calendar_event_dialog(
                state.clone(),
                window.clone().upcast(),
                None,
                true,
                None,
                None,
                None,
            )
        });
    }
    let weak = container.downgrade();
    glib::timeout_add_local(Duration::from_secs(2), move || {
        if weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        sync_status.set_text(&state.calendar_sync_status.borrow());
        let latest = visible_calendar_events(&state);
        if *events.borrow() != latest {
            *events.borrow_mut() = latest;
            redraw();
        }
        glib::ControlFlow::Continue
    });
    page
}

// ============================================================ event details & editing

pub(super) fn open_event_details(
    state: &Rc<AppState>,
    parent: &gtk::Window,
    event: &PlannerEvent,
    format: &str,
) {
    let dialog = gtk::Window::builder()
        .title("Event details")
        .transient_for(parent)
        .modal(true)
        .default_width(520)
        .default_height(420)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    for margin in [20] {
        content.set_margin_top(margin);
        content.set_margin_bottom(margin);
        content.set_margin_start(margin);
        content.set_margin_end(margin);
    }
    let details = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let title = gtk::Label::new(Some(&event.title));
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.set_selectable(true);
    title.add_css_class("mail-reader-subject");
    details.append(&title);
    for (heading, value) in [
        (
            "Calendar",
            format!(
                "{}{}",
                event.source,
                if event.subscription {
                    " (subscription · read-only)"
                } else {
                    ""
                }
            ),
        ),
        ("Starts", format_planner_datetime(&event.starts_at, format)),
        ("Ends", format_planner_datetime(&event.ends_at, format)),
        ("Location", event.location.clone()),
        ("Notes", event.notes.clone()),
    ] {
        if value.trim().is_empty() {
            continue;
        }
        let caption = gtk::Label::new(Some(heading));
        caption.set_xalign(0.0);
        caption.add_css_class("mail-section-label");
        details.append(&caption);
        let text = gtk::Label::new(Some(&value));
        text.set_xalign(0.0);
        text.set_wrap(true);
        text.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        text.set_selectable(true);
        details.append(&text);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&details)
        .build();
    content.append(&scroll);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    if !event.subscription {
        let recurring = state
            .database
            .calendar_event_link(event.id)
            .ok()
            .flatten()
            .is_some_and(|link| link.3);
        let edit = gtk::Button::with_label("Edit");
        let delete = gtk::Button::with_label("Delete");
        delete.add_css_class("mail-danger");
        if recurring {
            for button in [&edit, &delete] {
                button.set_sensitive(false);
                button.set_tooltip_text(Some(
                    "Recurring events are changed in iCloud or Google Calendar",
                ));
            }
        }
        {
            let state = state.clone();
            let parent = parent.clone();
            let event = event.event.clone();
            let weak = dialog.downgrade();
            edit.connect_clicked(move |_| {
                if let Some(dialog) = weak.upgrade() {
                    dialog.close();
                }
                open_calendar_event_dialog(
                    state.clone(),
                    parent.clone(),
                    None,
                    false,
                    None,
                    None,
                    Some(event.clone()),
                );
            });
        }
        {
            let state = state.clone();
            let id = event.id;
            let title = event.title.clone();
            let weak = dialog.downgrade();
            delete.connect_clicked(move |_| {
                let state = state.clone();
                let weak = weak.clone();
                let Some(dialog) = weak.upgrade() else { return };
                confirm(
                    &dialog,
                    "Delete event?",
                    &format!("Delete “{title}”? Synced calendars are updated too."),
                    "Delete",
                    move || {
                        match state.database.delete_calendar_event(id) {
                            Ok(()) => {
                                start_calendar_sync(state.clone());
                                set_status(&state, "Event deleted");
                            }
                            Err(error) => {
                                set_status(&state, &format!("Couldn’t delete the event: {error}"))
                            }
                        }
                        if let Some(dialog) = weak.upgrade() {
                            dialog.close();
                        }
                    },
                );
            });
        }
        buttons.append(&delete);
        buttons.append(&edit);
    }
    let close = gtk::Button::with_label("Close");
    let weak = dialog.downgrade();
    close.connect_clicked(move |_| {
        if let Some(dialog) = weak.upgrade() {
            dialog.close();
        }
    });
    buttons.append(&close);
    content.append(&buttons);
    let keys = gtk::EventControllerKey::new();
    let weak = dialog.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(dialog) = weak.upgrade() {
                dialog.close();
            }
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    dialog.add_controller(keys);
    dialog.set_child(Some(&content));
    dialog.present();
}

// ============================================================ settings sections

pub(super) fn build_reminder_settings(
    state: Rc<AppState>,
    session: Rc<SettingsSession>,
    save: gtk::Button,
) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.set_margin_top(10);
    page.append(&backup_section_label("Event reminders"));
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let label = gtk::Label::new(Some("Notify me before events"));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    row.append(&label);
    let options: [(Option<u32>, &str); 7] = [
        (None, "Off"),
        (Some(0), "At start time"),
        (Some(5), "5 minutes before"),
        (Some(10), "10 minutes before"),
        (Some(15), "15 minutes before"),
        (Some(30), "30 minutes before"),
        (Some(60), "1 hour before"),
    ];
    let dropdown = gtk::DropDown::from_strings(&options.map(|(_, label)| label));
    let current = state.preferences.borrow().reminder_minutes;
    dropdown.set_selected(
        options
            .iter()
            .position(|(value, _)| *value == current)
            .unwrap_or(3) as u32,
    );
    row.append(&dropdown);
    page.append(&row);
    let subscriptions = gtk::CheckButton::with_label("Also remind me about subscription events");
    subscriptions.set_active(state.preferences.borrow().remind_subscriptions);
    page.append(&subscriptions);
    {
        let state = state.clone();
        let session = session.clone();
        let save = save.clone();
        dropdown.connect_selected_notify(move |dropdown| {
            state.preferences.borrow_mut().reminder_minutes = options
                .get(dropdown.selected() as usize)
                .and_then(|(value, _)| *value);
            mark_settings_dirty(&session, &save);
        });
    }
    subscriptions.connect_toggled(move |check| {
        state.preferences.borrow_mut().remind_subscriptions = check.is_active();
        mark_settings_dirty(&session, &save);
    });
    page.append(&backup_help("Reminders appear as desktop notifications while Omarchy Mail is running. All-day events are not reminded."));
    page
}

pub(super) fn build_undo_send_setting(
    state: Rc<AppState>,
    session: Rc<SettingsSession>,
    save: gtk::Button,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.set_margin_top(10);
    let label = gtk::Label::new(Some("Undo send window"));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    row.append(&label);
    let options = [0_u32, 5, 10, 20, 30];
    let dropdown = gtk::DropDown::from_strings(&[
        "Off",
        "5 seconds",
        "10 seconds",
        "20 seconds",
        "30 seconds",
    ]);
    let current = state.preferences.borrow().undo_send_seconds;
    dropdown.set_selected(
        options
            .iter()
            .position(|value| *value == current)
            .unwrap_or(2) as u32,
    );
    dropdown.set_tooltip_text(Some("Sent messages wait this long so you can press Undo"));
    dropdown.connect_selected_notify(move |dropdown| {
        state.preferences.borrow_mut().undo_send_seconds =
            options[dropdown.selected() as usize % options.len()];
        mark_settings_dirty(&session, &save);
    });
    row.append(&dropdown);
    row
}

pub(super) fn build_google_tasks_settings(
    state: Rc<AppState>,
    session: Rc<SettingsSession>,
    save: gtk::Button,
) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.set_margin_top(10);
    page.append(&backup_section_label("Google Tasks"));
    page.append(&backup_help(
        "Sync your todos with a Google Tasks list, both ways. Uses the Google sign-in above; enable the Google Tasks API for your OAuth client. If you connected Google before this option existed, sign in to Google again.",
    ));
    let configured = state.preferences.borrow().google_tasks.clone();
    let status = backup_help(
        &configured
            .as_ref()
            .map(|a| format!("Syncing todos with “{}” ({})", a.list_name, a.email))
            .unwrap_or("Not connected".into()),
    );
    page.append(&status);
    let results = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page.append(&results);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let choose = gtk::Button::with_label("Choose task list…");
    let disconnect = gtk::Button::with_label("Stop syncing todos");
    disconnect.set_visible(configured.is_some());
    buttons.append(&choose);
    buttons.append(&disconnect);
    page.append(&buttons);
    {
        let (state, session, save, status) =
            (state.clone(), session.clone(), save.clone(), status.clone());
        disconnect.connect_clicked(move |_| {
            state.preferences.borrow_mut().google_tasks = None;
            mark_settings_dirty(&session, &save);
            status.set_text("Todo sync turned off. Save Changes to keep this setting.");
        });
    }
    choose.connect_clicked(move |button| {
        let Some(email) = state.preferences.borrow().google_calendar.as_ref().map(|a| a.email.clone()) else {
            status.set_text("Connect Google Calendar above first (Sign in with Google), then choose a task list.");
            return;
        };
        button.set_sensitive(false);
        status.set_text("Finding task lists…");
        let (tx, rx) = async_channel::bounded(1);
        let email_for_worker = email.clone();
        std::thread::spawn(move || {
            let result = crate::google_calendar::access_token(&email_for_worker)
                .and_then(|token| crate::google_tasks::discover(&token));
            let _ = tx.send_blocking(result);
        });
        let (state, session, save, status, results, button) =
            (state.clone(), session.clone(), save.clone(), status.clone(), results.clone(), button.clone());
        glib::MainContext::default().spawn_local(async move {
            let found = rx.recv().await;
            button.set_sensitive(true);
            match found {
                Ok(Ok(lists)) => {
                    clear(&results);
                    let names = lists.iter().map(|l| l.name.as_str()).collect::<Vec<_>>();
                    let dropdown = gtk::DropDown::from_strings(&names);
                    let lists = Rc::new(lists);
                    let configure = Rc::new(move |index: u32| {
                        if let Some(list) = lists.get(index as usize) {
                            state.preferences.borrow_mut().google_tasks = Some(crate::google_tasks::GoogleTasksAccount {
                                email: email.clone(),
                                list_id: list.url.clone(),
                                list_name: list.name.clone(),
                            });
                            mark_settings_dirty(&session, &save);
                        }
                    });
                    configure(0);
                    dropdown.connect_selected_notify(move |d| configure(d.selected()));
                    results.append(&dropdown);
                    status.set_text("Choose a task list, then Save Changes. Existing todos are uploaded to it.");
                }
                Ok(Err(error)) => status.set_text(&error),
                Err(_) => status.set_text("Task list lookup stopped."),
            }
        });
    });
    page
}

// ============================================================ automatic backups

const AUTO_BACKUP_ACCOUNT: &str = "automatic";
const AUTO_BACKUP_SLOT: &str = "backup-passphrase";
const AUTO_PREFIX: &str = "omarchy-mail-auto-";

pub(super) fn auto_backup_due(config: &preferences::AutoBackup, now: DateTime<Utc>) -> bool {
    config
        .last_run
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .is_none_or(|last| {
            now - last.with_timezone(&Utc)
                >= chrono::Duration::days(config.every_days.max(1) as i64)
        })
}

/// Deletes the oldest automatic backups beyond `keep`. Returns how many went.
pub(super) fn prune_auto_backups(folder: &std::path::Path, keep: usize) -> usize {
    let mut files = std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|name| {
                            name.starts_with(AUTO_PREFIX)
                                && name.ends_with(&format!(".{}", crate::backup::EXTENSION))
                        })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    files.sort();
    let excess = files.len().saturating_sub(keep.max(1));
    files
        .iter()
        .take(excess)
        .filter(|path| std::fs::remove_file(path).is_ok())
        .count()
}

fn run_auto_backup_if_due(state: &Rc<AppState>) {
    let Some(config) = state.preferences.borrow().auto_backup.clone() else {
        return;
    };
    if !auto_backup_due(&config, Utc::now()) || state.auto_backup_running.replace(true) {
        return;
    }
    let database = state.database.clone();
    let preferences_snapshot = state.preferences.borrow().clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = (|| {
            let passphrase = mail::credentials::load_password(AUTO_BACKUP_ACCOUNT, AUTO_BACKUP_SLOT)
                .map_err(|_| "The automatic backup passphrase is missing from the keyring; set it again in Settings → Backup.".to_string())?;
            let folder = PathBuf::from(&config.folder);
            std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
            let file = folder.join(format!(
                "{AUTO_PREFIX}{}.{}",
                Local::now().format("%Y-%m-%d-%H%M"),
                crate::backup::EXTENSION
            ));
            let secrets = crate::backup::collect_secrets(&database, &preferences_snapshot);
            crate::backup::create_backup(
                &crate::backup::Locations::current(),
                &file,
                &passphrase,
                &secrets,
                true,
            )
            .map_err(|e| e.to_string())?;
            prune_auto_backups(&folder, config.keep as usize);
            Ok::<_, String>(file)
        })();
        // Record the outcome in the saved preferences without touching any
        // unsaved Settings changes held in memory.
        let mut saved = preferences::load();
        if let Some(auto) = saved.auto_backup.as_mut() {
            match &result {
                Ok(_) => {
                    auto.last_run = Some(Utc::now().to_rfc3339());
                    auto.last_error = None;
                }
                Err(error) => {
                    // Retry in a day rather than every two minutes.
                    auto.last_run = Some(
                        (Utc::now() - chrono::Duration::days(auto.every_days.max(1) as i64 - 1))
                            .to_rfc3339(),
                    );
                    auto.last_error = Some(error.clone());
                }
            }
            let _ = preferences::save(&saved);
        }
        let _ = tx.send_blocking((result, saved.auto_backup));
    });
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        state.auto_backup_running.set(false);
        let Ok((result, saved)) = rx.recv().await else {
            return;
        };
        if let (Some(saved), Some(live)) =
            (saved, state.preferences.borrow_mut().auto_backup.as_mut())
        {
            live.last_run = saved.last_run;
            live.last_error = saved.last_error;
        }
        match result {
            Ok(file) => set_status(
                &state,
                &format!("Automatic backup saved to {}", file.display()),
            ),
            Err(error) => set_status(&state, &format!("Automatic backup failed: {error}")),
        }
    });
}

pub(super) fn build_auto_backup_settings(
    state: Rc<AppState>,
    session: Rc<SettingsSession>,
    save: gtk::Button,
    parent: adw::Window,
) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    page.append(&backup_section_label("Automatic backups"));
    page.append(&backup_help(
        "Creates an encrypted backup on a schedule while Omarchy Mail is running, including saved passwords, and keeps the newest few. Choose a folder that is itself backed up or synced (for example a NAS or cloud folder). The passphrase is kept in the system keyring.",
    ));
    let current = state.preferences.borrow().auto_backup.clone();
    let enabled = gtk::CheckButton::with_label("Back up automatically");
    enabled.set_active(current.is_some());
    page.append(&enabled);
    let folder = Rc::new(RefCell::new(
        current
            .as_ref()
            .map(|c| c.folder.clone())
            .unwrap_or_else(|| {
                glib::user_special_dir(glib::UserDirectory::Documents)
                    .unwrap_or_else(glib::home_dir)
                    .join("Omarchy Mail backups")
                    .to_string_lossy()
                    .to_string()
            }),
    ));
    let folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let folder_label = gtk::Label::new(Some(&folder.borrow()));
    folder_label.set_xalign(0.0);
    folder_label.set_hexpand(true);
    folder_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let choose = gtk::Button::with_label("Choose folder…");
    folder_row.append(&folder_label);
    folder_row.append(&choose);
    page.append(&folder_row);
    let schedule_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let frequency = gtk::DropDown::from_strings(&["Daily", "Weekly"]);
    frequency.set_selected(if current.as_ref().is_some_and(|c| c.every_days >= 7) {
        1
    } else {
        0
    });
    let keep_values = [3_u32, 7, 14, 30];
    let keep = gtk::DropDown::from_strings(&["Keep 3", "Keep 7", "Keep 14", "Keep 30"]);
    keep.set_selected(
        keep_values
            .iter()
            .position(|v| Some(*v) == current.as_ref().map(|c| c.keep))
            .unwrap_or(1) as u32,
    );
    schedule_row.append(&frequency);
    schedule_row.append(&keep);
    page.append(&schedule_row);
    let passphrase = backup_passphrase("Passphrase (leave blank to keep the saved one)");
    let confirm_entry = backup_passphrase("Repeat passphrase");
    page.append(&passphrase);
    page.append(&confirm_entry);
    let apply = gtk::Button::with_label("Apply backup schedule");
    apply.set_halign(gtk::Align::Start);
    page.append(&apply);
    let status = backup_help(
        &current
            .as_ref()
            .map(|c| match (&c.last_run, &c.last_error) {
                (_, Some(error)) => format!("Last attempt failed: {error}"),
                (Some(last), None) => format!(
                    "Last backup: {}",
                    DateTime::parse_from_rfc3339(last)
                        .map(|at| local_text(at.with_timezone(&Local)))
                        .unwrap_or_else(|_| last.clone())
                ),
                (None, None) => "No automatic backup has run yet.".into(),
            })
            .unwrap_or_default(),
    );
    page.append(&status);
    {
        let folder = folder.clone();
        let folder_label = folder_label.clone();
        choose.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Backup folder")
                .accept_label("Choose")
                .build();
            let folder = folder.clone();
            let folder_label = folder_label.clone();
            dialog.select_folder(Some(&parent), None::<&gio::Cancellable>, move |result| {
                if let Ok(file) = result
                    && let Some(path) = file.path()
                {
                    folder_label.set_text(&path.display().to_string());
                    *folder.borrow_mut() = path.to_string_lossy().to_string();
                }
            });
        });
    }
    apply.connect_clicked(move |_| {
        if !enabled.is_active() {
            state.preferences.borrow_mut().auto_backup = None;
            mark_settings_dirty(&session, &save);
            status.set_text("Automatic backups off. Save Changes to keep this setting.");
            return;
        }
        let secret = passphrase.text().to_string();
        let has_saved = mail::credentials::load_password(AUTO_BACKUP_ACCOUNT, AUTO_BACKUP_SLOT).is_ok();
        if secret.is_empty() && !has_saved {
            status.set_text("Choose a passphrase for automatic backups.");
            return;
        }
        if !secret.is_empty() {
            if secret.chars().count() < 8 {
                status.set_text("Use a passphrase of at least 8 characters.");
                return;
            }
            if secret != confirm_entry.text().as_str() {
                status.set_text("The passphrases do not match.");
                return;
            }
            if mail::credentials::store_password(AUTO_BACKUP_ACCOUNT, AUTO_BACKUP_SLOT, &secret).is_err() {
                status.set_text("Could not save the passphrase in the system keyring.");
                return;
            }
            passphrase.set_text("");
            confirm_entry.set_text("");
        }
        let previous = state.preferences.borrow().auto_backup.clone();
        state.preferences.borrow_mut().auto_backup = Some(preferences::AutoBackup {
            folder: folder.borrow().clone(),
            every_days: if frequency.selected() == 1 { 7 } else { 1 },
            keep: keep_values[keep.selected() as usize % keep_values.len()],
            last_run: previous.as_ref().and_then(|p| p.last_run.clone()),
            last_error: None,
        });
        mark_settings_dirty(&session, &save);
        status.set_text("Schedule ready. Save Changes to turn it on; the first backup runs within a few minutes.");
    });
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planner(starts: &str, ends: &str, subscription: bool) -> PlannerEvent {
        PlannerEvent {
            event: CalendarEvent {
                id: 1,
                title: "T".into(),
                notes: String::new(),
                starts_at: starts.into(),
                ends_at: ends.into(),
                location: String::new(),
                message_id: None,
            },
            source: "S".into(),
            subscription,
        }
    }

    #[test]
    fn reminders_fire_inside_the_lead_window_only() {
        let now = NaiveDateTime::parse_from_str("2026-11-11 09:00", "%Y-%m-%d %H:%M").unwrap();
        let events = vec![
            planner("2026-11-11 09:10", "2026-11-11 10:00", false),
            planner("2026-11-11 09:11", "2026-11-11 10:00", false),
            planner("2026-11-11 08:58", "2026-11-11 10:00", false),
            planner("2026-11-11 00:00", "2026-11-12 00:00", false),
        ];
        let due = due_reminders(&events, now, 10);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].starts_at, "2026-11-11 09:10");
        assert_eq!(due_reminders(&events, now, 0).len(), 0);
    }

    #[test]
    fn snooze_choices_are_in_the_future_and_sensible() {
        // Wednesday 11 Nov 2026, 10:00
        let now = Local.with_ymd_and_hms(2026, 11, 11, 10, 0, 0).unwrap();
        let choices = snooze_choices(now);
        assert!(choices.iter().all(|(_, at)| *at > now));
        let labels = choices.iter().map(|(l, _)| *l).collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "Later today",
                "Tomorrow morning",
                "This weekend",
                "Next week"
            ]
        );
        assert_eq!(choices[2].1.weekday(), Weekday::Sat);
        assert_eq!(choices[3].1.weekday(), Weekday::Mon);
        let friday_night = Local.with_ymd_and_hms(2026, 11, 13, 21, 0, 0).unwrap();
        let labels = snooze_choices(friday_night)
            .iter()
            .map(|(l, _)| *l)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["Tomorrow morning", "Next week"]);
    }

    #[test]
    fn unsubscribe_headers_and_mailto_are_parsed() {
        let targets = parse_unsubscribe(
            "<mailto:leave@list.example?subject=stop>, <https://list.example/u/1>",
        );
        assert_eq!(targets.https.as_deref(), Some("https://list.example/u/1"));
        assert_eq!(
            parse_mailto(targets.mailto.as_deref().unwrap()),
            Some((
                "leave@list.example".into(),
                "stop".into(),
                "unsubscribe".into()
            ))
        );
        assert_eq!(
            parse_unsubscribe("<http://insecure.example/u>"),
            UnsubscribeTargets::default()
        );
        assert!(parse_mailto("mailto:?subject=x").is_none());
    }

    #[test]
    fn contacts_match_prefixes_first() {
        let contact = |name: &str, email: &str| Contact {
            name: name.into(),
            email: email.into(),
            score: 1,
        };
        let book = vec![
            contact("Bob Annan", "bob@example.com"),
            contact("Ann Lee", "ann@example.com"),
            contact("", "zoe@annex.io"),
        ];
        let found = match_contacts(&book, "ann", 5)
            .into_iter()
            .map(|c| c.email.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            ["bob@example.com", "ann@example.com", "zoe@annex.io"]
        );
        assert!(match_contacts(&book, "a", 5).is_empty());
        assert_eq!(split_recipient_token("a@x.com, Bo"), ("a@x.com,", " Bo"));
        assert_eq!(split_recipient_token("Bo"), ("", "Bo"));
    }

    #[test]
    fn auto_backups_run_on_schedule_and_prune_oldest() {
        let mut config = preferences::AutoBackup {
            folder: String::new(),
            every_days: 1,
            keep: 2,
            last_run: None,
            last_error: None,
        };
        let now = Utc::now();
        assert!(auto_backup_due(&config, now));
        config.last_run = Some((now - chrono::Duration::hours(3)).to_rfc3339());
        assert!(!auto_backup_due(&config, now));
        config.last_run = Some((now - chrono::Duration::hours(25)).to_rfc3339());
        assert!(auto_backup_due(&config, now));

        let directory = tempfile::tempdir().unwrap();
        for day in ["01", "02", "03", "04"] {
            std::fs::write(
                directory
                    .path()
                    .join(format!("{AUTO_PREFIX}2026-11-{day}-0900.omailbackup")),
                b"x",
            )
            .unwrap();
        }
        std::fs::write(directory.path().join("manual.omailbackup"), b"x").unwrap();
        assert_eq!(prune_auto_backups(directory.path(), 2), 2);
        let mut left = std::fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        left.sort();
        assert_eq!(
            left,
            [
                "manual.omailbackup".to_string(),
                format!("{AUTO_PREFIX}2026-11-03-0900.omailbackup"),
                format!("{AUTO_PREFIX}2026-11-04-0900.omailbackup"),
            ]
        );
    }
}
