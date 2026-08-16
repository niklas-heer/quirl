# Quirl adoption plan

Status: working plan for after the Unix release candidate is stable

## Objective

Build a durable community around Quirl by proving one clear promise:

> Keep familiar command-line habits, add typed data when it helps, and extend
> the shell through one safe, well-tooled Lua SDK.

The initial objective is not to replace Bash, Zsh, Fish, or Nushell. It is to
make Quirl valuable as an optional secondary shell, earn repeat usage, and turn
early users into contributors and advocates.

## Success levels

Adoption is measured by recurring use and contribution, not GitHub stars alone.

| Stage | Evidence |
| --- | --- |
| Initial interest | 100 repository stars, 25 successful installs, 10 useful external reports |
| Early adoption | 100 recurring users, 10 external contributors, 3 third-party plugins or integrations |
| Recognized niche | 1,000 repository stars, active community support, reliable packaging, recurring releases |
| Durable ecosystem | Multiple maintainers, externally maintained plugins, documented production use, predictable compatibility |

Use privacy-respecting signals. Do not add product telemetry by default. Prefer
package-download counts, release downloads, issue activity, optional surveys,
and explicitly volunteered usage reports.

## Product principles

1. **Familiar first.** A new user should run ordinary commands immediately.
2. **Typed power is explicit.** Demonstrate data mode as an additional tool,
   not a replacement for everything users know.
3. **Secondary shell before login shell.** The low-risk adoption path is
   `quirl` inside an existing terminal session.
4. **Reliability beats breadth.** Improve daily workflows before adding another
   major subsystem.
5. **Boundaries stay honest.** Unsupported dialect syntax gets an actionable
   Bash/Zsh-island diagnostic, never silent emulation.
6. **Great defaults, graceful fallbacks.** The modern prompt, completion, and
   picker should feel polished without requiring color or a patched font.

## Phase 0: make the candidate trustworthy

Goal: turn the current repository into an artifact that can be recommended
without caveats hidden in conversation.

### Required work

- Complete the canonical quality gate on the final integrated source.
- Refresh performance evidence against the exact clean candidate artifact.
- Complete real-terminal checks on named Linux and macOS environments.
- Dogfood Quirl for normal repository, Git, scripting, and process workflows.
- Resolve every high-severity crash, hang, terminal-corruption, child-process,
  history-loss, or configuration-loss defect before launch.
- Publish clear known limitations and the Unix-first support contract.
- Prepare reproducible checksums and release notes for the measured artifact.

### Exit criteria

- The release checklist passes.
- The documented artifact and published artifact have the same identity.
- A maintainer can use Quirl for a working week without returning to another
  shell because of a Quirl defect in a supported workflow.

## Phase 1: make the value obvious

Goal: let a developer understand Quirl in under one minute and try it in under
five minutes.

### Launch assets

1. **A 30–60 second real-terminal recording** showing:
   - a familiar command and byte pipeline;
   - semantic completion;
   - an explicit switch into data mode;
   - a useful typed transformation;
   - history or file picking;
   - an honest Bash/Zsh island or unsupported-syntax diagnostic.
2. **A static fallback** with accurate alt text and a text-only `cargo xtask demo`.
3. **A five-minute quick start** that does not require changing the login shell.
4. **A comparison page** for Bash/Zsh, Fish, and Nushell users, focused on
   tradeoffs rather than claims that Quirl is universally better.
5. **Architecture and security summaries** that link to the detailed evidence
   without forcing first-time users to read design documents.

### Message hierarchy

- Headline: familiar commands plus typed data.
- Supporting proof: fast single Rust binary and polished interactive UX.
- Trust proof: bounded Lua runtime, explicit capabilities, and helpful errors.
- Extension story: one generated Lua SDK for configuration and plugins.

Avoid leading with implementation details, protocol versions, or the number of
crates. Those establish credibility after the user understands the benefit.

### Exit criteria

- Five unaffiliated developers can explain Quirl's distinction after watching
  the demo once.
- Five unaffiliated developers can install it and complete the quick start
  without maintainer assistance.

## Phase 2: remove installation friction

Goal: make trying and removing Quirl safe and unsurprising.

### Distribution priorities

1. Homebrew formula for macOS and Linuxbrew.
2. Checksummed Linux binaries for common architectures.
3. `cargo install` when it can reproduce a supported build.
4. Shell-completion and editor-integration setup instructions.
5. Uninstall and rollback instructions as visible as installation instructions.

Every installation route must report the same Quirl version and supported
platform scope. Do not publish many partially maintained packages at once.

### Adoption path

Recommend this progression:

1. Run `quirl` as a subprocess from the existing shell.
2. Use it for selected interactive and typed-data workflows.
3. Add personal configuration after the defaults are understood.
4. Consider making it the login shell only after sustained successful use.

### Exit criteria

- Installation succeeds from a clean supported system using one documented
  command.
