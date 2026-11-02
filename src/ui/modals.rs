use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::app::{ActivationForm, ActiveModal, App, ModalField};

pub fn render(f: &mut Frame, area: Rect, app: &App) {
    match &app.modal {
        ActiveModal::Activate { role_index, form } => {
            let role = &app.active_roles()[*role_index];
            render_activate_modal(
                f,
                area,
                &role.role_name,
                &role.scope_display_name,
                form,
                false,
                1,
            );
        }
        ActiveModal::BulkActivate { indices, form } => {
            render_activate_modal(f, area, "Multiple Roles", "", form, true, indices.len());
        }
        ActiveModal::DeactivateConfirm { role_index } => {
            let role = &app.active_roles()[*role_index];
            render_deactivate_modal(f, area, &role.role_name, &role.scope_display_name);
        }
        ActiveModal::Help => {
            crate::ui::help::render(f, area);
        }
        ActiveModal::None => {}
    }
}

fn render_activate_modal(
    f: &mut Frame,
    area: Rect,
    role_name: &str,
    scope_name: &str,
    form: &ActivationForm,
    is_bulk: bool,
    count: usize,
) {
    let height = if form.show_ticket { 50 } else { 40 };
    let modal_area = centered_rect(50, height, area);
    f.render_widget(Clear, modal_area);

    let title = if is_bulk {
        format!(" Activate {count} Roles ")
    } else {
        " Activate Role ".to_string()
    };

    let mut lines = vec![];

    if !is_bulk {
        lines.push(Line::from(vec![
            Span::styled("Role: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(role_name),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Scope: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(scope_name),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            format!("Activating {count} selected roles"),
            Style::default().add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::from(""));
    for &field in form.fields() {
        let focused = form.focused == field;
        let style = if focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}: ", field.label()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("[{}{}]", form.value(field), if focused { "▏" } else { "" }),
                style,
            ),
        ]));
        if field == ModalField::TicketNumber {
            let mut spans = vec![Span::raw("  ")];
            if let Some(issue) = form.current_issue() {
                spans.push(Span::styled(
                    issue.summary.clone(),
                    Style::default().fg(Color::Yellow),
                ));
            }
            if !form.jira_issues.is_empty() {
                let position = form
                    .jira_selected
                    .map_or("-".to_string(), |i| (i + 1).to_string());
                spans.push(Span::styled(
                    format!("  ({position}/{} ↑/↓)", form.jira_issues.len()),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if spans.len() > 1 {
                lines.push(Line::from(spans));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Tab/Shift+Tab: switch field  Enter: activate  Esc: cancel",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    f.render_widget(paragraph, modal_area);
}

fn render_deactivate_modal(f: &mut Frame, area: Rect, role_name: &str, scope_name: &str) {
    let modal_area = centered_rect(45, 25, area);
    f.render_widget(Clear, modal_area);

    let lines = vec![
        Line::from(vec![
            Span::styled("Role: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(role_name),
        ]),
        Line::from(vec![
            Span::styled("Scope: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(scope_name),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Deactivate this role?",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Enter: confirm  Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Deactivate Role ")
            .border_style(Style::default().fg(Color::Yellow)),
    );

    f.render_widget(paragraph, modal_area);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let x = area.width.saturating_sub(area.width * percent_x / 100) / 2;
    let y = area.height.saturating_sub(area.height * percent_y / 100) / 2;
    let w = area.width * percent_x / 100;
    let h = area.height * percent_y / 100;
    Rect::new(area.x + x, area.y + y, w, h)
}
