//! Colours. Kept in one place so the board reads consistently.

use ratatui::style::{Color, Modifier, Style};

use crate::model::Status;

/// Colour for a task's status.
pub fn status_color(status: Status) -> Color {
    match status {
        Status::Open => Color::Cyan,
        Status::Claimed => Color::Yellow,
        Status::InProgress => Color::Green,
        Status::Done => Color::Blue,
        Status::Failed => Color::Red,
        Status::Cancelled => Color::DarkGray,
    }
}

/// Colour for a harness badge.
///
/// The three harnesses named in the design get fixed, distinguishable colours;
/// anything else is hashed into the remaining palette so two unknown harnesses
/// still look different from each other.
pub fn harness_color(harness: &str) -> Color {
    match harness {
        "claude-code" => Color::Rgb(217, 119, 87),
        "codex" => Color::Rgb(120, 180, 255),
        "copilot" => Color::Rgb(160, 140, 220),
        "cli" | "tui" | "hird" => Color::DarkGray,
        other => {
            const PALETTE: [Color; 6] = [
                Color::LightGreen,
                Color::LightYellow,
                Color::LightMagenta,
                Color::LightCyan,
                Color::LightRed,
                Color::LightBlue,
            ];
            // FNV-1a: stable across runs, unlike DefaultHasher.
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in other.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            PALETTE[(hash % PALETTE.len() as u64) as usize]
        }
    }
}

/// Style for the badge showing who holds a task.
pub fn badge_style(harness: &str) -> Style {
    Style::default()
        .fg(harness_color(harness))
        .add_modifier(Modifier::BOLD)
}

/// Style for the selected row in a list.
pub fn selection_style() -> Style {
    Style::default()
        .bg(Color::Indexed(238))
        .add_modifier(Modifier::BOLD)
}

/// Style for the column or pane that currently has focus.
pub fn focus_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

/// Style for chrome the eye should skip past.
pub fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Style for a lease that has run out but not yet been swept.
pub fn overdue_style() -> Style {
    Style::default()
        .fg(Color::Red)
        .add_modifier(Modifier::SLOW_BLINK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_has_its_own_colour() {
        let colors: std::collections::HashSet<_> =
            Status::ALL.into_iter().map(status_color).collect();
        assert_eq!(colors.len(), Status::ALL.len());
    }

    #[test]
    fn the_named_harnesses_are_distinguishable() {
        let a = harness_color("claude-code");
        let b = harness_color("codex");
        let c = harness_color("copilot");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn unknown_harnesses_hash_to_a_stable_colour() {
        assert_eq!(harness_color("gemini-cli"), harness_color("gemini-cli"));
        assert_ne!(harness_color("gemini-cli"), harness_color("aider"));
    }

    #[test]
    fn human_actors_are_dimmed_rather_than_coloured() {
        for actor in ["cli", "tui", "hird"] {
            assert_eq!(harness_color(actor), Color::DarkGray);
        }
    }
}
