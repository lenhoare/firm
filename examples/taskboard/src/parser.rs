use crate::{ParseIssue, Status, Task};

pub fn parse_board(input: &str) -> Result<Vec<Task>, Vec<ParseIssue>> {
    let mut tasks = Vec::new();
    let mut issues = Vec::new();

    for (idx, raw) in input.lines().enumerate() {
        let line = idx + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let fields: Vec<&str> = trimmed.split('|').map(str::trim).collect();
        if fields.len() != 4 {
            issues.push(ParseIssue {
                line,
                message: "expected four fields".to_string(),
            });
            continue;
        }

        let status = match fields[0] {
            "todo" => Status::Todo,
            "doing" => Status::Doing,
            "done" => Status::Done,
            _ => {
                issues.push(ParseIssue {
                    line,
                    message: format!("invalid status '{}'", fields[0]),
                });
                continue;
            }
        };

        if fields[1].is_empty() {
            issues.push(ParseIssue {
                line,
                message: "owner must not be empty".to_string(),
            });
            continue;
        }

        let minutes = match fields[2].parse::<u32>() {
            Ok(0) => {
                issues.push(ParseIssue {
                    line,
                    message: "minutes must be positive".to_string(),
                });
                continue;
            }
            Ok(n) => n,
            Err(_) => {
                issues.push(ParseIssue {
                    line,
                    message: "invalid minutes".to_string(),
                });
                continue;
            }
        };

        if fields[3].is_empty() {
            issues.push(ParseIssue {
                line,
                message: "title must not be empty".to_string(),
            });
            continue;
        }

        tasks.push(Task {
            line,
            status,
            owner: fields[1].to_string(),
            minutes,
            title: fields[3].to_string(),
        });
    }

    if issues.is_empty() {
        Ok(tasks)
    } else {
        Err(issues)
    }
}
