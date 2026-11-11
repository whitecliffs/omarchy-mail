mod app;
mod backup;
mod cli;
mod database;
mod google_calendar;
mod google_tasks;
mod icloud;
mod mail;
mod models;
mod preferences;
mod security;
mod subscriptions;
mod theme;
mod ui;

fn main() {
    app::run();
}
