pub mod characters;
pub mod cleaning;
pub mod notes;
pub mod ps_editor;
pub mod settings;
pub mod terms;
pub mod translation;
pub mod typing;
pub mod wiki;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AppTab {
    Translation,
    Cleaning,
    Typing,
    PsEditor,
    Characters,
    Terms,
    Notes,
    Settings,
    Wiki,
}

impl AppTab {
    pub const ALL: [AppTab; 9] = [
        AppTab::Translation,
        AppTab::Cleaning,
        AppTab::Typing,
        AppTab::PsEditor,
        AppTab::Characters,
        AppTab::Terms,
        AppTab::Notes,
        AppTab::Settings,
        AppTab::Wiki,
    ];

    pub fn title(self) -> &'static str {
        match self {
            AppTab::Translation => "Перевод",
            AppTab::Cleaning => "Клининг",
            AppTab::Typing => "Текст",
            AppTab::PsEditor => "PS-подобный редактор",
            AppTab::Characters => "Персонажи",
            AppTab::Terms => "Термины",
            AppTab::Notes => "Заметки перевода",
            AppTab::Settings => "Настройки",
            AppTab::Wiki => "Вики",
        }
    }
}
