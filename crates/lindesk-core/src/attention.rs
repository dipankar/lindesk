//! The user can depend on more than the focused window at once.
use lindesk_plugin_api::QosClass;
use serde::{Deserialize, Serialize};

/// Why a workload is protected from throttling. Unknown presence is never guessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    Foreground,
    Critical,
    Media,
    Interactive,
    Visible,
    Dependency,
}

impl Attention {
    pub fn classify(class: QosClass, foreground: bool, visible: Option<bool>, audible: Option<bool>) -> Option<Self> {
        if foreground {
            Some(Self::Foreground)
        } else if class == QosClass::Critical {
            Some(Self::Critical)
        } else if audible == Some(true) {
            Some(Self::Media)
        } else if class == QosClass::Interactive {
            Some(Self::Interactive)
        } else if visible == Some(true) && class == QosClass::Normal {
            Some(Self::Visible)
        } else {
            None
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Critical => "critical",
            Self::Media => "playing or recording audio",
            Self::Interactive => "interactive work",
            Self::Visible => "visible app",
            Self::Dependency => "explicit dependency of an interactive app",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_work_and_media_survive_focus_changes_but_visible_builds_still_yield() {
        assert_eq!(Attention::classify(QosClass::Batch, false, Some(true), Some(false)), None);
        assert_eq!(Attention::classify(QosClass::Batch, false, Some(false), Some(true)), Some(Attention::Media));
        assert_eq!(Attention::classify(QosClass::Interactive, false, None, None), Some(Attention::Interactive));
        assert_eq!(Attention::classify(QosClass::Normal, false, None, None), None);
        assert_eq!(Attention::classify(QosClass::Normal, false, Some(true), None), Some(Attention::Visible));
    }
}
