# Changelog

All notable changes to this project will be documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) conventions and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Releases are generated automatically by [cocogitto](https://docs.cocogitto.io/) from [Conventional Commits](https://www.conventionalcommits.org/).

- - -
## [v0.3.0](https://github.com/anthonyoteri/cargo-rigtest/compare/e0e89f8befced8b1d8c2f30c70d944269c83e19d..v0.3.0) - 2026-06-02
#### Features
- (**context**) add ctx.global::<T>() typed helper for global state - ([1d8cbc2](https://github.com/anthonyoteri/cargo-rigtest/commit/1d8cbc2d982ad82d18774aa5b51ac22336ea91f4)) - [@anthonyoteri](https://github.com/anthonyoteri)
- (**ssh-client**) add ctx.ssh() for cached SSH sessions on Unix - ([e0e89f8](https://github.com/anthonyoteri/cargo-rigtest/commit/e0e89f8befced8b1d8c2f30c70d944269c83e19d)) - [@anthonyoteri](https://github.com/anthonyoteri)

- - -

## [v0.2.1](https://github.com/anthonyoteri/cargo-rigtest/compare/d61bd7d9ecd9271dc69c279617b60ceba339744e..v0.2.1) - 2026-06-01
#### Bug Fixes
- (**http-client**) lazily initialize reqwest::Client on first use - ([06cff3e](https://github.com/anthonyoteri/cargo-rigtest/commit/06cff3e50e1e8946581db5b387f434ef0da84a25)) - [@anthonyoteri](https://github.com/anthonyoteri)
- exit 0 silently when invoked outside of cargo-rigtest - ([d61bd7d](https://github.com/anthonyoteri/cargo-rigtest/commit/d61bd7d9ecd9271dc69c279617b60ceba339744e)) - [@anthonyoteri](https://github.com/anthonyoteri)
#### Documentation
- (**http-client**) add README section and expand client() inline docs - ([9955946](https://github.com/anthonyoteri/cargo-rigtest/commit/99559460dbb3fe7c17e0556019648e195c501e3a)) - [@anthonyoteri](https://github.com/anthonyoteri)

- - -

## [v0.2.0](https://github.com/anthonyoteri/cargo-rigtest/compare/59758892855da5f1af859f69b721296385506056..v0.2.0) - 2026-06-01
#### Features
- (**http-client**) add configurable HTTP client via #[rigtest::main(http_client = …)] - ([335a075](https://github.com/anthonyoteri/cargo-rigtest/commit/335a0758a9c1537ccddc08f10258bdc4b29a8c99)) - [@anthonyoteri](https://github.com/anthonyoteri)
- (**macros**) add #[rigtest::main] entry-point attribute - ([2ac1fcf](https://github.com/anthonyoteri/cargo-rigtest/commit/2ac1fcf9451df8576a07dd5f6e6e4f8946b5cbee)) - [@anthonyoteri](https://github.com/anthonyoteri)
#### Documentation
- (**rigtest**) add comprehensive module-level documentation - ([ddfaca0](https://github.com/anthonyoteri/cargo-rigtest/commit/ddfaca0d6cb19b6760813284ea68fcc13ac2ed1c)) - [@anthonyoteri](https://github.com/anthonyoteri)
- rewrite README with motivating use case, features section, and expanded installation guide - ([5975889](https://github.com/anthonyoteri/cargo-rigtest/commit/59758892855da5f1af859f69b721296385506056)) - [@anthonyoteri](https://github.com/anthonyoteri)

- - -

## [v0.1.0](https://github.com/anthonyoteri/cargo-rigtest/compare/be1bdbe4e72b08c6865d31905b6c193774b0f6da..v0.1.0) - 2026-05-29
#### Features
- implement cargo-rig acceptance test framework - ([be1bdbe](https://github.com/anthonyoteri/cargo-rigtest/commit/be1bdbe4e72b08c6865d31905b6c193774b0f6da)) - [@anthonyoteri](https://github.com/anthonyoteri)
#### Bug Fixes
- (**ci**) migrate cocogitto-action to v4 API - ([eee8b5f](https://github.com/anthonyoteri/cargo-rigtest/commit/eee8b5f88c4ec0d15a779e74bf24c6f337f953bb)) - [@anthonyoteri](https://github.com/anthonyoteri)
- address code review findings and add community docs (#5) - ([5c0b3cf](https://github.com/anthonyoteri/cargo-rigtest/commit/5c0b3cf88f7dad582e6d1fc54b41a0415dff96bf)) - [@anthonyoteri](https://github.com/anthonyoteri)
- pass toolchain via input rather than action ref in CI matrix - ([91845bb](https://github.com/anthonyoteri/cargo-rigtest/commit/91845bb6d7aa788503fbf90a95f3a0e52ad04f2c)) - [@anthonyoteri](https://github.com/anthonyoteri)
#### Documentation
- add cocogitto separator to CHANGELOG.md - ([adeb49d](https://github.com/anthonyoteri/cargo-rigtest/commit/adeb49d94650676ceba0b1137433f8c50df784fb)) - [@anthonyoteri](https://github.com/anthonyoteri)
- add CI, crates.io, docs.rs, MSRV, and license badges to README - ([623855d](https://github.com/anthonyoteri/cargo-rigtest/commit/623855d3034875187845f8413ce5145fb094c6b8)) - [@anthonyoteri](https://github.com/anthonyoteri)
#### Refactoring
- rename rig/cargo-rig to rigtest/cargo-rigtest - ([ce06bc8](https://github.com/anthonyoteri/cargo-rigtest/commit/ce06bc842beb5beb5dc5ea1877ae4e74b9d4294b)) - [@anthonyoteri](https://github.com/anthonyoteri)

- - -

