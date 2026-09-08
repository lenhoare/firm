use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Todo,
    Doing,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub line: usize,
    pub status: Status,
    pub owner: String,
    pub minutes: u32,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseIssue {
    pub line: usize,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnerSummary {
    pub tasks: usize,
    pub done: usize,
    pub open_minutes: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardSummary {
    pub tasks: usize,
    pub done: usize,
    pub open_minutes: u32,
    pub owners: BTreeMap<String, OwnerSummary>,
}
