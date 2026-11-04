//! Every string the App shows a person inside the App, in English and German, from Fluent
//! resources embedded in the binary. The exception are the Info.plist strings (the display
//! name and the local network usage text): they stay English, since dx doesn't generate
//! `InfoPlist.strings`.
//!
//! The language follows the device unless the person picked one in Settings
//! ([`LanguageChoice`]); a device language other than German falls back to English.

use std::sync::LazyLock;

use fluent_bundle::FluentResource;
use fluent_bundle::concurrent::FluentBundle;
use serde::{Deserialize, Serialize};
use unic_langid::LanguageIdentifier;

/// A language the App is translated into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    English,
    German,
}

impl Language {
    const ALL: [Language; 2] = [Language::English, Language::German];

    /// The language for a device locale such as `de-DE` or `de_AT`: German for any German
    /// locale, otherwise English.
    pub fn for_locale(locale: Option<&str>) -> Self {
        let primary = locale
            .and_then(|locale| locale.split(['-', '_']).next())
            .unwrap_or_default();
        if primary.eq_ignore_ascii_case("de") {
            Self::German
        } else {
            Self::English
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::German => "de",
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::English => include_str!("../locales/en.ftl"),
            Self::German => include_str!("../locales/de.ftl"),
        }
    }
}

/// The language a person picked in Settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageChoice {
    /// Follow the device's language.
    #[default]
    System,
    English,
    German,
}

impl LanguageChoice {
    pub const ALL: [LanguageChoice; 3] = [
        LanguageChoice::System,
        LanguageChoice::English,
        LanguageChoice::German,
    ];

    /// The language to show on a device set to `device`.
    pub fn resolve(self, device: Language) -> Language {
        match self {
            Self::System => device,
            Self::English => Language::English,
            Self::German => Language::German,
        }
    }

    /// How Settings names this choice.
    pub fn text(self) -> Text {
        match self {
            Self::System => Text::LanguageSystem,
            Self::English => Text::LanguageEnglish,
            Self::German => Text::LanguageGerman,
        }
    }
}

/// Declares [`Text`] with its Fluent message ids, and [`Text::ALL`] listing every variant.
macro_rules! texts {
    ($($variant:ident => $id:literal,)*) => {
        /// A message the App shows, by its Fluent id.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Text {
            $($variant,)*
        }

        impl Text {
            /// Every message, so a test can check that each language has all of them.
            pub const ALL: &[Text] = &[$(Text::$variant,)*];

            /// The Fluent message id.
            pub fn id(self) -> &'static str {
                match self {
                    $(Text::$variant => $id,)*
                }
            }
        }
    };
}

texts! {
    AppName => "app-name",
    Starting => "starting",
    Busy => "busy",
    BridgeUrlTitle => "bridge-url-title",
    BridgeUrlIntro => "bridge-url-intro",
    BridgeUrlLabel => "bridge-url-label",
    BridgeUrlConnect => "bridge-url-connect",
    LoginTitle => "login-title",
    LoginLibraryServer => "login-library-server",
    LoginApiKey => "login-api-key",
    LoginApiKeyHint => "login-api-key-hint",
    LoginSubmit => "login-submit",
    LoginNoLibraryServers => "login-no-library-servers",
    LoginRetry => "login-retry",
    LoginChangeBridge => "login-change-bridge",
    Bridge => "bridge",
    SettingsTitle => "settings-title",
    SettingsLibraryServer => "settings-library-server",
    SettingsAccount => "settings-account",
    SettingsLanguage => "settings-language",
    SettingsLogout => "settings-logout",
    LanguageSystem => "language-system",
    LanguageEnglish => "language-english",
    LanguageGerman => "language-german",
    ErrorBridgeUrlInvalid => "error-bridge-url-invalid",
    ErrorStorageLoad => "error-storage-load",
    ErrorStorageStuck => "error-storage-stuck",
    ErrorStorageSave => "error-storage-save",
    ErrorLogoutNotSaved => "error-logout-not-saved",
    ErrorNotABridge => "error-not-a-bridge",
    ErrorBridgeUnreachable => "error-bridge-unreachable",
    ErrorInternal => "error-internal",
    ErrorApiKeyRejected => "error-api-key-rejected",
    ErrorLibraryServerNotConfigured => "error-library-server-not-configured",
    ErrorLibraryServerUnreachable => "error-library-server-unreachable",
    ErrorSessionInvalid => "error-session-invalid",
    ErrorUnknown => "error-unknown",
}

