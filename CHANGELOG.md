# Changelog

## [0.9.0](https://github.com/suiflex/arsy-code/compare/v0.8.1...v0.9.0) (2026-10-03)


### Features

* **cli:** keep every transcript block across a resize ([3d0926a](https://github.com/suiflex/arsy-code/commit/3d0926a22a6766fcf2ddc21e7f1baa3ab7ff1d51))
* **effort:** offer each model only the reasoning efforts it takes ([b76c12b](https://github.com/suiflex/arsy-code/commit/b76c12b23aa5455c4482b558996975aba74bf135))


### Bug Fixes

* **cli:** recover a turn when a request, stream or tool fails ([b3cd2f6](https://github.com/suiflex/arsy-code/commit/b3cd2f6f2a8f2a54c123ecf7d3182d6483791935))
* **cli:** redraw the running tool card in place ([d67c5f1](https://github.com/suiflex/arsy-code/commit/d67c5f1fcce3d56f1ef2cb18be212ff1647b7acd))
* **cli:** run a repeated command again ([18e96d0](https://github.com/suiflex/arsy-code/commit/18e96d0093a63a0baf0c0b73ee53e487d0690d7a))
* **cli:** say why the provider is unavailable ([a09cb53](https://github.com/suiflex/arsy-code/commit/a09cb53232537fe3dfb4ed146137fd0ea076b4eb))
* **kernel:** tolerate loose tool-call deltas from compatible hosts ([4fdef55](https://github.com/suiflex/arsy-code/commit/4fdef55d5cb537eb858e61d0808ae2aae0811796))
* stabilize chat turns, per-model effort and resize-safe transcript ([5267f37](https://github.com/suiflex/arsy-code/commit/5267f3766e090bc26e54daf63114e590285a0e3d))

## [0.8.1](https://github.com/suiflex/arsy-code/compare/v0.8.0...v0.8.1) (2026-10-02)


### Bug Fixes

* accept pattern-safe tool names and proxy context limits ([f73b586](https://github.com/suiflex/arsy-code/commit/f73b58600c06152acdb1c91866e1e81da0055c40))
* **cli:** read context_length and max_model_len from model listings ([e42e90e](https://github.com/suiflex/arsy-code/commit/e42e90e0e5108bba0f74c6efb3543db51ecd4509))
* **config:** add sanitize_tool_names endpoint option ([d5c90fd](https://github.com/suiflex/arsy-code/commit/d5c90fd4bfa4f3411fe5fb1b6a19127f94b79727))
* **kernel:** let the openai adapter send pattern-safe tool names ([0e388e7](https://github.com/suiflex/arsy-code/commit/0e388e7b1e0328bbf56639f440aa10ebf62fc080))
* **npm:** install the launcher on Windows and unblock release smoke ([2555383](https://github.com/suiflex/arsy-code/commit/25553833bb4a67e14f473164e28a9c7e7437ef0f))
* **npm:** keep the .zip extension on the downloaded archive ([ed57a4c](https://github.com/suiflex/arsy-code/commit/ed57a4c7741c51f9f5bbc91694e2a9f81b4c9257))


### Performance Improvements

* **tui:** read terminal size without Unix subprocesses ([5ece8bc](https://github.com/suiflex/arsy-code/commit/5ece8bc9af2331e21ed77878fffdcfa2d8d0d4e2))
* **tui:** reduce idle terminal I/O over SSH ([66985ae](https://github.com/suiflex/arsy-code/commit/66985ae6ad954f00ce7a2865884868928df16480))
* **tui:** skip unchanged idle composer frames ([063db23](https://github.com/suiflex/arsy-code/commit/063db23644e54f4e4272057919ae2608fbe5627d))

## [0.8.0](https://github.com/suiflex/arsy-code/compare/v0.7.1...v0.8.0) (2026-10-01)


### Features

* **cli:** steer a running turn with Enter, queue with Tab ([9c99efa](https://github.com/suiflex/arsy-code/commit/9c99efaab7730e982840b093237c6149c315c80f))


### Bug Fixes

* **cli:** add --add-dir and ask before paths outside it ([58d5cb4](https://github.com/suiflex/arsy-code/commit/58d5cb4ac87f6ee38164c2cb7770fba4f2e2ad3f))
* **cli:** avoid unmatched calls in bounded transcripts ([1c56892](https://github.com/suiflex/arsy-code/commit/1c5689284ae57a5e6bf44cfdf716ed5feab60aad))
* **cli:** bound turn transcripts and recover malformed tool streams ([0fe6f16](https://github.com/suiflex/arsy-code/commit/0fe6f1603e355a45ed7d01933193fd7ad0e21606))
* **cli:** classify incomplete tool arguments for retry ([fcd1444](https://github.com/suiflex/arsy-code/commit/fcd14447bbacb6de97c85862d94957ef545acd2b))
* **cli:** discover selected model limits at runtime ([d8b251b](https://github.com/suiflex/arsy-code/commit/d8b251b6d2d2ed0c0f04f71c18d29726d72dd78c))
* **cli:** drop held lines when a turn is stopped ([89f9eb9](https://github.com/suiflex/arsy-code/commit/89f9eb9287a5d62dcca0e0c7c7072115c29afe17))
* **cli:** keep a turn going when the model re-reads ([3207546](https://github.com/suiflex/arsy-code/commit/32075463b8d274ba99cb6b8c99302ff4cba35041))
* **cli:** keep and show follow-ups queued during a turn ([e54c6d9](https://github.com/suiflex/arsy-code/commit/e54c6d91b96378203694c131e401083ba11c47d6))
* **cli:** keep turn guard within complexity limit ([e7a67f0](https://github.com/suiflex/arsy-code/commit/e7a67f04f3c4c49901aa6c7774e708ffa826cc0a))
* **cli:** reject oversized first transcript message ([7f5d813](https://github.com/suiflex/arsy-code/commit/7f5d8133b1be9c79113b3113de2c09cc9c96adc7))
* **cli:** run queued follow-ups before offering the plan ([6b8f553](https://github.com/suiflex/arsy-code/commit/6b8f55330955c8e843466847789fb88990fec9c8))
* **cli:** size compaction to selected model context window ([91bd5c1](https://github.com/suiflex/arsy-code/commit/91bd5c1f5172d0cc980513d3c432396a10a24b76))
* **cli:** stabilize compaction and provider recovery ([af5f893](https://github.com/suiflex/arsy-code/commit/af5f89365d48840829c35179242d97ac53845e35))
* **code,cli:** add --add-dir directories under the approval mode ([5e3cca1](https://github.com/suiflex/arsy-code/commit/5e3cca144e629a2a5a62a858597d376d9c898fa3))
* **code:** authorize both ends of a move ([ab9a09c](https://github.com/suiflex/arsy-code/commit/ab9a09cb6d2404412435c00a6e7474ed616addf5))
* **code:** record the directory a search read ([03052c9](https://github.com/suiflex/arsy-code/commit/03052c92623292ae5c63aa9d2eb1e6f4b59fc3bc))
* **code:** refuse an approved outside path behind a symlink ([a3ff913](https://github.com/suiflex/arsy-code/commit/a3ff913d7e99ab03a305051edef236aa8feeea02))
* **code:** work in additional directories and ask outside them ([7fbbae2](https://github.com/suiflex/arsy-code/commit/7fbbae2d35795e77f3ea9757d09660d5c6d29c30))
* **deps:** update Wasmtime for RustSec advisories ([1cf55f9](https://github.com/suiflex/arsy-code/commit/1cf55f963d00c5ca6ac2cf44982df2a6e9bd576e))
* **kernel:** add additional directories and exact operator grants ([694616b](https://github.com/suiflex/arsy-code/commit/694616b628b2d95d562bbcb4451a3632725b1697))
* **kernel:** send tool outputs before text in a Responses message ([5d64dfe](https://github.com/suiflex/arsy-code/commit/5d64dfe6aeed8efa623fe58865eec9b0d55409d7))

## [0.7.1](https://github.com/suiflex/arsy-code/compare/v0.7.0...v0.7.1) (2026-09-28)


### Bug Fixes

* **cli:** approve a plan into auto unless another mode is named ([3072811](https://github.com/suiflex/arsy-code/commit/3072811dd5a01eca7db1120b0f055f4feddd5071))
* **cli:** choose effort variants by effort instead of listing them ([dce07e9](https://github.com/suiflex/arsy-code/commit/dce07e92241243ae01ae78973be5f4349c41e4d9))
* **cli:** keep live command output on a character boundary ([b7ae71f](https://github.com/suiflex/arsy-code/commit/b7ae71f8a94b1448ea80785cbc43c1b5c8699b88))
* **cli:** keep mode and effort controls live while a turn runs ([327c61d](https://github.com/suiflex/arsy-code/commit/327c61d87af9405703e5a2a24b3686c18da2a407))
* **cli:** never ask in auto mode ([af96ae6](https://github.com/suiflex/arsy-code/commit/af96ae69263b9cd9007ec3147721a7e9f2039eac))
* **cli:** never split a streamed answer inside a table ([fa36c4e](https://github.com/suiflex/arsy-code/commit/fa36c4e10571469d3069c2015468b6852ad19e36))
* **cli:** print nothing when the model changes mid-session ([dce26a0](https://github.com/suiflex/arsy-code/commit/dce26a0d86acdb2aa1f0cd06fa71d2f84c7045a0))
* **cli:** redraw the resumed session's conversation on /resume ([d14eb9a](https://github.com/suiflex/arsy-code/commit/d14eb9aa70dfe90e801fb60e6d78e4110857ee3a))
* **cli:** refresh OAuth model lists at startup ([6e9bbd4](https://github.com/suiflex/arsy-code/commit/6e9bbd42a4c197f8c525bab87188872607eadb00))
* **cli:** render markdown in the thinking box, tables included ([eb684fa](https://github.com/suiflex/arsy-code/commit/eb684fa03b72a455717f449a1d71847fe9df57aa))
* **cli:** show a compaction's progress while it runs ([234110b](https://github.com/suiflex/arsy-code/commit/234110bf0434262e8c5d704ddd60457f5202f454))
* **cli:** show a mid-session model switch as a strip, not a new card ([bd96e0b](https://github.com/suiflex/arsy-code/commit/bd96e0bf04ed34298a8c7bd1b00127e07aa7a0da))
* **cli:** show each context compaction and add /compact ([667f74c](https://github.com/suiflex/arsy-code/commit/667f74c5a2629677c03af2336a69bb51860771c7))
* **code:** quote a long declaration up to a character boundary ([f1e2d88](https://github.com/suiflex/arsy-code/commit/f1e2d88b7b1072fd3e121fed76f6608c993164b2))
* **code:** report each compaction stage as it starts ([75f0328](https://github.com/suiflex/arsy-code/commit/75f032802674e15ac55d52f13970e2c014df1007))
* **code:** review what a shell command does before Auto runs it ([e0b173c](https://github.com/suiflex/arsy-code/commit/e0b173c5c9167c09c95185d165c3a3a9d70ec15c))
* **code:** skip a use whose brace closes before it opens ([871c7cc](https://github.com/suiflex/arsy-code/commit/871c7cc3f6d06729e65b494a4edec184d83ccd5f))
* **kernel:** block risky actions in attended review instead of asking ([29735f8](https://github.com/suiflex/arsy-code/commit/29735f88d9c3d6b0232d5b7290e914b5ee378719))
* **kernel:** cap a validation detail on a character boundary ([0aef2ba](https://github.com/suiflex/arsy-code/commit/0aef2ba881be4eb8943c0dc66a7e6387769bef70))
* **kernel:** record context compactions and fold the dialogue on request ([5d9b039](https://github.com/suiflex/arsy-code/commit/5d9b039823e19cd36198d61f87b7b0d6eab1d117))
* **kernel:** reject a non-hex state version before slicing it ([6e3dd3a](https://github.com/suiflex/arsy-code/commit/6e3dd3ac88387c60d393ef0edcacdd1eec6191ca))
* **kernel:** separate reasoning summary parts in the Responses stream ([8a2d2f4](https://github.com/suiflex/arsy-code/commit/8a2d2f43bb6b4f644e30833b5e95bef631085f7e))
* **tui:** add a live row for a compaction in progress ([63a24d3](https://github.com/suiflex/arsy-code/commit/63a24d337ca6ac766538531f358cfd3553c8e0b4))
* **tui:** keep the model dialog within the terminal's rows ([1027690](https://github.com/suiflex/arsy-code/commit/10276905107ddc44db4e675575b7dcb9d9293786))
* **tui:** keep walking history past a recalled slash command ([c8511c3](https://github.com/suiflex/arsy-code/commit/c8511c3ece782ee365789ce4989364ae06d8f29c))
* **tui:** make each plan card key choose the option beside it ([5c06160](https://github.com/suiflex/arsy-code/commit/5c061604d93a00c84c19722d1e26e5d1c9fc2e6f))
* **tui:** render markdown tables as aligned grids ([ee51d9c](https://github.com/suiflex/arsy-code/commit/ee51d9cf03bd3dfb807611879bce9da4c73d0334))
* **tui:** set code blocks off with a gutter instead of fences ([49f69c5](https://github.com/suiflex/arsy-code/commit/49f69c5a74dfd8af9cf2f505ec940eea5cbd8e61))
* **tui:** wrap long prompts in the composer instead of scrolling ([47495cd](https://github.com/suiflex/arsy-code/commit/47495cd9c5198c6d6ca791b04da90a04e65c769d))

## [0.7.0](https://github.com/suiflex/arsy-code/compare/v0.6.0...v0.7.0) (2026-09-28)


### Features

* **cli:** add --dangerously-skip-permissions with a risk prompt ([6624646](https://github.com/suiflex/arsy-code/commit/66246469cbd3d8aa2f027f3b0e98c89640b0f2b5))
* **cli:** auto-run allowlisted commands in accept-edits mode ([1ce2947](https://github.com/suiflex/arsy-code/commit/1ce29476b4e7b12bf74fe95773a5f15e12a26453))
* **cli:** choose the mode a plan is approved into ([1501512](https://github.com/suiflex/arsy-code/commit/150151265ba441867f69d3514443e10fc659b9a1))
* **config:** accept a trusted command allowlist ([a18a5de](https://github.com/suiflex/arsy-code/commit/a18a5de642ba5bee482e046f07127c948144c49c))
* **tui:** collapse large pastes into a placeholder ([bce4ac0](https://github.com/suiflex/arsy-code/commit/bce4ac0bc513478dc899381a58865fddb07d601a))


### Bug Fixes

* **cli:** apply the approval mode to the runtime every round ([7d7413b](https://github.com/suiflex/arsy-code/commit/7d7413b50d7fec47d369b114ad5d464d0186e096))
* **cli:** compact a context view so turns never slice past history ([e82e52e](https://github.com/suiflex/arsy-code/commit/e82e52ecf031eda9c253f0f94ae4f5f3bfd0dd95))
* **cli:** fail an unresolved provider instead of falling back to codex ([6117da9](https://github.com/suiflex/arsy-code/commit/6117da9c7ce8822a9ea50db129f59efc48b89177))
* **cli:** keep a legacy rule approval from switching to auto ([a15bb56](https://github.com/suiflex/arsy-code/commit/a15bb56470852ef1079f88993604cd90d7bdeda3))
* **cli:** let plan mode use its own planning tools ([203dd77](https://github.com/suiflex/arsy-code/commit/203dd772968210e8cc0d75fc0450d91a27053c26))
* resolve npm publish tag from release commit ([5a3a527](https://github.com/suiflex/arsy-code/commit/5a3a527632deb3c885d42923d4181c2af2f04daa))
* **tui:** keep line breaks in bracketed paste ([060c143](https://github.com/suiflex/arsy-code/commit/060c143cc8c3bb2d6e5651c69b80e3b724e872e4))

## [0.6.0](https://github.com/suiflex/arsy-code/compare/v0.5.1...v0.6.0) (2026-09-28)


### Features

* **cli:** add config set and unset ([10c2bb8](https://github.com/suiflex/arsy-code/commit/10c2bb80177e27b623c7eb7cd632bc7c3058c6e5))
* **cli:** add hook add and remove ([5854aef](https://github.com/suiflex/arsy-code/commit/5854aef59b72ab45561c3e612087b1a35762d2dc))
* **cli:** add storage listing, cleanup and history reset ([cf687f7](https://github.com/suiflex/arsy-code/commit/cf687f7cdbb156331906440eb18a5c1151b951ed))
* **cli:** edit ARSY guard files programmatically ([891126a](https://github.com/suiflex/arsy-code/commit/891126adbfb3f50a316720d7e63a827c943459a5))
* **cli:** honour the storage settings in the store and gc ([75b7b39](https://github.com/suiflex/arsy-code/commit/75b7b39f7c56520cd6917eb954ed092df078f196))
* **cli:** keep caches and remembered choices out of the ARSY home root ([96f4d75](https://github.com/suiflex/arsy-code/commit/96f4d75af7e8861c6c1bfe948dfa48ceb69a2661))
* **cli:** move an existing workspace state into .arsy/state once ([66626ce](https://github.com/suiflex/arsy-code/commit/66626cefc61077e3b36c521841b10f64143f113a))
* **code:** keep workspace runtime state under .arsy/state ([e237ec1](https://github.com/suiflex/arsy-code/commit/e237ec19a1b4b6d1a3dd5fa33ac19076037f5682))
* **code:** mark each view's lease on disk ([dc1b131](https://github.com/suiflex/arsy-code/commit/dc1b1318628867e3c46b2044cc198dc2644c7efd))
* **kernel:** add storage settings for state and artifact retention ([cb8d9a3](https://github.com/suiflex/arsy-code/commit/cb8d9a39e27dcc76f302e7ee247dd64ba9090c8f))
* **kernel:** checkpoint a session store's log into its database ([94298e6](https://github.com/suiflex/arsy-code/commit/94298e675e5d2b2ccab7bc864fc20725968bf948))
* **kernel:** keep file credentials under the secrets directory ([80af31e](https://github.com/suiflex/arsy-code/commit/80af31e8e928fa9cba9f83af3fed03c5cf1daae7))
* **kernel:** tell whether a session store is being written ([15f75cf](https://github.com/suiflex/arsy-code/commit/15f75cf44c497309bec03179bf4d4b221dc4ab0a))
* split the .arsy layout and manage it from ARSY CODE ([e4c2ec8](https://github.com/suiflex/arsy-code/commit/e4c2ec80f18d9acca46d997ddcc08c66e0e6b148))
* **tui:** add a /storage dialog ([ba1df08](https://github.com/suiflex/arsy-code/commit/ba1df0808d78e38d0c9f04b3c9b2fd3921a158d6))
* **tui:** add and remove hooks from the hooks dialog ([1cedfd9](https://github.com/suiflex/arsy-code/commit/1cedfd933594e5f310f603ac596448d9244e40f4))
* **tui:** let /settings write to the user or the project file ([e27ffb3](https://github.com/suiflex/arsy-code/commit/e27ffb39ea014a65e7c73cc7a5cb725579f0fa56))


### Bug Fixes

* **cli:** prepare the secrets directory before writing credentials ([e8a5291](https://github.com/suiflex/arsy-code/commit/e8a5291856d344c3c3aed959a6844ee9614c5de1))
* **cli:** prune only views whose lease has run out ([03c9599](https://github.com/suiflex/arsy-code/commit/03c9599351a77289214734467a4f324ef67ba9f2))
* **cli:** report a failed /rename instead of claiming success ([a830bbc](https://github.com/suiflex/arsy-code/commit/a830bbcc0214be6e3f9dd60c421cf20dcc83702b))
* **cli:** save what /settings, /hooks and /skill change ([6bede24](https://github.com/suiflex/arsy-code/commit/6bede24b5ba7b04185d663ddec5bef47726dbecf))
* **cli:** shift hook.disabled keys without overwriting each other ([46b658a](https://github.com/suiflex/arsy-code/commit/46b658ae45435157512a920e21aba0f808584d64))
* **cli:** show the saved title of a session with no turns ([38c0b7a](https://github.com/suiflex/arsy-code/commit/38c0b7ac07d9712aafbf001e0c895ec2631d19c7))
* **cli:** validate a settings edit before writing it ([ce35535](https://github.com/suiflex/arsy-code/commit/ce35535ef1a58e82490b8f4e8d196f28af38fbb9))
* **code:** checkpoint the session store before moving it ([ed8d652](https://github.com/suiflex/arsy-code/commit/ed8d6520346a16c8609bae39e238d54b2b609a1a))
* **hook:** read the user guard from the ARSY config home ([248b7ce](https://github.com/suiflex/arsy-code/commit/248b7cef3495ebe1d0795a6819718a1ca5cd938a))
* **kernel:** create the secrets directory owner-only in one step ([09a9c43](https://github.com/suiflex/arsy-code/commit/09a9c43c085cd20ef551b020cf2262c3281e6b31))
* **tui:** scan storage once and keep the dialog's entries current ([d0f4f59](https://github.com/suiflex/arsy-code/commit/d0f4f5989031d0d539609c6baca5ee35bce10495))


### Performance Improvements

* **cli:** load configuration in open_store only when it matters ([69d36bd](https://github.com/suiflex/arsy-code/commit/69d36bdd86ce8ed3a6be2e6c0ce8fd005a1b10df))
* **cli:** measure storage with an explicit stack ([409d692](https://github.com/suiflex/arsy-code/commit/409d69262a1468b28362510f55b3276388e03303))

## [0.5.1](https://github.com/suiflex/arsy-code/compare/v0.5.0...v0.5.1) (2026-09-25)


### Bug Fixes

* **ci:** extract Windows smoke archives with Expand-Archive ([880f988](https://github.com/suiflex/arsy-code/commit/880f9888cc98bac2bdde7f90549f825dd8a98649))
* **ci:** install cosign on Windows ARM64 smoke runners ([5c871a1](https://github.com/suiflex/arsy-code/commit/5c871a15bf48311d387645df4c8052f8cbdb1028))
* **ci:** make npm publish idempotent and never move latest backwards ([3295761](https://github.com/suiflex/arsy-code/commit/32957610e6f111de3f741059bd61b11c740066be))
* **ci:** retry the npm smoke install while the registry catches up ([f3cf943](https://github.com/suiflex/arsy-code/commit/f3cf9436822042074a9d3d3713987f7482698efa))
* **ci:** wait for npm to serve a version before smoke-testing it ([f669302](https://github.com/suiflex/arsy-code/commit/f66930216bb801ffa7b57f78784063437a502adf))

## [0.5.0](https://github.com/suiflex/arsy-code/compare/v0.4.0...v0.5.0) (2026-09-25)


### Features

* **cli:** implement binary self-update for arsy update command ([6962989](https://github.com/suiflex/arsy-code/commit/696298947b754cba7a22bb25f54dd2411dda0ddd))
* **cli:** pace streamed answers word by word ([8b94bd3](https://github.com/suiflex/arsy-code/commit/8b94bd37c3102b033282dee9ce2c955f15445201))
* **cli:** stream tool-call arguments into a live draft card ([26b35ee](https://github.com/suiflex/arsy-code/commit/26b35ee80835f0bfd760cd7994bf4a545892b184))
* **install:** add download progress and ascii logo to installer ([02c20d4](https://github.com/suiflex/arsy-code/commit/02c20d484959a41a12dc1a4b2b516aaeeb9b4c87))
* **tui:** label a tool card that is still being written ([9bc0141](https://github.com/suiflex/arsy-code/commit/9bc01414d9aae8d531fb8e87c132a4f1aa32ffe2))
* **tui:** let a response continue without its marker ([c030e2f](https://github.com/suiflex/arsy-code/commit/c030e2f6f72f8716edcb5a8307233dc3c168b9b9))


### Bug Fixes

* **cli:** reveal streamed text that has no spaces between words ([d9db696](https://github.com/suiflex/arsy-code/commit/d9db69624066515f571d7b86bf8ec088eab62253))
* **code:** hold a split character even behind an invalid byte ([d9d3982](https://github.com/suiflex/arsy-code/commit/d9d39825806589d73ee2dbe337adb57c3b5d1be7))
* **code:** keep multibyte characters whole across output chunks ([c82f8ab](https://github.com/suiflex/arsy-code/commit/c82f8ab7f29ca600e6ef6562800548faf3ede270))
* **npm:** install from the release archive, not a platform package ([e72c60b](https://github.com/suiflex/arsy-code/commit/e72c60b84cb57164e81e0f7289b5c13ad41fc38d))
* **npm:** publish the launcher from the release archives ([bd18645](https://github.com/suiflex/arsy-code/commit/bd1864560ef8afba6e919fea5d19a9358701db27))
* **provider:** bump codex client version to 0.156.1 and show loading state on fetch ([7a7d88a](https://github.com/suiflex/arsy-code/commit/7a7d88a545aff89b4d416c0c67e62b0182992c58))
* **updater:** address review feedback on binary safety, windows support, and rollback gates ([df23ed3](https://github.com/suiflex/arsy-code/commit/df23ed339941ae14e2f5b50ccf251ac67ad0dc2a))
* **updater:** require backups before replacing and report failed rollback ([73f0d93](https://github.com/suiflex/arsy-code/commit/73f0d934c4070af9538ab67ee4144968071dfdb9))

## [0.4.0](https://github.com/suiflex/arsy-code/compare/v0.3.0...v0.4.0) (2026-09-23)


### Features

* advance P1 agent safety and launch readiness ([366d68e](https://github.com/suiflex/arsy-code/commit/366d68e69f6b67ad6a269e6430373dd3bc0539d5))
* advance P1 agent safety and launch readiness ([0ad9166](https://github.com/suiflex/arsy-code/commit/0ad91666b1a96b2f113e076e19076af29d341b15))
* enforce safe auto and durable agent controls ([8f8506e](https://github.com/suiflex/arsy-code/commit/8f8506ea91c4b7afa9df534da80ad422143ac58f))
* **provider:** add provider presets and discovery ([b6694fc](https://github.com/suiflex/arsy-code/commit/b6694fc5dc855da8ff969fa64df07e69854c0193))
* **provider:** unify provider and model management ([d96fa94](https://github.com/suiflex/arsy-code/commit/d96fa9446dc6fc666c4be736c9988306ec85face))
* **tui:** add three-pane provider dialog ([5a5739d](https://github.com/suiflex/arsy-code/commit/5a5739dad3e450b6ec66e20dcf5135d8d348a58d))


### Bug Fixes

* **oauth:** restore Codex aliases ([f50af4b](https://github.com/suiflex/arsy-code/commit/f50af4b307fba91e018554e3cdf1e9e5052073e5))
* **provider:** repair Codex and Antigravity lifecycle ([8a58232](https://github.com/suiflex/arsy-code/commit/8a5823292e0d402492152bf7b233d85f933afb07))
* **tui:** clean provider review findings ([e02e698](https://github.com/suiflex/arsy-code/commit/e02e6983f04e175923a62e3ef37fcb228bfd27b1))

## [0.3.0](https://github.com/suiflex/arsy-code/compare/v0.2.0...v0.3.0) (2026-09-21)


### Features

* **cli:** add three-pane model picker ([b175a33](https://github.com/suiflex/arsy-code/commit/b175a330ac933ee6dfce35482e2b7148487a286b))
* **cli:** add three-pane model picker ([7da2b8e](https://github.com/suiflex/arsy-code/commit/7da2b8e8d4df3e3877f7bf99c428e5f86463822f))
* **cli:** edit a setting in the dialog, never by typing ([634c4e5](https://github.com/suiflex/arsy-code/commit/634c4e591411c1427eb3f8b603b48b73425de1ce))
* **cli:** make /hooks, /skill and /settings manage, not only print ([432adcd](https://github.com/suiflex/arsy-code/commit/432adcdad132016fc3734326052856e6e48aebdd))
* **cli:** refresh a stale OAuth token and retry once in arsy run ([0cc0c10](https://github.com/suiflex/arsy-code/commit/0cc0c106eff2c83897a4ab9a4aec4a2906e531d9))
* **cli:** refresh a stale OAuth token mid-session in the interactive TUI ([3b766af](https://github.com/suiflex/arsy-code/commit/3b766afbaa73e1b08b1635d9148a4a1ef7516a3e))
* **cli:** sign in to Claude Pro/Max via /auth login claude-oauth ([0d16bfb](https://github.com/suiflex/arsy-code/commit/0d16bfbc0f3bfc2e07da9444de81b75ac8946a5f))
* **cli:** split settings dialog into multi-pane layout ([69c3156](https://github.com/suiflex/arsy-code/commit/69c3156334d72a74592f751f51f689903a278b0b))
* **cli:** split settings dialog into multi-pane view ([ca14e83](https://github.com/suiflex/arsy-code/commit/ca14e83c0656a1ffa113c5566d592b6d8ef3626a))
* **code:** key each hook declaration so an operator can switch it off ([437116e](https://github.com/suiflex/arsy-code/commit/437116e54ded8c52338e6ef2dc4d4a300acea361))
* **code:** let the model read a skill it was told about, and open the session store at start ([a58d20a](https://github.com/suiflex/arsy-code/commit/a58d20a52f6d636a0c614a68574caabaa1ef10a7))
* **code:** tell the model which skills exist ([4b3a9f3](https://github.com/suiflex/arsy-code/commit/4b3a9f335b0b321ab00033f3e81b9f0fc5eabd2f))
* **compat:** read the operator's own skills from claude and codex ([ee9bde2](https://github.com/suiflex/arsy-code/commit/ee9bde2c95ef8708ac8b45f28b6ac755d3d2677a))
* **eval:** isolated benchmark trials and regression gates ([d687f20](https://github.com/suiflex/arsy-code/commit/d687f202a4fce12f4ecc6e6dac36a341fee18e2b))
* hold MCP server logs and show them at a turn boundary ([1089b4f](https://github.com/suiflex/arsy-code/commit/1089b4fe3ae8ce2ad30865d5eee5a70c4f80a386))
* **kernel:** add manual OAuth grant and claude-oauth preset ([f24da51](https://github.com/suiflex/arsy-code/commit/f24da512740a36096046d1f248a008722bfdf96c))
* **kernel:** durable task attempts, budget settlement, and evidence that survives restart ([175ddfc](https://github.com/suiflex/arsy-code/commit/175ddfcebbbcaf634ae9ab08c2096963c5a0c264))
* **kernel:** name the editable settings and record what is switched off ([3745aba](https://github.com/suiflex/arsy-code/commit/3745aba8bf0ea9823769370e957c8435296630b6))
* **kernel:** Phase 0 — runtime truth and evidence durability ([e3d2e95](https://github.com/suiflex/arsy-code/commit/e3d2e95510e43d734017b39608e59802c6f3df37))
* manage the transcript, its settings and its extensions end to end ([2e29b6b](https://github.com/suiflex/arsy-code/commit/2e29b6b384ec13bd268459ea23bcdd0a9bbe17e9))
* **proof:** proof-carrying completion ([4143c6c](https://github.com/suiflex/arsy-code/commit/4143c6c226dcbb864f953017381c1f3b4644d72c))
* **runtime:** async durable agent runtime ([140d686](https://github.com/suiflex/arsy-code/commit/140d686352e4a358825994c652492797d431380d))
* sign in to Claude Pro/Max via /auth login claude-oauth ([a10c3fc](https://github.com/suiflex/arsy-code/commit/a10c3fc576215739ca0222df767b49322fc9c02a))
* **tui:** add the arsy-tui crate and its cell model ([8fddba2](https://github.com/suiflex/arsy-code/commit/8fddba22e021e2fb70fb8c14b0e4faf73197a9b4))
* **tui:** draw a tool call as a tinted panel, not a box of lines ([8f2a799](https://github.com/suiflex/arsy-code/commit/8f2a79999777af3133405c44134e02f2dfe788c4))
* **tui:** draw the launch mark as hand-authored block art ([6589c8a](https://github.com/suiflex/arsy-code/commit/6589c8a31b8a2384dd6a5983f0a50f127e6b57e9))
* **tui:** draw the modern style as the mockup's tinted cards ([3b31959](https://github.com/suiflex/arsy-code/commit/3b319595a6de8db0bfa4102baf22724d3eea07bf))
* **tui:** give the Codex route the same cards, markdown and plan ([1d59384](https://github.com/suiflex/arsy-code/commit/1d59384befb50a490c89aa1d327e73f3f8e499cf))
* **tui:** highlight modern task rows ([86e3248](https://github.com/suiflex/arsy-code/commit/86e32489eb04019d7744e5f2d9f3fa935ce1aef5))
* **tui:** make modern projection default ([49da3a5](https://github.com/suiflex/arsy-code/commit/49da3a5ba78ea8f59b9c724fe54673052ad2c74a))
* **tui:** match target transcript design ([92d74c8](https://github.com/suiflex/arsy-code/commit/92d74c8c90eb75ca4698f1845f199656aa0995ff))
* **tui:** project modern execution cards ([c0bdca5](https://github.com/suiflex/arsy-code/commit/c0bdca59a7d4944deff3ee7de06d26142af63d58))
* **tui:** record the mode change and close a turn with its session ([84d66dd](https://github.com/suiflex/arsy-code/commit/84d66dd3180d09615940a2cedb27270649eb8252))
* **tui:** render live modern transcript rails ([8c328ea](https://github.com/suiflex/arsy-code/commit/8c328ea127e6251a6d9bce12e946762af390430a))
* **tui:** render modern tool lifecycle cards ([2175c6f](https://github.com/suiflex/arsy-code/commit/2175c6fcc5d792c53aa3c605d20a15123c977c8a))
* **tui:** sweep a scan band across the mark at launch ([d3e1aae](https://github.com/suiflex/arsy-code/commit/d3e1aae0c5368e7fb44a13563608de5b203e0dc7))
* **tui:** unify the launch mark and sweep it at launch ([602ebbb](https://github.com/suiflex/arsy-code/commit/602ebbb34aa776a8701863a003ddafa8427ac5c2))
* **workspace:** isolated writer agents and typed integration ([79516fd](https://github.com/suiflex/arsy-code/commit/79516fd6cd53a721db480834c7f1c61fb012db34))


### Bug Fixes

* address model picker review feedback ([d6a82a7](https://github.com/suiflex/arsy-code/commit/d6a82a780523dd943175ff0bc1c5a6be8af7f291))
* address settings dialog review feedback ([dbf3d41](https://github.com/suiflex/arsy-code/commit/dbf3d417bd4f7ea299d9f28ae8ef0c79c13c7920))
* **ci:** install dbus headers for the nightly fuzz job ([c5aa1bb](https://github.com/suiflex/arsy-code/commit/c5aa1bb5a3301743a1315088b0bbf71bf86b81c7))
* **ci:** restore nightly fuzz coverage ([4d5871a](https://github.com/suiflex/arsy-code/commit/4d5871a0245ca5465884777fffcaf2d9ef666654))
* **cli:** answer the dialog keys quietly and reuse what login resolves ([2f2b856](https://github.com/suiflex/arsy-code/commit/2f2b856626940657126462fc4759890427b3e628))
* **cli:** bind expand to Ctrl-O so the key is reachable ([2acafe4](https://github.com/suiflex/arsy-code/commit/2acafe44f0431ec569bcc6a119fce8b002de0ede))
* **cli:** don't block on stdin for manual OAuth paste in the TUI ([b8a5c7a](https://github.com/suiflex/arsy-code/commit/b8a5c7a9cdb0bde8d4f46c419966dbe47233ad7e))
* **cli:** drop the orphan doc comment and gate the unix-only import ([f5bfb24](https://github.com/suiflex/arsy-code/commit/f5bfb24a7b07fe9e66b004d94012a24cb74a52cb))
* **cli:** file rule grants before the failure path returns ([3ee5b33](https://github.com/suiflex/arsy-code/commit/3ee5b33fce99027bf6f9c9cd6298feca2807cab8))
* **cli:** format the rebased OAuth wizard ([1862aea](https://github.com/suiflex/arsy-code/commit/1862aea5813893111517dbe7e1f1e612317e2344))
* **cli:** give the session dialog a row to act on ([d2a31ce](https://github.com/suiflex/arsy-code/commit/d2a31ce598705727227a244868ee858ed68e24c9))
* **cli:** keep the input line alive while a tool runs ([0e5637b](https://github.com/suiflex/arsy-code/commit/0e5637b839a93dc0374a74c0e4d4b79e13d70f20))
* **cli:** let a workspace skill override the operator's home ([d2e8a58](https://github.com/suiflex/arsy-code/commit/d2e8a58033804e3d88fbde42265f282635e8d83c))
* **cli:** list only hook declarations the engine registers ([3b358d4](https://github.com/suiflex/arsy-code/commit/3b358d421306b6c6cfd9504fcd0b73a3629bfb84))
* **cli:** make the session dialog resolvable and honest about writes ([3865912](https://github.com/suiflex/arsy-code/commit/3865912fd3133ecb94492da901d68c893c41fc4c))
* **cli:** make the tool-round budget configurable and stop a model that loops ([9b6dbe2](https://github.com/suiflex/arsy-code/commit/9b6dbe268b70370e2692a40abc3309915b4844a6))
* **cli:** make the tool-round budget configurable and stop a model that loops ([72713d5](https://github.com/suiflex/arsy-code/commit/72713d5b3d083723ee1142f9173c42598d959e51))
* **cli:** preserve ansi escapes in settings multi-pane rows ([0da13bb](https://github.com/suiflex/arsy-code/commit/0da13bb781f0a2917f578076346a3cf7553ede51))
* **cli:** prevent model picker from falling back to history navigation ([fd72d30](https://github.com/suiflex/arsy-code/commit/fd72d308e24be75eb7a2e04621e09f2a29bf04b3))
* **cli:** prevent model picker from falling back to history navigation ([61273ae](https://github.com/suiflex/arsy-code/commit/61273ae3a4c0104f7e7f1d03df1fbc77687b56f3))
* **cli:** quiet the turn boundary and answer the keys a dialog was given ([2666e76](https://github.com/suiflex/arsy-code/commit/2666e76152f4eb8b716a08875ddef6f015c95301))
* **cli:** satisfy clippy across the workspace ([8d23af3](https://github.com/suiflex/arsy-code/commit/8d23af3800e443c84eedff7ec23325c4e4c5bcb5))
* **cli:** silence what the split left behind ([c46ca5f](https://github.com/suiflex/arsy-code/commit/c46ca5f3c7d179d8c882ddaea4b89fb5534523fe))
* close the gaps the PR review found ([8fdd788](https://github.com/suiflex/arsy-code/commit/8fdd7880f373f08d487d6db911c2b9f4767a7da3))
* close three gaps found reviewing the phase-0 diff ([7f955e0](https://github.com/suiflex/arsy-code/commit/7f955e0d2ab1715ad85ddfd760fa5c36040867d4))
* **code:** offer only the skills this session can open ([c4d4be9](https://github.com/suiflex/arsy-code/commit/c4d4be922d446a27275eb84886e7ab1ad0939829))
* **eval:** give the comparison suites enough trials to support a claim ([ca51d06](https://github.com/suiflex/arsy-code/commit/ca51d0608b0db574351954ce98b71f7c1187baa2))
* **kernel:** give ui.mcp_log its effective value ([73b6e05](https://github.com/suiflex/arsy-code/commit/73b6e05bb82f3b7b17e6606be2d4b680a7eadf74))
* **kernel:** match the real Claude Code OAuth wire contract ([814091b](https://github.com/suiflex/arsy-code/commit/814091bb3f85aa77f6b51dca9469852c8f802c04))
* **kernel:** normalize tool schemas for google code assist ([5132077](https://github.com/suiflex/arsy-code/commit/5132077e950b3e643a50485a4f1a7c64782f8066))
* **kernel:** route Claude thinking and plain Pro the way they were ([0f9cba0](https://github.com/suiflex/arsy-code/commit/0f9cba09ff6bcde35250995c298b7a8ef7c526ab))
* **kernel:** sanitize dotted tool names for Google Code Assist ([c5598c1](https://github.com/suiflex/arsy-code/commit/c5598c10f4e32366fc76919c30f0f6891a2528d2))
* **kernel:** sanitize dotted tool names for Google Code Assist ([794a647](https://github.com/suiflex/arsy-code/commit/794a64758670c076befa39ba7acc74a2c4207c6e))
* **kernel:** sanitize tool names to Anthropic's ^[a-zA-Z0-9_-]{1,128}$ pattern ([c660951](https://github.com/suiflex/arsy-code/commit/c660951b81037c0d9a3d134a3f7defa5fe6cd974))
* **kernel:** stop misreading a text/thinking block as an out-of-order tool call ([78ae964](https://github.com/suiflex/arsy-code/commit/78ae9644ffae395eb15dca2476936c36bcd17aaf))
* **kernel:** update claude-oauth preset to the current model lineup ([cfa7903](https://github.com/suiflex/arsy-code/commit/cfa79032fdc9827fd8f4688be0e90412c30c3887))
* prefix GitHub release titles ([456fddc](https://github.com/suiflex/arsy-code/commit/456fddc18855cb86ef2d273d6955caf6144d2152))
* prefix GitHub release titles ([ba863bd](https://github.com/suiflex/arsy-code/commit/ba863bd5106467ebc8ca18629b657e803148fe3e))
* refresh a stale OAuth token mid-session instead of failing the turn ([4babbfe](https://github.com/suiflex/arsy-code/commit/4babbfeb00d045969f0b97439f1c79bcea88614f))
* **test:** answer a retried request instead of dropping the connection ([b79ef70](https://github.com/suiflex/arsy-code/commit/b79ef70b1293bd0e81d6115997e0264e6deccc41))
* **test:** key each subagent's script by its goal, and pin line endings ([1943762](https://github.com/suiflex/arsy-code/commit/19437626496ceca417ff63377392d7ceb85e2826))
* **test:** prove subagent overlap with a barrier, not a fixed delay ([f1cede9](https://github.com/suiflex/arsy-code/commit/f1cede9ec7e18b034913f29a5975d4c0c08fe684))
* text/thinking block before a tool call is misread as an out-of-order block ([c01e0da](https://github.com/suiflex/arsy-code/commit/c01e0da710f007fe7ec0e1a591054db7ee0de68e))
* **tui:** a session answer of only digits can still be a UUID prefix ([746642d](https://github.com/suiflex/arsy-code/commit/746642de28a398c8f2937ccb372d9e40726908c1))
* **tui:** draw a running Codex command as a bullet, not a panel ([67e2629](https://github.com/suiflex/arsy-code/commit/67e26293dabb1c888e769ef5964e57d90cdf1b4e))
* **tui:** give a block a blank line above it ([836c3fb](https://github.com/suiflex/arsy-code/commit/836c3fbbc857e5ed94d4281161b52113407899ae))
* **tui:** leave modern reasoning unboxed, as the mockup draws it ([500129e](https://github.com/suiflex/arsy-code/commit/500129e4ed2eb063eb0a05b7df9ff32b8344ee13))
* **tui:** let e expand a running call's output ([9acf39a](https://github.com/suiflex/arsy-code/commit/9acf39a5d7b5d231b048918f09982a6c126d439f))
* **tui:** separate the blocks that are written outside the row writers ([8eca6a1](https://github.com/suiflex/arsy-code/commit/8eca6a165569f07a558576ace586a312ed0c5f38))
* **tui:** stop echoing a heading's hashes in a rendered answer ([1250fb2](https://github.com/suiflex/arsy-code/commit/1250fb25e21807973f55c040123f53217b72f7cb))
* **tui:** stop empty reasoning boxes and repeated Response headers ([2d0f1b5](https://github.com/suiflex/arsy-code/commit/2d0f1b53a0498928359dccc76e752433619efb5f))
* **tui:** stop empty reasoning boxes and repeated Response headers ([1b870a0](https://github.com/suiflex/arsy-code/commit/1b870a0523e7ae715322b4e25b63980cfdca5883))
* **ui:** normalize theme descriptions ([a98c74c](https://github.com/suiflex/arsy-code/commit/a98c74ce5a1bea31d6848b0f34ed2bb5dd864511))
* **ui:** normalize theme descriptions ([c848d67](https://github.com/suiflex/arsy-code/commit/c848d67efcde0017adde18bfaf21ed0605f18a7e))
* **verify:** make verification read-only and refuse an unknown session ([1bc1c43](https://github.com/suiflex/arsy-code/commit/1bc1c4329ed8370b1a74f5d48d0533d08a980f1e))
* **workspace:** hand Git a drive path, not an extended-length one ([a5fa3c3](https://github.com/suiflex/arsy-code/commit/a5fa3c3a6c270ade6644420b6f419b41dc2d203f))

## [0.2.0](https://github.com/suiflex/arsy-code/compare/v0.1.2...v0.2.0) (2026-09-18)


### Features

* **agent:** read CLAUDE.md beside AGENTS.md and the operator's own files ([bab27ba](https://github.com/suiflex/arsy-code/commit/bab27ba68a3879ae9f756b417ed6f29405afbd02))
* **cli:** apply Claude and Codex permissions to every decision ([ada09ab](https://github.com/suiflex/arsy-code/commit/ada09ab43e8dd9f1ac9227bf2a7f7bffa00f3479))
* **cli:** connect Claude Code and Codex MCP servers live ([e2edab9](https://github.com/suiflex/arsy-code/commit/e2edab9fd6683dde56ca9937af20f3486b024600))
* **cli:** draw the launch mark as an image where the terminal can ([07e9ec3](https://github.com/suiflex/arsy-code/commit/07e9ec3405d255bab25da60e92243569135a091c))
* **cli:** fall back to the Claude or Codex model when arsy.json names none ([42d4f13](https://github.com/suiflex/arsy-code/commit/42d4f1375532443357f1e0bb6e347acf9e08d5a4))
* **cli:** list MCP declarations as a card instead of a dump ([2b40e89](https://github.com/suiflex/arsy-code/commit/2b40e897376a1169213401d0b6cda2ef5d50614d))
* **cli:** reprint the launch card when the model or mode changes ([d530df8](https://github.com/suiflex/arsy-code/commit/d530df85753e4bec4a6b4015c6c27fee5438d037))
* **cli:** resolve and adopt Claude's user-scope MCP connections ([c1d26d7](https://github.com/suiflex/arsy-code/commit/c1d26d737230bdc661c92a169937594446865509))
* **code:** report the text a file write replaced ([048dfad](https://github.com/suiflex/arsy-code/commit/048dfad999e4afcc7ab0dbc43e76282d433da117))
* **compat:** find the operator's own Claude and Codex instructions ([0dafd4d](https://github.com/suiflex/arsy-code/commit/0dafd4df0b224bd5cb8fb88917efd3fa60d3dad2))
* **compat:** read the model Claude Code and Codex are set to use ([a135f8b](https://github.com/suiflex/arsy-code/commit/a135f8b0e7a707b304ff076f61292be4b37e444c))
* **compat:** resolve Claude and Codex homes from their own variables ([02d5599](https://github.com/suiflex/arsy-code/commit/02d559943135d33963d5310485769ac895b495bb))
* **compat:** run Claude Code and Codex setups without reconfiguring ([b2e8bae](https://github.com/suiflex/arsy-code/commit/b2e8bae18d26862674af931016dfbbd6c9e09428))
* **compat:** translate Claude and Codex MCP servers into seeds ([c51ebbc](https://github.com/suiflex/arsy-code/commit/c51ebbc4d780bdef61d1f868104851335888c389))
* **compat:** translate Claude and Codex permissions into policy rules ([b03ae8d](https://github.com/suiflex/arsy-code/commit/b03ae8d6c597ea59ba2ea45daedab9b3de210d7f))
* **kernel:** carry launch env and headers on MCP transports ([243622f](https://github.com/suiflex/arsy-code/commit/243622f0840359c0a35df9ba08aec6743ccb6077))
* **kernel:** declare the connection ARSY ships with itself ([94574b2](https://github.com/suiflex/arsy-code/commit/94574b2ec85b0bb03675c30ed43e0dd03f8ac258))
* **kernel:** let an amendment tune MCP limits without a transport ([b34f90f](https://github.com/suiflex/arsy-code/commit/b34f90f548a4ab22969390a18ba0567c46bacc47))
* **kernel:** let other tools name a fallback model per dialect ([412d0bc](https://github.com/suiflex/arsy-code/commit/412d0bc37476ce143f80d97c1e9eaf66adda5c6d))
* **kernel:** seed other tools' MCP declarations below arsy.json ([260cdd4](https://github.com/suiflex/arsy-code/commit/260cdd4528f850604a2d8efdb28cdbc2e6420456))
* **kernel:** seed other tools' permission rules below arsy.json ([a4ab6c6](https://github.com/suiflex/arsy-code/commit/a4ab6c62c6a945de31154bb7d530f2db13683fc8))
* **mcp:** hand launch env to stdio servers and headers to http ones ([930ab44](https://github.com/suiflex/arsy-code/commit/930ab44ae3a01c95031a6c8259f736e7afa256ef))
* **mcp:** let a call wait for a server still connecting ([e9e0dfc](https://github.com/suiflex/arsy-code/commit/e9e0dfc26c2919a7a49710fd1a706ecb364a7522))
* move settings to ~/.arsy/arsy.json ([c591df0](https://github.com/suiflex/arsy-code/commit/c591df007378b61c496893c62e139014928e0a7d))
* move settings to ~/.arsy/arsy.json ([9a98da8](https://github.com/suiflex/arsy-code/commit/9a98da88b2fc98d09fe1489e7167ca1a52eb2bfb))
* ship FluxGuard with arsy and declare it as a connection ([ddc013b](https://github.com/suiflex/arsy-code/commit/ddc013bd7c9b5514a7e46ff3be136f906e35a7f0))
* **tui:** add /mcp dialog and tidy the launch card ([1eb9e44](https://github.com/suiflex/arsy-code/commit/1eb9e44404a52f449c944e3419919fa72bf56953))
* **tui:** draw the half-block logo at 16x8 cells ([1cbddb2](https://github.com/suiflex/arsy-code/commit/1cbddb24ff24b5b5e11c80ba38300966d7f7261c))
* **tui:** hold MCP connections for the whole session ([e8ae02a](https://github.com/suiflex/arsy-code/commit/e8ae02adf492bb03ae89cc97d97ca79ddb2f87f5))
* **tui:** manage Claude Code and Codex MCP servers from /mcp ([006e628](https://github.com/suiflex/arsy-code/commit/006e6289a001160569940ecfb4871d351616428c))
* **tui:** run lifecycle hooks around interactive turns and tool calls ([c9f0e0c](https://github.com/suiflex/arsy-code/commit/c9f0e0cc50ebc914694952086f994e54e6c03a46))
* **tui:** toggle and adopt MCP connections from /mcp ([55e5fa7](https://github.com/suiflex/arsy-code/commit/55e5fa780738ecde7d7e0c7193d49082d011c29a))


### Bug Fixes

* **bench:** measure the operation, not the process start-up ([6f95e37](https://github.com/suiflex/arsy-code/commit/6f95e370f378ac0a98bbd1958a71f90757d6d48c))
* **cli:** carry a follow-up typed during a provider turn ([3b6cee5](https://github.com/suiflex/arsy-code/commit/3b6cee58eae97eb93ef2913b914471afcf63451c))
* **cli:** hide the password a listed MCP command carries ([3a5fa60](https://github.com/suiflex/arsy-code/commit/3a5fa607de04e836a1f622464163de2cd73310d5))
* **cli:** keep the mode a settings file was given when replacing it ([957dd5e](https://github.com/suiflex/arsy-code/commit/957dd5e5c66d655be1e5c99a13a4a3f62070ec03))
* **cli:** let the operator's MCP declarations outrank a checkout's ([f188999](https://github.com/suiflex/arsy-code/commit/f1889998b4f3c5a29e0579a6e6ebbeafa225d145))
* **cli:** let the shipped connection be switched off ([61eb423](https://github.com/suiflex/arsy-code/commit/61eb42384c694beabf6e2b3557ff51edcb3d74fb))
* **cli:** publish the bootstrapped user config atomically ([93ae7fb](https://github.com/suiflex/arsy-code/commit/93ae7fb8dac84b2316eaa9ae77da2a219f610f60))
* **cli:** replace a settings file in one step instead of two ([5a4adf4](https://github.com/suiflex/arsy-code/commit/5a4adf46b888dbb41acf2be33efff975096ac321))
* **cli:** resolve instructions against the invocation's config ([4c0b720](https://github.com/suiflex/arsy-code/commit/4c0b720351f2d9cbb05b0647e6804206b7be6feb))
* **cli:** size the launch logo to the label rows it sits beside ([2c80673](https://github.com/suiflex/arsy-code/commit/2c80673dfa8094d12fb6aa83970cdb1dc08ce674))
* **cli:** split the TUI renderer, and make the launch card and MCP tell the truth ([e247187](https://github.com/suiflex/arsy-code/commit/e2471879bcd06eaf9f095b63deda2a15d9cb320a))
* green CI, one release path, and docs that match the release ([8a9c6fe](https://github.com/suiflex/arsy-code/commit/8a9c6fe34664bad7571dad4fb61701fb6da931f1))
* **hook:** read settings.local.json and honour compat homes and switches ([23d449e](https://github.com/suiflex/arsy-code/commit/23d449e5e7d1bcb765cd4f1e511aa3e23664dcdf))
* **install:** fail clearly when the archive lacks fluxguard.exe ([6c4d881](https://github.com/suiflex/arsy-code/commit/6c4d8811691d18ee101519af1b4824d20d77534c))
* **integrations:** keep an OMP listing independent of arsy.json ([52c08c1](https://github.com/suiflex/arsy-code/commit/52c08c1edd72db6d67734b0e96560e5034cd2db0))
* **kernel:** only trust a FluxGuard found beside an absolute arsy path ([464b554](https://github.com/suiflex/arsy-code/commit/464b5543eda586b5d5f7f5d2b6f9689262b11ab9))
* **kernel:** stop asking OpenAI dialects for parallel tool calls ([0ace4c8](https://github.com/suiflex/arsy-code/commit/0ace4c8eb911074c8c8394ada8530d3ce8052bc7))
* **mcp:** fingerprint transport fields structurally ([946e8c8](https://github.com/suiflex/arsy-code/commit/946e8c89d2d351cee132974e793a8c503cbccbfa))
* **mcp:** ignore a superseded connection attempt ([c4a9ce5](https://github.com/suiflex/arsy-code/commit/c4a9ce5774df81763ee103bd4419d3d9fe67d76a))
* **mcp:** refuse to write a toggle for a name arsy.json cannot hold ([e96d6f4](https://github.com/suiflex/arsy-code/commit/e96d6f4a7c1b18d2c5268e5699568f457ddd0262))
* **mcp:** scrub launch values from a stdio server's log ([a2ee370](https://github.com/suiflex/arsy-code/commit/a2ee3709ab9e270313fe04e36ba8324dd46ed54d))
* **release:** stop asking cosign for a signature file it will not write ([a0c7945](https://github.com/suiflex/arsy-code/commit/a0c79457097f6e2431d8b8304fa22e561e0167ab))
* **test:** stop asserting how fast the runner is ([537f9fc](https://github.com/suiflex/arsy-code/commit/537f9fc510105f617dad03396d38e041828cfb10))
* **tui:** crop and supersample the half-block logo ([80e3509](https://github.com/suiflex/arsy-code/commit/80e3509e44abaa26583e80b3e287525346f06729))
* **tui:** stop reprinting the launch card on Shift+Tab ([fc61a3b](https://github.com/suiflex/arsy-code/commit/fc61a3b2ebeb3b5bd4666b6833293b08ea789b65))

## [Unreleased]

### Features

* **tui:** match the modern transcript mockup with compact tool rows, visible
  TODO progress, outlined composer chrome, bounded rule approvals, and truthful
  turn summaries

### Bug Fixes

* **tui:** keep mode changes, plan previews, tool execution, and provider
  authentication stable during interactive turns


## [0.1.2](https://github.com/suiflex/arsy-code/compare/v0.1.1...v0.1.2) (2026-09-14)


### Bug Fixes

* compile the sandbox on aarch64 Linux ([338bc08](https://github.com/suiflex/arsy-code/commit/338bc0807798ccd132659b6a15df716a9b7c9b36))
* **sandbox:** deny fork and vfork only where the kernel has them ([7e0dda5](https://github.com/suiflex/arsy-code/commit/7e0dda5182f0530ecd546bdf6a72a14a1db58863))

## [0.1.1](https://github.com/suiflex/arsy-code/compare/v0.1.0...v0.1.1) (2026-09-14)


### Bug Fixes

* install the Linux credential-store dependencies in the release build ([72f81a0](https://github.com/suiflex/arsy-code/commit/72f81a0b662ba55df9075feecf018cedebddc674))
* **release:** install the Linux credential-store dependencies ([b2e8b55](https://github.com/suiflex/arsy-code/commit/b2e8b55aae80f16c815761367ef371e4ddff3a1c))

## 0.1.0 (2026-09-14)


### Features

* **acp:** an editor can drive a session over stdio ([07ab402](https://github.com/suiflex/arsy-code/commit/07ab4025e9a422321652120cd5569ee2b95c1003))
* add compatibility and supervised runtime foundations ([#21](https://github.com/suiflex/arsy-code/issues/21)) ([88f6417](https://github.com/suiflex/arsy-code/commit/88f6417951148495ce873c6b83f2cf04316eeb40))
* add npm distribution and release-please CI ([1c776c6](https://github.com/suiflex/arsy-code/commit/1c776c61131e4b234a529b9b7cd098cdd90d7ae5))
* **agent:** enforce Plan Mode at the tool-execution boundary ([#36](https://github.com/suiflex/arsy-code/issues/36)) ([268c4f0](https://github.com/suiflex/arsy-code/commit/268c4f030e2bd594482fcbec03ef29f3a73c08d3))
* **cli:** add interactive /auth menu, effort dials, and model reasoning events ([#26](https://github.com/suiflex/arsy-code/issues/26)) ([d880938](https://github.com/suiflex/arsy-code/commit/d8809387e75b437c0be9664c20f1d85451cbc291))
* **cli:** arsy migrate reaches the store migration engine ([6773f8a](https://github.com/suiflex/arsy-code/commit/6773f8aa80dc714bdb3cd2d1ee5f7e854025f3f1))
* **cli:** improve tui responsiveness, tool boxes, and themes ([#33](https://github.com/suiflex/arsy-code/issues/33)) ([13d511c](https://github.com/suiflex/arsy-code/commit/13d511caccd0953e7e89457505b1a30f01142b62))
* close the 2026-09-11 coding-agent completeness audit ([3f3027a](https://github.com/suiflex/arsy-code/commit/3f3027a1ac44b9e3cab2d27749c5df3334c45d57))
* close the 2026-09-11 coding-agent completeness audit ([4d5bd23](https://github.com/suiflex/arsy-code/commit/4d5bd238948848e3edce8ff82052d842d2356aa3))
* **code:** semantic navigation the model can actually reach ([fdb6730](https://github.com/suiflex/arsy-code/commit/fdb6730813dbfdc32102b8a96dd814498b14c8fb))
* compatibility and supervised runtime foundations ([#23](https://github.com/suiflex/arsy-code/issues/23)) ([de5ab43](https://github.com/suiflex/arsy-code/commit/de5ab4340b1cc553c94c9c6fe699f5e3e393e841))
* **debug:** a breakpoint run, instead of a print statement ([d5cce8e](https://github.com/suiflex/arsy-code/commit/d5cce8e35d6b56b4530a5a270ea4d5aa7a338b7e))
* **eval:** measure the comparison a gate asks for, instead of a pass rate ([e62007a](https://github.com/suiflex/arsy-code/commit/e62007aa45d94b1a23364b556686981c5ec8315b))
* **harness:** one path from a model tool call to an effect ([8966714](https://github.com/suiflex/arsy-code/commit/89667140af039e756c707e334b3254980a072382))
* **hook:** run the lifecycle engine, sourced from whichever ecosystem you already use ([#31](https://github.com/suiflex/arsy-code/issues/31)) ([8826a10](https://github.com/suiflex/arsy-code/commit/8826a10ebf574df00e2a3b103d8649eaa7965bbd))
* **ide:** the reference thin client, and what makes it thin ([b69687e](https://github.com/suiflex/arsy-code/commit/b69687ed0aaadff7c63d4ceb916ff5dcdf80d5c2))
* implement Project 3 prompt sandbox and LSP tasks ([#20](https://github.com/suiflex/arsy-code/issues/20)) ([36da046](https://github.com/suiflex/arsy-code/commit/36da0461007cd02d079dee2cc1fd52ce92bcb18c))
* implement the first 30 Project 3 harness tasks ([bfc7b40](https://github.com/suiflex/arsy-code/commit/bfc7b405b1916a982f3aa5906c3ae4523816c27e))
* **install:** add curl/PowerShell installers, checksum-only ([203d714](https://github.com/suiflex/arsy-code/commit/203d71409e382b043b2f87659309d2431ef1223b))
* **kernel:** add canonical domain types ([aa0baec](https://github.com/suiflex/arsy-code/commit/aa0baec24d527dc37f0ac92758efd008b10834dc))
* **kernel:** add durable artifact CAS ([4585930](https://github.com/suiflex/arsy-code/commit/458593038f89118617a887a2bd7aada53130b182))
* **kernel:** add migration planning with verified backup ([b30e88d](https://github.com/suiflex/arsy-code/commit/b30e88d3c76940f30b0b89e0e748fb875e69e164))
* **kernel:** add optimistic event store contract ([5f084c9](https://github.com/suiflex/arsy-code/commit/5f084c958b195ce8987e8f7b08b5cb8f5827f963))
* **kernel:** add Phase 1a runtime foundations ([bfc25ab](https://github.com/suiflex/arsy-code/commit/bfc25ab3cc37a0dd1990cfcaabaabe22ef6b9a3e))
* **kernel:** add rebuildable event projections ([49295c5](https://github.com/suiflex/arsy-code/commit/49295c5cfd95b04fc02f660d7d2f8ea162795a80))
* **kernel:** add SQLite WAL event store ([1352338](https://github.com/suiflex/arsy-code/commit/13523385055ce6e211098e018789247a07d1a63f))
* **kernel:** add the capability vocabulary and grant attenuation ([74cd2d0](https://github.com/suiflex/arsy-code/commit/74cd2d0b044407b5c94dc535cf64996e0323fe9b))
* **kernel:** add the operation contract and executor registry ([902b5f7](https://github.com/suiflex/arsy-code/commit/902b5f75555f8775b4bf5a321c26c9f64d9107e7))
* **kernel:** add the P1b authority core ([b795a2b](https://github.com/suiflex/arsy-code/commit/b795a2b718b6866f673e7a5da996350ffe254768))
* **kernel:** add the policy engine with an explanation trace ([59162cc](https://github.com/suiflex/arsy-code/commit/59162cc7ebe14f5ffea12db5bfae67bda3a4007e))
* **kernel:** close Phase 1a with properties and schema migration ([39066c0](https://github.com/suiflex/arsy-code/commit/39066c0028a3b410dbed9b7f411c494a5069422a))
* **kernel:** stamp and gate the SQLite schema version ([ddc531d](https://github.com/suiflex/arsy-code/commit/ddc531d64bd95118c845da28cb939a264e32cda5))
* **lsp:** the semantic tier speaks the protocol a real server answers ([05db32f](https://github.com/suiflex/arsy-code/commit/05db32f39ac0bd9dbe83797f1253e1be140e57a9))
* **memory:** a workspace remembers, and the model is told what it remembers ([96c455a](https://github.com/suiflex/arsy-code/commit/96c455a2f06d8368130775d31950f24ddf5c274c))
* **npm:** add npm distribution package for the CLI ([0cc18a4](https://github.com/suiflex/arsy-code/commit/0cc18a440928a9699898fba3fcf52537bba45c86))
* OAuth login presets (Codex, Antigravity) and a selectable theme system ([#27](https://github.com/suiflex/arsy-code/issues/27)) ([5c3eab3](https://github.com/suiflex/arsy-code/commit/5c3eab3ad17ad052103e6e54de38d0de5a327a17))
* overhaul sessions, providers, and TUI execution ([#29](https://github.com/suiflex/arsy-code/issues/29)) ([33be63a](https://github.com/suiflex/arsy-code/commit/33be63a8e58dfd0c2665340d3dd022e7c39ba83b))
* **plugin:** an installed WASM plugin can actually be run ([481d7da](https://github.com/suiflex/arsy-code/commit/481d7dae3eff0870ff68984e5f42f685e5a066b1))
* **provider:** replay a recorded conversation, so a measurement can hold the model still ([c4e9833](https://github.com/suiflex/arsy-code/commit/c4e9833f679cea5644ef402cf8af2836c2f26a06))
* reach any OpenAI- or Anthropic-compatible endpoint natively ([#24](https://github.com/suiflex/arsy-code/issues/24)) ([3525d79](https://github.com/suiflex/arsy-code/commit/3525d79cd14278a14b86e612e9e901c0f58ed36d))
* refresh ARSY branding and TUI logo ([7656e6e](https://github.com/suiflex/arsy-code/commit/7656e6e8d6252c775a3744db0e2252c19c79d728))
* **release:** build Windows on ARM and ship the installers as assets ([8eca1d0](https://github.com/suiflex/arsy-code/commit/8eca1d0cec55d7d875adff87b86d61c03113159d))
* **release:** publish the formula and manifest to the tap and bucket ([544cc20](https://github.com/suiflex/arsy-code/commit/544cc20add521ff0cbf0825da8b8c53c1ced76f6))
* **release:** sign an aggregate SHA256SUMS with keyless Sigstore ([fd13454](https://github.com/suiflex/arsy-code/commit/fd1345412de171b03c766c8584d6d8506b020470))
* repair the release pipeline and enable CI for v0.1.0 ([d8cb699](https://github.com/suiflex/arsy-code/commit/d8cb699f0cd78397f927e38fc7b2cc313c8e1034))
* **review:** arsy review reads the change and says what it deserves ([b5c04f4](https://github.com/suiflex/arsy-code/commit/b5c04f483f7ac2c05b90d4b299ba2043653d46b3))
* **runtime:** a run is a leased task in a durable graph, and resume finishes it ([f395d59](https://github.com/suiflex/arsy-code/commit/f395d59f6449f8e97249eb9536e1edfd0c991420))
* session/evidence/policy CLI, MCP client and server, extensions ([ab752b7](https://github.com/suiflex/arsy-code/commit/ab752b7b91a331868122a238503ddc1f56c72022))
* **subagent:** a child task that holds less than the parent that made it ([a7a0a27](https://github.com/suiflex/arsy-code/commit/a7a0a274475e0b4be816fe115d790fdf643df4de))
* **telemetry:** a run reports what it spent and why it stopped ([7f620e1](https://github.com/suiflex/arsy-code/commit/7f620e19cc6e06412ecdd2e5d4fe628247edca6d))
* TUI slash commands, reasoning effort, and an optional keychain ([#25](https://github.com/suiflex/arsy-code/issues/25)) ([aca83ca](https://github.com/suiflex/arsy-code/commit/aca83ca4cc0694fe25d3b4b5b77f9e0a07ec7cec))


### Bug Fixes

* address the review findings on the completeness-audit branch ([5480bcb](https://github.com/suiflex/arsy-code/commit/5480bcb4d3f7524e5d3da0d903f7e5e871ef833d))
* address the twelve review findings on the extensions branch ([#32](https://github.com/suiflex/arsy-code/issues/32)) ([c0e436d](https://github.com/suiflex/arsy-code/commit/c0e436d7b981febe675bf6ec38cf6db3f405ae36))
* **agent:** a half-applied patch now says what it already wrote ([3177ac6](https://github.com/suiflex/arsy-code/commit/3177ac6b4cde883b47e62fc0f10d089c1821d13b))
* **agent:** remove a cancellation flag nothing consulted, and two dead paths ([b7f8313](https://github.com/suiflex/arsy-code/commit/b7f8313276767230d52d23e7c1263f37947bbc5a))
* **cli:** keep TUI composer responsive ([cd0aa60](https://github.com/suiflex/arsy-code/commit/cd0aa6048536ee4ead982708ccb2adc7201bea05))
* **cli:** review and migrate take the arguments they were documented to take ([c253d3c](https://github.com/suiflex/arsy-code/commit/c253d3c68baea1e9ba03348193609bc65df904eb))
* **code:** the rename staleness check was checking nothing ([ffb05f1](https://github.com/suiflex/arsy-code/commit/ffb05f10c849f475e26651362a95af06985db775))
* compile on Windows and satisfy clippy on Linux ([ca3b457](https://github.com/suiflex/arsy-code/commit/ca3b457d5ba263fe928915f2ee05b59bb157aaad))
* **eval:** a pinned fixture could never be run, including the one that shipped ([92c7a97](https://github.com/suiflex/arsy-code/commit/92c7a97795e199ea74611cf69dba91aa49e3a80a))
* **eval:** give the gate's checks time to finish, and stop paying for a trial twice ([86273ff](https://github.com/suiflex/arsy-code/commit/86273ffd4730b75954f2cd5cdd2ac26ce623d17d))
* fail fast without Linux session bus ([789e802](https://github.com/suiflex/arsy-code/commit/789e80217465a51b74778847fc4c4a02b22a838f))
* follow up merged provider and TUI behavior ([#30](https://github.com/suiflex/arsy-code/issues/30)) ([63d7672](https://github.com/suiflex/arsy-code/commit/63d7672504c20196f583adbc1bdf382d7843f564))
* grant the release-please call the permissions it needs ([0143d1e](https://github.com/suiflex/arsy-code/commit/0143d1eb5ac5daccc74ca437ce9cdba95711d6a9))
* honor gitignore outside Git repositories ([3d0aedf](https://github.com/suiflex/arsy-code/commit/3d0aedf59ce237a1daf1503fae4671e531e243ea))
* **install:** support Windows on ARM and bare version numbers ([669e3c9](https://github.com/suiflex/arsy-code/commit/669e3c9a12cd068d0547bf93b089f90f358a716b))
* let the release lockfile sync reach the crates.io index ([bbe6ded](https://github.com/suiflex/arsy-code/commit/bbe6ded74ce5fb9448577a3e59f4471be9aadce8))
* **lsp:** give back the path a file URI names on Windows ([1b1b63b](https://github.com/suiflex/arsy-code/commit/1b1b63beec607828134478831ccb65665e7a63d5))
* **mcp:** accept a Windows path as a connection command ([0267f22](https://github.com/suiflex/arsy-code/commit/0267f222c573ac25988236a739fbd2673d62f881))
* **mcp:** escape the values written into a server definition ([a26f96e](https://github.com/suiflex/arsy-code/commit/a26f96ece90a47a7327342030a8169525b40fc50))
* **mcp:** forward the server's stderr rather than inheriting it ([2c13d95](https://github.com/suiflex/arsy-code/commit/2c13d959da1474377996eaa5a37e047fdeb5982f))
* **npm-publish:** publish the single package instead of staging five ([049838d](https://github.com/suiflex/arsy-code/commit/049838dafd12209682720f1bec5804d1e7708e4e))
* **npm:** download the binary at install time instead of fanning out packages ([6e1ef96](https://github.com/suiflex/arsy-code/commit/6e1ef96b9f6ded3e726da8602939c4695ea59f5a))
* **release-please:** grant the release call what it asks for ([757c09c](https://github.com/suiflex/arsy-code/commit/757c09c740fa3838e09d701431d3157cc590a44c))
* **release-please:** let the lockfile sync reach the crates.io index ([f595ae7](https://github.com/suiflex/arsy-code/commit/f595ae7b4d1ff54b55e140c8f914eb589e60476f))
* **release-please:** reset the baseline so the next release is v0.1.0 ([975d17c](https://github.com/suiflex/arsy-code/commit/975d17c54a86fae2aa20cea7d35c7786d8ff5865))
* repair the all-features build and clear the clippy gate ([9678663](https://github.com/suiflex/arsy-code/commit/967866392bb7aac2ac9038ba747b02b40e6f3278))
* resolve workspace paths on Windows ([d2fa98a](https://github.com/suiflex/arsy-code/commit/d2fa98ad9364fffb1ee658a9ee3f0c7afa1d01ef))
* satisfy Rust 1.98 clippy ([047acfa](https://github.com/suiflex/arsy-code/commit/047acfa89e76e0ec4ca7704e07e4233bc252beda))
* signal Unix process groups portably ([baf2d73](https://github.com/suiflex/arsy-code/commit/baf2d7380f8c1eda5f9b98bf333247f10139991d))
* **subagent:** an intervention recorded a failure count where the sequence goes ([c2363c8](https://github.com/suiflex/arsy-code/commit/c2363c8ade4f03dc80dac94ede0bd06aa63267ef))
* **subagent:** the spawn bound was checked and never counted ([ed49171](https://github.com/suiflex/arsy-code/commit/ed491719b593e4bb595fddaa8f9b1860f307ae3a))
* **test:** answer the confirmation prompt once it is actually up ([eba3d1e](https://github.com/suiflex/arsy-code/commit/eba3d1e790ff211012d1fb01d7bb3e4a800aac45))
* **test:** write workspace trust keys as TOML literal strings ([3983571](https://github.com/suiflex/arsy-code/commit/398357163ffbd0ba1f857e3029d44238f0e7f32e))
* **tui:** always name the approval mode, `default` included ([78ef051](https://github.com/suiflex/arsy-code/commit/78ef051e6717f71de33063668dc5c5d9c46759e2))
* **tui:** an interactive turn is a leased task too ([c7195e7](https://github.com/suiflex/arsy-code/commit/c7195e75e0f485f59d9dec4bdd0faf01883c05f7))
