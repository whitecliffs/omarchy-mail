use adw::prelude::*;

pub fn run() {
    adw::init().expect("libadwaita could not be initialized");
    let application = adw::Application::builder()
        .application_id("org.omarchy.Mail")
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
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
