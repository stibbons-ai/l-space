//! Rendering: one component per [`Screen`]. Every action goes through [`App`]; every string
//! the App shows comes from [`crate::i18n`] (Info.plist strings are the exception).

use std::sync::Arc;

use api::{ApiKey, EpisodeFilter, Item, Library, LibraryEntry, LibraryServer};
use dioxus::prelude::*;

use crate::flow::{App, Notice, Screen};
use crate::i18n::{self, LanguageChoice, Text};
use crate::storage::FileStore;

const STYLE: &str = include_str!("../assets/app.css");

/// The App's root component.
pub fn root() -> Element {
    let app = use_signal(|| {
        let store = FileStore::in_data_dir().expect("the platform has an application data dir");
        App::open(Arc::new(store), sys_locale::get_locale().as_deref())
    });
    let busy = use_signal(|| false);
    use_hook(move || act(app, busy, async |app: &mut App| app.start().await));

    let screen = app.read().screen().clone();
    rsx! {
        document::Title { {app.read().text(Text::AppName)} }
        style { {STYLE} }
        main {
            match screen {
                Screen::Starting => rsx! {
                    p { class: "status", {app.read().text(Text::Starting)} }
                },
                Screen::BridgeUrl { notice } => rsx! {
                    BridgeUrlScreen { app, busy, notice }
                },
                Screen::Login { library_servers, notice } => rsx! {
                    LoginScreen { app, busy, library_servers, notice }
                },
                Screen::Libraries { libraries, notice } => rsx! {
                    LibrariesScreen { app, busy, libraries, notice }
                },
                Screen::Library { library, entries, notice } => rsx! {
                    LibraryScreen { app, busy, library, entries, notice }
                },
                Screen::Podcast { podcast, notice } => rsx! {
                    PodcastScreen { app, busy, podcast, notice }
                },
                Screen::Item { item, notice } => rsx! {
                    ItemScreen { app, busy, item, notice }
                },
                Screen::Settings { notice } => rsx! {
                    SettingsScreen { app, busy, notice }
                },
            }
        }
    }
}

/// Runs `action` on a copy of the App while `busy`, then shows the result.
fn act(mut app: Signal<App>, mut busy: Signal<bool>, action: impl AsyncFnOnce(&mut App) + 'static) {
    busy.set(true);
    spawn(async move {
        let mut next = app.peek().clone();
        action(&mut next).await;
        app.set(next);
        busy.set(false);
    });
}

#[component]
fn NoticeView(app: Signal<App>, notice: Option<Notice>) -> Element {
    let Some(notice) = notice else {
        return rsx! {};
    };
    rsx! {
        p { class: "notice", role: "alert", {app.read().text(notice.text())} }
    }
}

#[component]
fn BridgeUrlScreen(app: Signal<App>, busy: Signal<bool>, notice: Option<Notice>) -> Element {
    let initial = use_hook(|| app.peek().bridge_url().unwrap_or_default().to_owned());
    let mut url = use_signal(|| initial.clone());
    // Re-renders only when emptiness flips, not on every keystroke.
    let url_empty = use_memo(move || url.read().trim().is_empty());
    let text = |text| app.read().text(text);
    rsx! {
        h1 { {text(Text::BridgeUrlTitle)} }
        p { {text(Text::BridgeUrlIntro)} }
        form {
            onsubmit: move |event| {
                event.prevent_default();
                let input = url();
                act(app, busy, async move |app: &mut App| app.submit_bridge_url(&input).await);
            },
            label {
                {text(Text::BridgeUrlLabel)}
                input {
                    // Not `url`: native validation would reject bare addresses such as
                    // `192.168.1.10:8080` before the flow can add `http://`. `url` also turned
                    // iOS autocorrection off, which a text input needs `autocorrect` for: a
                    // host name such as `clacks.lan` must not be "corrected".
                    r#type: "text",
                    inputmode: "url",
                    autocapitalize: "none",
                    autocorrect: "off",
                    autocomplete: "off",
                    spellcheck: false,
                    placeholder: "http://192.168.1.10:8080",
                    // Not bound to `url`: re-setting the value while typing drops keystrokes.
                    initial_value: "{initial}",
                    oninput: move |event| url.set(event.value()),
                }
            }
            NoticeView { app, notice }
            button {
                r#type: "submit",
                disabled: busy() || url_empty(),
                {text(if busy() { Text::Busy } else { Text::BridgeUrlConnect })}
            }
        }
    }
}

