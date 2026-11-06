//! `/swarm [n] <goal>`: Claude splits the goal into independent parts and runs them as parallel subagents. claude++ starts no
//! agents and makes no model calls of its own; it sends one message that tells Claude to use its own `Agent` tool this
//! way. The subagents live inside the tab's single `claude` process (about 4 MB each, against ~234 MB for a process
//! each; see "Many agents, one process" in the README), each that changes files gets its own git worktree
//! (`isolation: "worktree"`, which Claude Code cleans up when nothing changed), and `/tasks` shows them working.

pub const DEFAULT: usize = 3;
pub const MAX: usize = 8;

/// (number of agents, goal). A leading number from 2 to 8 is the count; any other start is the goal.
pub fn parse(arg: &str) -> Result<(usize, String), &'static str> {
    let arg = arg.trim();
    let (n, goal) = match arg.split_once(char::is_whitespace) {
        Some((first, rest)) if first.parse::<usize>().is_ok_and(|n| (2..=MAX).contains(&n)) => (first.parse().unwrap(), rest.trim()),
        _ if arg.parse::<usize>().is_ok_and(|n| (2..=MAX).contains(&n)) => return Err("usage: /swarm [2-8] <goal>"),
        _ => (DEFAULT, arg),
    };
    if goal.is_empty() { Err("usage: /swarm [2-8] <goal>") } else { Ok((n, goal.to_string())) }
}

pub fn prompt(n: usize, goal: &str) -> String {
    format!(
        "Do this as a team of parallel subagents.\n\nGoal: {goal}\n\n\
         1. Split the goal into at most {n} parts that can proceed independently: no part waits for another's result, and no two parts edit the same file. If it does not split that way, say so and do it yourself instead.\n\
         2. Launch one subagent per part with the Agent tool, all in the same message so they run at the same time. Give each a self-contained brief: the overall goal, its part, the files it owns, and what to report back. Use isolation: \"worktree\" for every subagent that changes files.\n\
         3. Do not do the parts yourself while they run.\n\
         4. When all have reported, check that the results fit together, resolve conflicts, and answer with one summary: what each part did, which branch or worktree holds each change, and what is left undone."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leading_count_from_2_to_8_sets_the_agents_and_anything_else_is_the_goal() {
        assert_eq!(parse("5 add tests to every module"), Ok((5, "add tests to every module".into())));
        assert_eq!(parse("  fix the flaky tests  "), Ok((3, "fix the flaky tests".into())));
        assert_eq!(parse("1 file at a time"), Ok((3, "1 file at a time".into())), "1 is not a team");
        assert_eq!(parse("12 months of logs"), Ok((3, "12 months of logs".into())));
        assert!(parse("").is_err() && parse("4").is_err() && parse("4   ").is_err(), "a count alone is no goal");
        let p = prompt(4, "port the parser");
        assert!(p.contains("at most 4 parts") && p.contains("Goal: port the parser") && p.contains("isolation: \"worktree\"") && !p.starts_with('/'));
    }
}
