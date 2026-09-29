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
    /// Lighter sheen of the accent — shimmer peaks and brand gradients.
    pub glow: Color,
    pub mission: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub selection: Color,
    /// Gel backdrop + pixel art: only where real RGB colours are guaranteed.
    pub gel: bool,
}

impl Theme {
    pub fn name(value: Option<&str>) -> &'static str {
        match value {
            Some("terminal") => "terminal",
            Some("dark") => "dark",
            _ => "slime",
        }
    }

    pub fn next(value: Option<&str>) -> &'static str {
        match Self::name(value) {
            "slime" => "terminal",
            "terminal" => "dark",
            _ => "slime",
        }
    }

    pub fn new(value: Option<&str>) -> Self {
        let name = Self::name(value);
        if name == "slime" {
            // Black-navy backdrop, deep-blue panels, azure accent.
            Self {
                background: Color::Rgb(5, 9, 20),
                surface: Color::Rgb(10, 17, 38),
                text: Color::Rgb(226, 236, 252),
                muted: Color::Rgb(122, 143, 186),
                border: Color::Rgb(34, 60, 118),
                accent: Color::Rgb(92, 162, 255),
                glow: Color::Rgb(150, 214, 255),
                mission: Color::Rgb(190, 150, 255),
                success: Color::Rgb(96, 226, 160),
                warning: Color::Rgb(240, 200, 110),
                error: Color::Rgb(255, 120, 146),
                selection: Color::Rgb(22, 50, 112),
                gel: true,
            }
        } else if name == "terminal" {
            Self {
                background: Color::Reset,
                surface: Color::Reset,
                text: Color::Reset,
                muted: Color::Reset,
                border: Color::DarkGray,
                accent: Color::Reset,
                glow: Color::Reset,
                mission: Color::Reset,
                success: Color::Reset,
                warning: Color::Reset,
                error: Color::Reset,
                selection: Color::DarkGray,
                gel: false,
            }
        } else {
            Self {
                background: Color::Rgb(20, 23, 29),
                surface: Color::Rgb(29, 33, 41),
                text: Color::Rgb(230, 234, 240),
                muted: Color::Rgb(150, 160, 176),
                border: Color::Rgb(62, 72, 88),
                accent: Color::Rgb(105, 212, 225),
                glow: Color::Rgb(170, 232, 245),
                mission: Color::Rgb(187, 164, 247),
                success: Color::Rgb(143, 206, 158),
                warning: Color::Rgb(237, 196, 121),
                error: Color::Rgb(245, 139, 148),
                selection: Color::Rgb(50, 66, 82),
                gel: false,
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
