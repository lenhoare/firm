use firm_taskboard::{Status, next_task, parse_board, render_report, summarize};

const VALID: &str = "\n# sprint board\ntodo | alice | 30 | Write parser\ndoing | bob | 20 | Add report\ndone | alice | 10 | Define types\ntodo | bob | 5 | Polish docs\n";

#[test]
fn parses_valid_rows_and_source_lines() {
    let tasks = parse_board(VALID).expect("valid board");
    assert_eq!(tasks.len(), 4);
    assert_eq!(
        (
            tasks[0].line,
            tasks[0].status,
            tasks[0].owner.as_str(),
            tasks[0].minutes,
            tasks[0].title.as_str()
        ),
        (3, Status::Todo, "alice", 30, "Write parser")
    );
    assert_eq!(tasks[3].line, 6);
}

#[test]
fn collects_all_parse_issues_in_line_order() {
    let bad = "later | a | 10 | bad status\ntodo | | 4 | no owner\ndoing | b | 0 | zero\ndone | c | xx | bad minutes\ntodo | d | 3 |\ntodo | e | 2\n";
    let issues = parse_board(bad).expect_err("all rows invalid");
    assert_eq!(
        issues.iter().map(|i| i.line).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
    assert!(issues[0].message.contains("status"));
    assert!(issues[1].message.contains("owner"));
    assert!(issues[2].message.contains("positive"));
    assert!(issues[3].message.contains("minutes"));
    assert!(issues[4].message.contains("title"));
    assert!(issues[5].message.contains("four fields"));
}

#[test]
fn summarizes_and_selects_next_work() {
    let tasks = parse_board(VALID).unwrap();
    let summary = summarize(&tasks);
    assert_eq!(
        (summary.tasks, summary.done, summary.open_minutes),
        (4, 1, 55)
    );
    assert_eq!(
        (
            summary.owners["alice"].tasks,
            summary.owners["alice"].done,
            summary.owners["alice"].open_minutes
        ),
        (2, 1, 30)
    );
    assert_eq!(
        (
            summary.owners["bob"].tasks,
            summary.owners["bob"].done,
            summary.owners["bob"].open_minutes
        ),
        (2, 0, 25)
    );
    let next = next_task(&tasks).unwrap();
    assert_eq!(
        (next.status, next.owner.as_str(), next.title.as_str()),
        (Status::Doing, "bob", "Add report")
    );
}

#[test]
fn next_task_tiebreaks_by_minutes_then_line_and_ignores_done() {
    let tasks =
        parse_board("todo|z|5|later line\ntodo|a|5|same estimate\ndone|a|1|finished").unwrap();
    assert_eq!(next_task(&tasks).unwrap().title, "later line");
    assert!(next_task(&parse_board("done|a|1|finished").unwrap()).is_none());
}

#[test]
fn report_is_exact_and_owner_order_is_stable() {
    let report = render_report(&summarize(&parse_board(VALID).unwrap()));
    assert_eq!(
        report,
        "Tasks: 4 | done: 1 | open: 55m\nalice: 2 tasks | done: 1 | open: 30m\nbob: 2 tasks | done: 0 | open: 25m\n"
    );
}

#[test]
fn empty_board_is_valid_and_has_a_report() {
    let tasks = parse_board("\n # nothing today\n").unwrap();
    assert!(tasks.is_empty());
    assert_eq!(
        render_report(&summarize(&tasks)),
        "Tasks: 0 | done: 0 | open: 0m\n"
    );
}
