//! How readily an interactive tool call runs without asking the operator.
//!
//! Named after the modes an operator coming from another agent CLI already
//! knows, mapped onto what this harness actually tracks: a capability grant
//! per action, and a policy engine that can already deny a call outright.
//! [`ApprovalMode::Plan`] and [`ApprovalMode::BypassPermissions`] are defined
//! by what they refuse or skip. `Auto` never asks: every call policy left for
//! approval goes through the Safe Auto review (`ToolRuntime::review_auto`,
//! with a shell-command denylist), which either runs it or blocks it and
//! tells the model why. `BypassPermissions` skips that review but remains
//! inside hard policy ceilings.
//!
//! Policy can still deny a call in every mode: none of this skips
//! `RuleSet::evaluate`, only how a call that reaches `NeedsApproval` is
//! answered.

use arsy_kernel::provider::Effort;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Mutex,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalMode {
    /// Reads run; anything else asks. The starting mode.
    Default,
    /// Reads and file writes/creates/edits/moves run; deletes, shell
    /// commands, and everything else still asks.
    AcceptEdits,
    /// Reads and the plan/validation tools run; anything that would change
    /// the workspace is refused outright, not asked. For exploring a
    /// codebase before deciding to change it.
    Plan,
    /// Whatever the Safe Auto review allows runs; a risky action is blocked
    /// and the model is told why. Never asks.
    Auto,
    /// Reads run; anything else that would ask is refused instead of
    /// prompting. For a script or CI run where nobody is at the keyboard.
    DontAsk,
    /// Everything policy left for approval runs without asking.
    BypassPermissions,
}

/// The mode to enter after the operator approves a Plan Mode result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanTarget {
    Auto,
    AcceptEdits,
    Default,
}

impl PlanTarget {
    /// The target `/plan approve <target>` names.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "auto" => Some(Self::Auto),
            "accept-edits" | "acceptEdits" | "acceptedits" | "edits" => Some(Self::AcceptEdits),
            "manual" | "default" => Some(Self::Default),
            _ => None,
        }
    }

    pub const fn mode(self) -> ApprovalMode {
        match self {
            Self::Auto => ApprovalMode::Auto,
            Self::AcceptEdits => ApprovalMode::AcceptEdits,
            Self::Default => ApprovalMode::Default,
        }
    }
}

impl ApprovalMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "default" | "prompt" | "manual" | "ask" | "off" => Some(Self::Default),
            "accept-edits" | "acceptedits" | "acceptEdits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "auto" | "all" | "always" | "on" => Some(Self::Auto),
            "dont-ask" | "dontask" | "dontAsk" => Some(Self::DontAsk),
            "bypass" | "bypass-permissions" | "bypasspermissions" | "bypassPermissions" => {
                Some(Self::BypassPermissions)
            }
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    /// One line naming what runs without asking, for `/approval` with no
    /// argument.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Default => "reads only; everything else asks",
            Self::AcceptEdits => "reads and file writes/creates/edits/moves; everything else asks",
            Self::Plan => "reads and the plan/validation tools; everything else is refused",
            Self::Auto => "runs what the safety review allows; blocks risky actions, never asks",
            Self::DontAsk => "reads only; everything else is refused instead of asked",
            Self::BypassPermissions => "everything inside hard policy ceilings runs",
        }
    }

    /// The next mode Shift+Tab steps to.
    ///
    /// Only the modes an operator at a keyboard would choose between are in the
    /// ring. `dontAsk` exists for a run with nobody watching, and
    /// `bypassPermissions` deliberately stays out of the keyboard cycle: it
    /// skips Safe Auto's independent review and is too risky to enable with an
    /// accidental keypress. `dontAsk` is reserved for unattended runs.
    pub const fn cycle(self) -> Self {
        self.cycle_with(false)
    }

    /// The same ring, with `bypassPermissions` after `auto` when the session
    /// was started with `--dangerously-skip-permissions` and its warning was
    /// confirmed. That start is the deliberate act the keypress alone is not.
    pub const fn cycle_with(self, bypass: bool) -> Self {
        match self {
            Self::Default => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => Self::Auto,
            Self::Auto if bypass => Self::BypassPermissions,
            _ => Self::Default,
        }
    }

    pub const fn execution_mode(self) -> arsy_code::agent::ExecutionMode {
        match self {
            Self::Plan => arsy_code::agent::ExecutionMode::Plan,
            _ => arsy_code::agent::ExecutionMode::Normal,
        }
    }

    const fn as_u8(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::AcceptEdits => 1,
            Self::Plan => 2,
            Self::Auto => 3,
            Self::DontAsk => 4,
            Self::BypassPermissions => 5,
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::AcceptEdits,
            2 => Self::Plan,
            3 => Self::Auto,
            4 => Self::DontAsk,
            5 => Self::BypassPermissions,
            _ => Self::Default,
        }
    }
}

