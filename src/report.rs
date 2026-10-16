use std::path::Path;
use std::time::Duration;

use crate::plan::{CopyReason, Plan, Removals};
use crate::sync::{Event, Outcome};

pub fn event_line(event: Event) -> String {
    match event {
        Event::Removed(path) => line("remove", path),
        Event::CreatedDir(path) => line("mkdir", path),
        Event::Copied(path, CopyReason::New) => line("new", path),
        Event::Copied(path, CopyReason::Changed) => line("update", path),
    }
}

pub fn plan_lines(plan: &Plan, removals: Removals) -> Vec<String> {
    let removed = plan
        .removals(removals)
        .into_iter()
        .map(|p| event_line(Event::Removed(p)));
    let dirs = plan.mkdir.iter().map(|p| event_line(Event::CreatedDir(p)));
    let copies = plan
        .copy
        .iter()
        .map(|c| event_line(Event::Copied(&c.path, c.reason)));
    removed.chain(dirs).chain(copies).collect()
}

pub fn dry_run_summary(plan: &Plan, removals: Removals) -> String {
    let new = plan
        .copy
        .iter()
        .filter(|c| c.reason == CopyReason::New)
        .count();
    let mut text = format!(
        "dry run: would copy {new} new, update {}, remove {}; {} unchanged",
        plan.copy.len() - new,
        plan.removals(removals).len(),
        plan.unchanged,
    );
    if removals == Removals::KeepDeleted && !plan.remove.is_empty() {
        text.push_str(&kept_note(plan.remove.len()));
    }
    text
}

pub fn summary(outcome: &Outcome, unchanged: usize, elapsed: Duration) -> String {
    let mut text = format!(
        "{} new, {} updated, {} removed, {} unchanged; {} copied in {:.2}s",
        outcome.copied_new,
        outcome.updated,
        outcome.removed,
        unchanged,
        human_bytes(outcome.bytes),
        elapsed.as_secs_f64(),
    );
    if outcome.kept > 0 {
        text.push_str(&kept_note(outcome.kept));
    }
    if outcome.removals_skipped > 0 {
        text.push_str(&format!(
            "; {} removals skipped because the source could not be fully read",
            outcome.removals_skipped
        ));
    }
    if outcome.not_attempted > 0 {
        text.push_str(&format!(
            "; {} copies not attempted because the target is full",
            outcome.not_attempted
        ));
    }
    if !outcome.errors.is_empty() {
        text.push_str(&format!("; {} errors", outcome.errors.len()));
    }
    text
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn kept_note(count: usize) -> String {
    format!("; {count} deleted from the source kept in the backup")
}

fn line(verb: &str, path: &Path) -> String {
    format!("{verb:<7}{}", path.display())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use super::*;
    use crate::plan::CopyAction;
    use crate::scan::Entry;

    fn sample_plan() -> Plan {
        let action = |path: &str, reason| CopyAction {
            path: path.into(),
            entry: Entry::File {
                size: 1,
                mtime: SystemTime::UNIX_EPOCH,
            },
            reason,
        };
        Plan {
            remove: vec![PathBuf::from("old.txt")],
            replace: vec![PathBuf::from("was_dir")],
            mkdir: vec![PathBuf::from("docs")],
            copy: vec![
                action("docs/a.txt", CopyReason::New),
                action("b.txt", CopyReason::Changed),
            ],
            unchanged: 4,
        }
    }

    #[test]
    fn plan_lines_list_removals_dirs_then_copies() {
        // given
        let plan = sample_plan();

        // when
        let lines = plan_lines(&plan, Removals::All);

        // then
        assert_eq!(
            lines,
            vec![
                "remove old.txt",
                "remove was_dir",
                "mkdir  docs",
                "new    docs/a.txt",
                "update b.txt"
            ]
        );
    }

    #[test]
    fn plan_lines_omit_removals_when_deletion_is_disabled() {
        // given
        let plan = sample_plan();

        // when
        let lines = plan_lines(&plan, Removals::None);

        // then
        assert!(!lines.iter().any(|l| l.starts_with("remove")));
    }

    #[test]
    fn dry_run_summary_counts_actions() {
        // given
        let plan = sample_plan();

        // when
        let text = dry_run_summary(&plan, Removals::All);

        // then
        assert_eq!(
            text,
            "dry run: would copy 1 new, update 1, remove 2; 4 unchanged"
        );
    }

    #[test]
    fn dry_run_keeping_deleted_files_lists_only_replacements() {
        // given
        let plan = sample_plan();

        // when
        let lines = plan_lines(&plan, Removals::KeepDeleted);
        let text = dry_run_summary(&plan, Removals::KeepDeleted);

        // then
        assert_eq!(lines[0], "remove was_dir");
        assert!(!lines.iter().any(|l| l.contains("old.txt")));
        assert_eq!(
            text,
            "dry run: would copy 1 new, update 1, remove 1; 4 unchanged; \
             1 deleted from the source kept in the backup"
        );
    }

    #[test]
    fn summary_mentions_skipped_removals_and_errors() {
        // given
        let outcome = Outcome {
            copied_new: 2,
            updated: 1,
            removals_skipped: 3,
            kept: 4,
            not_attempted: 5,
            bytes: 2048,
            errors: vec![crate::sync::SyncError {
                path: "x".into(),
                action: "copy",
                message: "boom".into(),
            }],
            ..Outcome::default()
        };

        // when
        let text = summary(&outcome, 10, Duration::from_millis(1500));

        // then
        assert_eq!(
            text,
            "2 new, 1 updated, 0 removed, 10 unchanged; 2.0 KiB copied in 1.50s; \
             4 deleted from the source kept in the backup; \
             3 removals skipped because the source could not be fully read; \
             5 copies not attempted because the target is full; 1 errors"
        );
    }

    #[test]
    fn human_bytes_picks_a_readable_unit() {
        // given
        let sizes = [0, 1023, 1536, 5 * 1024 * 1024 * 1024];

        // when
        let texts = sizes.map(human_bytes);

        // then
        assert_eq!(texts, ["0 B", "1023 B", "1.5 KiB", "5.0 GiB"]);
    }
}
