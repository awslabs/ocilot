# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/awslabs/ocilot/compare/v0.1.0...v0.2.0) - 2026-06-11

### Other

- *(deps)* migrate keyring v3 to v4
- address review feedback for soundness, correctness, and API hygiene

### Fixed (review-driven)

- **#1** Replaced all `unsafe impl Send/Sync` (`RegistryClient`, `Registry`, `Decompress`, `Reader`) with sound trait-bound types backed by `Arc<dyn ... + Send + Sync>` and `Pin<Box<dyn AsyncRead + Send + Sync>>`.
- **#2** Introduced `Digest` newtype (`src/digest.rs`) that validates `algo:hex`; replaced every digest `unwrap()` / `strip_prefix("sha256:").unwrap()` with `Digest::parse(...)?`.
- **#3** `Token::parse` and ECR base64 decoding now return typed `InvalidAuth` errors instead of panicking on malformed config.
- **#4** Replaced `join_all` with `try_join_all` in `Image::to_tarball`, `Index::to_oci`, and CLI `copy`/`push` so the first error aborts the operation.
- **#5/#19** Chunked-upload `Location` handling: `start_blob_upload` parses and resolves the upload URL; `upload_part` and `finish_blob_upload` PATCH/PUT against the URL returned by the previous response (preserving any pre-existing query string) instead of reformatting `/v2/{repo}/blobs/uploads/{upload}`.
- **#6** Removed all `_progress` API duplication: `Layer::create`, `Layer::open`, `Image::filesystem`, `Image::to_tarball`, and `Index::to_oci` now take `Option<&dyn ProgressReporter>` (or `SharedProgress` where spawning is required).
- **#7** Compression: `lz4` now returns `Error::Unsupported` instead of misusing the LZMA decoder. `Decompress::new` is fallible; `DockerImageRootfs(None)` passes through uncompressed.
- **#8** Writer rewritten as an explicit state machine (`Initial → Starting → Idle ↔ Uploading → Finishing → Done`) with no `wake_by_ref`. Hash and offset advance only after the registry confirms each part. `Done` rejects further writes; `poll_shutdown` drains.
- **#9** Auth precedence enforced: `.finch` → `.docker` → keyring; loop breaks on the first hit and never overwrites a resolved token with `None`.
- **#10** Layer copy now advances by `read_size`, not `chunk_size` (was a real bug for `size < MIN_CHUNK_SIZE`); covered by `copy_handles_short_reads` test.
- **#11** Spawned `JoinError`s now map to a typed `Error::Join` variant rather than being context-shifted into a generic error.
- **#12** `Reader::poll_read` ticks progress by `after - before` per poll instead of only at EOF.
- **#13** Reader validates streamed size and digest against expected values at EOF and returns `LayerSizeMismatch` / `DigestMismatch`.
- **#14** `RegistryClient` upload methods now take `&self`.
- **#15** `Ctx` exposes a single Arc-backed `SharedProgress` instead of leaking a `MultiProgress`; spawned tasks clone the `Arc` rather than relying on `unsafe` lifetime extension.

## [0.1.0](https://github.com/awslabs/ocilot/compare/v0.1.0-beta.2...v0.1.0) - 2026-04-30

### Fixed

- Improve doc comment formatting and remove review artifacts
- Update documentation and styling
- ECR repository names for both private and public
- Public ECR list tags operation

### Changed

- Update dependencies, fix edition and migrate to bon

### Other

- Initial open source release

## [0.1.0-beta.2](https://github.com/awslabs/ocilot/compare/v0.1.0-beta.1...v0.1.0-beta.2) - 2025-06-03

### Other

- enable github release in release-plz

## [0.1.0-beta.1](https://github.com/awslabs/ocilot/releases/tag/v0.1.0-beta.1) - 2025-06-03

### Fixed

- ecr repository names for both private and public
- replace templates in readme
- fix public ecr list tags operation

### Other

- add description and setup release-plz
- update version to reflect beta
- add cargo deny
- standardize on tracing and migrate to correct dependency versions
- open source current state of ocilot
- Initial commit
