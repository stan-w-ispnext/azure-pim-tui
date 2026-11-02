use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use azure_core::credentials::TokenCredential;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::client::auth::SubscriptionInfo;
use crate::client::graph_credential::GraphCredential;
use crate::client::jira::JiraIssue;
use crate::client::jira_cache::JiraCache;
use crate::client::models::{
    sort_group_roles, sort_resource_roles, ActivationRequest, PimRole, RoleFetch, RoleStatus,
    TicketInfo,
};
use crate::config::Config;

pub struct AuthData {
    pub credential: Arc<dyn TokenCredential>,
    pub graph_credential: Arc<GraphCredential>,
    pub principal_id: String,
    pub user_display: String,
    pub subscriptions: Vec<SubscriptionInfo>,
}

pub enum BgEvent {
    RolesLoaded(Result<RoleFetch, String>),
    GroupRolesLoaded(Result<RoleFetch, String>),
    ActivationResult {
        index: usize,
        result: Result<(), String>,
    },
    DeactivationResult {
        index: usize,
        result: Result<(), String>,
    },
    AuthReady(Result<AuthData, String>),
    RolePermissionsLoaded(Result<HashMap<String, Vec<String>>, String>),
    /// Status message from graph credential (device code flow)
    GraphStatus(String),
    JiraIssuesLoaded(Result<Vec<JiraIssue>, String>),
    /// Lookup of a manually typed ticket number; `None` when it doesn't exist.
    JiraIssueLookedUp {
        key: String,
        result: Result<Option<JiraIssue>, String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pane {
    Resources,
    Groups,
}

impl Pane {
    pub fn label(&self) -> &str {
        match self {
            Pane::Resources => "Resources",
            Pane::Groups => "Groups",
        }
    }

    pub fn toggle(&self) -> Self {
        match self {
            Pane::Resources => Pane::Groups,
            Pane::Groups => Pane::Resources,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ViewFilter {
    All,
    Eligible,
    Active,
}

impl ViewFilter {
    pub fn label(&self) -> &str {
        match self {
            ViewFilter::All => "All",
            ViewFilter::Eligible => "Eligible",
            ViewFilter::Active => "Active",
        }
    }

    pub fn cycle(&self) -> Self {
        match self {
            ViewFilter::All => ViewFilter::Eligible,
            ViewFilter::Eligible => ViewFilter::Active,
            ViewFilter::Active => ViewFilter::All,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActiveModal {
    None,
    Activate {
        role_index: usize,
        form: ActivationForm,
    },
    BulkActivate {
        indices: Vec<usize>,
        form: ActivationForm,
    },
    DeactivateConfirm {
        role_index: usize,
    },
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ModalField {
    Justification,
    Duration,
    TicketNumber,
    TicketSystem,
}

impl ModalField {
    pub fn label(self) -> &'static str {
        match self {
            ModalField::Justification => "Justification",
            ModalField::Duration => "Duration (hrs)",
            ModalField::TicketNumber => "Ticket number",
            ModalField::TicketSystem => "Ticket system",
        }
    }
}

/// Editable state of the activation modal.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivationForm {
    pub justification: String,
    pub duration: String,
    pub ticket_number: String,
    pub ticket_system: String,
    pub show_ticket: bool,
    pub focused: ModalField,
    /// In-progress Jira issues offered for the ticket number field.
    pub jira_issues: Vec<JiraIssue>,
    pub jira_selected: Option<usize>,
    /// Issue found by looking up a manually typed ticket number.
    pub looked_up_issue: Option<JiraIssue>,
    /// Set once the user types in the justification; until then it follows
    /// the current issue's title.
    pub justification_edited: bool,
    default_justification: String,
    /// When the ticket number last changed by hand, for debouncing lookups.
    ticket_edited_at: Option<Instant>,
}

impl ActivationForm {
    pub fn new(config: &Config) -> Self {
        Self {
            justification: config.default_justification.clone(),
            duration: config.default_duration_hours.to_string(),
            ticket_number: String::new(),
            ticket_system: config.default_ticket_system.clone(),
            show_ticket: config.ticket_fields_enabled,
            focused: ModalField::Duration,
            jira_issues: Vec::new(),
            jira_selected: None,
            looked_up_issue: None,
            justification_edited: false,
            default_justification: config.default_justification.clone(),
            ticket_edited_at: None,
        }
    }

    /// Offer `issues` for the ticket number, picking the first one unless the
    /// user already typed a ticket number.
    pub fn set_jira_issues(&mut self, issues: Vec<JiraIssue>) {
        self.jira_issues = issues;
        if self.ticket_number.is_empty() {
            if !self.jira_issues.is_empty() {
                self.select_jira_issue(0);
            }
        } else {
            self.on_ticket_number_edited();
        }
    }

    /// Re-match the ticket number after a manual edit: pick it from the loaded
    /// issues if present, otherwise queue a lookup.
    pub fn on_ticket_number_edited(&mut self) {
        let key = normalize_issue_key(&self.ticket_number);
        self.jira_selected = self
            .jira_issues
            .iter()
            .position(|i| i.key.eq_ignore_ascii_case(&key));
        self.looked_up_issue = None;
        self.ticket_edited_at =
            (self.jira_selected.is_none() && is_issue_key(&key)).then(Instant::now);
        self.apply_issue_summary();
    }

    /// The ticket number to look up once it has been left alone for `debounce`.
    pub fn take_pending_lookup(&mut self, debounce: Duration) -> Option<String> {
        if self.ticket_edited_at?.elapsed() < debounce {
            return None;
        }
        self.ticket_edited_at = None;
        Some(normalize_issue_key(&self.ticket_number))
    }

    /// The ticket number waiting for a lookup, if any.
    pub fn pending_lookup_key(&self) -> Option<String> {
        self.ticket_edited_at
            .map(|_| normalize_issue_key(&self.ticket_number))
    }

    /// Apply a lookup result, ignoring it if the ticket number changed since.
    pub fn set_looked_up_issue(&mut self, key: &str, issue: Option<JiraIssue>) {
        if normalize_issue_key(&self.ticket_number) != key {
            return;
        }
        // The answer is known, so no request is needed for this key.
        self.ticket_edited_at = None;
        if issue.is_some() && self.ticket_system.is_empty() {
            self.ticket_system = "Jira".to_string();
        }
        self.looked_up_issue = issue;
        self.apply_issue_summary();
    }

    /// The issue matching the current ticket number, if known.
    pub fn current_issue(&self) -> Option<&JiraIssue> {
        self.selected_jira_issue().or(self.looked_up_issue.as_ref())
    }

    fn apply_issue_summary(&mut self) {
        if self.justification_edited {
            return;
        }
        self.justification = match self.current_issue() {
            Some(issue) if !issue.summary.is_empty() => issue.summary.clone(),
            _ => self.default_justification.clone(),
        };
    }

    /// Step through the Jira issues (wrapping) and copy the key into the form.
    pub fn cycle_jira_issue(&mut self, forward: bool) {
        let len = self.jira_issues.len();
        if len == 0 {
            return;
        }
        let next = match self.jira_selected {
            None => 0,
            Some(i) if forward => (i + 1) % len,
            Some(i) => (i + len - 1) % len,
        };
        self.select_jira_issue(next);
    }

    fn select_jira_issue(&mut self, index: usize) {
        self.jira_selected = Some(index);
        self.looked_up_issue = None;
        self.ticket_edited_at = None;
        self.ticket_number = self.jira_issues[index].key.clone();
        if self.ticket_system.is_empty() {
            self.ticket_system = "Jira".to_string();
        }
        self.apply_issue_summary();
    }

    pub fn selected_jira_issue(&self) -> Option<&JiraIssue> {
        self.jira_selected.and_then(|i| self.jira_issues.get(i))
    }

    /// Fields shown in the modal, in Tab order.
    pub fn fields(&self) -> &'static [ModalField] {
        if self.show_ticket {
            &[
                ModalField::Duration,
                ModalField::TicketSystem,
                ModalField::TicketNumber,
                ModalField::Justification,
            ]
        } else {
            &[ModalField::Duration, ModalField::Justification]
        }
    }

    pub fn value(&self, field: ModalField) -> &str {
        match field {
            ModalField::Justification => &self.justification,
            ModalField::Duration => &self.duration,
            ModalField::TicketNumber => &self.ticket_number,
            ModalField::TicketSystem => &self.ticket_system,
        }
    }

    pub fn value_mut(&mut self, field: ModalField) -> &mut String {
        match field {
            ModalField::Justification => &mut self.justification,
            ModalField::Duration => &mut self.duration,
            ModalField::TicketNumber => &mut self.ticket_number,
            ModalField::TicketSystem => &mut self.ticket_system,
        }
    }

    /// Move focus forward (or backward) through the visible fields, wrapping.
    pub fn cycle_focus(&mut self, forward: bool) {
        let fields = self.fields();
        let pos = fields.iter().position(|f| *f == self.focused).unwrap_or(0);
        let next = if forward {
            (pos + 1) % fields.len()
        } else {
            (pos + fields.len() - 1) % fields.len()
        };
        self.focused = fields[next];
    }

    pub fn to_request(&self, default_duration_hours: u32) -> ActivationRequest {
        let ticket_number = self.ticket_number.trim();
        let ticket_info = (self.show_ticket && !ticket_number.is_empty()).then(|| TicketInfo {
            ticket_number: ticket_number.to_string(),
            ticket_system: self.ticket_system.trim().to_string(),
        });
        ActivationRequest {
            justification: self.justification.clone(),
            duration_hours: self.duration.parse().unwrap_or(default_duration_hours),
            ticket_info,
        }
    }
}

fn normalize_issue_key(s: &str) -> String {
    s.trim().to_ascii_uppercase()
}

/// Matches Jira issue keys like `PROJ-123` (also keeps the key safe to put in a URL).
fn is_issue_key(key: &str) -> bool {
    let Some((project, number)) = key.rsplit_once('-') else {
        return false;
    };
    project.starts_with(|c: char| c.is_ascii_alphabetic())
        && project
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

pub struct App {
    // Resource roles
    pub roles: Vec<PimRole>,
    // Group roles
    pub group_roles: Vec<PimRole>,
    pub groups_loaded: bool,
    pub groups_loading: bool,
    pub group_status_message: String,

    // Shared UI state
    pub active_pane: Pane,
    pub filtered_indices: Vec<usize>,
    pub selected: usize,
    pub view_filter: ViewFilter,
    pub filter_text: String,
    pub filtering: bool,
    pub modal: ActiveModal,
    pub status_message: String,
    pub user_display: String,
    pub loading: bool,
    pub should_quit: bool,
    pub bg_tx: mpsc::UnboundedSender<BgEvent>,
    pub bg_rx: mpsc::UnboundedReceiver<BgEvent>,
    pub config: Config,
    pub auth: Option<Arc<AuthData>>,
    pub last_refresh: Option<DateTime<Utc>>,
    pub role_permissions: HashMap<String, Vec<String>>,
    pub jira_cache: JiraCache,
    pub detail_scroll: usize,
}

impl App {
    pub fn new(config: Config) -> Self {
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        Self {
            roles: Vec::new(),
            group_roles: Vec::new(),
            groups_loaded: false,
            groups_loading: false,
            group_status_message: "Authenticating...".to_string(),
            active_pane: Pane::Groups,
            filtered_indices: Vec::new(),
            selected: 0,
            view_filter: ViewFilter::All,
            filter_text: String::new(),
            filtering: false,
            modal: ActiveModal::None,
            status_message: "Authenticating...".to_string(),
            user_display: String::new(),
            loading: true,
            should_quit: false,
            bg_tx,
            bg_rx,
            config,
            auth: None,
            last_refresh: None,
            role_permissions: HashMap::new(),
            jira_cache: JiraCache::default(),
            detail_scroll: 0,
        }
    }

    /// The open activation form, single or bulk.
    pub fn activation_form_mut(&mut self) -> Option<&mut ActivationForm> {
        match &mut self.modal {
            ActiveModal::Activate { form, .. } | ActiveModal::BulkActivate { form, .. } => {
                Some(form)
            }
            _ => None,
        }
    }

    /// Returns the role list for the active pane
    pub fn active_roles(&self) -> &[PimRole] {
        match self.active_pane {
            Pane::Resources => &self.roles,
            Pane::Groups => &self.group_roles,
        }
    }

    /// Returns a mutable reference to the role list for the active pane
    pub fn active_roles_mut(&mut self) -> &mut Vec<PimRole> {
        match self.active_pane {
            Pane::Resources => &mut self.roles,
            Pane::Groups => &mut self.group_roles,
        }
    }

    /// Returns the status message for the active pane
    pub fn active_status(&self) -> &str {
        match self.active_pane {
            Pane::Resources => &self.status_message,
            Pane::Groups => &self.group_status_message,
        }
    }

    /// Is the active pane loading?
    pub fn active_loading(&self) -> bool {
        match self.active_pane {
            Pane::Resources => self.loading,
            Pane::Groups => self.groups_loading,
        }
    }

    pub fn switch_pane(&mut self) {
        self.active_pane = self.active_pane.toggle();
        self.selected = 0;
        self.detail_scroll = 0;
        self.filter_text.clear();
        self.update_filtered_indices();
    }

    /// Returns true if groups need to be fetched (first visit)
    pub fn needs_group_fetch(&self) -> bool {
        self.active_pane == Pane::Groups && !self.groups_loaded && !self.groups_loading
    }

    pub fn update_filtered_indices(&mut self) {
        let roles = self.active_roles();
        self.filtered_indices = roles
            .iter()
            .enumerate()
            .filter(|(_, role)| {
                let matches_filter = match self.view_filter {
                    ViewFilter::All => true,
                    ViewFilter::Eligible => role.status.is_eligible(),
                    ViewFilter::Active => role.status.is_active(),
                };
                let matches_search = if self.filter_text.is_empty() {
                    true
                } else {
                    let lower = self.filter_text.to_lowercase();
                    role.role_name.to_lowercase().contains(&lower)
                        || role.scope_display_name.to_lowercase().contains(&lower)
                };
                matches_filter && matches_search
            })
            .map(|(i, _)| i)
            .collect();

        if self.selected >= self.filtered_indices.len() {
            self.selected = self.filtered_indices.len().saturating_sub(1);
        }
    }

    pub fn selected_role(&self) -> Option<&PimRole> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|&i| self.active_roles().get(i))
    }

    pub fn selected_role_index(&self) -> Option<usize> {
        self.filtered_indices.get(self.selected).copied()
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let len = self.filtered_indices.len() as i32;
        let new = (self.selected as i32 + delta).rem_euclid(len);
        self.selected = new as usize;
        self.detail_scroll = 0;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
        self.detail_scroll = 0;
    }

    pub fn select_last(&mut self) {
        if !self.filtered_indices.is_empty() {
            self.selected = self.filtered_indices.len() - 1;
        }
        self.detail_scroll = 0;
    }

    /// Toggles selection on the current role and returns `true` if the role is now selected.
    pub fn toggle_selected(&mut self) -> bool {
        if let Some(&i) = self.filtered_indices.get(self.selected) {
            let new_val = !self.active_roles_mut()[i].selected;
            self.active_roles_mut()[i].selected = new_val;
            return new_val;
        }
        false
    }

    pub fn selected_indices(&self) -> Vec<usize> {
        self.active_roles()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.selected)
            .map(|(i, _)| i)
            .collect()
    }

    pub fn eligible_count(&self) -> usize {
        self.active_roles()
            .iter()
            .filter(|r| r.status.is_eligible())
            .count()
    }

    pub fn active_count(&self) -> usize {
        self.active_roles()
            .iter()
            .filter(|r| r.status.is_active())
            .count()
    }

    /// Owned copy of the highlighted role's identity, for restoring the cursor
    /// after the role list is replaced by a refresh.
    fn selected_identity(&self) -> Option<(String, String, String)> {
        self.selected_role().map(|r| {
            let (rd, scope, group) = r.identity();
            (rd.to_string(), scope.to_string(), group.to_string())
        })
    }

    /// Folds a fetch result into the previous list.
    ///
    /// A refresh is not authoritative about scopes it failed to query, so roles under a
    /// missing scope are carried over rather than dropped, and roles whose active-status
    /// query failed keep the status they already had. Multi-select marks survive too.
    fn merge_fetch(old: &[PimRole], fetch: RoleFetch, pane: Pane) -> Vec<PimRole> {
        let under = |role: &PimRole, scopes: &[String]| {
            scopes.iter().any(|prefix| role.scope.starts_with(prefix))
        };

        let mut roles = fetch.roles;

        let marked: HashSet<(&str, &str, &str)> = old
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.identity())
            .collect();
        let previous: HashMap<(&str, &str, &str), &PimRole> =
            old.iter().map(|r| (r.identity(), r)).collect();

        for role in roles.iter_mut() {
            role.selected = marked.contains(&role.identity());
            if under(role, &fetch.stale_status_scopes) {
                if let Some(prev) = previous.get(&role.identity()) {
                    role.status = prev.status.clone();
                }
            }
        }

        let carried: Vec<PimRole> = {
            let fetched: HashSet<(&str, &str, &str)> = roles.iter().map(|r| r.identity()).collect();
            old.iter()
                .filter(|r| under(r, &fetch.missing_scopes) && !fetched.contains(&r.identity()))
                .cloned()
                .collect()
        };
        roles.extend(carried);

        match pane {
            Pane::Resources => sort_resource_roles(&mut roles),
            Pane::Groups => sort_group_roles(&mut roles),
        }
        roles
    }

    /// Status line for a fetch that only partly succeeded.
    fn partial_status(fetch: &RoleFetch) -> String {
        let missing = fetch.missing_scopes.len();
        let stale = fetch.stale_status_scopes.len();
        if missing > 0 {
            format!("Ready - {missing} scope(s) unavailable, showing last known roles")
        } else {
            format!("Ready - {stale} scope(s) unavailable, active status may be stale")
        }
    }

    /// Rebuilds the filtered view and puts the cursor back on `ident` if it is
    /// still visible, so a refresh does not jump the highlight to another role.
    fn refilter_keeping_cursor(&mut self, ident: Option<(String, String, String)>) {
        self.update_filtered_indices();
        let Some((rd, scope, group)) = ident else {
            return;
        };
        let pos = self.filtered_indices.iter().position(|&i| {
            self.active_roles()
                .get(i)
                .is_some_and(|r| r.identity() == (rd.as_str(), scope.as_str(), group.as_str()))
        });
        if let Some(pos) = pos {
            self.selected = pos;
        }
    }

    pub fn handle_bg_event(&mut self, event: BgEvent) {
        match event {
            BgEvent::AuthReady(Ok(auth_data)) => {
                self.user_display = auth_data.user_display.clone();
                self.auth = Some(Arc::new(auth_data));
                self.status_message = "Fetching roles...".to_string();
            }
            BgEvent::AuthReady(Err(e)) => {
                self.loading = false;
                self.status_message = format!("Auth failed: {e}");
            }
            BgEvent::RolesLoaded(Ok(fetch)) => {
                let ident = (self.active_pane == Pane::Resources)
                    .then(|| self.selected_identity())
                    .flatten();
                self.status_message = if fetch.is_partial() {
                    Self::partial_status(&fetch)
                } else {
                    "Ready".to_string()
                };
                self.roles = Self::merge_fetch(&self.roles, fetch, Pane::Resources);
                if self.active_pane == Pane::Resources {
                    self.refilter_keeping_cursor(ident);
                }
                self.loading = false;
                self.last_refresh = Some(Utc::now());
            }
            BgEvent::RolesLoaded(Err(e)) => {
                self.loading = false;
                self.status_message = format!("Failed to load roles: {e}");
            }
            BgEvent::GroupRolesLoaded(Ok(fetch)) => {
                let ident = (self.active_pane == Pane::Groups)
                    .then(|| self.selected_identity())
                    .flatten();
                self.group_status_message = if fetch.is_partial() {
                    Self::partial_status(&fetch)
                } else {
                    "Ready".to_string()
                };
                self.group_roles = Self::merge_fetch(&self.group_roles, fetch, Pane::Groups);
                self.groups_loaded = true;
                self.groups_loading = false;
                if self.active_pane == Pane::Groups {
                    self.refilter_keeping_cursor(ident);
                }
            }
            BgEvent::GroupRolesLoaded(Err(e)) => {
                self.groups_loading = false;
                self.group_status_message = format!("Failed: {e}");
            }
            BgEvent::ActivationResult { index, result } => {
                let roles = self.active_roles_mut();
                match result {
                    Ok(()) => {
                        if let Some(role) = roles.get_mut(index) {
                            role.status = RoleStatus::Activating;
                            role.selected = false;
                        }
                        match self.active_pane {
                            Pane::Resources => {
                                self.status_message =
                                    "Activation requested - refreshing...".to_string();
                            }
                            Pane::Groups => {
                                self.group_status_message =
                                    "Activation requested - refreshing...".to_string();
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(role) = roles.get_mut(index) {
                            role.status = RoleStatus::Failed(e.clone());
                        }
                        match self.active_pane {
                            Pane::Resources => {
                                self.status_message = format!("Activation failed: {e}");
                            }
                            Pane::Groups => {
                                self.group_status_message = format!("Activation failed: {e}");
                            }
                        }
                    }
                }
                self.update_filtered_indices();
            }
            BgEvent::DeactivationResult { index, result } => {
                let roles = self.active_roles_mut();
                match result {
                    Ok(()) => {
                        if let Some(role) = roles.get_mut(index) {
                            role.status = RoleStatus::Eligible;
                            role.selected = false;
                        }
                        match self.active_pane {
                            Pane::Resources => {
                                self.status_message = "Deactivated - refreshing...".to_string();
                            }
                            Pane::Groups => {
                                self.group_status_message =
                                    "Deactivated - refreshing...".to_string();
                            }
                        }
                    }
                    Err(e) => match self.active_pane {
                        Pane::Resources => {
                            self.status_message = format!("Deactivation failed: {e}");
                        }
                        Pane::Groups => {
                            self.group_status_message = format!("Deactivation failed: {e}");
                        }
                    },
                }
                self.update_filtered_indices();
            }
            BgEvent::RolePermissionsLoaded(Ok(perms)) => {
                self.role_permissions.extend(perms);
            }
            BgEvent::RolePermissionsLoaded(Err(_)) => {}
            BgEvent::GraphStatus(msg) => {
                self.group_status_message = msg;
            }
            BgEvent::JiraIssuesLoaded(Ok(issues)) => {
                self.jira_cache.store_search(Ok(issues.clone()));
                if let Some(form) = self.activation_form_mut() {
                    form.set_jira_issues(issues);
                }
            }
            BgEvent::JiraIssuesLoaded(Err(e)) => {
                self.jira_cache.store_search(Err(()));
                tracing::warn!("Jira lookup failed: {e}");
            }
            BgEvent::JiraIssueLookedUp { key, result } => match result {
                Ok(issue) => {
                    self.jira_cache.store_issue(&key, Ok(issue.clone()));
                    if let Some(form) = self.activation_form_mut() {
                        form.set_looked_up_issue(&key, issue);
                    }
                }
                Err(e) => {
                    self.jira_cache.store_issue(&key, Err(()));
                    tracing::warn!("Jira lookup of {key} failed: {e}");
                }
            },
        }
    }
}
