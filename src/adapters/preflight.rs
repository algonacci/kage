//! Resolving every role's program before a run spends anything.
//!
//! A missing harness is the most common way a run dies, and the expensive way to find out is at the
//! phase that needs it: by the time REVIEW discovers `codex` is not installed, a planner and an
//! executor have already been paid for. The same lookup `kage doctor` performs is therefore run
//! once at the top of every run, and reports the same remedy.
//!
//! Nothing is executed here. Reachability means the file exists and can be spawned — whether the
//! harness is logged in is its own business, and finding that out would mean running it.

use anyhow::{Result, bail};

use crate::adapters::{Role, proc};
use crate::config::{AdapterKind, RoleConfig, Roles};

/// The program a role would spawn when its `adapter: command` has no `command` to spawn.
const UNCONFIGURED: &str = "<unconfigured>";

/// One role, the program it would spawn, and whether that program was found.
pub struct RoleProgram<'a> {
    pub role: Role,
    pub config: &'a RoleConfig,
    pub program: String,
    /// `None` when the program could not be resolved.
    pub resolved: Option<proc::Resolved>,
}

impl RoleProgram<'_> {
    pub fn found(&self) -> bool {
        self.resolved.is_some()
    }

    /// How an unreachable role is named in the remedy line: `planner (claude)`.
    pub fn label(&self) -> String {
        format!("{} ({})", self.role, self.program)
    }
}

/// The executable a role would actually spawn, including a custom `command` override.
pub fn program_for(config: &RoleConfig) -> String {
    if let Some(command) = &config.command
        && let Some(program) = command.first()
    {
        return program.clone();
    }

    match config.adapter {
        AdapterKind::ClaudeCode => "claude".to_string(),
        AdapterKind::Codex => "codex".to_string(),
        AdapterKind::OpenCode => "opencode".to_string(),
        AdapterKind::Kamui => "kamui".to_string(),
        // Not a program on PATH at all: the provider name is what identifies this role.
        AdapterKind::Api => config
            .provider
            .clone()
            .unwrap_or_else(|| UNCONFIGURED.to_string()),
        AdapterKind::Command => UNCONFIGURED.to_string(),
    }
}

/// Resolve all three roles, in the order the loop uses them.
pub fn inspect(roles: &Roles) -> Vec<RoleProgram<'_>> {
    [
        (Role::Planner, &roles.planner),
        (Role::Executor, &roles.executor),
        (Role::Reviewer, &roles.reviewer),
    ]
    .into_iter()
    .map(|(role, config)| {
        let program = program_for(config);
        let resolved = proc::resolve_program(&program).ok();
        RoleProgram {
            role,
            config,
            program,
            resolved,
        }
    })
    .collect()
}

/// The remedy line `kage doctor` ends with, and the reason a run refuses to start.
///
/// Both callers go through this so the two can never drift into telling the user different things.
pub fn not_ready(missing: &[String]) -> String {
    format!("Not ready: install or reconfigure {}.", missing.join(", "))
}

