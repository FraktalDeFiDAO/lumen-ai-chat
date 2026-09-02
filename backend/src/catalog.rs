//! What each AI coder can actually be asked to run.
//!
//! Every coder answers the question "which models do you have?" differently:
//! Hermes keeps a JSON cache of every provider's live `/v1/models`, OpenCode
//! and KiloCode each print their own line format, and Claude Code and Codex
//! cannot be asked at all. Without somewhere to put that, `discover_routes`
//! would have to know all five mechanisms, so it knows none of them and
//! offers a single opaque `default` route per coder instead.
//!
//! A [`ModelCatalog`] is one coder's answer. It separates *where the answer
//! comes from* ([`Probe`]) from *what the answer means* ([`ModelCatalog::interpret`]),
//! which is what lets every adapter be tested against real recorded output
//! with none of these CLIs installed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;
use tracing::debug;

use crate::coders::CoderKind;

/// One model a coder can be asked to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub provider: String,
    pub model: String,
}

impl DiscoveredModel {
    fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

/// Where a coder's answer comes from.
pub enum Probe {
    /// Read this file, if it is there.
    File(PathBuf),
    /// Run the coder's own executable with these arguments.
    Command(&'static [&'static str]),
    /// The coder cannot be asked; [`ModelCatalog::interpret`] already knows.
    Fixed,
}

/// How one coder answers for its models.
pub trait ModelCatalog: Send + Sync {
    fn kind(&self) -> CoderKind;
    /// How to obtain this coder's raw answer.
    fn probe(&self) -> Probe;
    /// What that answer means. Given `""` for [`Probe::Fixed`].
    fn interpret(&self, raw: &str) -> Vec<DiscoveredModel>;
}

/// Hermes refreshes every provider's live `/v1/models` into one disk cache,
/// keyed by provider, each entry carrying a fingerprint, a fetch time and the
/// model list. Only the last of those three is a model name.
pub struct HermesCatalog;

impl ModelCatalog for HermesCatalog {
    fn kind(&self) -> CoderKind {
        CoderKind::Hermes
    }

    fn probe(&self) -> Probe {
        Probe::File(
            dirs_home()
                .join(".hermes")
                .join("provider_models_cache.json"),
        )
    }

    fn interpret(&self, raw: &str) -> Vec<DiscoveredModel> {
        let Ok(cache) = serde_json::from_str::<HashMap<String, HermesEntry>>(raw) else {
            return Vec::new();
        };
        let mut found: Vec<_> = cache
            .into_iter()
            .flat_map(|(provider, entry)| {
                entry
                    .models
                    .into_iter()
                    .map(move |model| DiscoveredModel::new(provider.clone(), model))
            })
            .collect();
        found.sort_by(|a, b| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)));
        found
    }
}

/// Only `models` is a model list; `fp` and `at` are the cache's own bookkeeping.
#[derive(Deserialize)]
struct HermesEntry {
    models: Vec<String>,
}

/// OpenCode and KiloCode both document `--model` as taking `provider/model`
/// and both print exactly that, one per line. The vendor, where there is one,
/// belongs to the model id -- `zenmux/anthropic/claude-fable-5` is the model
/// `anthropic/claude-fable-5` served by `zenmux` -- so only the first slash
/// separates the two halves.
pub struct LineCatalog {
    kind: CoderKind,
    args: &'static [&'static str],
}

pub const OPENCODE_CATALOG: LineCatalog = LineCatalog {
    kind: CoderKind::Opencode,
    args: &["models"],
};
pub const KILOCODE_CATALOG: LineCatalog = LineCatalog {
    kind: CoderKind::Kilocode,
    args: &["models"],
};

impl ModelCatalog for LineCatalog {
    fn kind(&self) -> CoderKind {
        self.kind
    }

    fn probe(&self) -> Probe {
        Probe::Command(self.args)
    }

    fn interpret(&self, raw: &str) -> Vec<DiscoveredModel> {
        raw.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter_map(|line| line.split_once('/'))
            .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
            .map(|(provider, model)| DiscoveredModel::new(provider, model))
            .collect()
    }
}