- Upgrade, uninstall, and rollback paths are tested.
- Install-related questions no longer dominate early feedback.

## Phase 3: publish killer workflows

Goal: demonstrate benefits that justify learning and retaining Quirl.

Create polished, copyable walkthroughs for at least these workflows:

1. Filter and inspect structured command or file output in data mode.
2. Move between familiar byte pipelines and typed values without ambiguity.
3. Search history, files, commands, jobs, and data through the shared picker.
4. Write, check, format, test, and document a Lua automation script.
5. Create a permission-scoped plugin with completion and prompt integration.
6. Diagnose an unsupported shell form and move it into an explicit dialect
   island.

Each walkthrough should include the problem, the complete commands, expected
output, failure behavior, and why the Quirl approach is useful. Prefer realistic
repository, JSON, Git, process, and operations data over toy arithmetic.

### Flagship integration

Build one genuinely useful, deeply polished integration instead of many shallow
examples. Strong candidates are:

- Git and repository navigation;
- containers and local development environments;
- Kubernetes operations;
- project/task discovery.

Choose one using interviews with early users. It should exercise catalog facts,
completion, typed output, picker views, documentation, and explicit permissions
as one coherent experience.

## Phase 4: build the feedback loop

Goal: turn early usage into product quality and community ownership.

### Maintainer practices

- Triage early bug reports quickly, especially terminal and process-lifecycle
  problems.
- Convert every reproducible defect into a regression test.
- Label beginner-friendly issues with enough context to complete them.
- Publish a short release cadence and compatibility policy.
- Explain rejected scope as carefully as accepted features.
- Credit external reports and contributions prominently.

### Feedback questions

Ask early users:

- What made you try Quirl?
- Which workflow made you return?
- Where did familiar shell knowledge stop transferring?
- What prevented you from using it for a full session?
- Which error or interaction felt least trustworthy?
- Would you recommend it, and what caveat would you mention first?

### Metrics to review monthly

- Release and package downloads.
- Repeat participants across issues, discussions, and pull requests.
- Time to first maintainer response and time to confirmed fix.
- Number of external contributors and integrations.
- Common installation, compatibility, and reliability failures.
- Voluntary reports of daily or weekly use.

Stars and social reach are useful discovery indicators, but they are not
retention metrics.

## Launch sequence

### Two to four weeks before launch

- Freeze large features.
- Run daily-driver testing and supported-terminal checks.
- Finish packaging and the quick start.
- Record the demo only after the release candidate is final.
- Ask a small group of external developers to test installation and messaging.

### Launch week

- Publish the release, checksums, demo, quick start, and known limitations
  together.
- Share one clear technical story rather than a broad feature inventory.
- Be available for installation problems and high-impact bugs.
- Record recurring questions for immediate documentation improvements.

### First month after launch

- Prioritize reliability and onboarding defects over roadmap features.
- Publish small, frequent fixes with clear release notes.
- Release the first killer-workflow tutorial.
- Select the flagship integration from observed user demand.
- Review retention evidence and revise this plan.

## Risks and mitigations

| Risk | Mitigation |
| --- | --- |
| Users see Quirl as another incompatible shell | Lead with familiar command mode and the secondary-shell adoption path |
| Typed data looks like a weaker Nushell | Show the benefit of explicit coexistence with byte-oriented commands |
| Beautiful prompt is mistaken for the main value | Use UX as proof of polish, then demonstrate a workflow competitors handle differently |
| Early terminal or process bugs damage trust | Freeze features, dogfood heavily, and prioritize lifecycle regressions |
| Installation is harder than trying a competitor | Maintain one excellent install route per supported platform first |
| Plugin platform appears theoretical | Ship one useful flagship integration built entirely through public contracts |
| Maintainer bandwidth becomes the bottleneck | Keep scope bounded, document contribution paths, and grow co-maintainers deliberately |
| Attention produces stars but not recurring users | Track repeat participation and volunteered usage, then improve the workflows that drive return visits |

## Explicit non-goals for initial adoption

- Competing for default-shell status immediately.
- Native emulation of every Bash or Zsh construct.
- Treating Windows as a supported interactive platform without maintainable
  native evidence.
- Launching a remote plugin registry before local plugin workflows are proven.
- Adding product telemetry merely to produce adoption numbers.
- Expanding the architecture before existing features are reliable and easy to
  discover.

## Immediate next actions

When implementation work resumes, start here:

1. Finish and sign off the Unix release checklist.
2. Produce the exact candidate artifact and refreshed performance record.
3. Test the five-minute quick start with external developers.
4. Record the real-terminal demo from the measured release binary.
5. Prepare Homebrew and checksummed Linux installation paths.
6. Publish the first three killer-workflow guides.
7. Interview early users and select one flagship integration.

Revisit this plan after the first 30 days of public availability. Replace
assumptions with observed installation, retention, workflow, and contribution
evidence.
