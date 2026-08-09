//! Instructions handed to each role.
//!
//! Context moves between agents through persisted artifacts, not shared chat history. Three
//! different harnesses with three different context windows cannot share a conversation, and an
//! artifact on disk survives a crash, gets read by `kage status`, and can be inspected after the
//! fact. So each prompt is assembled from files and stands entirely on its own.

use std::path::Path;

use crate::engine::gates::Verdict;
use crate::state::Artifacts;

/// Rules that apply to every role, restated per prompt because the agents share no history.
fn preamble(role: &str, workdir: &Path) -> String {
    format!(
        "You are the **{role}** in an automated engineering loop called Kage.\n\
         You are running non-interactively: nobody will answer a question, so do not ask any.\n\
         The repository you are working in is at `{}`. Work only inside it.\n\n",
        workdir.display()
    )
}

/// Instruct an agent to write an artifact, and to write it as a file rather than to the terminal.
///
/// Stating this explicitly matters: a CLI agent's natural instinct is to print its answer, and a
/// printed plan is invisible to the next phase, which reads from disk.
fn deliverable(delivery: Delivery, path: &Path, what: &str) -> String {
    match delivery {
        Delivery::AgentWrites => format!(
            "## Deliverable\n\n\
             Write {what} to this exact path:\n\n\
             `{}`\n\n\
             Create or overwrite that file. Do not print the content instead of writing it — the \
             next stage of the loop reads the file, not your terminal output.\n\n",
            path.display()
        ),
        Delivery::KageWrites => format!(
            "## Deliverable\n\n\
             Reply with {what} and nothing else. Your entire response is saved verbatim as `{}`, \
             so anything else you write — a preamble, a closing remark, a fence wrapped around the \
             whole answer — ends up inside the artifact the next stage reads.\n\n\
             You have no tools and no filesystem access. Everything you need is in this message.\n\n",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
    }
}

/// Who puts the deliverable on disk.
///
/// A spawned harness writes its own file; an API-backed role returns text and Kage saves it. The
/// rest of the prompt is identical either way — only this instruction differs, because telling a
/// model with no filesystem to "write to this path" earns a confident description of a file that
/// was never created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    AgentWrites,
    KageWrites,
}

impl Delivery {
    /// The adapter's `writes_own_artifacts`, as a delivery mode.
    pub fn from_adapter(writes_own_artifacts: bool) -> Self {
        if writes_own_artifacts {
            Self::AgentWrites
        } else {
            Self::KageWrites
        }
    }
}

/// Ask the planner for an executable engineering contract.
///
/// The structure below is what turns a plan into something a cheap executor can follow literally.
/// A vague plan pushes design decisions down to the weakest model in the loop, which is exactly
/// backwards from the premise that expensive reasoning should happen once, up front.
pub fn planner(task: &str, workdir: &Path, artifacts: &Artifacts, delivery: Delivery) -> String {
    format!(
        "{}\
         Your job is to produce an implementation plan. You are the most capable model in this \
         loop; the agent that implements your plan is cheaper and will follow it literally. Spend \
         your effort on the decisions that are expensive to get wrong: architecture, interfaces, \
         edge cases, and what must not change.\n\n\
         Do not implement anything. Read the repository as much as you need first.\n\n\
         ## Task\n\n{task}\n\n\
         {}\
         ## Required structure\n\n\
         ```markdown\n\
         # Objective\n\n\
         # Current System\n\n\
         # Desired Behavior\n\n\
         # Scope\n\n\
         ## Must Change\n\n\
         ## May Change\n\n\
         ## Must NOT Change\n\n\
         # Architecture Decision\n\n\
         # Files\n\n\
         | File | Action | Purpose |\n\
         | ---- | ------ | ------- |\n\n\
         # Implementation Steps\n\n\
         ## Step 1\n\n\
         Expected result:\n\n\
         # Interfaces / Contracts\n\n\
         # Edge Cases\n\n\
         # Error Handling\n\n\
         # Security Considerations\n\n\
         # Tests Required\n\n\
         # Acceptance Criteria\n\n\
         # Definition of Done\n\
         ```\n\n\
         Every file you list must really exist or be one you are asking to create — verify paths \
         against the repository rather than guessing them. Acceptance criteria must be checkable \
         by someone who cannot read your mind.\n",
        preamble("planner", workdir),
        deliverable(delivery, &artifacts.plan(), "the plan"),
    )
}

/// Ask the executor to implement the plan as written.
pub fn executor(workdir: &Path, artifacts: &Artifacts, delivery: Delivery) -> String {
    format!(
        "{}\
         A plan has already been written by an architect. Implement it.\n\n\
         {}\
         ## The plan\n\n\
         The full plan is at `{}`. Read it first. Its contents follow.\n\n\
         ---\n\n{}\n\n---\n\n\
         ## Rules\n\n\
         - Implement this plan exactly.\n\
         - Do not redesign the architecture unless implementing it as written is impossible.\n\
         - If the plan conflicts with the repository, stop and document the conflict in your \
           deliverable instead of improvising a different design.\n\
         - Respect the `Must NOT Change` scope.\n\
         - Write the tests the plan asks for.\n\
         - Build and test your work before you finish.\n\n\
         In your deliverable, record what you changed, which plan steps are done, anything you \
         could not do, and any place you deviated from the plan and why.\n",
        preamble("executor", workdir),
        deliverable(
            delivery,
            &artifacts.execution(),
            "a summary of the work you did"
        ),
        artifacts.plan().display(),
        artifacts.read_or_placeholder(&artifacts.plan()),
    )
}

