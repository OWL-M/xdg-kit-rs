//! The user's languages, formatted for translation lookups in the
//! freedesktop.org specs.
//!
//! [`Locales::from_env`] reads them as gettext does, and [`Locales::keys`]
//! lists the keys to try in the order of the Desktop Entry spec
//! ("Localized values for keys"): `lang_COUNTRY@MODIFIER`, `lang_COUNTRY`,
//! `lang@MODIFIER`, `lang`, for each language in turn. The untranslated
//! value comes last; the caller must look it up.
//!
//! ```
//! use xdg_kit_locale::Locales;
//!
//! let vars = |name: &str| match name {
//!     "LANG" => Some("de_DE.UTF-8@euro".into()),
//!     _ => None,
//! };
//! let locales = Locales::from_vars(vars);
//! let keys: Vec<&str> = locales.keys().collect();
//! assert_eq!(keys, ["de_DE@euro", "de_DE", "de@euro", "de"]);
//! ```

use std::ffi::OsString;
use std::fmt;

/// One language: `lang`, optionally with a country and a modifier, as in
/// `sr_RS@latin`. The encoding of a POSIX locale name is not kept.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Locale {
    lang: String,
    country: Option<String>,
    modifier: Option<String>,
}

impl Locale {
    /// Parses `lang[_COUNTRY][.ENCODING][@MODIFIER]`. Returns `None` for an
    /// empty language or one with characters other than ASCII letters,
    /// digits and `-`, and for the `C` and `POSIX` locales, which have no
    /// language.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let (rest, modifier) = match name.split_once('@') {
            Some((rest, modifier)) => (rest, Some(modifier)),
            None => (name, None),
        };
        let rest = rest.split_once('.').map_or(rest, |(rest, _encoding)| rest);
        let (lang, country) = match rest.split_once('_') {
            Some((lang, country)) => (lang, Some(country)),
            None => (rest, None),
        };
        let plain = |part: &str| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        };
        if !plain(lang) || lang == "C" || lang == "POSIX" {
            return None;
        }
        if !country.is_none_or(plain) || !modifier.is_none_or(plain) {
            return None;
        }
        Some(Self {
            lang: lang.to_owned(),
            country: country.map(str::to_owned),
            modifier: modifier.map(str::to_owned),
        })
    }

    /// The language, as in `sr`.
    #[must_use]
    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// The country, as in `RS`.
    #[must_use]
    pub fn country(&self) -> Option<&str> {
        self.country.as_deref()
    }

    /// The modifier, as in `latin`.
    #[must_use]
    pub fn modifier(&self) -> Option<&str> {
        self.modifier.as_deref()
    }

    /// The keys to try for this language, most specific first, as the
    /// Desktop Entry spec orders them.
    fn variants(&self) -> Vec<String> {
        let lang = &self.lang;
        let mut keys = Vec::with_capacity(4);
        match (&self.country, &self.modifier) {
            (Some(country), Some(modifier)) => {
                keys.push(format!("{lang}_{country}@{modifier}"));
                keys.push(format!("{lang}_{country}"));
                keys.push(format!("{lang}@{modifier}"));
            }
            (Some(country), None) => keys.push(format!("{lang}_{country}")),
            (None, Some(modifier)) => keys.push(format!("{lang}@{modifier}")),
            (None, None) => {}
        }
        keys.push(lang.clone());
        keys
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.lang)?;
        if let Some(country) = &self.country {
            write!(f, "_{country}")?;
        }
        if let Some(modifier) = &self.modifier {
            write!(f, "@{modifier}")?;
        }
        Ok(())
    }
}

/// The user's languages, most preferred first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Locales {
    locales: Vec<Locale>,
    keys: Vec<String>,
}

impl Locales {
    /// Reads the languages from this process's environment; see
    /// [`Locales::from_vars`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// Reads the languages as gettext does, from `vars`, which returns the
    /// value of an environment variable by name.
    ///
    /// The locale is the first non-empty one of `LC_ALL`, `LC_MESSAGES` and
    /// `LANG`. When it is unset, `C` or `POSIX` (with or without an
    /// encoding), there is no language and nothing is translated, whatever
    /// `LANGUAGE` says. Otherwise `LANGUAGE`, a colon-separated list, gives
    /// the languages when it is non-empty, and the locale itself when it is
    /// not. An entry `C`, `C.<encoding>` or `POSIX` ends the list, as in
    /// gettext; untranslated text comes next. Unparseable names are skipped.
    /// A locale variable that is set but not UTF-8 still counts as set, so
    /// nothing is translated.
    #[must_use]
    pub fn from_vars(vars: impl Fn(&str) -> Option<OsString>) -> Self {
        let value = |name: &str| {
            vars(name)
                .and_then(|value| value.into_string().ok())
                .filter(|value| !value.is_empty())
        };
        let set = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .find_map(|name| vars(name).filter(|value| !value.is_empty()));
        let Some(locale) = set.and_then(|value| value.into_string().ok()) else {
            return Self::untranslated();
        };
        let base = locale.split(['.', '@']).next().unwrap_or_default();
        if base == "C" || base == "POSIX" {
            return Self::untranslated();
        }
        match value("LANGUAGE") {
            Some(languages) => {
                Self::new(languages.split(':').take_while(|name| {
                    !(*name == "C" || name.starts_with("C.") || *name == "POSIX")
                }))
            }
            None => Self::new([locale.as_str()]),
        }
    }

