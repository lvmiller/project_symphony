use std::cmp::Ordering;

use crate::config::{EffectiveConfig, normalize_state};
use crate::domain::{Issue, StateCounts};
use crate::orchestrator::state::OrchestratorState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchIneligibleReason {
    MissingRequiredFields,
    InactiveState,
    TerminalState,
    AlreadyRunningOrClaimed,
    GlobalSlotsExhausted,
    StateSlotsExhausted,
    TodoBlocked,
}

pub fn sort_for_dispatch(issues: &mut [Issue]) {
    issues.sort_by(compare_issue_for_dispatch);
}

pub fn compare_issue_for_dispatch(a: &Issue, b: &Issue) -> Ordering {
    let priority = match (a.priority, b.priority) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    priority
        .then_with(|| compare_created_at(a, b))
        .then_with(|| a.identifier.cmp(&b.identifier))
}

fn compare_created_at(a: &Issue, b: &Issue) -> Ordering {
    match (a.created_at, b.created_at) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

pub fn available_global_slots(state: &OrchestratorState, config: &EffectiveConfig) -> usize {
    config
        .agent
        .max_concurrent_agents
        .saturating_sub(state.running.len())
}

pub fn is_dispatch_eligible(
    issue: &Issue,
    state: &OrchestratorState,
    config: &EffectiveConfig,
) -> bool {
    dispatch_ineligible_reason(issue, state, config).is_none()
}

pub fn dispatch_ineligible_reason(
    issue: &Issue,
    state: &OrchestratorState,
    config: &EffectiveConfig,
) -> Option<DispatchIneligibleReason> {
    if !issue.required_fields_present() {
        return Some(DispatchIneligibleReason::MissingRequiredFields);
    }
    if config.is_terminal_state(&issue.state) {
        return Some(DispatchIneligibleReason::TerminalState);
    }
    if !config.is_active_state(&issue.state) {
        return Some(DispatchIneligibleReason::InactiveState);
    }
    if state.is_issue_or_workspace_claimed(issue) {
        return Some(DispatchIneligibleReason::AlreadyRunningOrClaimed);
    }
    if available_global_slots(state, config) == 0 {
        return Some(DispatchIneligibleReason::GlobalSlotsExhausted);
    }
    if !per_state_slot_available(issue, state.running_state_counts(), config) {
        return Some(DispatchIneligibleReason::StateSlotsExhausted);
    }
    if normalize_state(&issue.state) == "todo"
        && issue.blocked_by.iter().any(|blocker| {
            blocker
                .state
                .as_deref()
                .map(|state| !config.is_terminal_state(state))
                .unwrap_or(true)
        })
    {
        return Some(DispatchIneligibleReason::TodoBlocked);
    }
    None
}

fn per_state_slot_available(issue: &Issue, counts: StateCounts, config: &EffectiveConfig) -> bool {
    let key = normalize_state(&issue.state);
    let current = counts.get(&key).copied().unwrap_or(0);
    let limit = config
        .agent
        .max_concurrent_agents_by_state
        .get(&key)
        .copied()
        .unwrap_or(config.agent.max_concurrent_agents);
    current < limit
}