/// The current mode, shared the way the single auto-approve flag it replaces
/// was: passed by reference into everything that asks, updated from
/// wherever the operator changes it.
pub struct ApprovalCell {
    current: AtomicU8,
    before_plan: AtomicU8,
    opened: AtomicUsize,
    rules: Mutex<BTreeMap<String, arsy_kernel::capability::CapabilityGrant>>,
    recorded: Mutex<Vec<arsy_kernel::capability::CapabilityGrant>>,
    /// Command prefixes Accept Edits runs without asking: the configured ones,
    /// replaced each turn, and the ones approved "always" this session.
    configured_commands: Mutex<Vec<String>>,
    session_commands: Mutex<Vec<String>>,
    /// Set once, when `--dangerously-skip-permissions` was confirmed.
    bypass: AtomicBool,
    /// The reasoning effort the next model request asks for: 0 is off, then
    /// low, medium, high. Kept beside the mode for the same reason: both are
    /// operator controls a running turn reads each round, so Ctrl+T or
    /// `/effort` mid-turn reaches the next request instead of the next turn.
    // ponytail: rides in the approval cell because every key loop already
    // holds it; split into its own cell if more live controls join it.
    effort: AtomicU8,
    /// The efforts the routed model takes, in the order Ctrl+T steps
    /// through them: every level and `off`, or only the levels a model
    /// listed once per effort offers.
    effort_choices: Mutex<Vec<Option<Effort>>>,
}

impl Default for ApprovalCell {
    fn default() -> Self {
        Self::new(ApprovalMode::Default)
    }
}

impl ApprovalCell {
    pub fn new(mode: ApprovalMode) -> Self {
        Self {
            current: AtomicU8::new(mode.as_u8()),
            before_plan: AtomicU8::new(ApprovalMode::Default.as_u8()),
            opened: AtomicUsize::new(0),
            rules: Mutex::new(BTreeMap::new()),
            recorded: Mutex::new(Vec::new()),
            configured_commands: Mutex::new(Vec::new()),
            session_commands: Mutex::new(Vec::new()),
            bypass: AtomicBool::new(false),
            effort: AtomicU8::new(0),
            effort_choices: Mutex::new(crate::tui::effort_choices()),
        }
    }

    /// The efforts the routed model takes; see `effort_choices`.
    pub fn set_effort_choices(&self, choices: Vec<Option<Effort>>) {
        *self
            .effort_choices
            .lock()
            .unwrap_or_else(|held| held.into_inner()) = choices;
    }

    pub fn effort_choices(&self) -> Vec<Option<Effort>> {
        self.effort_choices
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .clone()
    }

    pub fn effort(&self) -> Option<Effort> {
        match self.effort.load(Ordering::Relaxed) {
            1 => Some(Effort::Low),
            2 => Some(Effort::Medium),
            3 => Some(Effort::High),
            _ => None,
        }
    }

