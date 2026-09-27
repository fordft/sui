//! Semantic presentation colors. Terminal mode leaves the user's base colors alone.
use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy)]
pub struct Theme {
    pub background: Color,
    pub surface: Color,
    pub text: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub mission: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub selection: Color,
}

impl Theme {
    pub fn name(value: Option<&str>) -> &'static str {
        if value == Some("terminal") {
            "terminal"
        } else {
            "dark"
        }
    }

    pub fn new(value: Option<&str>) -> Self {
        if Self::name(value) == "terminal" {
            Self {
                background: Color::Reset,
                surface: Color::Reset,
                text: Color::Reset,
                muted: Color::Reset,
                border: Color::DarkGray,
                accent: Color::Reset,
                mission: Color::Reset,
                success: Color::Reset,
                warning: Color::Reset,
                error: Color::Reset,
                selection: Color::DarkGray,
            }
        } else {
            Self {
                background: Color::Rgb(20, 23, 29),
                surface: Color::Rgb(29, 33, 41),
                text: Color::Rgb(230, 234, 240),
                muted: Color::Rgb(150, 160, 176),
                border: Color::Rgb(62, 72, 88),
                accent: Color::Rgb(105, 212, 225),
                mission: Color::Rgb(187, 164, 247),
                success: Color::Rgb(143, 206, 158),
                warning: Color::Rgb(237, 196, 121),
                error: Color::Rgb(245, 139, 148),
                selection: Color::Rgb(50, 66, 82),
            }
        }
    }

    pub fn base(self) -> Style {
        Style::default().fg(self.text).bg(self.background)
    }
    pub fn panel(self) -> Style {
        self.base().bg(self.surface)
    }
    pub fn selected(self) -> Style {
        // Underline survives NO_COLOR; explicit white also contrasts on light terminals.
        Style::default()
            .fg(Color::White)
            .bg(self.selection)
            .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
    }
}
