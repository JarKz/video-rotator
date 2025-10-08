use std::sync::LazyLock;

use gettext::Catalog;

const MO_RU: &[u8] = include_bytes!("../locales/ru/LC_MESSAGES/video-rotator.mo");

pub static TRANSLATION: LazyLock<Translation> =
    LazyLock::new(|| Translation::load().expect("The translation files should be fine"));

#[macro_export]
macro_rules! tr {
    ($tokens:tt) => {
        $crate::locale::TRANSLATION.gettext($tokens)
    };
}

pub struct Translation {
    _lang: Lang,
    catalog: Catalog,
}

impl Translation {
    pub fn load() -> anyhow::Result<Self> {
        let lang = Lang::load();
        let catalog = match lang {
            Lang::En => Catalog::empty(),
            Lang::Ru => Catalog::parse(MO_RU)?,
        };

        Ok(Translation {
            _lang: lang,
            catalog,
        })
    }

    pub fn gettext<'a, 'b: 'a>(&'a self, text: &'b str) -> &'a str {
        self.catalog.gettext(text)
    }
}

pub enum Lang {
    Ru,
    En,
}

impl Lang {
    pub fn load() -> Self {
        match sys_locale::get_locale() {
            Some(lang_code) => {
                if lang_code.starts_with("ru") {
                    Lang::Ru
                } else {
                    Lang::En
                }
            }
            None => Lang::En,
        }
    }
}