#[component]
fn LoginScreen(
    app: Signal<App>,
    busy: Signal<bool>,
    library_servers: Vec<LibraryServer>,
    notice: Option<Notice>,
) -> Element {
    let first = library_servers
        .first()
        .map(|server| server.base_url.clone());
    // `None` until the person picks one: the first listed.
    let mut picked = use_signal(|| None::<String>);
    // Only in this screen's state: it goes when a login moves on to Settings.
    let mut api_key = use_signal(String::new);
    let key_empty = use_memo(move || api_key.read().trim().is_empty());
    let text = |text| app.read().text(text);
    let bridge_url = app.read().bridge_url().unwrap_or_default().to_owned();
    let has_servers = !library_servers.is_empty();
    rsx! {
        h1 { {text(Text::LoginTitle)} }
        form {
            onsubmit: move |event| {
                event.prevent_default();
                let Some(library_server) = picked().or_else(|| first.clone()) else {
                    return;
                };
                let key = ApiKey::new(api_key());
                act(app, busy, async move |app: &mut App| app.log_in(&library_server, key).await);
            },
            if has_servers {
                label {
                    {text(Text::LoginLibraryServer)}
                    select {
                        onchange: move |event| picked.set(Some(event.value())),
                        for server in library_servers {
                            option {
                                key: "{server.base_url}",
                                value: "{server.base_url}",
                                {server.name.as_deref().unwrap_or(&server.base_url)}
                            }
                        }
                    }
                }
                label {
                    {text(Text::LoginApiKey)}
                    input {
                        r#type: "password",
                        autocapitalize: "none",
                        autocomplete: "off",
                        spellcheck: false,
                        oninput: move |event| api_key.set(event.value()),
                    }
                }
                p { class: "hint", {text(Text::LoginApiKeyHint)} }
            } else if notice.is_none() {
                p { {text(Text::LoginNoLibraryServers)} }
            }
            NoticeView { app, notice }
            if has_servers {
                button {
                    r#type: "submit",
                    disabled: busy() || key_empty(),
                    {text(if busy() { Text::Busy } else { Text::LoginSubmit })}
                }
            } else {
                button {
                    r#type: "button",
                    disabled: busy(),
                    onclick: move |_| act(app, busy, async |app: &mut App| app.reload_library_servers().await),
                    {text(Text::Retry)}
                }
            }
        }
        footer {
            dl {
                dt { {text(Text::Bridge)} }
                dd { "{bridge_url}" }
            }
            button {
                r#type: "button",
                class: "secondary",
                disabled: busy(),
                onclick: move |_| app.write().change_bridge_url(),
                {text(Text::LoginChangeBridge)}
            }
        }
    }
}

/// A button above the screen's title, to move between screens.
#[component]
fn NavButton(busy: Signal<bool>, label: String, onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        nav {
            button {
                r#type: "button",
                class: "secondary",
                disabled: busy(),
                onclick: move |event| onclick.call(event),
                {label}
            }
        }
    }
}

/// "Back" to the screen this one was opened from.
#[component]
fn BackButton(app: Signal<App>, busy: Signal<bool>) -> Element {
    rsx! {
        NavButton {
            busy,
            label: app.read().text(Text::Back),
            onclick: move |_| app.write().go_back(),
        }
    }
}

#[component]
fn LibrariesScreen(
    app: Signal<App>,
    busy: Signal<bool>,
    libraries: Vec<Library>,
    notice: Option<Notice>,
) -> Element {
    let text = |text| app.read().text(text);
    // Nothing listed because the listing failed, not because there is nothing.
    let failed = libraries.is_empty() && matches!(notice, Some(Notice::Api(_)));
    rsx! {
        NavButton {
            busy,
            label: text(Text::SettingsTitle),
            onclick: move |_| app.write().open_settings(),
        }
        h1 { {text(Text::LibrariesTitle)} }
        NoticeView { app, notice }
        if failed {
            button {
                r#type: "button",
                disabled: busy(),
                onclick: move |_| act(app, busy, async |app: &mut App| app.open_libraries().await),
                {text(Text::Retry)}
            }
        } else if libraries.is_empty() {
            p { {text(Text::LibrariesNone)} }
        }
        ul { class: "entries",
            for library in libraries {
                EntryButton {
                    key: "{library.id}",
                    busy,
                    title: library.name.clone(),
                    detail: text(library.kind.into()),
                    onclick: {
                        let library = library.clone();
                        move |_| {
                            let library = library.clone();
                            act(app, busy, async move |app: &mut App| app.open_library(library).await);
                        }
                    },
                }
            }
        }
    }
}

/// One row of a list: a Library, an audiobook or podcast, or an episode.
#[component]
fn EntryButton(
    busy: Signal<bool>,
    title: String,
    detail: Option<String>,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    rsx! {
        li {
            button {
                r#type: "button",
                class: "entry",
                disabled: busy(),
                onclick: move |event| onclick.call(event),
                span { class: "title", "{title}" }
                if let Some(detail) = detail {
                    span { class: "detail", "{detail}" }
                }
            }
        }
    }
}