/// Claude Code has no way to be asked what it can run. Its help documents the
/// aliases it accepts, and `--model` takes one of those or a full model name;
/// the aliases are the honest answer, because they are what stays correct as
/// Anthropic ships new versions.
pub struct ClaudeCatalog;

const CLAUDE_ALIASES: [&str; 3] = ["fable", "opus", "sonnet"];

impl ModelCatalog for ClaudeCatalog {
    fn kind(&self) -> CoderKind {
        CoderKind::Claude
    }

    fn probe(&self) -> Probe {
        Probe::Fixed
    }

    fn interpret(&self, _raw: &str) -> Vec<DiscoveredModel> {
        CLAUDE_ALIASES
            .iter()
            .map(|alias| DiscoveredModel::new("claude", *alias))
            .collect()
    }
}

/// Codex has no list command either, but it does record the model it will use.
/// Only the top-level key is that model: a `[projects."..."]` section carries
/// per-directory settings that say nothing about what this service would run.
pub struct CodexCatalog;

impl ModelCatalog for CodexCatalog {
    fn kind(&self) -> CoderKind {
        CoderKind::Codex
    }

    fn probe(&self) -> Probe {
        Probe::File(dirs_home().join(".codex").join("config.toml"))
    }

    fn interpret(&self, raw: &str) -> Vec<DiscoveredModel> {
        raw.lines()
            .map(str::trim)
            .take_while(|line| !line.starts_with('['))
            .filter_map(|line| line.strip_prefix("model"))
            .filter_map(|rest| rest.trim_start().strip_prefix('='))
            .map(|value| value.trim().trim_matches('"'))
            .filter(|model| !model.is_empty())
            .map(|model| DiscoveredModel::new("codex", model))
            .collect()
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// A coder's answer is never worth more than this. A CLI that streams
/// unbounded output must not be able to exhaust the service's memory.
const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;

/// The catalog for one coder.
pub fn catalog_for(kind: CoderKind) -> &'static dyn ModelCatalog {
    match kind {
        CoderKind::Hermes => &HermesCatalog,
        CoderKind::Opencode => &OPENCODE_CATALOG,
        CoderKind::Kilocode => &KILOCODE_CATALOG,
        CoderKind::Claude => &ClaudeCatalog,
        CoderKind::Codex => &CodexCatalog,
    }
}

/// Ask one coder what it can run.
///
/// Discovery is best-effort by construction: a coder that is not logged in,
/// has no cache yet, or has no list command is not a failure worth surfacing.
/// It simply has nothing to add, and the caller keeps whatever it already had.
pub async fn discover(
    catalog: &dyn ModelCatalog,
    executable: &Path,
    timeout: Duration,
) -> Vec<DiscoveredModel> {
    let raw = match catalog.probe() {
        Probe::Fixed => String::new(),
        Probe::File(path) => match tokio::fs::read_to_string(&path).await {
            Ok(contents) => contents,
            Err(error) => {
                debug!(coder = catalog.kind().id(), path = %path.display(), %error, "no model cache to read");
                return Vec::new();
            }
        },
        Probe::Command(args) => match run(executable, args, timeout).await {
            Ok(output) => output,
            Err(error) => {
                debug!(coder = catalog.kind().id(), %error, "could not ask the coder for its models");
                return Vec::new();
            }
        },
    };
    catalog.interpret(&raw)
}

async fn run(executable: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs()))?
        .map_err(|error| error.to_string())?;

    if !output.status.success() {
        return Err(format!("exited with {}", output.status));
    }
    if output.stdout.len() > MAX_CATALOG_BYTES {
        return Err("produced too much output".into());
    }
    String::from_utf8(output.stdout).map_err(|_| "produced output that is not text".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const OPENCODE_MODELS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/catalog/opencode-models.txt"
    ));
    pub(super) const KILOCODE_MODELS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/catalog/kilocode-models.txt"
    ));

    pub(super) const HERMES_CACHE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/catalog/hermes-cache.json"
    ));

    #[test]
    fn hermes_reports_a_model_under_the_provider_that_serves_it() {
        let found = HermesCatalog.interpret(HERMES_CACHE);

        assert!(
            found.contains(&DiscoveredModel::new("anthropic", "claude-opus-5")),
            "the anthropic provider's own model list was lost: {found:?}",
        );
    }

    /// A provider that resells other vendors names its models `vendor/model`.
    /// Splitting on `/` would file `claude-opus-5` under a provider called
    /// `anthropic` that Hermes never offered.
    #[test]
    fn hermes_keeps_a_reseller_model_whole() {
        let found = HermesCatalog.interpret(HERMES_CACHE);

        assert!(
            found.contains(&DiscoveredModel::new(
                "openrouter",
                "anthropic/claude-opus-5"
            )),
            "a slash-bearing model id was mangled: {found:?}",
        );
    }

    #[test]
    fn hermes_ignores_the_cache_bookkeeping() {
        let found = HermesCatalog.interpret(HERMES_CACHE);

        assert!(
            !found
                .iter()
                .any(|model| model.model == "fp" || model.model == "at"),
            "cache metadata was offered as a model: {found:?}",
        );
    }
}

