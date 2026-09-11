use std::io::{BufRead, Write};

/// Permission gate for mutating tools. y = once, a = rest of session,
/// n = deny (returned to the model as a tool result).
pub struct Gate {
    auto: bool,
    session_allow: bool,
}

impl Gate {
    pub fn new(auto: bool) -> Self {
        Self {
            auto,
            session_allow: false,
        }
    }

    /// Returns true if the action may proceed.
    pub fn check(&mut self, summary: &str) -> bool {
        if self.auto || self.session_allow {
            eprintln!("» allow (auto): {summary}");
            return true;
        }
        eprint!("» allow {summary}? [y/n/a] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        match line.trim().to_lowercase().as_str() {
            "y" | "yes" | "" => true, // empty = yes, single-keystroke flow
            "a" | "all" => {
                self.session_allow = true;
                true
            }
            _ => false,
        }
    }
}
