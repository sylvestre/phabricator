//! The statuses of buildables, builds, targets and lint messages, with
//! the icons and colors PHP shows them with.

use crate::diff_html;

/// A status: its name, its Font Awesome icon and its color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Status {
    pub(crate) name: String,
    pub(crate) icon: &'static str,
    pub(crate) color: &'static str,
}

impl Status {
    fn new(name: impl Into<String>, icon: &'static str, color: &'static str) -> Self {
        Self {
            name: name.into(),
            icon,
            color,
        }
    }

    /// The icon, colored, with the name for screen readers and tooltips.
    pub(crate) fn icon_html(&self) -> String {
        let name = diff_html::escape(&self.name);
        format!(
            "<span class=\"fa {} remarkup-icon-{}\" title=\"{name}\" aria-hidden=\"true\"></span>",
            self.icon, self.color
        )
    }

    /// The icon and the name.
    pub(crate) fn html(&self) -> String {
        format!(
            "<span class=\"hm-status\">{} {}</span>",
            self.icon_html(),
            diff_html::escape(&self.name)
        )
    }
}

/// `HarbormasterBuildableStatus`.
pub(crate) fn buildable(status: &str) -> Status {
    match status {
        "preparing" => Status::new("Preparing", "fa-hourglass-o", "blue"),
        "building" => Status::new("Building", "fa-chevron-circle-right", "blue"),
        "passed" => Status::new("Passed", "fa-check-circle", "green"),
        "failed" => Status::new("Failed", "fa-times-circle", "red"),
        other => Status::new(
            format!("Unknown (\"{other}\")"),
            "fa-question-circle",
            "bluegrey",
        ),
    }
}

/// `HarbormasterBuildStatus`, with the pending statuses of commands.
pub(crate) fn build(status: &str) -> Status {
    let (name, icon, color) = match status {
        "inactive" => ("Inactive", "fa-circle-o", "dark"),
        "pending" => ("Pending", "fa-circle-o", "blue"),
        "building" => ("Building", "fa-chevron-circle-right", "blue"),
        "passed" => ("Passed", "fa-check-circle", "green"),
        "failed" => ("Failed", "fa-times-circle", "red"),
        "aborted" => ("Aborted", "fa-minus-circle", "red"),
        "error" => ("Unexpected Error", "fa-minus-circle", "red"),
        "paused" => ("Paused", "fa-pause", "yellow"),
        "deadlocked" => ("Deadlocked", "fa-exclamation-circle", "red"),
        "x-pausing" => ("Pausing", "fa-exclamation-triangle", "red"),
        "x-resuming" => ("Resuming", "fa-exclamation-triangle", "red"),
        "x-restarting" => ("Restarting", "fa-exclamation-triangle", "red"),
        "x-aborting" => ("Aborting", "fa-exclamation-triangle", "red"),
        other => {
            return Status::new(
                format!("Unknown (\"{other}\")"),
                "fa-question-circle",
                "bluegrey",
            );
        }
    };
    Status::new(name, icon, color)
}

/// `HarbormasterBuildStatus::isComplete()`.
pub(crate) fn build_is_complete(status: &str) -> bool {
    matches!(
        status,
        "passed" | "failed" | "aborted" | "error" | "deadlocked"
    )
}

/// `HarbormasterBuildTarget::getBuildTargetStatusName()`, `...Icon()` and
/// `...Color()` (`PHUIStatusItemView` icons).
pub(crate) fn target(status: &str) -> Status {
    let (name, icon, color) = match status {
        "target/pending" => ("Pending", "fa-circle-o", "blue"),
        "target/building" => ("Building", "fa-chevron-circle-right", "blue"),
        "target/waiting" => ("Waiting for Message", "fa-chevron-circle-right", "blue"),
        "target/passed" => ("Passed", "fa-check-circle", "green"),
        "target/failed" => ("Failed", "fa-times-circle", "red"),
        "target/aborted" => ("Aborted", "fa-minus-circle", "red"),
        _ => ("Unknown", "fa-question-circle", "bluegrey"),
    };
    Status::new(name, icon, color)
}

/// `ArcanistLintSeverity::getLintSeverities()`: the name of a severity,
/// or the severity itself.
pub(crate) fn lint_severity(severity: &str) -> &str {
    match severity {
        "advice" => "Advice",
        "autofix" => "Auto-Fix",
        "warning" => "Warning",
        "error" => "Error",
        "disabled" => "Disabled",
        other => other,
    }
}

/// `HarbormasterBuildLintMessage::getSortKey()`.
pub(crate) fn lint_sort_key(severity: &str, path: &str, line: Option<i64>, code: &str) -> String {
    let rank = match severity {
        "error" => "A",
        "warning" => "B",
        "autofix" => "C",
        "advice" => "Y",
        "disabled" => "Z",
        _ => "N",
    };
    format!("{rank}\0{path}\0{:08}\0{code}", line.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_like_php() {
        assert_eq!(buildable("passed").name, "Passed");
        assert_eq!(buildable("odd").name, "Unknown (\"odd\")");
        assert_eq!(build("x-restarting").name, "Restarting");
        assert_eq!(build("error").name, "Unexpected Error");
        assert_eq!(target("target/waiting").name, "Waiting for Message");
        assert_eq!(target("target/weird").name, "Unknown");
        assert!(build_is_complete("deadlocked"));
        assert!(!build_is_complete("paused"));
        assert_eq!(lint_severity("autofix"), "Auto-Fix");
        assert_eq!(lint_severity("odd"), "odd");
    }

    #[test]
    fn lint_messages_sort_by_severity_then_place() {
        let mut keys = [
            lint_sort_key("warning", "a", Some(1), "W"),
            lint_sort_key("error", "b", Some(10), "E"),
            lint_sort_key("error", "b", Some(9), "E"),
        ];
        keys.sort();
        assert!(keys[0].contains("00000009"));
        assert!(keys[2].starts_with('B'));
    }
}