    pub fn set_effort(&self, effort: Option<Effort>) {
        let raw = match effort {
            None => 0,
            Some(Effort::Low) => 1,
            Some(Effort::Medium) => 2,
            Some(Effort::High) => 3,
        };
        self.effort.store(raw, Ordering::Relaxed);
    }

    /// Step to the next effort the routed model takes — low → medium →
    /// high → off for most models, only a family's own levels for one listed
    /// per effort — answering the new one.
    pub fn cycle_effort(&self) -> Option<Effort> {
        let choices = self.effort_choices();
        let next = choices
            .iter()
            .position(|choice| *choice == self.effort())
            .map_or(0, |at| at + 1);
        let next = choices.get(next % choices.len().max(1)).copied().flatten();
        self.set_effort(next);
        next
    }

    /// Let Shift+Tab reach `bypassPermissions` for the rest of this session.
    pub fn allow_bypass(&self) {
        self.bypass.store(true, Ordering::Relaxed);
    }

    /// The mode Shift+Tab steps to from the current one.
    pub fn next_mode(&self) -> ApprovalMode {
        self.get().cycle_with(self.bypass.load(Ordering::Relaxed))
    }

    /// The configured allowlist, taken from the configuration a turn loaded.
    pub fn set_configured_commands(&self, commands: &[String]) {
        *self
            .configured_commands
            .lock()
            .unwrap_or_else(|held| held.into_inner()) = commands.to_vec();
    }

    /// Run this exact command again without asking, for this session.
    pub fn remember_command(&self, command: &str) {
        let command = command.trim();
        if command.is_empty() || !is_simple_command(command) {
            return;
        }
        let mut session = self
            .session_commands
            .lock()
            .unwrap_or_else(|held| held.into_inner());
        if !session.iter().any(|held| held == command) {
            session.push(command.to_owned());
        }
    }

    /// Whether `command` is covered by the configured or the session list.
    pub fn allows_command(&self, command: &str) -> bool {
        let lock = |list: &Mutex<Vec<String>>| {
            list.lock().unwrap_or_else(|held| held.into_inner()).clone()
        };
        command_allowed(command, &lock(&self.configured_commands))
            || command_allowed(command, &lock(&self.session_commands))
    }

    /// Record that a confirmation prompt is on screen and about to block on the
    /// keyboard.
    ///
    /// Waiting for input is the one moment in a turn with no other observable
    /// effect, which leaves a caller that wants to answer it no way to know it
    /// has arrived. Counting it makes that moment visible.
    pub fn open(&self) {
        self.opened.fetch_add(1, Ordering::SeqCst);
    }

    /// How many confirmation prompts have been shown.
    #[cfg(test)]
    pub fn opened(&self) -> usize {
        self.opened.load(Ordering::SeqCst)
    }

    pub fn get(&self) -> ApprovalMode {
        ApprovalMode::from_u8(self.current.load(Ordering::Relaxed))
    }

    pub fn set(&self, mode: ApprovalMode) {
        if mode == ApprovalMode::Plan {
            self.enter_plan();
        } else {
            self.current.store(mode.as_u8(), Ordering::Relaxed);
            self.before_plan.store(mode.as_u8(), Ordering::Relaxed);
        }
    }

    /// Remember exactly the capability/resource pairs the approval card
    /// displayed. This is narrower than Auto mode: a later call is covered
    /// only when every outstanding requirement is admitted by one of these
    /// leaf grants.
    pub fn remember(&self, authorization: &arsy_code::agent::Authorization) {
        let arsy_code::agent::Authorization::NeedsApproval { approvals, .. } = authorization else {
            return;
        };
        let mut rules = self.rules.lock().unwrap_or_else(|held| held.into_inner());
        let mut recorded = Vec::new();
        for request in approvals {
            let Ok(grant) = request.grant() else {
                continue;
            };
            if rules.insert(rule_key(request), grant.clone()).is_none() {
                recorded.push(grant);
            }
        }
        if !recorded.is_empty() {
            self.recorded
                .lock()
                .unwrap_or_else(|held| held.into_inner())
                .extend(recorded);
        }
    }

