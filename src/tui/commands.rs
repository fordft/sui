//! A bounded registry shared by palette, shortcuts, and slash commands.
use super::app::{App, Mode, Tab};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    View(Tab),
    Mode(Mode),
    Sidebar,
    Reasoning,
    Theme,
    Motion,
    Export,
    Help,
    Stop,
    Quit,
}
impl Command {
    pub const ALL: [Self; 15] = [
        Self::View(Tab::Chat),
        Self::View(Tab::Tasks),
        Self::View(Tab::Changes),
        Self::View(Tab::Usage),
        Self::View(Tab::Settings),
        Self::Mode(Mode::Solo),
        Self::Mode(Mode::Mission),
        Self::Sidebar,
        Self::Reasoning,
        Self::Theme,
        Self::Motion,
        Self::Export,
        Self::Help,
        Self::Stop,
        Self::Quit,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::View(Tab::Chat) => "Chat",
            Self::View(Tab::Tasks) => "Tasks",
            Self::View(Tab::Changes) => "Changes",
            Self::View(Tab::Usage) => "Usage",
            Self::View(Tab::Settings) => "Settings",
            Self::Mode(Mode::Solo) => "Switch to Solo",
            Self::Mode(Mode::Mission) => "Switch to Mission",
            Self::Sidebar => "Toggle sidebar",
            Self::Reasoning => "Cycle reasoning display",
            Self::Theme => "Toggle theme: slime / terminal / dark",
            Self::Motion => "Cycle motion: full / calm / off",
            Self::Export => "Export run report",
            Self::Help => "Help",
            Self::Stop => "Stop task",
            Self::Quit => "Quit",
        }
    }
    pub fn shortcut(self) -> &'static str {
        match self {
            Self::Sidebar => "Ctrl+B",
            Self::Reasoning => "Ctrl+R",
            Self::Help => "F1",
            Self::Stop => "Ctrl+S",
            Self::Quit => "Ctrl+Q",
            Self::Export => "/export",
            _ => "",
        }
    }
    pub fn unavailable(self, app: &App) -> Option<&'static str> {
        match self {
            Self::Stop if !app.running => Some("no task running"),
            Self::Mode(_) if app.running => Some("stop the current task first"),
            Self::Export if app.run_dir.is_none() => Some("no run journal available"),
            _ => None,
        }
    }
    pub fn matching(query: &str) -> Vec<Self> {
        let query = query.to_lowercase();
        Self::ALL
            .into_iter()
            .filter(|c| c.label().to_lowercase().contains(&query))
            .collect()
    }
}
