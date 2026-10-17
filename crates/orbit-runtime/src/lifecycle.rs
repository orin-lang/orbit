/// Lifecycle state of an [`Isolate`](crate::Isolate).
///
/// ```text
/// Created --init--> Running --> Initialized --invoke--> Running --> Idle --invoke--> ...
///    any live state --> Stopping --> Terminated        Running/Created/... --trap--> Failed
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IsolateState {
    /// Instantiated; `_initialize`/`orbit_init` have not run.
    Created,
    /// Initialisation succeeded; no invocation has completed yet.
    Initialized,
    /// Between invocations, ready for the next one.
    Idle,
    /// Guest code is executing (initialisation or an invocation).
    Running,
    /// Reserved for host calls that suspend on I/O (no capability in milestone 1 does).
    WaitingForIo,
    /// `terminate` is in progress; resources are being released.
    Stopping,
    /// All resources are released. Terminal.
    Terminated,
    /// A trap, limit violation or failed initialisation. Only `terminate` is possible.
    Failed,
}

impl IsolateState {
    /// Whether the transition `self -> next` is allowed.
    #[must_use]
    pub fn can_transition_to(self, next: IsolateState) -> bool {
        use IsolateState::{
            Created, Failed, Idle, Initialized, Running, Stopping, Terminated, WaitingForIo,
        };
        matches!(
            (self, next),
            (Created | Initialized | Idle | WaitingForIo, Running)
                | (Running, Initialized | Idle | WaitingForIo | Failed)
                | (WaitingForIo, Failed)
                | (
                    Created | Initialized | Idle | Running | WaitingForIo | Failed,
                    Stopping
                )
                | (Stopping, Terminated)
                | (Failed, Terminated)
        )
    }

    /// Whether an invocation may start from this state.
    #[must_use]
    pub fn accepts_invocation(self) -> bool {
        matches!(self, IsolateState::Initialized | IsolateState::Idle)
    }

    /// Whether the isolate still holds resources.
    #[must_use]
    pub fn is_live(self) -> bool {
        !matches!(self, IsolateState::Terminated)
    }
}

/// Rejected lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid isolate transition {from:?} -> {to:?}")]
pub struct TransitionError {
    /// Current state.
    pub from: IsolateState,
    /// Requested state.
    pub to: IsolateState,
}

/// Validated holder of an [`IsolateState`].
#[derive(Debug)]
pub(crate) struct Lifecycle(IsolateState);

impl Lifecycle {
    pub(crate) fn new() -> Self {
        Self(IsolateState::Created)
    }

    pub(crate) fn get(&self) -> IsolateState {
        self.0
    }

    pub(crate) fn transition(&mut self, to: IsolateState) -> Result<(), TransitionError> {
        if self.0.can_transition_to(to) {
            self.0 = to;
            Ok(())
        } else {
            Err(TransitionError { from: self.0, to })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IsolateState::{self, *};
    use super::*;

    const ALL: [IsolateState; 8] = [
        Created,
        Initialized,
        Idle,
        Running,
        WaitingForIo,
        Stopping,
        Terminated,
        Failed,
    ];

    #[test]
    fn happy_path() {
        let mut l = Lifecycle::new();
        for s in [
            Running,
            Initialized,
            Running,
            Idle,
            Running,
            Idle,
            Stopping,
            Terminated,
        ] {
            l.transition(s).unwrap();
            assert_eq!(l.get(), s);
        }
    }

    #[test]
    fn failure_path() {
        let mut l = Lifecycle::new();
        l.transition(Running).unwrap();
        l.transition(Failed).unwrap();
        assert!(l.transition(Running).is_err());
        assert!(l.transition(Idle).is_err());
        l.transition(Stopping).unwrap();
        l.transition(Terminated).unwrap();
    }

    #[test]
    fn terminated_is_terminal() {
        for to in ALL {
            assert!(!Terminated.can_transition_to(to), "{to:?}");
        }
    }

    #[test]
    fn nothing_leaves_failed_except_teardown() {
        for to in ALL {
            assert_eq!(
                Failed.can_transition_to(to),
                matches!(to, Stopping | Terminated)
            );
        }
    }

    #[test]
    fn cannot_skip_initialisation() {
        assert!(!Created.can_transition_to(Idle));
        assert!(!Created.can_transition_to(Initialized));
        assert!(!Created.accepts_invocation());
        assert!(Idle.accepts_invocation() && Initialized.accepts_invocation());
    }

    #[test]
    fn running_cannot_nest() {
        assert!(!Running.can_transition_to(Running));
        assert!(Running.can_transition_to(WaitingForIo));
        assert!(WaitingForIo.can_transition_to(Running));
        assert!(!Stopping.can_transition_to(Running));
    }

    #[test]
    fn rejected_transition_leaves_state() {
        let mut l = Lifecycle::new();
        let err = l.transition(Idle).unwrap_err();
        assert_eq!(
            err,
            TransitionError {
                from: Created,
                to: Idle
            }
        );
        assert_eq!(l.get(), Created);
    }
}