    /// The given languages, most preferred first. Names that cannot be
    /// parsed, and repeats, are skipped.
    pub fn new(names: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut locales: Vec<Locale> = Vec::new();
        for locale in names
            .into_iter()
            .filter_map(|name| Locale::parse(name.as_ref()))
        {
            if !locales.contains(&locale) {
                locales.push(locale);
            }
        }
        let mut keys: Vec<String> = Vec::new();
        for key in locales.iter().flat_map(Locale::variants) {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        Self { locales, keys }
    }

    /// No languages: only untranslated values are used.
    #[must_use]
    pub fn untranslated() -> Self {
        Self::default()
    }

    /// The languages, most preferred first.
    #[must_use]
    pub fn locales(&self) -> &[Locale] {
        &self.locales
    }

    /// The keys to try, best first: each language's variants in the Desktop
    /// Entry spec's order, each key once.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(String::as_str)
    }

    /// The first value `lookup` finds for a key, trying keys in the order of
    /// [`Locales::keys`]. The untranslated value is not tried.
    pub fn select<T>(&self, mut lookup: impl FnMut(&str) -> Option<T>) -> Option<T> {
        self.keys().find_map(&mut lookup)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn from(vars: &[(&str, &str)]) -> Locales {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).into()))
            .collect();
        Locales::from_vars(move |name| vars.get(name).cloned())
    }

    fn keys(locales: &Locales) -> Vec<&str> {
        locales.keys().collect()
    }

    #[test]
    fn a_full_name_gives_the_spec_order() {
        let locales = Locales::new(["sr_RS.UTF-8@latin"]);
        assert_eq!(keys(&locales), ["sr_RS@latin", "sr_RS", "sr@latin", "sr"]);
        assert_eq!(locales.locales()[0].to_string(), "sr_RS@latin");
    }

    #[test]
    fn shorter_names_give_their_own_orders() {
        assert_eq!(keys(&Locales::new(["pt_BR"])), ["pt_BR", "pt"]);
        assert_eq!(keys(&Locales::new(["ca@valencia"])), ["ca@valencia", "ca"]);
        assert_eq!(keys(&Locales::new(["fr"])), ["fr"]);
    }

    #[test]
    fn several_languages_keep_their_order_and_drop_repeats() {
        let locales = Locales::new(["de_AT", "de", "en_GB", "de_AT"]);
        assert_eq!(keys(&locales), ["de_AT", "de", "en_GB", "en"]);
        assert_eq!(locales.locales().len(), 3);
        let owned = vec![String::from("de_AT"), String::from("en_GB")];
        assert_eq!(keys(&Locales::new(&owned)), ["de_AT", "de", "en_GB", "en"]);
    }

    #[test]
    fn the_locale_comes_from_the_first_set_variable() {
        let locales = from(&[("LANG", "fr_FR.UTF-8"), ("LC_MESSAGES", "de_DE")]);
        assert_eq!(keys(&locales), ["de_DE", "de"]);
        let locales = from(&[("LC_ALL", "it_IT"), ("LC_MESSAGES", "de_DE")]);
        assert_eq!(keys(&locales), ["it_IT", "it"]);
        let locales = from(&[("LC_ALL", ""), ("LANG", "nl_NL")]);
        assert_eq!(keys(&locales), ["nl_NL", "nl"]);
    }

    #[test]
    fn language_overrides_the_locale() {
        let locales = from(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "fr_CA:fr::de")]);
        assert_eq!(keys(&locales), ["fr_CA", "fr", "de"]);
    }

    #[test]
    fn c_in_language_ends_the_list() {
        for stop in ["C", "C.UTF-8", "POSIX"] {
            let language = format!("fr:{stop}:de");
            let locales = from(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", &language)]);
            assert_eq!(keys(&locales), ["fr"], "{language}");
        }
        let locales = from(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "Ca:de")]);
        assert_eq!(keys(&locales), ["Ca", "de"], "Ca is not C");
    }

    #[test]
    fn the_c_locale_translates_nothing_whatever_language_says() {
        for locale in ["C", "POSIX", "C.UTF-8", "C@whatever"] {
            let locales = from(&[("LANG", locale), ("LANGUAGE", "fr")]);
            assert_eq!(keys(&locales), Vec::<&str>::new(), "{locale}");
        }
        assert_eq!(keys(&from(&[("LANGUAGE", "fr")])), Vec::<&str>::new());
    }

    #[test]
    fn a_locale_that_is_not_utf8_translates_nothing() {
        use std::os::unix::ffi::OsStringExt;
        let vars = |name: &str| match name {
            "LC_ALL" => Some(OsString::from_vec(b"de_DE.\xff".to_vec())),
            "LANG" => Some("fr_FR".into()),
            _ => None,
        };
        assert_eq!(keys(&Locales::from_vars(vars)), Vec::<&str>::new());
    }

    #[test]
    fn bad_names_are_skipped() {
        for bad in ["", "_DE", "de_", "de@", "d e", "de/../x", "C", "POSIX"] {
            assert_eq!(Locale::parse(bad), None, "{bad:?}");
        }
        assert_eq!(keys(&Locales::new(["", "de/x", "es"])), ["es"]);
    }

    #[test]
    fn select_tries_keys_in_order() {
        let locales = Locales::new(["de_AT"]);
        let table = [("de", "Bild"), ("en", "Image")];
        let found = locales.select(|key| {
            table
                .iter()
                .find(|(lang, _)| *lang == key)
                .map(|(_, value)| *value)
        });
        assert_eq!(found, Some("Bild"));
        assert_eq!(Locales::untranslated().select(|_| Some(1)), None);
    }
}
