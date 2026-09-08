use crate::{BoardSummary, Status, Task};

pub fn summarize(tasks: &[Task]) -> BoardSummary {
    // Second worker assignment: aggregate totals and per-owner values.
    let mut summary = BoardSummary::default();
    for task in tasks {
        let done = task.status == Status::Done;

        summary.tasks += 1;
        if done {
            summary.done += 1;
        } else {
            summary.open_minutes += task.minutes;
        }

        let owner = summary.owners.entry(task.owner.clone()).or_default();
        owner.tasks += 1;
        if done {
            owner.done += 1;
        } else {
            owner.open_minutes += task.minutes;
        }
    }
    summary
}

pub fn next_task(tasks: &[Task]) -> Option<&Task> {
    // Prefer doing over todo, then fewer minutes, then earlier source line.
    tasks
        .iter()
        .filter(|task| task.status != Status::Done)
        .min_by_key(|task| (status_rank(task.status), task.minutes, task.line))
}

fn status_rank(status: Status) -> u8 {
    match status {
        Status::Doing => 0,
        Status::Todo => 1,
        Status::Done => 2,
    }
}
