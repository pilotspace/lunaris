//! `memory.profile` — the human-readable memory artifact (W4.2).
//!
//! `memory.recall` answers a question. It cannot answer "what do you actually
//! know about me?", and that is the question that exposed the curation gap: a
//! store holding 233k episodes could not produce one page a human would read.
//!
//! This renders a scope's captured knowledge as prose, grouped by kind, newest
//! first. It is a READ — it writes nothing, summarises nothing with an LLM,
//! and costs one scan of the scope's episode partition.
//!
//! ## Two decisions worth keeping
//!
//! **An empty profile is never blank.** A blank page and "nothing has ever
//! been curated into this store" are the same bytes and opposite meanings, and
//! the blank one reads as "nothing to report". The empty rendering says which
//! tool fills it — a problem stated without its remedy just moves the
//! confusion.
//!
//! **Telemetry is excluded, not ranked down.** The profile is a knowledge
//! artifact. Rendering the 91.6% of a real store that is `lunaris:tool_call`
//! envelopes would produce a page that looks busy and says nothing.

use serde::{Deserialize, Serialize};

use crate::ServiceError;
use lunaris::{Lunaris, recent_by_source};
use lunaris_core::{Episode, Scope};

/// The sections a profile renders, in order, as `(source prefix, heading)`.
///
/// Ordered most-durable-first: a constraint outlives the decision that
/// respected it, which outlives the fix that was needed once. A reader
/// skimming the top of the page should meet the longest-lived things first.
const SECTIONS: &[(&str, &str)] = &[
    ("constraint:", "Constraints"),
    ("preference:", "Preferences"),
    ("decision:", "Decisions"),
    ("fix:", "Fixes"),
    ("distilled:", "Distilled"),
    ("edit:", "Notable edits"),
];

fn default_limit_per_section() -> usize {
    10
}

/// Input parameters for `memory.profile`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileParams {
    /// How many of the most recent memories to render per section.
    #[serde(default = "default_limit_per_section")]
    pub limit_per_section: usize,
}

impl Default for ProfileParams {
    fn default() -> Self {
        Self { limit_per_section: default_limit_per_section() }
    }
}

/// Output of `memory.profile`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProfileResponse {
    /// The rendered artifact. Always non-empty, including for an empty scope.
    pub markdown: String,

    /// How many memories were rendered per section heading.
    pub counts: std::collections::BTreeMap<String, usize>,

    /// Total memories rendered across every section.
    pub total: usize,
}

/// One rendered bullet: the first line as the claim, the rest indented under it.
///
/// `memory.remember` stores `content`, then `Why: …` on its own line. Markdown
/// would swallow that second line into the bullet, so it is re-indented rather
/// than flattened — the rationale is the part worth keeping.
fn render_entry(episode: &Episode) -> String {
    let text = episode.content.trim();
    let mut lines = text.lines();
    let head = lines.next().unwrap_or_default().trim();
    let rest: Vec<&str> = lines.map(str::trim).filter(|l| !l.is_empty()).collect();
    if rest.is_empty() {
        format!("- {head}\n")
    } else {
        let tail = rest.iter().map(|l| format!("  {l}\n")).collect::<String>();
        format!("- {head}\n{tail}")
    }
}

/// Execute `memory.profile`.
///
/// # Errors
/// Propagates a storage failure. A profile that cannot read must not report an
/// empty store — that is the one wrong answer, because it reads as a true
/// negative.
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: ProfileParams,
) -> Result<ProfileResponse, ServiceError> {
    let limit = params.limit_per_section.clamp(1, 200);
    let storage = lunaris.storage();

    let mut counts = std::collections::BTreeMap::new();
    let mut body = String::new();
    let mut total = 0usize;

    for (prefix, heading) in SECTIONS {
        let episodes =
            recent_by_source(storage.as_ref(), scope, &[(*prefix).to_owned()], limit).await?;
        if episodes.is_empty() {
            continue;
        }
        counts.insert((*heading).to_owned(), episodes.len());
        total += episodes.len();

        body.push_str(&format!("\n## {heading}\n\n"));
        for episode in &episodes {
            body.push_str(&render_entry(episode));
        }
    }

    let markdown = if total == 0 {
        format!(
            "# What Lunaris knows about `{scope}`\n\n\
             Nothing has been captured in this scope yet.\n\n\
             This is not the same as an empty store — raw activity may well have been \
             recorded. It means nothing has been written down as knowledge, and nothing \
             else in the system turns activity into knowledge on its own.\n\n\
             Use `memory.remember` as you work, with one of four kinds: `decision` (a \
             choice and its rationale), `fix` (what broke and what actually fixed it), \
             `preference` (how this user wants to work), `constraint` (project state or \
             an invariant that bounds future work).\n",
            scope = scope.as_str()
        )
    } else {
        format!(
            "# What Lunaris knows about `{scope}`\n\n\
             {total} captured {noun}, newest first in each section.\n{body}",
            scope = scope.as_str(),
            noun = if total == 1 { "memory" } else { "memories" },
        )
    };

    Ok(ProfileResponse { markdown, counts, total })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::HlcClock;

    fn episode(content: &str) -> Episode {
        let clock = HlcClock::new(0);
        Episode::new(Scope::new("t").unwrap(), "decision:t", content, &clock)
    }

    #[test]
    fn a_rationale_is_indented_under_its_claim_not_flattened_into_it() {
        let out = render_entry(&episode("we chose Moon\n\nWhy: one round trip"));
        assert_eq!(out, "- we chose Moon\n  Why: one round trip\n");
    }

    #[test]
    fn a_single_line_memory_renders_as_one_bullet() {
        assert_eq!(render_entry(&episode("just a claim")), "- just a claim\n");
    }

    /// Every section heading is distinct, or two kinds collapse into one and a
    /// reader looking for preferences finds decisions.
    #[test]
    fn section_headings_and_prefixes_are_unique() {
        let mut headings: Vec<&str> = SECTIONS.iter().map(|(_, h)| *h).collect();
        let n = headings.len();
        headings.sort_unstable();
        headings.dedup();
        assert_eq!(headings.len(), n, "two sections share a heading");

        let mut prefixes: Vec<&str> = SECTIONS.iter().map(|(p, _)| *p).collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        assert_eq!(prefixes.len(), n, "two sections share a source prefix");
    }

    /// The four `memory.remember` kinds must each have a section, or a memory
    /// an agent deliberately captured is written but never shown.
    #[test]
    fn every_remember_kind_has_a_section() {
        for kind in [
            crate::remember::RememberKind::Decision,
            crate::remember::RememberKind::Fix,
            crate::remember::RememberKind::Preference,
            crate::remember::RememberKind::Constraint,
        ] {
            let want = format!("{}:", kind.as_str());
            assert!(
                SECTIONS.iter().any(|(p, _)| *p == want),
                "{} is capturable but has no profile section — it would be write-only",
                kind.as_str()
            );
        }
    }
}
