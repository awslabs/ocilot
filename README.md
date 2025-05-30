# Ocilot

Ocilot is a command-line tool and Rust library for interacting with OCI (Open Container Initiative) images and container registries.

## Features

- Push and pull OCI images to/from container registries
- List images and tags in repositories
- Image manifest inspection and manipulation
- Copy images between registries
- Export images as filesystem archives
- Export images as a docker loadable tarball or as an oci image archive

## Installation

### Using Cargo

```bash
cargo install --git https://github.com/awslabs/ocilot.git
```

### From Source

```bash
git clone https://github.com/awslabs/ocilot.git
cd ocilot
cargo build --release
```

## CLI Usage Examples

```bash
# List images in a repository
ocilot list myregistry.com/myrepository
# Pull an image as an oci archive
ocilot pull myregistry.com/myrepository:latest archive.tar
# Pull an image with specific platform as a loadable tarball
ocilot pull --format=tarball myregistry.com/myrepository:latest archive.tar
# Push an oci image archive to a registry
ocilot push oci_image.tar myregistry.com/myrepository:latest
# Copy from one registry to another
ocilot copy source.io/mysource:v1.0.0 target.io/mytarget:v1.0.0
```

## Library Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
ocilot = "0.1.0"
```

### Features

- **progress** - Enable support for logging progress of push and pull operations to progressbars using indicatif
- **compression** - Enables support for automatically decompressing layers based off media type.

## Authentication

Ocilot will handle automatic authorization with aws ecr both private and public based on the aws credentials in the calling environment. Any other registry credentials must be done via using `docker login`

## Security

See [CONTRIBUTING](CONTRIBUTING.md#security-issue-notifications) for more information.

## License

This project is licensed under the Apache-2.0 License.
