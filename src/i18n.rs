//! Interface language: Russian or English.
//!
//! Defaults to the system: Russian when the first macOS language is Russian, English otherwise. Settings can
//! pin either one (`LangPref` in `state.json`). Every interface string lives in `locales/app.yml` (rust-i18n,
//! the same format GPUI Kit uses for its own labels): `t("section.key")` for fixed text,
//! `tr!("section.key", name = value)` with arguments, `count(n, "section.key")` for numbers with words. Each
//! language is written as its own interface, not a literal translation (Design Guides, Interface language).

use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

use serde::{Deserialize, Serialize};

/// What is chosen in Settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LangPref {
    #[default]
    System,
    Ru,
    En,
}

/// The language the interface is shown in now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Ru,
    En,
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The system language: the first preferred language in macOS.
pub fn system() -> Lang {
    let first = sys_locale::get_locales().next().unwrap_or_default();
    if first.to_lowercase().starts_with("ru") { Lang::Ru } else { Lang::En }
}

/// The language a Settings choice resolves to.
pub fn resolve(pref: LangPref) -> Lang {
    match pref {
        LangPref::System => system(),
        LangPref::Ru => Lang::Ru,
        LangPref::En => Lang::En,
    }
}

/// Switch the language: Pult's strings and the labels inside GPUI Kit components.
pub fn apply(pref: LangPref) {
    let lang = resolve(pref);
    CURRENT.store(if lang == Lang::Ru { 0 } else { 1 }, Ordering::Relaxed);
    gpui_kit::component::set_locale(if lang == Lang::Ru { "ru" } else { "en" });
}

pub fn lang() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == 0 { Lang::Ru } else { Lang::En }
}

pub fn is_ru() -> bool {
    lang() == Lang::Ru
}

/// The locale name in `locales/app.yml`.
pub fn locale() -> &'static str {
    if is_ru() { "ru" } else { "en" }
}

/// The text of a key from `locales/app.yml` in the current language. A missing key shows itself, so it is
/// noticed instead of failing silently.
pub fn t(key: &'static str) -> &'static str {
    match crate::_rust_i18n_try_translate(locale(), key) {
        Some(Cow::Borrowed(text)) => text,
        // The built-in backend lends static text; anything else is kept once per key.
        Some(Cow::Owned(text)) => owned(key, text),
        None => key,
    }
}

fn owned(key: &'static str, text: String) -> &'static str {
    static KEPT: OnceLock<Mutex<HashMap<(&'static str, &'static str), &'static str>>> = OnceLock::new();
    let mut kept = KEPT.get_or_init(Default::default).lock().unwrap_or_else(|error| error.into_inner());
    kept.entry((locale(), key)).or_insert_with(|| Box::leak(text.into_boxed_str()))
}

/// A number with the word in the right form. The key holds `one`, `few` and `many` forms (English uses
/// `one` and `many`); `%{count}` in a form is replaced by the number.
pub fn count(n: u64, key: &'static str) -> String {
    let form = match plural(lang(), n) {
        Plural::One => "one",
        Plural::Few => "few",
        Plural::Many => "many",
    };
    let full = format!("{key}.{form}");
    let text = crate::_rust_i18n_try_translate(locale(), &full).map(Cow::into_owned).unwrap_or(full);
    text.replace("%{count}", &n.to_string())
}

#[derive(Debug, PartialEq, Eq)]
enum Plural {
    One,
    Few,
    Many,
}

/// Plural rules: Russian one (1, 21), few (2-4, 22-24), many (the rest); English one (1) and many.
fn plural(lang: Lang, n: u64) -> Plural {
    match lang {
        Lang::Ru => match (n % 10, n % 100) {
            (1, rest) if rest != 11 => Plural::One,
            (2..=4, rest) if !(12..=14).contains(&rest) => Plural::Few,
            _ => Plural::Many,
        },
        Lang::En if n == 1 => Plural::One,
        Lang::En => Plural::Many,
    }
}

/// A decimal number in a string: a comma in Russian, a point in English.
pub fn decimal(text: &str) -> String {
    if is_ru() { text.replace('.', ",") } else { text.to_string() }
}

/// A text with arguments from `locales/app.yml`: `tr!("workspace.paused", name = project.name())` fills
/// `%{name}`; a format spec goes after the value: `value = size : {:.1}`.
#[macro_export]
macro_rules! tr {
    ($key:literal $(, $($rest:tt)*)?) => {
        rust_i18n::t!($key, locale = $crate::i18n::locale() $(, $($rest)*)?).into_owned()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_forms() {
        assert_eq!(plural(Lang::Ru, 1), Plural::One);
        assert_eq!(plural(Lang::Ru, 3), Plural::Few);
        assert_eq!(plural(Lang::Ru, 11), Plural::Many);
        assert_eq!(plural(Lang::Ru, 21), Plural::One);
        assert_eq!(plural(Lang::Ru, 25), Plural::Many);
        assert_eq!(plural(Lang::En, 1), Plural::One);
        assert_eq!(plural(Lang::En, 21), Plural::Many);
    }

    /// Every leaf in `locales/app.yml` has both a Russian and an English text.
    #[test]
    fn every_key_has_both_languages() {
        let text = include_str!("../locales/app.yml");
        let mut missing = Vec::new();
        let mut path: Vec<(usize, String)> = Vec::new();
        let mut current: Option<(String, bool, bool)> = None;
        let close = |entry: Option<(String, bool, bool)>, missing: &mut Vec<String>| {
            if let Some((key, ru, en)) = entry
                && !(ru && en)
            {
                missing.push(key);
            }
        };
        for line in text.lines().filter(|line| !line.trim().is_empty() && !line.starts_with('_')) {
            let indent = line.len() - line.trim_start().len();
            let (name, value) = line.trim().split_once(':').expect("key: value");
            path.retain(|(level, _)| *level < indent);
            if name == "ru" || name == "en" {
                let key = path.iter().map(|(_, part)| part.as_str()).collect::<Vec<_>>().join(".");
                if current.as_ref().is_none_or(|(open, _, _)| *open != key) {
                    close(current.take(), &mut missing);
                    current = Some((key, false, false));
                }
                let entry = current.as_mut().expect("entry");
                if name == "ru" { entry.1 = true } else { entry.2 = true }
                assert!(!value.trim().is_empty(), "empty {name} at {}", entry.0);
            } else {
                path.push((indent, name.to_string()));
            }
        }
        close(current.take(), &mut missing);
        assert!(missing.is_empty(), "keys without both languages: {missing:?}");
    }
}
