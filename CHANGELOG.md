# Changelog

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