#[component]
fn LibraryScreen(
    app: Signal<App>,
    busy: Signal<bool>,
    library: Library,
    entries: Vec<LibraryEntry>,
    notice: Option<Notice>,
) -> Element {
    let text = |text| app.read().text(text);
    rsx! {
        BackButton { app, busy }
        h1 { "{library.name}" }
        NoticeView { app, notice }
        if entries.is_empty() {
            p { {text(Text::LibraryEmpty)} }
        }
        ul { class: "entries",
            for entry in entries {
                EntryButton {
                    key: "{entry.id}",
                    busy,
                    title: entry.title.clone(),
                    detail: entry.author.clone(),
                    onclick: {
                        let id = entry.id.clone();
                        move |_| {
                            let id = id.clone();
                            act(app, busy, async move |app: &mut App| app.open_entry(&id).await);
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn PodcastScreen(
    app: Signal<App>,
    busy: Signal<bool>,
    podcast: api::Podcast,
    notice: Option<Notice>,
) -> Element {
    let text = |text| app.read().text(text);
    let language = app.read().language();
    let filter = podcast.filter;
    rsx! {
        BackButton { app, busy }
        h1 { "{podcast.title}" }
        if let Some(author) = &podcast.author {
            p { class: "byline", "{author}" }
        }
        div { class: "filters", role: "group",
            for option in EpisodeFilter::CHOICES {
                button {
                    key: "{option:?}",
                    r#type: "button",
                    class: if option == filter { "selected" } else { "" },
                    aria_pressed: option == filter,
                    disabled: busy(),
                    onclick: move |_| act(app, busy, async move |app: &mut App| app.filter_episodes(option).await),
                    {text(option.into())}
                }
            }
        }
        NoticeView { app, notice }
        if podcast.episodes.is_empty() {
            p { {text(Text::EpisodesNone)} }
        }
        ul { class: "entries",
            for episode in podcast.episodes {
                EntryButton {
                    key: "{episode.id}",
                    busy,
                    title: episode.title.clone(),
                    detail: i18n::episode_summary(language, &episode),
                    onclick: {
                        let id = episode.id.clone();
                        move |_| {
                            let id = id.clone();
                            act(app, busy, async move |app: &mut App| app.open_episode(&id).await);
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn ItemScreen(app: Signal<App>, busy: Signal<bool>, item: Item, notice: Option<Notice>) -> Element {
    let text = |text| app.read().text(text);
    let language = app.read().language();
    let by = item.author.as_ref().or(item.podcast.as_ref());
    rsx! {
        BackButton { app, busy }
        h1 { "{item.title}" }
        if let Some(by) = by {
            p { class: "byline", "{by}" }
        }
        NoticeView { app, notice }
        dl {
            for (label, value) in i18n::item_facts(language, &item) {
                dt { {text(label)} }
                dd { "{value}" }
            }
        }
        if let Some(description) = &item.description {
            p { class: "description", "{description}" }
        }
        if !item.chapters.is_empty() {
            h2 { {text(Text::ItemChapters)} }
            ol { class: "chapters",
                for (index, (title, start)) in i18n::chapter_rows(&item).into_iter().enumerate() {
                    li { key: "{index}",
                        span { class: "title", "{title}" }
                        span { class: "detail", "{start}" }
                    }
                }
            }
        }
    }
}

#[component]
fn SettingsScreen(app: Signal<App>, busy: Signal<bool>, notice: Option<Notice>) -> Element {
    let text = |text| app.read().text(text);
    let (library_server, username, bridge_url, choice) = {
        let app = app.read();
        let Some(session) = app.session() else {
            return rsx! {};
        };
        (
            session.library_server.clone(),
            session.user.username.clone(),
            app.bridge_url().unwrap_or_default().to_owned(),
            app.language_choice(),
        )
    };
    rsx! {
        NavButton {
            busy,
            label: text(Text::LibrariesTitle),
            onclick: move |_| act(app, busy, async |app: &mut App| app.open_libraries().await),
        }
        h1 { {text(Text::SettingsTitle)} }
        NoticeView { app, notice }
        dl {
            dt { {text(Text::SettingsLibraryServer)} }
            dd {
                if let Some(name) = &library_server.name {
                    span { class: "name", "{name}" }
                }
                span { class: "url", "{library_server.base_url}" }
            }
            dt { {text(Text::SettingsAccount)} }
            dd { "{username}" }
            dt { {text(Text::Bridge)} }
            dd { "{bridge_url}" }
        }
        label {
            {text(Text::SettingsLanguage)}
            select {
                // A change during logout would be lost when the logout's result replaces the App.
                disabled: busy(),
                onchange: move |event| {
                    let picked = LanguageChoice::ALL
                        .into_iter()
                        .find(|choice| choice_value(*choice) == event.value());
                    if let Some(picked) = picked {
                        app.write().set_language(picked);
                    }
                },
                for option_choice in LanguageChoice::ALL {
                    option {
                        key: "{choice_value(option_choice)}",
                        value: choice_value(option_choice),
                        selected: option_choice == choice,
                        {text(option_choice.text())}
                    }
                }
            }
        }
        button {
            r#type: "button",
            disabled: busy(),
            onclick: move |_| act(app, busy, async |app: &mut App| app.log_out().await),
            {text(if busy() { Text::Busy } else { Text::SettingsLogout })}
        }
    }
}

/// The `<option>` value for a language choice.
fn choice_value(choice: LanguageChoice) -> &'static str {
    match choice {
        LanguageChoice::System => "system",
        LanguageChoice::English => "en",
        LanguageChoice::German => "de",
    }
}
