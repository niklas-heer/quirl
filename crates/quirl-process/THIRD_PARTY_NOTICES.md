# Third-party notices for `quirl-process`

The local-completion process boundary adapts the shell-bridge approach from
[`carapace-sh/carapace-bridge`](https://github.com/carapace-sh/carapace-bridge)
at immutable commit
[`33ff17e5a6aff0d201a3e105b9e753053c2460cd`](https://github.com/carapace-sh/carapace-bridge/tree/33ff17e5a6aff0d201a3e105b9e753053c2460cd).
The reviewed source files are
[`pkg/actions/bridge/fish.go`](https://github.com/carapace-sh/carapace-bridge/blob/33ff17e5a6aff0d201a3e105b9e753053c2460cd/pkg/actions/bridge/fish.go),
[`pkg/actions/bridge/zsh.go`](https://github.com/carapace-sh/carapace-bridge/blob/33ff17e5a6aff0d201a3e105b9e753053c2460cd/pkg/actions/bridge/zsh.go), and
[`third_party/github.com/Valodim/zsh-capture-completion/capture.zsh`](https://github.com/carapace-sh/carapace-bridge/blob/33ff17e5a6aff0d201a3e105b9e753053c2460cd/third_party/github.com/Valodim/zsh-capture-completion/capture.zsh).
Quirl does not depend on or copy the Carapace runtime. It adapts the Fish
`complete --do-complete` invocation and the Zsh `zsh/zpty` plus `compadd`
capture technique, replacing textual result delimiters with a bounded
length-framed protocol and removing implicit user startup-file sourcing.

## Carapace Bridge license

MIT License

Copyright (c) 2021 rsteube

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## zsh-capture-completion license

The MIT License (MIT)

Copyright (c) 2015 Vincent Breitmoser

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