/// Fail unless every role can actually be spawned.
///
/// Called before a run touches the disk, so a run that cannot finish leaves no run directory, no
/// worktree, and no state file behind.
pub fn check(config: &crate::config::Config) -> Result<()> {
    let mut missing = Vec::new();

    for status in inspect(&config.roles) {
        // A role that is misconfigured rather than missing has a better message of its own, and it
        // is worth more than reporting `<unconfigured>` as absent from PATH.
        crate::adapters::build(status.role, config)?;

        // An API-backed role has no program to find: its reachability is the endpoint's, and a
        // missing key already failed in `build` above with a message naming the variable.
        if status.config.adapter == crate::config::AdapterKind::Api {
            continue;
        }

        if !status.found() {
            missing.push(status.label());
        }
    }

    if !missing.is_empty() {
        bail!(
            "{}\nNo agent was spawned — run `kage doctor` for the full report.",
            not_ready(&missing)
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role_step_with_program(program: &str) -> RoleConfig {
        let mut role = RoleConfig::preset(AdapterKind::Command);
        role.command = Some(vec![program.to_string()]);
        role
    }

    /// A `Config` carrying only these roles, which is all `check` reads for a CLI-backed setup.
    fn config_with(roles: Roles) -> crate::config::Config {
        let mut config: crate::config::Config = serde_yaml_ng::from_str("{}").unwrap();
        config.roles = roles;
        config
    }

    fn current_exe() -> String {
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn a_reachable_program_passes_the_check() {
        let roles = Roles {
            planner: role_step_with_program(&current_exe()),
            executor: role_step_with_program(&current_exe()),
            reviewer: role_step_with_program(&current_exe()),
        };

        assert!(check(&config_with(roles)).is_ok());
    }

    #[test]
    fn a_missing_program_is_reported_with_the_doctor_sentence() {
        let roles = Roles {
            planner: role_step_with_program(&current_exe()),
            executor: role_step_with_program(&current_exe()),
            reviewer: role_step_with_program("kage-definitely-not-installed"),
        };

        let error = check(&config_with(roles)).unwrap_err().to_string();

        assert!(error.contains(
            "Not ready: install or reconfigure reviewer (kage-definitely-not-installed)."
        ));
    }

    #[test]
    fn every_unreachable_role_is_named_at_once() {
        let roles = Roles {
            planner: role_step_with_program("kage-definitely-not-installed"),
            executor: role_step_with_program(&current_exe()),
            reviewer: role_step_with_program("kage-definitely-not-installed"),
        };

        let error = check(&config_with(roles)).unwrap_err().to_string();

        assert!(error.contains("planner (kage-definitely-not-installed)"));
        assert!(error.contains("reviewer (kage-definitely-not-installed)"));
        assert!(
            error.find("planner (").unwrap() < error.find("reviewer (").unwrap(),
            "roles must be named in loop order"
        );
        assert!(
            !error.contains("executor"),
            "a reachable role must not be named: {error}"
        );
    }

    #[test]
    fn a_command_role_without_a_command_fails_before_the_path_lookup() {
        let roles = Roles {
            planner: role_step_with_program(&current_exe()),
            executor: role_step_with_program(&current_exe()),
            reviewer: RoleConfig::preset(AdapterKind::Command),
        };

        let error = check(&config_with(roles)).unwrap_err().to_string();

        assert!(error.contains("no `command`"));
        assert!(
            !error.contains(UNCONFIGURED),
            "<unconfigured> must never reach the remedy line: {error}"
        );
    }

    #[test]
    fn each_preset_resolves_to_the_program_its_adapter_actually_spawns() {
        // Drift guard: `program_for` and `adapters::cli::preset_template` both decide what a preset
        // role spawns, independently. `describe` names argv[0] in `(`program`)` form; that is what
        // pins the mapping to what the adapter would really run.
        for kind in [
            AdapterKind::ClaudeCode,
            AdapterKind::Codex,
            AdapterKind::OpenCode,
            AdapterKind::Kamui,
        ] {
            let role_config = RoleConfig::preset(kind);
            let program = program_for(&role_config);
            let mut config: crate::config::Config = serde_yaml_ng::from_str("{}").unwrap();
            config.roles.planner = role_config;
            let described = crate::adapters::build(Role::Planner, &config)
                .unwrap()
                .describe();

            assert!(
                described.contains(&format!("(`{program}`)")),
                "{described} did not name the program `{program}`"
            );
        }
    }

    #[test]
    fn an_explicit_command_overrides_the_adapter_preset() {
        let mut config = RoleConfig::preset(AdapterKind::ClaudeCode);
        config.command = Some(vec!["my-agent".to_string()]);

        assert_eq!(program_for(&config), "my-agent");
    }

    #[test]
    fn the_remedy_sentence_is_the_one_doctor_prints() {
        assert_eq!(
            not_ready(&[
                "planner (claude)".to_string(),
                "reviewer (codex)".to_string()
            ]),
            "Not ready: install or reconfigure planner (claude), reviewer (codex)."
        );
    }
}
