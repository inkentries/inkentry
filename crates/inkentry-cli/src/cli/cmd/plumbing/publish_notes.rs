use anyhow::Result;
use inkentry_core::storage::{PublishOutcome, SkipReason, publish_notes as core_publish_notes};

use super::PlumbingPublishNotesArgs;

// Runs against the git repo holding the CWD: git notes are the store of record
// before `inkentry init`, so publishing must not require an index.
pub async fn publish_notes(args: PlumbingPublishNotesArgs) -> Result<()> {
    let remote = args.remote.as_deref().unwrap_or("origin");

    match core_publish_notes(None, remote).await {
        Ok(PublishOutcome::Published { attempts }) => {
            emit(&serde_json::json!({
                "published": true,
                "remote": remote,
                "ref": "refs/notes/inkentry",
                "attempts": attempts,
            }));
            Ok(())
        }
        Ok(PublishOutcome::Skipped(reason)) => {
            // The hook drops stdout, so this skip must also go to stderr; the
            // other skips had nothing to publish.
            if reason == SkipReason::LockUnavailable {
                eprintln!(
                    "inkentry: memory not published: another inkentry process holds the \
                     notes lock."
                );
                eprintln!("inkentry: your code push is unaffected; your next push publishes it.");
            }
            emit(&serde_json::json!({
                "published": false,
                "remote": remote,
                "skipped": reason.as_str(),
            }));
            Ok(())
        }
        // A non-zero hook exit aborts the user's branch push.
        Err(e) if args.best_effort => {
            eprintln!("inkentry: {e:#}");
            eprintln!(
                "inkentry: your code push is unaffected. Retry with: \
                 git push {remote} refs/notes/inkentry"
            );
            emit(&serde_json::json!({
                "published": false,
                "remote": remote,
                "error": format!("{e:#}"),
            }));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn emit(value: &serde_json::Value) {
    println!("{value}");
}
