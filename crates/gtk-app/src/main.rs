use gtk4::prelude::*;
use libadwaita::prelude::*;

const APP_ID: &str = "mx.serfim.PipewireControl";

fn main() {
    tracing_subscriber::fmt::init();

    let app = libadwaita::Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_activate(build_ui);
    app.run();
}

fn build_ui(app: &libadwaita::Application) {
    let window = libadwaita::ApplicationWindow::builder()
        .application(app)
        .title("PipeWire Control")
        .default_width(900)
        .default_height(600)
        .build();

    let header = libadwaita::HeaderBar::new();
    let toolbar_view = libadwaita::ToolbarView::new();
    toolbar_view.add_top_bar(&header);

    let label = gtk4::Label::new(Some("PipeWire Control — UI under construction"));
    toolbar_view.set_content(Some(&label));

    window.set_content(Some(&toolbar_view));
    window.present();
}