    /// Grants covering every outstanding request, or `None` when the operator
    /// must see the approval card. Already-policy-granted capabilities are
    /// retained, so reusing a displayed rule never narrows a legitimate call.
    pub fn cached(
        &self,
        authorization: &arsy_code::agent::Authorization,
    ) -> Option<Vec<arsy_kernel::capability::CapabilityGrant>> {
        let arsy_code::agent::Authorization::NeedsApproval { granted, approvals } = authorization
        else {
            return None;
        };
        let now = arsy_kernel::artifact::unix_time_ms();
        let rules = self.rules.lock().unwrap_or_else(|held| held.into_inner());
        let mut covered = granted.clone();
        for request in approvals {
            let grant = rules.get(&rule_key(request))?;
            if !grant.permits(&request.requirement, now) {
                return None;
            }
            covered.push(grant.clone());
        }
        Some(covered)
    }

    /// Newly displayed rules that still need their durable audit event.
    pub fn take_recorded(&self) -> Vec<arsy_kernel::capability::CapabilityGrant> {
        std::mem::take(
            &mut *self
                .recorded
                .lock()
                .unwrap_or_else(|held| held.into_inner()),
        )
    }

    /// A displayed rule belongs to one interactive session only.
    pub fn clear_rules(&self) {
        self.rules
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .clear();
        self.recorded
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .clear();
    }

    pub fn enter_plan(&self) {
        let current = self.get();
        if current != ApprovalMode::Plan {
            self.before_plan.store(current.as_u8(), Ordering::Relaxed);
        }
        self.current
            .store(ApprovalMode::Plan.as_u8(), Ordering::Relaxed);
    }

    /// An approved plan enters the existing edit-capable mode. Shell and
    /// destructive operations still use their normal approval path.
    /// Approve into the first choice the plan card offers.
    pub fn approve_plan(&self) -> ApprovalMode {
        self.approve_plan_as(PlanTarget::Auto)
    }

    pub fn approve_plan_as(&self, target: PlanTarget) -> ApprovalMode {
        let mode = target.mode();
        self.set(mode);
        mode
    }

    pub fn cancel_plan(&self) -> ApprovalMode {
        let mode = ApprovalMode::from_u8(self.before_plan.load(Ordering::Relaxed));
        self.set(mode);
        mode
    }
}

