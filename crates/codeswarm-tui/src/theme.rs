//! Resolve semantic UI colors against an explicit or terminal-owned canvas.
use ratatui::{buffer::Buffer, layout::Rect, style::Color};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Theme {
    #[default]
    Terminal,
    Light,
    Dark,
}

impl Theme {
    pub fn from_setting(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::Terminal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Terminal => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::Terminal,
        }
    }

    fn color(self, color: Color, background: bool) -> Color {
        use Color::*;
        if self == Self::Terminal {
            return match color {
                super::ACCENT => Cyan,
                super::SECONDARY_TEXT => Reset,
                super::THOUGHT_TEXT => DarkGray,
                _ => super::AGENT_COLORS
                    .iter()
                    .position(|candidate| *candidate == color)
                    .map(|index| [Magenta, Yellow, Red, Green][index])
                    .unwrap_or(color),
            };
        }
        let light = self == Self::Light;
        match color {
            Reset if background => {
                if light {
                    Rgb(250, 250, 250)
                } else {
                    Rgb(20, 22, 26)
                }
            }
            Reset | White | Black => {
                if light {
                    Rgb(28, 28, 30)
                } else {
                    Rgb(230, 233, 239)
                }
            }
            super::ACCENT | Cyan | LightCyan => {
                if light {
                    Rgb(0, 105, 100)
                } else {
                    Rgb(74, 210, 200)
                }
            }
            // Thought text is deliberately faint, exempt from the normal contrast target.
            super::THOUGHT_TEXT => {
                if light {
                    Rgb(155, 155, 160)
                } else {
                    Rgb(110, 114, 124)
                }
            }
            super::SECONDARY_TEXT | Gray | DarkGray => {
                if light {
                    Rgb(90, 90, 95)
                } else {
                    Rgb(164, 169, 180)
                }
            }
            Red | LightRed => {
                if light {
                    Rgb(170, 30, 45)
                } else {
                    Rgb(255, 120, 135)
                }
            }
            Green | LightGreen => {
                if light {
                    Rgb(25, 110, 45)
                } else {
                    Rgb(110, 210, 140)
                }
            }
            Yellow | LightYellow => {
                if light {
                    Rgb(130, 85, 0)
                } else {
                    Rgb(235, 195, 100)
                }
            }
            Blue | LightBlue => {
                if light {
                    Rgb(35, 80, 170)
                } else {
                    Rgb(130, 175, 255)
                }
            }
            Magenta | LightMagenta => {
                if light {
                    Rgb(130, 45, 175)
                } else {
                    Rgb(205, 145, 245)
                }
            }
            _ => {
                if let Some(index) = super::AGENT_COLORS
                    .iter()
                    .position(|candidate| *candidate == color)
                {
                    if light {
                        [
                            Rgb(130, 45, 175),
                            Rgb(150, 80, 0),
                            Rgb(170, 35, 80),
                            Rgb(25, 110, 45),
                        ][index]
                    } else {
                        [
                            Rgb(205, 145, 245),
                            Rgb(240, 180, 100),
                            Rgb(255, 135, 175),
                            Rgb(110, 210, 140),
                        ][index]
                    }
                } else {
                    color
                }
            }
        }
    }

    pub fn apply(self, buffer: &mut Buffer, area: Rect) {
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let cell = &mut buffer[(x, y)];
                cell.fg = self.color(cell.fg, false);
                cell.bg = self.color(cell.bg, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("explicit palette color")
        };
        [r, g, b]
            .into_iter()
            .zip([0.2126, 0.7152, 0.0722])
            .map(|(v, weight)| {
                let v = f64::from(v) / 255.0;
                weight
                    * if v <= 0.04045 {
                        v / 12.92
                    } else {
                        ((v + 0.055) / 1.055).powf(2.4)
                    }
            })
            .sum()
    }
    #[test]
    fn explicit_palettes_keep_text_readable_on_their_canvas() {
        for theme in [Theme::Light, Theme::Dark] {
            let bg = luminance(theme.color(Color::Reset, true));
            for role in [
                Color::Reset,
                super::super::ACCENT,
                super::super::SECONDARY_TEXT,
                Color::Red,
                Color::Yellow,
            ]
            .into_iter()
            .chain(super::super::AGENT_COLORS)
            {
                let fg = luminance(theme.color(role, false));
                assert!(
                    (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05) >= 4.5,
                    "{theme:?} {role:?}"
                );
            }
        }
        assert_eq!(Theme::from_setting("invalid"), Theme::Terminal);
        assert_eq!(Theme::Terminal.color(Color::Reset, true), Color::Reset);
    }
}