impl From<&api::ApiError> for Text {
    fn from(error: &api::ApiError) -> Self {
        use api::ApiError;
        match error {
            ApiError::BridgeUnreachable => Self::ErrorBridgeUnreachable,
            ApiError::NotABridge => Self::ErrorNotABridge,
            ApiError::Internal => Self::ErrorInternal,
            ApiError::ApiKeyRejected => Self::ErrorApiKeyRejected,
            ApiError::LibraryServerNotConfigured => Self::ErrorLibraryServerNotConfigured,
            ApiError::LibraryServerUnreachable => Self::ErrorLibraryServerUnreachable,
            ApiError::SessionInvalid => Self::ErrorSessionInvalid,
            ApiError::Unknown => Self::ErrorUnknown,
        }
    }
}

/// One bundle per language, parsed once.
static BUNDLES: LazyLock<[FluentBundle<FluentResource>; 2]> =
    LazyLock::new(|| Language::ALL.map(bundle));

fn bundle(language: Language) -> FluentBundle<FluentResource> {
    let tag: LanguageIdentifier = language.tag().parse().expect("valid language tag");
    let resource = FluentResource::try_new(language.source().to_owned())
        .unwrap_or_else(|(_, errors)| panic!("{} messages: {errors:?}", language.tag()));
    let mut bundle = FluentBundle::new_concurrent(vec![tag]);
    // Unicode isolation marks only matter for right-to-left languages, which the App lacks.
    bundle.set_use_isolating(false);
    bundle
        .add_resource(resource)
        .unwrap_or_else(|errors| panic!("{} messages: {errors:?}", language.tag()));
    bundle
}

/// `text` in `language`. A missing or broken message shows its id, which the completeness test
/// rules out.
pub fn text(language: Language, text: Text) -> String {
    format(language, text).unwrap_or_else(|_| text.id().to_owned())
}

/// `text` in `language`, or what is wrong with its message.
fn format(language: Language, text: Text) -> Result<String, String> {
    let [english, german] = &*BUNDLES;
    let bundle = match language {
        Language::English => english,
        Language::German => german,
    };
    let pattern = bundle
        .get_message(text.id())
        .and_then(|message| message.value())
        .ok_or("no message")?;
    let mut errors = Vec::new();
    let formatted = bundle.format_pattern(pattern, None, &mut errors);
    if errors.is_empty() {
        Ok(formatted.into_owned())
    } else {
        Err(format!("{errors:?}"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use api::ApiError;

    use super::{Language, Text, format};

    #[test]
    fn every_message_exists_in_every_language_without_errors() {
        for language in Language::ALL {
            for &text in Text::ALL {
                if let Err(error) = format(language, text) {
                    panic!("{language:?} {}: {error}", text.id());
                }
            }
        }
    }

    #[test]
    fn no_language_has_messages_the_app_does_not_use() {
        let used: BTreeSet<_> = Text::ALL.iter().map(|text| text.id()).collect();
        for language in Language::ALL {
            let resource = fluent_syntax::parser::parse(language.source())
                .unwrap_or_else(|(_, errors)| panic!("{language:?}: {errors:?}"));
            let defined: BTreeSet<_> = resource
                .body
                .iter()
                .filter_map(|entry| match entry {
                    fluent_syntax::ast::Entry::Message(message) => Some(message.id.name),
                    _ => None,
                })
                .collect();
            assert_eq!(defined, used, "{language:?}");
        }
    }

    /// Every `error-<code>` message, with the [`ApiError`] its code decodes to. `Text::from`
    /// matches every [`ApiError`] exhaustively, so a new code doesn't compile without a message.
    fn api_error_messages() -> Vec<(Text, ApiError)> {
        Text::ALL
            .iter()
            .filter_map(|&text| {
                let code = text.id().strip_prefix("error-")?.replace('-', "_");
                let error: ApiError =
                    serde_json::from_value(serde_json::json!({ "code": code })).ok()?;
                (error != ApiError::Unknown || code == "unknown").then_some((text, error))
            })
            .collect()
    }

    #[test]
    fn every_api_error_code_shows_its_own_message() {
        let messages = api_error_messages();
        for (text, error) in &messages {
            assert_eq!(Text::from(error), *text, "{error:?}");
        }
        // One message per code (ApiError has 8), each different in every language.
        assert_eq!(messages.len(), 8, "{messages:?}");
        for language in Language::ALL {
            let distinct: BTreeSet<_> = messages
                .iter()
                .map(|(text, _)| super::text(language, *text))
                .collect();
            assert_eq!(distinct.len(), messages.len(), "{language:?}");
        }
    }

    #[test]
    fn german_locales_choose_german_and_others_english() {
        for (locale, language) in [
            (Some("de-DE"), Language::German),
            (Some("de_AT"), Language::German),
            (Some("DE"), Language::German),
            (Some("en-US"), Language::English),
            (Some("fr-FR"), Language::English),
            (Some("dex"), Language::English),
            (None, Language::English),
        ] {
            assert_eq!(Language::for_locale(locale), language, "{locale:?}");
        }
    }
}