fn rule_key(request: &arsy_kernel::policy::ApprovalRequest) -> String {
    format!(
        "{}\0{}\0{}:{}",
        serde_json::to_string(&request.actor).unwrap_or_default(),
        request.requirement.action,
        request.requirement.resource.scheme(),
        request.requirement.resource.value(),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    /// Runs without asking.
    Approve,
    /// The operator is asked.
    Ask,
    /// Refused without asking anyone.
    Refuse,
}

/// A read, in the sense every mode above means it: it cannot change the
/// workspace, so approving it costs nothing a mistake could not already cost
/// by reading the wrong file.
fn is_read(name: &str) -> bool {
    matches!(
        name,
        "fs.read"
            | "fs.list"
            | "search.files"
            | "search.text"
            | "code.symbol"
            | "code.explain"
            | "code.references"
            | "code.diagnostics"
            | "repo_discover"
            | "repo_map"
            | "git_status"
            | "git_branch"
            | "git_diff"
            | "git_log"
            | "git_blame"
            | "plan_list"
            | "todo_list"
            | "validate_status"
    )
}

/// A structured file write, as opposed to a shell command that could do
/// anything a write can and more.
fn is_edit(name: &str) -> bool {
    matches!(
        name,
        "fs.write"
            | "fs.create"
            | "fs.edit"
            | "fs.move"
            | "fs.patch"
            | "code.rename"
            | "apply_patch"
    )
}

/// Recording a plan or a validation result changes no file and runs no
/// process; it is the harness's own bookkeeping, which `Plan` mode exists to
/// still allow while nothing else that could change the workspace does.
fn is_plan_tool(name: &str) -> bool {
    matches!(
        name,
        "plan_add"
            | "plan_update"
            | "plan_remove"
            | "plan_reorder"
            | "plan_commit"
            | "validate_record"
    )
}

/// A command with no chaining, piping, substitution, or redirection: one
/// program and its arguments. Anything else could hide a second command
/// behind an allowed prefix.
fn is_simple_command(command: &str) -> bool {
    !command.contains(['&', ';', '|', '`', '$', '>', '<', '(', ')', '\n', '\r'])
}

/// Whether `command` starts with one of `allowed` on a word boundary and is a
/// simple command. `cargo test` covers `cargo test -p x`, not `cargo testify`.
pub fn command_allowed(command: &str, allowed: &[String]) -> bool {
    let command = command.trim();
    is_simple_command(command)
        && allowed.iter().map(|prefix| prefix.trim()).any(|prefix| {
            !prefix.is_empty()
                && command
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        })
}

/// Whether `name` runs, asks, or is refused, under `mode`. Called only once
/// policy has already said the call needs an answer — a call policy allows
/// or denies outright never reaches this.
pub fn decide(mode: ApprovalMode, name: &str) -> Decision {
    match mode {
        // Auto's gate is the Safe Auto review, which has already blocked
        // what it judged risky by the time a call gets here; Auto itself
        // never asks.
        ApprovalMode::Auto | ApprovalMode::BypassPermissions => Decision::Approve,
        ApprovalMode::Default => {
            if is_read(name) {
                Decision::Approve
            } else {
                Decision::Ask
            }
        }
        ApprovalMode::AcceptEdits => {
            if is_read(name) || is_edit(name) {
                Decision::Approve
            } else {
                Decision::Ask
            }
        }
        ApprovalMode::Plan => {
            if is_read(name) || is_plan_tool(name) {
                Decision::Approve
            } else {
                Decision::Refuse
            }
        }
        ApprovalMode::DontAsk => {
            if is_read(name) {
                Decision::Approve
            } else {
                Decision::Refuse
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_name_round_trips_through_its_label() {
        for mode in [
            ApprovalMode::Default,
            ApprovalMode::AcceptEdits,
            ApprovalMode::Plan,
            ApprovalMode::Auto,
            ApprovalMode::DontAsk,
            ApprovalMode::BypassPermissions,
        ] {
            assert_eq!(ApprovalMode::parse(mode.label()), Some(mode));
        }
    }

    #[test]
    fn default_asks_for_anything_that_is_not_a_read() {
        assert_eq!(decide(ApprovalMode::Default, "fs.read"), Decision::Approve);
        assert_eq!(decide(ApprovalMode::Default, "fs.write"), Decision::Ask);
        assert_eq!(decide(ApprovalMode::Default, "bash"), Decision::Ask);
    }

    #[test]
    fn accept_edits_runs_writes_but_still_asks_for_bash_and_delete() {
        assert_eq!(
            decide(ApprovalMode::AcceptEdits, "fs.write"),
            Decision::Approve
        );
        assert_eq!(
            decide(ApprovalMode::AcceptEdits, "apply_patch"),
            Decision::Approve
        );
        assert_eq!(
            decide(ApprovalMode::AcceptEdits, "code.rename"),
            Decision::Approve
        );
        assert_eq!(decide(ApprovalMode::AcceptEdits, "bash"), Decision::Ask);
        assert_eq!(
            decide(ApprovalMode::AcceptEdits, "fs.delete"),
            Decision::Ask
        );
    }

    #[test]
    fn plan_mode_allows_the_plan_tools_and_refuses_everything_that_changes_the_workspace() {
        assert_eq!(decide(ApprovalMode::Plan, "fs.read"), Decision::Approve);
        assert_eq!(decide(ApprovalMode::Plan, "plan_add"), Decision::Approve);
        assert_eq!(
            decide(ApprovalMode::Plan, "validate_record"),
            Decision::Approve
        );
        assert_eq!(decide(ApprovalMode::Plan, "fs.write"), Decision::Refuse);
        assert_eq!(decide(ApprovalMode::Plan, "bash"), Decision::Refuse);
    }

    #[test]
    fn an_allowed_command_covers_its_arguments_but_nothing_chained_to_it() {
        let allowed = vec!["cargo test".to_owned(), "git status".to_owned()];
        assert!(command_allowed("cargo test", &allowed));
        assert!(command_allowed("  cargo test -p arsy-cli ", &allowed));
        assert!(!command_allowed("cargo testify", &allowed));
        assert!(!command_allowed("cargo build", &allowed));
        for chained in [
            "cargo test && rm -rf target",
            "cargo test; curl x",
            "cargo test | sh",
            "cargo test $(whoami)",
            "cargo test > out",
            "cargo test\nrm x",
        ] {
            assert!(!command_allowed(chained, &allowed), "{chained}");
        }
    }

    #[test]
    fn a_session_approval_covers_only_that_simple_command() {
        let cell = ApprovalCell::default();
        cell.set_configured_commands(&["git status".to_owned()]);
        cell.remember_command("npm run lint");
        cell.remember_command("npm test && curl x");
        assert!(cell.allows_command("git status --short"));
        assert!(cell.allows_command("npm run lint"));
        assert!(!cell.allows_command("npm test"));
        cell.set_configured_commands(&[]);
        assert!(!cell.allows_command("git status"));
    }

    #[test]
    fn shift_tab_reaches_bypass_only_after_the_flag_was_confirmed() {
        let cell = ApprovalCell::new(ApprovalMode::Auto);
        assert_eq!(cell.next_mode(), ApprovalMode::Default);
        cell.allow_bypass();
        assert_eq!(cell.next_mode(), ApprovalMode::BypassPermissions);
        assert_eq!(
            ApprovalMode::BypassPermissions.cycle_with(true),
            ApprovalMode::Default
        );
    }

    /// The runtime lets Plan mode map the repository and commit its plan; a
    /// table that did not know those names refused what the runtime allowed.
    #[test]
    fn plan_mode_can_map_the_repository_and_commit_its_plan() {
        for name in ["repo_map", "todo_list", "plan_commit"] {
            assert_eq!(decide(ApprovalMode::Plan, name), Decision::Approve);
        }
    }

    #[test]
    fn dont_ask_refuses_instead_of_prompting() {
        assert_eq!(decide(ApprovalMode::DontAsk, "fs.read"), Decision::Approve);
        assert_eq!(decide(ApprovalMode::DontAsk, "bash"), Decision::Refuse);
    }

    #[test]
    fn auto_and_bypass_never_ask() {
        for name in [
            "fs.write",
            "bash",
            "bash_start",
            "fs.delete",
            "web_fetch",
            "todo_add",
            "mcp__github__create_issue",
            "unknown.future.tool",
        ] {
            assert_eq!(
                decide(ApprovalMode::Auto, name),
                Decision::Approve,
                "{name}"
            );
            assert_eq!(
                decide(ApprovalMode::BypassPermissions, name),
                Decision::Approve
            );
        }
    }

    #[test]
    fn displayed_rules_cover_only_the_exact_capability_and_resource() {
        use arsy_code::agent::Authorization;
        use arsy_kernel::{
            capability::{CapabilityAction, CapabilityRequirement},
            domain::{ApprovalId, Principal, ResourceRef, StateVersion},
            operation::OperationKind,
            policy::ApprovalRequest,
        };

        let request = |resource: &str| ApprovalRequest {
            id: ApprovalId::new(),
            actor: Principal::System,
            operation: OperationKind::new("process.exec").unwrap(),
            requirement: CapabilityRequirement::new(
                CapabilityAction::NetworkConnect,
                ResourceRef::new("host", resource).unwrap(),
            ),
            operation_digest: StateVersion::from_digest([7; 32]),
            reversible: true,
            expires_at_ms: None,
            delegation_depth: 0,
            reason: "integration test network".to_owned(),
        };
        let authorization = Authorization::NeedsApproval {
            granted: Vec::new(),
            approvals: vec![request("sandbox.example")],
        };
        let other = Authorization::NeedsApproval {
            granted: Vec::new(),
            approvals: vec![request("production.example")],
        };
        let cell = ApprovalCell::new(ApprovalMode::Default);

        cell.remember(&authorization);

        assert!(cell.cached(&authorization).is_some());
        assert!(cell.cached(&other).is_none());
        assert_eq!(cell.get(), ApprovalMode::Default, "a rule is not Auto mode");
        assert_eq!(cell.take_recorded().len(), 1);
    }

    #[test]
    fn the_cell_starts_at_the_mode_it_was_built_with_and_can_be_changed() {
        let cell = ApprovalCell::new(ApprovalMode::Default);
        assert_eq!(cell.get(), ApprovalMode::Default);
        cell.set(ApprovalMode::Auto);
        assert_eq!(cell.get(), ApprovalMode::Auto);
    }

    #[test]
    fn shift_tab_steps_through_the_interactive_modes_and_returns_to_the_start() {
        let mut mode = ApprovalMode::Default;
        let mut seen = Vec::new();
        for _ in 0..4 {
            mode = mode.cycle();
            seen.push(mode);
        }
        assert_eq!(
            seen,
            [
                ApprovalMode::AcceptEdits,
                ApprovalMode::Plan,
                ApprovalMode::Auto,
                ApprovalMode::Default,
            ]
        );
        // A mode outside the ring is a way in, never a dead end.
        assert_eq!(ApprovalMode::DontAsk.cycle(), ApprovalMode::Default);
        assert_eq!(
            ApprovalMode::BypassPermissions.cycle(),
            ApprovalMode::Default
        );
    }
    #[test]
    fn an_explicit_mode_change_clears_plan_mode_instead_of_restoring_it() {
        let cell = ApprovalCell::new(ApprovalMode::Default);
        cell.enter_plan();
        cell.set(ApprovalMode::Auto);

        assert_eq!(cell.get(), ApprovalMode::Auto);
        assert_eq!(cell.cancel_plan(), ApprovalMode::Auto);
        assert_ne!(cell.get(), ApprovalMode::Plan);
    }

    #[test]
    fn stepping_into_plan_and_back_out_restores_the_mode_it_started_from() {
        let cell = ApprovalCell::new(ApprovalMode::AcceptEdits);
        cell.set(cell.get().cycle());
        assert_eq!(cell.get(), ApprovalMode::Plan);
        // Stepping out is not approving: the plan was never accepted, so the
        // mode before planning is what comes back.
        assert_eq!(cell.cancel_plan(), ApprovalMode::AcceptEdits);
    }

    #[test]
    fn plan_lifecycle_approves_into_auto_and_cancel_restores_the_previous_mode() {
        let approval = ApprovalCell::new(ApprovalMode::Auto);
        approval.enter_plan();
        assert_eq!(approval.get(), ApprovalMode::Plan);
        assert_eq!(approval.cancel_plan(), ApprovalMode::Auto);

        approval.enter_plan();
        assert_eq!(approval.approve_plan(), ApprovalMode::Auto);
        assert_eq!(approval.get(), ApprovalMode::Auto);

        for (text, mode) in [
            ("auto", ApprovalMode::Auto),
            ("accept-edits", ApprovalMode::AcceptEdits),
            ("manual", ApprovalMode::Default),
        ] {
            approval.enter_plan();
            let target = PlanTarget::parse(text).unwrap();
            assert_eq!(approval.approve_plan_as(target), mode, "{text}");
            assert_eq!(approval.get(), mode, "{text}");
        }
        assert_eq!(PlanTarget::parse("please"), None);
    }
}
