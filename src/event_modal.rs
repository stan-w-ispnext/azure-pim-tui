use crossterm::event::{KeyCode, KeyEvent};

use crate::app::{ActivationForm, ActiveModal, App, ModalField};
use crate::client::jira_cache::JiraCache;
use crate::client::models::ActivationRequest;

pub fn handle_modal_key(app: &mut App, key: KeyEvent) -> Option<ModalAction> {
    match &mut app.modal {
        ActiveModal::Activate { form, .. } | ActiveModal::BulkActivate { form, .. } => {
            match key.code {
                KeyCode::Esc => {
                    app.modal = ActiveModal::None;
                    None
                }
                KeyCode::Tab => {
                    form.cycle_focus(true);
                    None
                }
                KeyCode::BackTab => {
                    form.cycle_focus(false);
                    None
                }
                KeyCode::Up | KeyCode::Down if form.focused == ModalField::TicketNumber => {
                    form.cycle_jira_issue(key.code == KeyCode::Down);
                    None
                }
                KeyCode::Enter => {
                    let request = form.to_request(app.config.default_duration_hours);
                    let indices = match &app.modal {
                        ActiveModal::Activate { role_index, .. } => vec![*role_index],
                        ActiveModal::BulkActivate { indices, .. } => indices.clone(),
                        _ => return None,
                    };
                    app.modal = ActiveModal::None;
                    Some(ModalAction::Activate { indices, request })
                }
                KeyCode::Char(c) => {
                    if form.focused != ModalField::Duration || c.is_ascii_digit() {
                        form.value_mut(form.focused).push(c);
                        after_edit(form, &app.jira_cache);
                    }
                    None
                }
                KeyCode::Backspace => {
                    form.value_mut(form.focused).pop();
                    after_edit(form, &app.jira_cache);
                    None
                }
                _ => None,
            }
        }
        ActiveModal::DeactivateConfirm { role_index } => match key.code {
            KeyCode::Enter => {
                let idx = *role_index;
                app.modal = ActiveModal::None;
                Some(ModalAction::Deactivate { index: idx })
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                app.modal = ActiveModal::None;
                None
            }
            _ => None,
        },
        ActiveModal::Help => {
            app.modal = ActiveModal::None;
            None
        }
        ActiveModal::None => None,
    }
}

fn after_edit(form: &mut ActivationForm, cache: &JiraCache) {
    match form.focused {
        ModalField::TicketNumber => {
            form.on_ticket_number_edited();
            // Answer from the cache immediately; only uncached keys wait for
            // typing to pause before a request is sent.
            if let Some(key) = form.pending_lookup_key() {
                if let Some(issue) = cache.peek_issue(&key) {
                    form.set_looked_up_issue(&key, issue);
                }
            }
        }
        ModalField::Justification => form.justification_edited = true,
        _ => {}
    }
}

pub enum ModalAction {
    Activate {
        indices: Vec<usize>,
        request: ActivationRequest,
    },
    Deactivate {
        index: usize,
    },
}