/// Ask the executor to repair specific findings.
///
/// The fix prompt names the issues rather than restating the whole task, because a rewrite is a new
/// chance to break what already passed. Narrow instructions produce narrow diffs.
pub fn fixer(
    workdir: &Path,
    artifacts: &Artifacts,
    verdict: &Verdict,
    iteration: usize,
    max_iterations: usize,
) -> String {
    format!(
        "{}\
         Your previous implementation was reviewed and rejected. Fix the findings below.\n\n\
         This is fix attempt {iteration} of {max_iterations}.\n\n\
         {}\
         ## Review findings\n\n{}\n\n\
         ## Test results from the last run\n\n{}\n\n\
         ## The plan you are implementing\n\n---\n\n{}\n\n---\n\n\
         ## Rules\n\n\
         - Fix exactly these findings. Do not refactor unrelated code.\n\
         - Do not weaken, skip, or delete a test to make it pass. If a test is genuinely wrong, \
           say so explicitly in your deliverable and explain why.\n\
         - Re-run the tests before you finish.\n\n\
         In your deliverable, record what you changed for each finding, by id.\n",
        preamble("executor", workdir),
        deliverable(
            Delivery::AgentWrites,
            &artifacts.execution(),
            "a summary of the fixes you made",
        ),
        verdict.issues_markdown(),
        artifacts.read_or_placeholder(&artifacts.test_results()),
        artifacts.read_or_placeholder(&artifacts.plan()),
    )
}

/// Ask the reviewer to judge the work and emit a machine-readable verdict.
pub fn reviewer(workdir: &Path, artifacts: &Artifacts, diff: &str, delivery: Delivery) -> String {
    format!(
        "{}\
         Review the implementation below against the plan it was supposed to follow. Be skeptical: \
         the code was written by a cheaper model working quickly, and your judgement is the only \
         thing standing between it and the user's repository.\n\n\
         {}\
         ## The plan\n\n---\n\n{}\n\n---\n\n\
         ## What the executor reported\n\n---\n\n{}\n\n---\n\n\
         ## Automated test results\n\n---\n\n{}\n\n---\n\n\
         ## Changes made\n\n---\n\n{diff}\n\n---\n\n\
         ## What to check\n\n\
         - correctness, including edge cases the plan named\n\
         - are the acceptance criteria actually met?\n\
         - tests: do they exist, do they test behaviour, were any weakened or skipped?\n\
         - security, error handling, and concurrency\n\
         - regressions and changes outside the plan's scope\n\
         - anything in `Must NOT Change` that changed\n\n\
         Judge what the code does, not what the executor says it does.\n\n\
         ## Verdict\n\n\
         {}\
         It must be valid JSON in exactly this shape:\n\n\
         ```json\n\
         {{\n\
         \x20 \"verdict\": \"PASS\",\n\
         \x20 \"severity\": \"none\",\n\
         \x20 \"summary\": \"one sentence\",\n\
         \x20 \"issues\": [\n\
         \x20   {{ \"id\": \"REV-001\", \"description\": \"what is wrong\", \"severity\": \"major\", \
         \"file\": \"src/x.rs\" }}\n\
         \x20 ]\n\
         }}\n\
         ```\n\n\
         `verdict` must be exactly one of `PASS`, `FAIL`, or `BLOCKED`, uppercase.\n\n\
         - `PASS` — the work satisfies the plan and can be accepted.\n\
         - `FAIL` — there are problems the implementer can fix. List every one as an issue.\n\
         - `BLOCKED` — no amount of implementation work can resolve this: the plan is \
           self-contradictory, a requirement is impossible, or something outside the repository is \
           missing. Do not use `BLOCKED` for ordinary bugs.\n\n\
         Do not return `PASS` with unresolved issues listed. If the work is not acceptable, say \
         `FAIL` — approving broken work costs far more than another iteration.\n",
        preamble("reviewer", workdir),
        deliverable(delivery, &artifacts.review(), "your full review"),
        artifacts.read_or_placeholder(&artifacts.plan()),
        artifacts.read_or_placeholder(&artifacts.execution()),
        artifacts.read_or_placeholder(&artifacts.test_results()),
        verdict_instruction(delivery, &artifacts.verdict()),
    )
}

