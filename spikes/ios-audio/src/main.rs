//! Spike for issue #2: does a Dioxus app drive native AVPlayer well enough on a physical iPhone?
//! See docs/adr/0007-dioxus-native-app.md for the five gate checks.

#[cfg(not(target_os = "ios"))]
compile_error!("the iOS audio spike only builds for iOS: use `dx serve --ios`");

mod player;

use std::time::Duration;

use dioxus::prelude::*;

/// Endless public MP3 stream, so the 10-minute lock test never runs out of audio.
const DEFAULT_URL: &str = "https://ice1.somafm.com/groovesalad-128-mp3";

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    use_hook(|| {
        player::init();
        // Lets a simulator smoke run play without tapping: `SIMCTL_CHILD_SPIKE_AUTOPLAY=<url>`.
        if let Ok(url) = std::env::var("SPIKE_AUTOPLAY") {
            player::play_url(&url);
        }
    });
    let mut url = use_signal(|| DEFAULT_URL.to_owned());
    let mut status = use_signal(player::status);
    let mut route = use_signal(player::current_outputs);
    let mut lines = use_signal(player::log_lines);

    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            status.set(player::status());
            route.set(player::current_outputs());
            lines.set(player::log_lines());
        }
    });

    rsx! {
        div { style: "font-family: -apple-system; padding: 56px 16px 16px; font-size: 15px;",
            h3 { "l-space audio spike" }
            input {
                style: "width: 100%; font-size: 14px; padding: 6px; box-sizing: border-box;",
                value: "{url}",
                autocapitalize: "off",
                autocorrect: "off",
                oninput: move |e| url.set(e.value()),
            }
            div { style: "display: flex; gap: 8px; margin: 12px 0;",
                button { onclick: move |_| player::play_url(&url.read()), "Load + play" }
                button { onclick: move |_| player::play(), "Play" }
                button { onclick: move |_| player::pause(), "Pause" }
            }
            p { "Status: {status}" }
            p { "Output: {route}" }
            pre { style: "font-size: 11px; white-space: pre-wrap;",
                for line in lines.read().iter().rev() {
                    "{line}\n"
                }
            }
        }
    }
}
