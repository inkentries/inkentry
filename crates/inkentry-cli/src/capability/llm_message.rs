//! User-facing text for "no LLM is available", shared by every command that
//! needs one so they read as one product rather than several dialects. Since
//! chunk summaries moved to the deterministic built-in tier, `harvest` is the
//! only feature that reaches for an LLM.

/// Why LLM routing found nothing to run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoLlmReason {
    /// Offline mode is explicitly in force: no probe was made and none will be.
    Offline,
    /// `llm_url` is configured, but the reachable local server does not serve
    /// an LLM. Deliberately terminal: falling through to a remote LLM after the
    /// user asked for a local one would ship their code somewhere they did not
    /// choose.
    LocalConfiguredButNotServed,
    /// Neither the local server nor an explicitly configured `server_url`
    /// offers an LLM.
    NoLlmAnywhere,
}

/// Render the no-LLM notice for `harvest`, the sole LLM-backed feature.
///
/// Every branch names the cause and the next step, and none of them names a
/// type, module or internal field: `llm_url` and `server_url` appear only
/// because they are config keys the reader can actually edit.
pub fn no_llm_message(reason: NoLlmReason) -> String {
    let subject = "'inkentry harvest' cannot run";
    let body = match reason {
        NoLlmReason::Offline => "offline mode is on, so no inference will run.\n\
             Turn offline mode off to enable it: unset INKENTRY_NO_SERVER, or remove \
             `mode = \"offline\"` from your inkentry config."
            .to_string(),
        NoLlmReason::LocalConfiguredButNotServed => {
            "your local inkentry server is running without the LLM endpoint you set in \
             `llm_url`, so it cannot answer LLM requests.\n\
             A running server keeps the settings it started with, so restart it to pick \
             yours up:\n  \
             inkentry server stop\n  \
             inkentry server start"
                .to_string()
        }
        NoLlmReason::NoLlmAnywhere => "no LLM is available.\n\
             There are two ways to get one:\n  \
             set `llm_url` in ~/.config/inkentry/config.toml to your own \
             chat-completions endpoint, then run `inkentry server stop` and \
             `inkentry server start`;\n  \
             or set `server_url` to a inkentry server that already provides one."
            .to_string(),
    };
    format!("{subject}: {body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const REASONS: [NoLlmReason; 3] = [
        NoLlmReason::Offline,
        NoLlmReason::LocalConfiguredButNotServed,
        NoLlmReason::NoLlmAnywhere,
    ];

    // The jargon in the message this task replaces is what created the task.
    // No message may name an internal type, adapter or field; a reader can
    // only act on things they can edit.
    #[test]
    fn no_message_leaks_an_internal_type_or_field() {
        for reason in REASONS {
            let msg = no_llm_message(reason);
            for jargon in [
                "ServerInferenceClient",
                "ServerLlmClient",
                "ServerLlmAdapter",
                "ServerEmbedAdapter",
                "Capabilities",
                "inference_url",
                "llm.complete",
                "Tier",
            ] {
                assert!(
                    !msg.contains(jargon),
                    "{reason:?} message leaks {jargon:?}: {msg}"
                );
            }
        }
    }

    #[test]
    fn every_message_names_the_command_and_a_next_step() {
        for reason in REASONS {
            let msg = no_llm_message(reason);
            assert!(
                msg.starts_with("'inkentry harvest' cannot run"),
                "{reason:?} must lead with the command it concerns: {msg}"
            );
            assert!(
                msg.contains("inkentry ") || msg.contains("INKENTRY_"),
                "{reason:?} must give a command or setting to act on: {msg}"
            );
        }
    }

    #[test]
    fn offline_message_names_offline_mode_and_how_to_leave_it() {
        let msg = no_llm_message(NoLlmReason::Offline);
        assert!(msg.contains("offline mode is on"), "{msg}");
        assert!(msg.contains("INKENTRY_NO_SERVER"), "{msg}");
        assert!(msg.contains("mode = \"offline\""), "{msg}");
    }

    // The stale-daemon case: the setting is right, the running process is
    // older than it. The only useful instruction is the restart.
    #[test]
    fn local_configured_but_not_served_message_names_llm_url_and_the_restart() {
        let msg = no_llm_message(NoLlmReason::LocalConfiguredButNotServed);
        assert!(msg.contains("llm_url"), "{msg}");
        assert!(msg.contains("inkentry server stop"), "{msg}");
        assert!(msg.contains("inkentry server start"), "{msg}");
        assert!(
            !msg.contains("server_url"),
            "must not send the user to a remote LLM they deliberately did not choose: {msg}"
        );
    }

    #[test]
    fn no_llm_anywhere_message_offers_both_routes_to_an_llm() {
        let msg = no_llm_message(NoLlmReason::NoLlmAnywhere);
        assert!(msg.contains("llm_url"), "local route missing: {msg}");
        assert!(msg.contains("server_url"), "remote route missing: {msg}");
    }

    // Harvest is a top-level command; its no-LLM subject must name
    // `inkentry harvest`, not the deprecated `inkentry memory harvest` spelling.
    #[test]
    fn harvest_subject_names_the_top_level_command() {
        let msg = no_llm_message(NoLlmReason::NoLlmAnywhere);
        assert!(
            msg.starts_with("'inkentry harvest' cannot run"),
            "harvest no-LLM message must name the top-level command: {msg}"
        );
    }

    #[test]
    fn each_reason_renders_a_distinct_message() {
        let rendered: Vec<String> = REASONS.iter().map(|r| no_llm_message(*r)).collect();
        for (i, a) in rendered.iter().enumerate() {
            for b in rendered.iter().skip(i + 1) {
                assert_ne!(a, b, "two reasons render identically, so one is unusable");
            }
        }
    }
}
