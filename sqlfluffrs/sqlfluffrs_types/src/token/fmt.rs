use super::Token;
use std::fmt::Display;

impl Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A `Display` impl must never panic. Synthetic tokens built for grammar
        // re-validation (segment reparse) carry no position marker, so fall back
        // to a placeholder rather than `expect`-panicking when one is formatted.
        match &self.pos_marker {
            Some(pm) => write!(
                f,
                "<{}: ({}) '{}'>",
                self.class_name,
                pm,
                self.raw.as_str().escape_debug(),
            ),
            None => write!(
                f,
                "<{}: (?) '{}'>",
                self.class_name,
                self.raw.as_str().escape_debug(),
            ),
        }
    }
}
