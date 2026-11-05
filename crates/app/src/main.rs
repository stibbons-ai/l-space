use app::i18n::{self, Language, Text};
use dioxus::mobile::{Config, WindowBuilder};

fn main() {
    // The window title on desktop (iOS shows none) until the root component's `Title` renders.
    let language = Language::for_locale(sys_locale::get_locale().as_deref());
    let window = WindowBuilder::new().with_title(i18n::text(language, Text::AppName));
    dioxus::LaunchBuilder::mobile()
        .with_cfg(Config::new().with_window(window))
        .launch(app::ui::root);
}
