use same_core::Language;

pub fn t(lang: Language, ja: &'static str, en: &'static str) -> &'static str {
    match lang {
        Language::Ja => ja,
        Language::En => en,
    }
}
