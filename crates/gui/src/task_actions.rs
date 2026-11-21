//! Per-state capability predicates shared by the toolbar, the row, and the
//! detail window. One match, so a state cannot enable one surface and not
//! the other.

use usagi::store::records::TaskState;

/// What one real task can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StripCaps {
    pub can_pause: bool,
    pub can_resume: bool,
    pub can_cancel: bool,
    pub can_retry: bool,
    pub can_open_file: bool,
    pub can_open_folder: bool,
    pub can_remove: bool,
    pub can_copy: bool,
}

pub(crate) fn caps_for_state(state: TaskState) -> StripCaps {
    StripCaps {
        can_pause: matches!(
            state,
            TaskState::Queued | TaskState::Probing | TaskState::Downloading | TaskState::Verifying
        ),
        can_resume: matches!(state, TaskState::Paused | TaskState::Canceled),
        can_cancel: matches!(
            state,
            TaskState::Queued | TaskState::Probing | TaskState::Downloading | TaskState::Verifying
        ),
        can_retry: state == TaskState::Failed,
        can_open_file: state == TaskState::Completed,
        can_open_folder: matches!(state, TaskState::Completed | TaskState::Paused),
        can_remove: true,
        can_copy: true,
    }
}
