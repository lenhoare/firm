use crate::BoardSummary;

pub fn render_report(summary: &BoardSummary) -> String {
    let mut out = format!(
        "Tasks: {} | done: {} | open: {}m\n",
        summary.tasks, summary.done, summary.open_minutes
    );
    for (owner, owner_summary) in &summary.owners {
        out.push_str(&format!(
            "{}: {} tasks | done: {} | open: {}m\n",
            owner, owner_summary.tasks, owner_summary.done, owner_summary.open_minutes
        ));
    }
    out
}
