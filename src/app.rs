use adw::prelude::*;

pub fn run() {
    if let Some(code) = crate::cli::handle(std::env::args().skip(1).collect()) {
        std::process::exit(code);
    }
    // A verified restore is swapped in before anything opens the database.
    if let Err(error) = crate::backup::apply_pending_restore(&crate::backup::Locations::current()) {
        eprintln!("omarchy-mail: restore could not be completed: {error}");
        crate::backup::set_restore_notice(format!(
            "Restore could not be completed ({error}). Previous data is in ~/.local/share/omarchy-mail-recovery/."
        ));
    }
    adw::init().expect("libadwaita could not be initialized");
    let application = adw::Application::builder()
        .application_id("org.omarchy.Mail")
        .build();

    application.connect_activate(|application| {
        if let Some(window) = application.windows().first() {
            window.present();
        } else {
            crate::ui::build_window(application);
        }
    });

    application.run();
}