/// How the reviewer is asked for its machine-readable verdict.
///
/// A reviewer that cannot touch the filesystem must not be told to write a file — the prompt said
/// "you have no tools" and then named a path two paragraphs later, and a model handed two
/// instructions that cancel each other produces exactly the confusion that invites. `gates::read`
/// already recovers a verdict from the returned text, so asking for it inline loses nothing.
fn verdict_instruction(delivery: Delivery, path: &Path) -> String {
    match delivery {
        Delivery::AgentWrites => format!(
            "Also write a machine-readable verdict to this exact path:\n\n`{}`\n\n",
            path.display()
        ),
        Delivery::KageWrites => {
            "End your reply with a machine-readable verdict, as the last thing \
             you write, in a fenced ```json block.\n\n"
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Project;
    use crate::engine::gates;

    fn artifacts(label: &str) -> (std::path::PathBuf, Artifacts) {
        let root = std::env::temp_dir().join(format!("kage-prompt-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::config::init(&root, true).unwrap();
        let project = Project::discover(&root).unwrap();
        let artifacts = Artifacts::new(&project, "run_1");
        artifacts.ensure_dirs().unwrap();
        (root, artifacts)
    }

    #[test]
    fn the_planner_is_told_where_to_write_and_not_to_implement() {
        let (root, artifacts) = artifacts("planner");

        let prompt = planner(
            "add rate limiting",
            &root,
            &artifacts,
            Delivery::AgentWrites,
        );

        assert!(prompt.contains("add rate limiting"));
        assert!(prompt.contains("PLAN.md"));
        assert!(prompt.contains("Do not implement"));
        assert!(prompt.contains("Acceptance Criteria"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_executor_prompt_carries_the_plan_inline() {
        let (root, artifacts) = artifacts("executor");
        std::fs::write(artifacts.plan(), "# Objective\nShip the widget.").unwrap();

        let prompt = executor(&root, &artifacts, Delivery::AgentWrites);

        assert!(prompt.contains("Ship the widget."));
        assert!(prompt.contains("Implement this plan exactly."));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_plan_degrades_to_a_placeholder_rather_than_a_crash() {
        let (root, artifacts) = artifacts("missing");

        let prompt = executor(&root, &artifacts, Delivery::AgentWrites);

        assert!(prompt.contains("PLAN.md is missing or empty"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_fixer_names_the_findings_and_forbids_weakening_tests() {
        let (root, artifacts) = artifacts("fixer");
        let verdict = gates::extract(
            r#"{"verdict":"FAIL","issues":[{"id":"REV-007","description":"race on the counter"}]}"#,
        )
        .unwrap();

        let prompt = fixer(&root, &artifacts, &verdict, 2, 3);

        assert!(prompt.contains("REV-007"));
        assert!(prompt.contains("race on the counter"));
        assert!(prompt.contains("fix attempt 2 of 3"));
        assert!(prompt.contains("Do not weaken, skip, or delete a test"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_model_with_no_filesystem_is_asked_for_a_reply_not_a_file() {
        // Telling a completions endpoint to "write to this exact path" earns a confident
        // description of a file that was never created.
        let (root, artifacts) = artifacts("kage-writes");

        let prompt = planner("add caching", &root, &artifacts, Delivery::KageWrites);

        assert!(prompt.contains("Reply with the plan and nothing else"));
        assert!(prompt.contains("saved verbatim as `PLAN.md`"));
        assert!(prompt.contains("no tools and no filesystem access"));
        assert!(
            !prompt.contains("Write the plan to this exact path"),
            "the file instruction must be gone, not merely supplemented"
        );
        // Everything else about the prompt is unchanged.
        assert!(prompt.contains("Acceptance Criteria"));
        assert!(prompt.contains("add caching"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reviewer_with_no_filesystem_is_never_told_to_write_a_file() {
        // Regression guard: the prompt used to say "you have no tools and no filesystem access"
        // and then name a path for VERDICT.json two paragraphs later. Two instructions that cancel
        // each other are the most reliable way to get confused output.
        let (root, artifacts) = artifacts("verdict-inline");

        let prompt = reviewer(&root, &artifacts, "diff", Delivery::KageWrites);

        assert!(prompt.contains("no tools and no filesystem access"));
        assert!(prompt.contains("End your reply with a machine-readable verdict"));
        assert!(
            !prompt.contains("VERDICT.json"),
            "a model with no filesystem must not be handed a path:\n{prompt}"
        );
        // The contract itself is unchanged — only who puts it on disk.
        assert!(prompt.contains("\"verdict\": \"PASS\""));
        assert!(prompt.contains("BLOCKED"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_reviewer_gets_the_diff_and_the_verdict_contract() {
        let (root, artifacts) = artifacts("reviewer");
        std::fs::write(artifacts.plan(), "# Objective\nx").unwrap();

        let prompt = reviewer(
            &root,
            &artifacts,
            "diff --git a/src/a.rs b/src/a.rs",
            Delivery::AgentWrites,
        );

        assert!(prompt.contains("diff --git"));
        assert!(prompt.contains("VERDICT.json"));
        assert!(prompt.contains("BLOCKED"));
        // The JSON example must survive `format!` escaping as real single braces.
        assert!(prompt.contains("\"verdict\": \"PASS\""));
        assert!(!prompt.contains("{{"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
