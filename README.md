# Forkpicker

Find useful changes hiding in GitHub forks.

Forkpicker scans a repository’s fork network, removes work already present upstream, and groups the remaining commits into feature candidates. It checks whether patches apply to upstream HEAD and produces a searchable HTML dashboard with source links, change sizes, dates, and integration results.

With an installed coding-agent CLI, it also identifies coherent patch sets, reviews their code and fit with upstream style, and connects them to open issues. It uses the CLI’s existing login and billing configuration.

## Install

With a current stable Rust toolchain and Git:

```sh
git clone https://github.com/erikwb/forkpicker.git
cd forkpicker
cargo install --path . --locked
forkpicker --help
```

Git is required at runtime. Linux and macOS are the CI targets. Make sure Cargo’s install directory (usually `~/.cargo/bin`) is on your `PATH`.

On Arch Linux, build a pacman package from the checkout:

```sh
./build.sh       # Build the package into dist/.
./build.sh -si   # Install missing dependencies, build, and install the package.
```

Run as your normal user; makepkg requests elevated privileges when needed. The script forwards makepkg options and keeps build files under `.makepkg/`. The package installs the executable at `/usr/bin/forkpicker`. If you previously installed through Cargo or copied the binary into `~/.local/bin`, check `command -v forkpicker` for an older copy taking precedence.

For GitHub authentication, set `GH_TOKEN` or `GITHUB_TOKEN`, or sign in with `gh auth login`. Forkpicker checks them in that order. Public repositories work without authentication, but GitHub’s smaller request allowance limits scans. Git fetch uses your existing Git credentials.

## Run

```sh
forkpicker run bluez/bluez --open
```

This collects the available fork network, checks patch application, reviews selected feature sets, and opens the dashboard in your default browser. Reports are saved under `reports/bluez-bluez/TIMESTAMP/`; the command prints the exact HTML path. You can reopen that file without a running server. Feature, fork, and commit pages support browser Back and Forward; avatars load from GitHub.

Install and sign in to **Codex, Claude Code, or Grok** before using model review. Forkpicker uses your configured agent, or the first installed CLI in that order. Choose one explicitly or run entirely without a model:

```sh
forkpicker run bluez/bluez --agent claude --model sonnet --open
forkpicker run bluez/bluez --no-llm --open
```

Without a model, you still get the fork inventory, grouped candidates, commit links, change statistics, and local patch-application results.

## Scope and budgets

The full workflow gathers all discovered forks and branches, subject to GitHub access and API limits. Model selection then excludes verified open-PR coverage and requires patches to apply directly or through a clean three-way merge. Its initial window starts one calendar month before the latest stable GitHub release. Repositories without a published release use all history; `--all-history` also selects that scope.

The default model budget is **30 CLI calls and 4 MB of serialized input**, including issue matching and failed calls. Code inspections run two at a time and receive complete diffs; sets too large for a request remain deferred. Call and input limits bound work, but do not translate directly into subscription usage or a dollar price.

```sh
forkpicker run bluez/bluez --limit 10 --total-bytes 1500000 --open
forkpicker run bluez/bluez --all-history --review-jobs 4 --open
```

Fresh runs reuse raw Git/API evidence and application checks, not prior model judgments. Saved reports retain coverage, the selection funnel, model inputs/results, and stage timings. See [run settings and continuation](docs/usage.md) for the individual budgets and partial-run behavior.

## Individual commands

Use individual stages to inspect a known fork, browse saved evidence, or choose exactly what to review:

```sh
forkpicker scan bluez/bluez --fork Yiin/bluez \
  --output reports/bluez.json --html reports/bluez.html

forkpicker inventory reports/bluez.json --check-apply \
  --output reports/inventory.json --html reports/inventory.html

forkpicker agents
forkpicker review reports/bluez.json --all --agent codex --dry-run
```

[The usage guide](docs/usage.md) covers local repositories, saved decisions, source export, and advanced review commands. See [agent configuration](docs/agents.md) for custom wrappers.

## What the results mean

Application checks describe integration effort. Code size, recency, and file counts are separate facts, not a combined quality score. Model summaries and issue connections cite the supplied evidence and remain assessments for a maintainer to inspect.

Forkpicker reads Git objects without running fork code, builds, or tests. Grouping and patch-equivalence checks can miss dependencies, squashes, or partial ports. A clean application does not establish correctness, and a model budget can leave eligible work unreviewed. The tool does not modify upstream repositories or submit PRs.

Caches live under `$XDG_CACHE_HOME/forkpicker` (default `~/.cache/forkpicker`), and CLI decisions under `$XDG_DATA_HOME/forkpicker` (default `~/.local/share/forkpicker`). Override these with `--cache-dir` and `--state-dir`. Reports contain source code, so treat reports from private repositories accordingly.

## Documentation

- [Usage and saved workflows](docs/usage.md)
- [Agent configuration](docs/agents.md)
- [Internals and evidence](docs/internals.md)
- [Releasing](docs/releasing.md)

See also the [changelog](CHANGELOG.md).

MIT licensed. See [LICENSE](LICENSE).