#[cfg(test)]
mod line_tests {
    use super::tests::{KILOCODE_MODELS, OPENCODE_MODELS};
    use super::*;

    #[test]
    fn opencode_splits_a_plain_line_into_provider_and_model() {
        let found = OPENCODE_CATALOG.interpret(OPENCODE_MODELS);

        assert!(
            found.contains(&DiscoveredModel::new("opencode", "big-pickle")),
            "a plain provider/model line was lost: {found:?}",
        );
    }

    /// `--model` is documented as taking `provider/model`, so a line naming a
    /// vendor as well must keep the vendor with the model. Splitting on the
    /// last slash would ask `zenmux/anthropic` for a model it has never heard of.
    #[test]
    fn opencode_leaves_the_vendor_with_the_model() {
        let found = OPENCODE_CATALOG.interpret(OPENCODE_MODELS);

        assert!(
            found.contains(&DiscoveredModel::new("zenmux", "anthropic/claude-fable-5")),
            "a vendor-qualified model was split in the wrong place: {found:?}",
        );
    }

    #[test]
    fn kilocode_reads_both_of_its_providers() {
        let found = KILOCODE_CATALOG.interpret(KILOCODE_MODELS);

        assert!(found.contains(&DiscoveredModel::new(
            "kilo",
            "~anthropic/claude-opus-latest"
        )));
        assert!(found.contains(&DiscoveredModel::new("zenmux", "anthropic/claude-opus-4")));
    }

    /// Whatever is stored is handed straight back as `--model provider/model`,
    /// so it has to be exactly the identifier the CLI printed.
    #[test]
    fn every_parsed_model_round_trips_to_the_line_it_came_from() {
        for (catalog, raw) in [
            (&OPENCODE_CATALOG, OPENCODE_MODELS),
            (&KILOCODE_CATALOG, KILOCODE_MODELS),
        ] {
            let lines: Vec<_> = raw
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            let rebuilt: Vec<_> = catalog
                .interpret(raw)
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.model))
                .collect();
            assert_eq!(rebuilt, lines, "{:?} did not round-trip", catalog.kind());
        }
    }

    #[test]
    fn a_blank_or_malformed_line_is_not_a_model() {
        let found = OPENCODE_CATALOG.interpret("\n\nnot-a-route\n  \nopencode/real\n");

        assert_eq!(found, vec![DiscoveredModel::new("opencode", "real")]);
    }
}

#[cfg(test)]
mod fixed_tests {
    use super::*;

