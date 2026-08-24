//! Turning a controller's answer — or its absence — into an action.
//!
//! The rules live here rather than in the run loop because they are the part
//! most likely to be argued about and the part most worth testing: every path
//! through them has to produce a decision, including the paths where nothing is
//! listening.

use crate::config::{Fallback, Reboot, RebootPolicy, UpdatePolicy, Updates};
use crate::ipc::protocol::{RebootDecision, UpdateDecision};

/// Why the agent did what it did. Logged, and worth reporting to NervesHub with
/// an ignored update so an operator can tell a policy from a broken device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `policy = "apply"`; the controller was never asked.
    Policy,
    Controller,
    NoController,
    Timeout,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decided<T> {
    pub decision: T,
    pub source: Source,
}

/// What to do about an available update.
///
/// `answer` is `None` when no controller is connected or it did not answer in
/// time; the two cases are told apart by `had_controller` because they are
/// configured separately — a device between application restarts is not the
/// same as a device whose application is wedged.
pub fn decide_update(
    config: &Updates,
    had_controller: bool,
    answer: Option<UpdateDecision>,
) -> Decided<UpdateDecision> {
    if config.policy == UpdatePolicy::Apply {
        return Decided {
            decision: UpdateDecision::Apply,
            source: Source::Policy,
        };
    }

    match answer {
        Some(decision) => Decided {
            decision,
            source: Source::Controller,
        },
        None => {
            let (fallback, source) = if had_controller {
                (config.on_timeout, Source::Timeout)
            } else {
                (config.on_no_controller, Source::NoController)
            };

            Decided {
                decision: match fallback {
                    Fallback::Apply => UpdateDecision::Apply,
                    Fallback::Ignore => UpdateDecision::Ignore {
                        reason: format!("no answer from controller ({source:?})"),
                    },
                },
                source,
            }
        }
    }
}

/// Whether to reboot into an installed update.
///
/// A deferral is clamped to `max_defer_secs`. Without a clamp an application
/// that answers `defer` forever leaves the device running old firmware while
/// NervesHub shows the update as installed, which is worse than a reboot at an
/// inconvenient moment because nobody is looking for it.
pub fn decide_reboot(
    config: &Reboot,
    had_controller: bool,
    answer: Option<RebootDecision>,
    already_deferred_secs: u64,
) -> Decided<RebootDecision> {
    match config.policy {
        RebootPolicy::Immediate => {
            return Decided {
                decision: RebootDecision::Reboot,
                source: Source::Policy,
            }
        }
        RebootPolicy::Never => {
            return Decided {
                decision: RebootDecision::Defer {
                    delay_ms: u64::MAX,
                    reason: "reboot.policy = never".into(),
                },
                source: Source::Policy,
            }
        }
        RebootPolicy::Ask => {}
    }

    if let Some(max) = config.max_defer_secs {
        if already_deferred_secs >= max {
            return Decided {
                decision: RebootDecision::Reboot,
                source: Source::Policy,
            };
        }
    }

    match answer {
        Some(decision) => Decided {
            decision,
            source: Source::Controller,
        },
        None => Decided {
            decision: RebootDecision::Reboot,
            source: if had_controller {
                Source::Timeout
            } else {
                Source::NoController
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_policy_never_asks() {
        let config = Updates {
            policy: UpdatePolicy::Apply,
            ..Default::default()
        };

        let decided = decide_update(
            &config,
            true,
            Some(UpdateDecision::Ignore {
                reason: "no".into(),
            }),
        );

        assert_eq!(decided.decision, UpdateDecision::Apply);
        assert_eq!(decided.source, Source::Policy);
    }

    #[test]
    fn ask_policy_with_no_controller_uses_its_own_fallback() {
        let config = Updates {
            policy: UpdatePolicy::Ask,
            on_no_controller: Fallback::Ignore,
            on_timeout: Fallback::Apply,
            ..Default::default()
        };

        let decided = decide_update(&config, false, None);

        assert_eq!(decided.source, Source::NoController);
        assert!(matches!(decided.decision, UpdateDecision::Ignore { .. }));
    }

    #[test]
    fn a_deferral_past_the_cap_reboots() {
        let config = Reboot {
            policy: RebootPolicy::Ask,
            max_defer_secs: Some(3600),
            ..Default::default()
        };

        let decided = decide_reboot(&config, true, None, 3600);

        assert_eq!(decided.decision, RebootDecision::Reboot);
    }
}
