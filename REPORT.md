# testopt — report

Progress and evidence for GOAL.md. Host: `macnoMac-Studio.local`, 18
cores, a shared host. The 1-minute load was 25–45 throughout, so
load/cores was 1.4–2.5, and every timing below is flagged as such where it
matters.

<!-- sections filled in as the work lands -->

## Incident: the agent killed its own tmux session

While starting the afkconvoy record in tmux, the agent ran
`tmux kill-session -t testopt` to clear an old job session. The agent was
itself running inside a tmux session named `testopt`, so that ended the
agent's own session mid-task. The pushed commits were safe, and the work
resumed in session `testopt-agent`.

**The invariant that broke:** a tool must never kill, replace, or fill
the tmux session its caller runs inside.

**The mechanism now:**
- `--tmux` and the new `testopt tmux-kill` read the caller's session from
  `$TMUX` (`tmux display-message -p '#S'`).
- They refuse with an error when it is the target session.
- `--tmux-session` picks the job session.
- `testopt tmux-kill` replaces raw `kill-session` in the workflow.

The test
`refuses_to_use_or_kill_the_tmux_session_it_runs_inside` (tests/cli.rs)
runs testopt inside a session on a private tmux server. It checks that
both the use and the kill are refused, that the session survives, and that
another session can still be killed. With the guard disabled (a mutation),
the test fails; restored, it passes.