    const CODEX_CONFIG: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/catalog/codex-config.toml"
    ));

    #[test]
    fn claude_offers_the_aliases_its_help_documents() {
        let found = ClaudeCatalog.interpret("");

        assert_eq!(
            found,
            vec![
                DiscoveredModel::new("claude", "fable"),
                DiscoveredModel::new("claude", "opus"),
                DiscoveredModel::new("claude", "sonnet"),
            ]
        );
    }

    #[test]
    fn codex_reads_the_model_it_is_configured_with() {
        let found = CodexCatalog.interpret(CODEX_CONFIG);

        assert_eq!(found, vec![DiscoveredModel::new("codex", "gpt-5.6-sol")]);
    }

    /// `model_reasoning_effort` starts with `model`, and a per-project section
    /// can set its own `model`. Neither is the model this service would run.
    #[test]
    fn codex_ignores_neighbouring_keys_and_project_sections() {
        let found = CodexCatalog.interpret(CODEX_CONFIG);

        assert!(
            !found
                .iter()
                .any(|m| m.model == "medium" || m.model == "not-the-default"),
            "{found:?}"
        );
    }
}

#[cfg(test)]
mod driver_tests {
    use super::*;

    /// The driver is tested through the trait rather than through any one
    /// coder, because what it owes every adapter is the same: obtain the raw
    /// answer, or treat the coder as having nothing to say.
    struct Fake(Probe);

    impl ModelCatalog for Fake {
        fn kind(&self) -> CoderKind {
            CoderKind::Hermes
        }
        fn probe(&self) -> Probe {
            match &self.0 {
                Probe::File(path) => Probe::File(path.clone()),
                Probe::Command(args) => Probe::Command(args),
                Probe::Fixed => Probe::Fixed,
            }
        }
        fn interpret(&self, raw: &str) -> Vec<DiscoveredModel> {
            OPENCODE_CATALOG.interpret(raw)
        }
    }

    const SECOND: Duration = Duration::from_secs(10);
    fn nowhere() -> PathBuf {
        PathBuf::from("/nonexistent/executable")
    }

    #[tokio::test]
    async fn a_fixed_catalog_is_asked_for_nothing() {
        let found = discover(&ClaudeCatalog, &nowhere(), SECOND).await;

        assert_eq!(
            found.len(),
            3,
            "a coder that needs no probe was not consulted"
        );
    }

    #[tokio::test]
    async fn a_file_catalog_reads_what_is_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("models.txt");
        std::fs::write(&path, "zenmux/anthropic/claude-opus-5\n").expect("write");

        let found = discover(&Fake(Probe::File(path)), &nowhere(), SECOND).await;

        assert_eq!(
            found,
            vec![DiscoveredModel::new("zenmux", "anthropic/claude-opus-5")]
        );
    }

    /// A coder that was never logged in has no cache. That is the ordinary
    /// case, not an error, and it must leave the caller's routes untouched.
    #[tokio::test]
    async fn a_missing_file_means_the_coder_has_nothing_to_say() {
        let found = discover(
            &Fake(Probe::File("/nonexistent/cache.json".into())),
            &nowhere(),
            SECOND,
        )
        .await;

        assert_eq!(found, Vec::new());
    }

    #[tokio::test]
    async fn a_command_catalog_reads_the_lines_the_coder_printed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("coder");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'kilo/a-model\\nzenmux/vendor/other\\n'\n",
        )
        .expect("write");
        make_executable(&script);

        let found = discover(&Fake(Probe::Command(&["models"])), &script, SECOND).await;

        assert_eq!(
            found,
            vec![
                DiscoveredModel::new("kilo", "a-model"),
                DiscoveredModel::new("zenmux", "vendor/other"),
            ]
        );
    }

    #[tokio::test]
    async fn a_coder_that_fails_is_not_a_service_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("coder");
        std::fs::write(&script, "#!/bin/sh\necho 'not logged in' >&2\nexit 1\n").expect("write");
        make_executable(&script);

        let found = discover(&Fake(Probe::Command(&["models"])), &script, SECOND).await;

        assert_eq!(found, Vec::new(), "a failing CLI must not fabricate routes");
    }

    /// KiloCode takes seven seconds to answer. A coder that never answers must
    /// not hold up whatever is waiting on discovery.
    #[tokio::test]
    async fn a_coder_that_hangs_is_given_up_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("coder");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").expect("write");
        make_executable(&script);

        let started = std::time::Instant::now();
        let found = discover(
            &Fake(Probe::Command(&["models"])),
            &script,
            Duration::from_millis(300),
        )
        .await;

        assert_eq!(found, Vec::new());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout did not fire"
        );
    }

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}
