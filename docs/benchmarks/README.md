# Performance evidence

Release measurements apply to one exact executable, source revision, harness, and runner. A later documentation commit records that evidence without rebuilding or relabeling the released artifact.

- [Quirl 0.5.1 native release evidence](release-v0.5.1.md): all four published native executables passing on their first attempt, with the recording and screenshots made from the macOS ARM64 executable.
- [Quirl 0.5.0 native release evidence](release-v0.5.0.md): all four published native executables, unchanged reports, and a preserved macOS x86_64 first attempt that a local A/B traced to runner variance.
- [Quirl 0.4.0 native release evidence](release-v0.4.0.md): all four published native executables, unchanged reports, preserved failed candidates, and the Spotlight indexing finding on the macOS ARM64 runner.
- [Quirl 0.3.0 native release evidence](release-v0.3.0.md): all four published native executables, unchanged reports, preserved failed candidates, and exact release identities.
- [Quirl 0.2.0 native release evidence](release-v0.2.0.md): historical measurements of the published 0.2.0 executables.
- [Earlier release performance research](release-v1.0.md): historical candidate measurements; see the recorded artifact scope.
- [Preview benchmarks](preview-v0.1.md) and [project discovery](project-discovery.md): scoped engineering experiments.
- [Embedded-language selection](embedded-language-selection.md) and [Steel/Lua/Fennel comparison](steel-lua-fennel.md): runtime research, separate from product release gates.

The project records executable size and advisory warnings without a default size ceiling. Latency, artifact identity, sample completeness, bounded retention, and process cleanup remain release requirements. Hosted PTY results and automated recordings do not substitute for human or physical-terminal review. The [release checklist](../release-checklist.md) explains the candidate and evidence-commit distinction.
